use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HyperPodFile {
    pub rootfs_path: PathBuf,
    pub kernel_path: PathBuf,
    pub cmdline: String,
    pub limits: Limits,
    #[serde(default)]
    pub gpu: Option<GpuPolicy>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Minimum guest RAM, in MiB.
    pub min_ram: u64,
    /// Maximum guest RAM, in MiB. Burstable ceiling.
    pub max_ram: u64,
    /// Minimum CPU "shares" assigned to the VMM process (cgroup v2 `cpu.weight`
    /// on Linux, JobObject `CpuRate` in hundredths-of-a-percent on Windows).
    /// Valid range: 1..=10_000.
    pub min_cpu_shares: u64,
    /// Maximum CPU shares the scaler may grant on a burst.
    pub max_cpu_shares: u64,
}

/// Per-VM GPU policy. The host honours this policy at the host side (resource
/// admission, accounting). Actually surfacing GPU acceleration into the guest
/// requires a paravirtualised graphics device (virtio-gpu + Venus / VirGL) or
/// PCIe passthrough — neither is shipped in 0.1.x; this field declares intent.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, tag = "mode", rename_all = "snake_case")]
pub enum GpuPolicy {
    /// Credit-based admission. The VM may consume up to `max_credits` and
    /// refills at `credits_per_second`.
    Credits {
        credits_per_second: u64,
        max_credits: u64,
    },
    /// Full, unrestricted GPU access (no credit accounting on the host side).
    Full,
}

impl HyperPodFile {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read HyperPod file {}", path.display()))?;
        let parsed: HyperPodFile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse HyperPod file {}", path.display()))?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> Result<()> {
        ensure_file_exists(&self.kernel_path, "kernel_path")?;
        ensure_file_exists(&self.rootfs_path, "rootfs_path")?;

        if self.cmdline.trim().is_empty() {
            bail!("cmdline must not be empty");
        }

        let l = &self.limits;
        if l.min_ram == 0 {
            bail!("limits.min_ram must be greater than zero");
        }
        if l.max_ram < l.min_ram {
            bail!(
                "limits.max_ram ({}) must be >= limits.min_ram ({})",
                l.max_ram,
                l.min_ram
            );
        }
        if l.min_cpu_shares == 0 {
            bail!("limits.min_cpu_shares must be greater than zero");
        }
        if l.max_cpu_shares < l.min_cpu_shares {
            bail!(
                "limits.max_cpu_shares ({}) must be >= limits.min_cpu_shares ({})",
                l.max_cpu_shares,
                l.min_cpu_shares
            );
        }
        if l.min_cpu_shares > 10_000 || l.max_cpu_shares > 10_000 {
            bail!("cpu_shares values must lie in 1..=10000 (matches cgroup v2 cpu.weight and JobObject CpuRate)");
        }

        if let Some(GpuPolicy::Credits {
            credits_per_second,
            max_credits,
        }) = self.gpu
        {
            if credits_per_second == 0 || max_credits == 0 {
                bail!("gpu.credits values must be greater than zero");
            }
        }

        Ok(())
    }
}

