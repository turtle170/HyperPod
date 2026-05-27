use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use windows_sys::Win32::System::Hypervisor::{
    WHvPartitionPropertyCodeLocalApicEmulationMode, WHvPartitionPropertyCodeProcessorCount,
    WHvRunVpExitReasonCanceled, WHvRunVpExitReasonMemoryAccess, WHvRunVpExitReasonNone,
    WHvRunVpExitReasonUnrecoverableException, WHvRunVpExitReasonX64Halt,
    WHvRunVpExitReasonX64IoPortAccess, WHvX64LocalApicEmulationModeXApic, WHvX64RegisterCr0,
    WHvX64RegisterCr3, WHvX64RegisterCr4, WHvX64RegisterCs, WHvX64RegisterDs, WHvX64RegisterEfer,
    WHvX64RegisterEs, WHvX64RegisterFs, WHvX64RegisterGdtr, WHvX64RegisterGs,
    WHvX64RegisterIdtr, WHvX64RegisterLdtr, WHvX64RegisterRax, WHvX64RegisterRflags,
    WHvX64RegisterRip, WHvX64RegisterRsi, WHvX64RegisterRsp, WHvX64RegisterSs, WHvX64RegisterTr,
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_X64_LOCAL_APIC_EMULATION_MODE,
    WHV_X64_SEGMENT_REGISTER, WHV_X64_SEGMENT_REGISTER_0, WHV_X64_TABLE_REGISTER,
};

use crate::config::HyperPodFile;
use crate::scaling::{
    probes::{GetSystemTimesProbe, JobObjectCpuRateSink},
    CpuShareSink, LoadProbe, Monitor, MonitorState, ScalingPolicy,
};
use crate::vmm::common::boot::{
    setup_page_tables, write_cmdline, write_gdt, write_zero_page, BOOT_GDT_OFFSET,
    BOOT_IDT_OFFSET, BOOT_STACK_POINTER, PML4_START, ZERO_PAGE_START,
};
use crate::vmm::common::ram::GuestRam;
use crate::vmm::common::serial::Serial;
use crate::vmm::common::{elf, storage};

use super::partition::Partition;

const KCS_SELECTOR: u16 = 0x08;
const KDS_SELECTOR: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;

const CR0_PE: u64 = 1;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

pub fn run_vm(cfg: &HyperPodFile) -> Result<()> {
    // Validate the rootfs opens cleanly before standing up the partition.
    let _backend = storage::open(&cfg.rootfs_path)?;

    let ram = GuestRam::new(cfg.limits.min_ram)?;
    let partition = Partition::create()?;

    let processor_count: u32 = 1;
    partition.set_property(WHvPartitionPropertyCodeProcessorCount, &processor_count)?;
    let apic_mode: WHV_X64_LOCAL_APIC_EMULATION_MODE = WHvX64LocalApicEmulationModeXApic;
    partition.set_property(
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        &apic_mode,
    )?;
    partition.setup()?;
    partition.map_memory(ram.host_ptr(), 0, ram.size())?;

    write_gdt(&ram)?;
    setup_page_tables(&ram)?;
    write_cmdline(&ram, &cfg.cmdline)?;
    write_zero_page(&ram, cfg.cmdline.len(), cfg.limits.min_ram * 1024 * 1024)?;

    let kernel = elf::load(&cfg.kernel_path, &ram)?;

    partition.create_vcpu(0)?;
    set_long_mode_registers(&partition, 0, kernel.entry)?;

    let scaling = start_scaling(&cfg.limits)?;
    let serial = Serial::new();

    eprintln!(
        "hyperpod[whpx]: entering vCPU at 0x{:x} with {} MiB RAM",
        kernel.entry, cfg.limits.min_ram
    );

    let result = run_loop(&partition, &serial);
    scaling.state.shutdown.store(true, Ordering::Release);
    result
}

struct ScalingHandles {
    state: Arc<MonitorState>,
    _pool: rayon::ThreadPool,
}

fn start_scaling(limits: &crate::config::Limits) -> Result<ScalingHandles> {
    let policy = ScalingPolicy::from_limits(limits);
    let state = MonitorState::new(&policy);

    let load: Arc<dyn LoadProbe> = Arc::new(GetSystemTimesProbe::new()?);
    let sink: Arc<dyn CpuShareSink> = Arc::new(JobObjectCpuRateSink::new()?);

    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("hyperpod-scaler-{i}"))
        .build()
        .context("build rayon pool")?;

    let monitor = Monitor {
        policy,
        state: state.clone(),
        load,
        sink,
    };
    pool.spawn(move || monitor.run_loop());
    Ok(ScalingHandles { state, _pool: pool })
}

