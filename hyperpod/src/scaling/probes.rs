//! Real host load probes and CPU-share sinks. No synthetic fallbacks: if the
//! OS-specific facility is unavailable, construction returns an error and the
//! caller can decide whether to abort.

#[cfg(target_os = "linux")]
pub use linux_impl::{discover_cgroup_share_sink, CgroupShareSink, ProcLoadProbe};

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::{bail, Context, Result};

    use crate::scaling::{CpuShareSink, LoadProbe};

    pub struct ProcLoadProbe {
        num_cpus: f64,
    }

    impl ProcLoadProbe {
        pub fn new() -> Result<Self> {
            let cpuinfo = fs::read_to_string("/proc/cpuinfo").context("read /proc/cpuinfo")?;
            let n = cpuinfo
                .lines()
                .filter(|l| l.starts_with("processor"))
                .count()
                .max(1);
            Ok(Self {
                num_cpus: n as f64,
            })
        }
    }

    impl LoadProbe for ProcLoadProbe {
        fn load_normalized(&self) -> Result<f64> {
            let s = fs::read_to_string("/proc/loadavg").context("read /proc/loadavg")?;
            let first = s
                .split_whitespace()
                .next()
                .context("/proc/loadavg empty")?;
            let load1: f64 = first.parse().context("parse loadavg")?;
            Ok((load1 / self.num_cpus).clamp(0.0, 1.0))
        }
    }

    /// Writes integer values to a cgroup-v2 `cpu.weight` file (valid range
    /// 1..=10_000). Values outside that range are clamped before the write.
    pub struct CgroupShareSink {
        path: PathBuf,
    }

    impl CgroupShareSink {
        pub fn new(path: PathBuf) -> Result<Self> {
            if !path.is_file() {
                bail!("cgroup file {} does not exist", path.display());
            }
            // Probe writability so the failure surfaces here rather than on the
            // first tick.
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .with_context(|| format!("open {} for writing", path.display()))?;
            Ok(Self { path })
        }
    }

    impl CpuShareSink for CgroupShareSink {
        fn set_shares(&self, shares: u64) -> Result<()> {
            let v = shares.clamp(1, 10_000);
            fs::write(&self.path, v.to_string())
                .with_context(|| format!("write {} = {v}", self.path.display()))
        }
    }

    /// Discover the current process's cgroup-v2 `cpu.weight` path by parsing
    /// `/proc/self/cgroup` and joining it under `/sys/fs/cgroup`.
    pub fn discover_cgroup_share_sink() -> Result<CgroupShareSink> {
        let raw = fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?;
        // cgroup-v2 unified hierarchy is a single line "0::/path".
        let line = raw
            .lines()
            .find(|l| l.starts_with("0::"))
            .context("no cgroup-v2 line in /proc/self/cgroup; HyperPod requires cgroup v2 for CPU scaling")?;
        let rel = line.trim_start_matches("0::").trim();
        let rel = rel.trim_start_matches('/');
        let path: PathBuf = Path::new("/sys/fs/cgroup").join(rel).join("cpu.weight");
        CgroupShareSink::new(path)
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{GetSystemTimesProbe, JobObjectCpuRateSink};

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::sync::Mutex;

    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
        SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetSystemTimes};

    use crate::scaling::{CpuShareSink, LoadProbe};

    fn ft_to_u64(ft: FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
    }

    fn snapshot() -> Result<(u64, u64)> {
        let mut idle = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut kernel = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut user = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        if ok == 0 {
            bail!("GetSystemTimes failed");
        }
        let idle_t = ft_to_u64(idle);
        let total_t = ft_to_u64(kernel).wrapping_add(ft_to_u64(user));
        Ok((idle_t, total_t))
    }

    /// System-wide CPU load via `GetSystemTimes`. The probe stores the last
    /// snapshot and reports the busy fraction since the previous read.
    pub struct GetSystemTimesProbe {
        last: Mutex<(u64, u64)>,
    }

    impl GetSystemTimesProbe {
        pub fn new() -> Result<Self> {
            let s = snapshot()?;
            Ok(Self {
                last: Mutex::new(s),
            })
        }
    }

    impl LoadProbe for GetSystemTimesProbe {
        fn load_normalized(&self) -> Result<f64> {
            let now = snapshot()?;
            let mut last = self.last.lock().unwrap();
            let didle = now.0.saturating_sub(last.0);
            let dtotal = now.1.saturating_sub(last.1);
            *last = now;
            if dtotal == 0 {
                return Ok(0.0);
            }
            let load = 1.0 - (didle as f64 / dtotal as f64);
            Ok(load.clamp(0.0, 1.0))
        }
    }

    /// Caps the host CPU rate of the VMM process by assigning it to a Job
    /// Object and updating `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` with a
    /// hard CpuRate. Rate is expressed in hundredths of a percent in the range
    /// 1..=10_000 (e.g. 5_000 = 50%).
    pub struct JobObjectCpuRateSink {
        job: HANDLE,
    }

    // The HANDLE is owned by this struct and only used from within methods
    // protected by the OS-level job-object synchronization.
    unsafe impl Send for JobObjectCpuRateSink {}
    unsafe impl Sync for JobObjectCpuRateSink {}

    impl JobObjectCpuRateSink {
        pub fn new() -> Result<Self> {
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                bail!("CreateJobObjectW failed");
            }
            let proc = unsafe { GetCurrentProcess() };
            let ok = unsafe { AssignProcessToJobObject(job, proc) };
            if ok == 0 {
                unsafe {
                    CloseHandle(job);
                }
                bail!(
                    "AssignProcessToJobObject failed (this process may already be in a job that disallows nesting)"
                );
            }
            Ok(Self { job })
        }
    }

    impl CpuShareSink for JobObjectCpuRateSink {
        fn set_shares(&self, shares: u64) -> Result<()> {
            let rate = shares.clamp(1, 10_000) as u32;
            let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION =
                unsafe { std::mem::zeroed() };
            info.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            // The CpuRate field lives in the anonymous union at offset 4.
            // Union *writes* are safe in Rust 2021; only reads require `unsafe`.
            info.Anonymous.CpuRate = rate;
            let ok = unsafe {
                SetInformationJobObject(
                    self.job,
                    JobObjectCpuRateControlInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                bail!("SetInformationJobObject(CpuRateControl) failed");
            }
            Ok(())
        }
    }

    impl Drop for JobObjectCpuRateSink {
        fn drop(&mut self) {
            if !self.job.is_null() {
                unsafe {
                    CloseHandle(self.job);
                }
            }
        }
    }
}
