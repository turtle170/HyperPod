use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use kvm_bindings::{
    kvm_segment, kvm_userspace_memory_region, KVM_API_VERSION, KVM_MAX_CPUID_ENTRIES,
};
use kvm_ioctls::{Kvm, VcpuExit};

use crate::config::HyperPodFile;
use crate::scaling::{
    probes::{discover_cgroup_share_sink, ProcLoadProbe},
    CpuShareSink, LoadProbe, Monitor, MonitorState, ScalingPolicy,
};
use crate::vmm::common::boot::{
    setup_page_tables, write_cmdline, write_gdt, write_zero_page, BOOT_GDT_OFFSET,
    BOOT_IDT_OFFSET, BOOT_STACK_POINTER, PML4_START, ZERO_PAGE_START,
};
use crate::vmm::common::ram::GuestRam;
use crate::vmm::common::serial::Serial;
use crate::vmm::common::{elf, storage};

const KCS_SELECTOR: u16 = 0x08;
const KDS_SELECTOR: u16 = 0x10;
const TSS_SELECTOR: u16 = 0x18;

const CR0_PE: u64 = 1;
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;

pub fn run_vm(cfg: &HyperPodFile) -> Result<()> {
    let _backend = storage::open(&cfg.rootfs_path)?;
    let ram = GuestRam::new(cfg.limits.min_ram)?;

    let kvm = Kvm::new().context("open /dev/kvm")?;
    if kvm.get_api_version() != KVM_API_VERSION as i32 {
        bail!("unexpected KVM API version: {}", kvm.get_api_version());
    }
    let vm = kvm.create_vm().context("KVM_CREATE_VM")?;
    vm.set_tss_address(0xfffb_d000)
        .context("KVM_SET_TSS_ADDR")?;

    let region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: ram.size() as u64,
        userspace_addr: ram.host_ptr() as u64,
        flags: 0,
    };
    // SAFETY: `ram` lives for the entire run, so userspace_addr stays valid.
    unsafe { vm.set_user_memory_region(region) }
        .context("KVM_SET_USER_MEMORY_REGION")?;

    write_gdt(&ram)?;
    setup_page_tables(&ram)?;
    write_cmdline(&ram, &cfg.cmdline)?;
    write_zero_page(&ram, cfg.cmdline.len(), cfg.limits.min_ram * 1024 * 1024)?;

    let kernel = elf::load(&cfg.kernel_path, &ram)?;

    let vcpu = vm.create_vcpu(0).context("KVM_CREATE_VCPU")?;
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .context("KVM_GET_SUPPORTED_CPUID")?;
    vcpu.set_cpuid2(&cpuid).context("KVM_SET_CPUID2")?;
    configure_long_mode(&vcpu, kernel.entry)?;

    let scaling = start_scaling(&cfg.limits)?;
    let serial = Serial::new();
    eprintln!(
        "hyperpod[kvm]: entering vCPU at 0x{:x} with {} MiB RAM",
        kernel.entry, cfg.limits.min_ram
    );

    let result: Result<()> = loop {
        let exit = match vcpu.run() {
            Ok(e) => e,
            Err(e) => break Err(anyhow!(e).context("KVM_RUN")),
        };
        match exit {
            VcpuExit::IoIn(port, data) if Serial::handles(port) => serial.read(port, data),
            VcpuExit::IoOut(port, data) if Serial::handles(port) => serial.write(port, data),
            VcpuExit::IoIn(port, _) => eprintln!("hyperpod[kvm]: unhandled PIO in 0x{port:x}"),
            VcpuExit::IoOut(port, _) => eprintln!("hyperpod[kvm]: unhandled PIO out 0x{port:x}"),
            VcpuExit::MmioRead(addr, _) => {
                eprintln!("hyperpod[kvm]: unhandled MMIO read at 0x{addr:x}")
            }
            VcpuExit::MmioWrite(addr, _) => {
                eprintln!("hyperpod[kvm]: unhandled MMIO write at 0x{addr:x}")
            }
            VcpuExit::Hlt => {
                eprintln!("hyperpod[kvm]: guest HLT");
                break Ok(());
            }
            VcpuExit::Shutdown => {
                eprintln!("hyperpod[kvm]: guest shutdown (panic / triple fault)");
                break Ok(());
            }
            other => eprintln!("hyperpod[kvm]: unhandled vCPU exit {other:?}"),
        }
    };

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

    let load: Arc<dyn LoadProbe> = Arc::new(ProcLoadProbe::new()?);
    let sink: Arc<dyn CpuShareSink> = Arc::new(discover_cgroup_share_sink()?);

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

fn make_segment(selector: u16, type_: u8, s: u8, db: u8, l: u8, g: u8, limit: u32) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit,
        selector,
        type_,
        present: 1,
        dpl: 0,
        db,
        s,
        l,
        g,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

fn configure_long_mode(vcpu: &kvm_ioctls::VcpuFd, entry: u64) -> Result<()> {
    let mut sregs = vcpu.get_sregs().context("KVM_GET_SREGS")?;

    sregs.cs = make_segment(KCS_SELECTOR, 0b1011, 1, 0, 1, 1, 0xffff_ffff);
    let data = make_segment(KDS_SELECTOR, 0b0011, 1, 1, 0, 1, 0xffff_ffff);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = make_segment(TSS_SELECTOR, 0b1011, 0, 0, 0, 0, 0x67);

    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = (8 * 4) - 1;
    sregs.idt.base = BOOT_IDT_OFFSET;
    sregs.idt.limit = 8 - 1;

    sregs.cr0 = CR0_PE | CR0_PG;
    sregs.cr3 = PML4_START;
    sregs.cr4 = CR4_PAE;
    sregs.efer = EFER_LME | EFER_LMA;

    vcpu.set_sregs(&sregs).context("KVM_SET_SREGS")?;

    let mut regs = vcpu.get_regs().context("KVM_GET_REGS")?;
    regs.rflags = 0x2;
    regs.rip = entry;
    regs.rsp = BOOT_STACK_POINTER;
    regs.rsi = ZERO_PAGE_START;
    vcpu.set_regs(&regs).context("KVM_SET_REGS")?;
    Ok(())
}