fn ensure_file_exists(path: &Path, field: &str) -> Result<()> {
    let meta = fs::metadata(path)
        .with_context(|| format!("{field} `{}` is not accessible", path.display()))?;
    if !meta.is_file() {
        bail!("{field} `{}` is not a regular file", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn touch(dir: &TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::File::create(&path).unwrap();
        path
    }

    fn write_config(tmp: &TempDir, body: &str) -> PathBuf {
        let path = tmp.path().join("HyperPod.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parses_valid_config() {
        let tmp = TempDir::new().unwrap();
        let kernel = touch(&tmp, "vmlinux");
        let rootfs = touch(&tmp, "rootfs.ext4");
        let cfg = write_config(
            &tmp,
            &format!(
                r#"
rootfs_path = "{rootfs}"
kernel_path = "{kernel}"
cmdline = "console=ttyS0 reboot=k panic=1"

[limits]
min_ram = 128
max_ram = 512
min_cpu_shares = 256
max_cpu_shares = 1024
"#,
                rootfs = rootfs.display().to_string().replace('\\', "\\\\"),
                kernel = kernel.display().to_string().replace('\\', "\\\\"),
            ),
        );
        let parsed = HyperPodFile::load(&cfg).expect("config should parse");
        assert_eq!(parsed.limits.min_ram, 128);
        assert_eq!(parsed.limits.max_ram, 512);
        assert!(parsed.gpu.is_none());
    }

    #[test]
    fn parses_gpu_credits_section() {
        let tmp = TempDir::new().unwrap();
        let kernel = touch(&tmp, "vmlinux");
        let rootfs = touch(&tmp, "rootfs.ext4");
        let cfg = write_config(
            &tmp,
            &format!(
                r#"
rootfs_path = "{rootfs}"
kernel_path = "{kernel}"
cmdline = "console=ttyS0"

[limits]
min_ram = 128
max_ram = 256
min_cpu_shares = 1
max_cpu_shares = 10000

[gpu]
mode = "credits"
credits_per_second = 100
max_credits = 1000
"#,
                rootfs = rootfs.display().to_string().replace('\\', "\\\\"),
                kernel = kernel.display().to_string().replace('\\', "\\\\"),
            ),
        );
        let parsed = HyperPodFile::load(&cfg).unwrap();
        assert!(matches!(
            parsed.gpu,
            Some(GpuPolicy::Credits {
                credits_per_second: 100,
                max_credits: 1000
            })
        ));
    }

    #[test]
    fn parses_gpu_full_access() {
        let tmp = TempDir::new().unwrap();
        let kernel = touch(&tmp, "vmlinux");
        let rootfs = touch(&tmp, "rootfs.ext4");
        let cfg = write_config(
            &tmp,
            &format!(
                r#"
rootfs_path = "{rootfs}"
kernel_path = "{kernel}"
cmdline = "x"

[limits]
min_ram = 64
max_ram = 64
min_cpu_shares = 1
max_cpu_shares = 1

[gpu]
mode = "full"
"#,
                rootfs = rootfs.display().to_string().replace('\\', "\\\\"),
                kernel = kernel.display().to_string().replace('\\', "\\\\"),
            ),
        );
        let parsed = HyperPodFile::load(&cfg).unwrap();
        assert!(matches!(parsed.gpu, Some(GpuPolicy::Full)));
    }

    #[test]
    fn rejects_zero_gpu_credits() {
        let tmp = TempDir::new().unwrap();
        let kernel = touch(&tmp, "vmlinux");
        let rootfs = touch(&tmp, "rootfs.ext4");
        let cfg = write_config(
            &tmp,
            &format!(
                r#"
rootfs_path = "{rootfs}"
kernel_path = "{kernel}"
cmdline = "x"

[limits]
min_ram = 1
max_ram = 1
min_cpu_shares = 1
max_cpu_shares = 1

[gpu]
mode = "credits"
credits_per_second = 0
max_credits = 100
"#,
                rootfs = rootfs.display().to_string().replace('\\', "\\\\"),
                kernel = kernel.display().to_string().replace('\\', "\\\\"),
            ),
        );
        let err = HyperPodFile::load(&cfg).unwrap_err();
        assert!(format!("{err:#}").contains("gpu.credits"));
    }

    #[test]
    fn rejects_missing_kernel() {
        let tmp = TempDir::new().unwrap();
        let rootfs = touch(&tmp, "rootfs.ext4");
        let cfg = write_config(
            &tmp,
            &format!(
                r#"
rootfs_path = "{rootfs}"
kernel_path = "/nonexistent/vmlinux"
cmdline = "console=ttyS0"

[limits]
min_ram = 128
max_ram = 256
min_cpu_shares = 128
max_cpu_shares = 512
"#,
                rootfs = rootfs.display().to_string().replace('\\', "\\\\"),
            ),
        );
        let err = HyperPodFile::load(&cfg).unwrap_err();
        assert!(err.to_string().contains("kernel_path"));
    }

    #[test]
    fn rejects_inverted_ram_bounds() {
        let tmp = TempDir::new().unwrap();
        let kernel = touch(&tmp, "vmlinux");
        let rootfs = touch(&tmp, "rootfs.ext4");
        let cfg = write_config(
            &tmp,
            &format!(
                r#"
rootfs_path = "{rootfs}"
kernel_path = "{kernel}"
cmdline = "x"

[limits]
min_ram = 512
max_ram = 128
min_cpu_shares = 128
max_cpu_shares = 512
"#,
                rootfs = rootfs.display().to_string().replace('\\', "\\\\"),
                kernel = kernel.display().to_string().replace('\\', "\\\\"),
            ),
        );
        let err = HyperPodFile::load(&cfg).unwrap_err();
        assert!(err.to_string().contains("max_ram"));
    }

    #[test]
    fn rejects_unknown_field() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
rootfs_path = "/tmp/r"
kernel_path = "/tmp/k"
cmdline = "x"
mystery_field = 1

[limits]
min_ram = 1
max_ram = 1
min_cpu_shares = 1
max_cpu_shares = 1
"#
        )
        .unwrap();
        let err = HyperPodFile::load(f.path()).unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(msg.contains("unknown"), "unexpected error: {msg}");
    }
}