fn run_loop(partition: &Partition, serial: &Serial) -> Result<()> {
    // The WHV_RUN_VP_EXIT_REASON_* constants come from windows-sys in PascalCase
    // to mirror the Win32 headers; suppress the Rust style lint on the patterns.
    #[allow(non_upper_case_globals)]
    loop {
        let exit = partition.run_vcpu(0)?;
        match exit.ExitReason {
            WHvRunVpExitReasonNone => continue,
            WHvRunVpExitReasonX64Halt => {
                eprintln!("hyperpod[whpx]: guest HLT");
                return Ok(());
            }
            WHvRunVpExitReasonUnrecoverableException => {
                eprintln!("hyperpod[whpx]: guest unrecoverable exception");
                return Ok(());
            }
            WHvRunVpExitReasonX64IoPortAccess => {
                let io = unsafe { exit.Anonymous.IoPortAccess };
                let port = io.PortNumber;
                let raw_info = unsafe { io.AccessInfo.AsUINT32 };
                let is_write = (raw_info & 1) != 0;
                let access_size = ((raw_info >> 1) & 0x7) as usize;
                let next_rip = exit.VpContext.Rip + io.InstructionByteCount as u64;

                if Serial::handles(port) && is_write {
                    let bytes = io.Rax.to_le_bytes();
                    let n = access_size.min(4).max(1);
                    serial.write(port, &bytes[..n]);
                    partition.set_registers(
                        0,
                        &[WHvX64RegisterRip],
                        &[WHV_REGISTER_VALUE { Reg64: next_rip }],
                    )?;
                } else if Serial::handles(port) {
                    let mut buf = [0u8; 4];
                    let n = access_size.min(4).max(1);
                    serial.read(port, &mut buf[..n]);
                    let new_rax = u32::from_le_bytes(buf) as u64;
                    partition.set_registers(
                        0,
                        &[WHvX64RegisterRax, WHvX64RegisterRip],
                        &[
                            WHV_REGISTER_VALUE { Reg64: new_rax },
                            WHV_REGISTER_VALUE { Reg64: next_rip },
                        ],
                    )?;
                } else {
                    if is_write {
                        eprintln!("hyperpod[whpx]: unhandled OUT 0x{port:x}");
                    } else {
                        eprintln!("hyperpod[whpx]: unhandled IN 0x{port:x}");
                    }
                    partition.set_registers(
                        0,
                        &[WHvX64RegisterRip],
                        &[WHV_REGISTER_VALUE { Reg64: next_rip }],
                    )?;
                }
            }
            WHvRunVpExitReasonMemoryAccess => {
                let m = unsafe { exit.Anonymous.MemoryAccess };
                eprintln!("hyperpod[whpx]: unhandled MMIO @ 0x{:x}", m.Gpa);
                let next_rip = exit.VpContext.Rip + m.InstructionByteCount as u64;
                partition.set_registers(
                    0,
                    &[WHvX64RegisterRip],
                    &[WHV_REGISTER_VALUE { Reg64: next_rip }],
                )?;
            }
            WHvRunVpExitReasonCanceled => {
                eprintln!("hyperpod[whpx]: run canceled, continuing");
            }
            other => {
                eprintln!("hyperpod[whpx]: unhandled exit reason {other}");
                return Ok(());
            }
        }
    }
}

fn make_segment(selector: u16, attributes: u16, limit: u32) -> WHV_X64_SEGMENT_REGISTER {
    WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: limit,
        Selector: selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: attributes,
        },
    }
}

fn make_table(base: u64, limit: u16) -> WHV_X64_TABLE_REGISTER {
    WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Limit: limit,
        Base: base,
    }
}

fn set_long_mode_registers(p: &Partition, vp: u32, entry: u64) -> Result<()> {
    // GDT entry attribute bytes mirror what was written into the in-memory GDT:
    //   KCS (64-bit code): type=B, S=1, P=1, L=1, G=1   -> 0xa09b
    //   KDS (data):        type=3, S=1, P=1, DB=1, G=1  -> 0xc093
    //   TSS:               type=B, S=0, P=1, G=0        -> 0x008b
    //   LDT:               type=2, S=0, P=1, G=0        -> 0x0082
    let cs = make_segment(KCS_SELECTOR, 0xa09b, 0xffff_ffff);
    let ds = make_segment(KDS_SELECTOR, 0xc093, 0xffff_ffff);
    let tr = make_segment(TSS_SELECTOR, 0x008b, 0x67);
    let ldt = make_segment(0, 0x0082, 0xffff);
    let gdt = make_table(BOOT_GDT_OFFSET, 31);
    let idt = make_table(BOOT_IDT_OFFSET, 7);

    let names: [WHV_REGISTER_NAME; 18] = [
        WHvX64RegisterCs,
        WHvX64RegisterDs,
        WHvX64RegisterEs,
        WHvX64RegisterFs,
        WHvX64RegisterGs,
        WHvX64RegisterSs,
        WHvX64RegisterTr,
        WHvX64RegisterLdtr,
        WHvX64RegisterGdtr,
        WHvX64RegisterIdtr,
        WHvX64RegisterCr0,
        WHvX64RegisterCr3,
        WHvX64RegisterCr4,
        WHvX64RegisterEfer,
        WHvX64RegisterRflags,
        WHvX64RegisterRip,
        WHvX64RegisterRsp,
        WHvX64RegisterRsi,
    ];
    let values: [WHV_REGISTER_VALUE; 18] = [
        WHV_REGISTER_VALUE { Segment: cs },
        WHV_REGISTER_VALUE { Segment: ds },
        WHV_REGISTER_VALUE { Segment: ds },
        WHV_REGISTER_VALUE { Segment: ds },
        WHV_REGISTER_VALUE { Segment: ds },
        WHV_REGISTER_VALUE { Segment: ds },
        WHV_REGISTER_VALUE { Segment: tr },
        WHV_REGISTER_VALUE { Segment: ldt },
        WHV_REGISTER_VALUE { Table: gdt },
        WHV_REGISTER_VALUE { Table: idt },
        WHV_REGISTER_VALUE { Reg64: CR0_PE | CR0_PG },
        WHV_REGISTER_VALUE { Reg64: PML4_START },
        WHV_REGISTER_VALUE { Reg64: CR4_PAE },
        WHV_REGISTER_VALUE { Reg64: EFER_LME | EFER_LMA },
        WHV_REGISTER_VALUE { Reg64: 0x2 },
        WHV_REGISTER_VALUE { Reg64: entry },
        WHV_REGISTER_VALUE { Reg64: BOOT_STACK_POINTER },
        WHV_REGISTER_VALUE { Reg64: ZERO_PAGE_START },
    ];
    p.set_registers(vp, &names, &values)
}
