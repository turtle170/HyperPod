# HyperPod

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

A lightweight, open-source virtual machine manager and scaling engine written
in safe Rust. HyperPod targets **Windows-first** via the Windows Hypervisor
Platform (WHPX) API, with a secondary backend for **Linux** via KVM.

> **Status.** Alpha (0.1.x). The CLI, configuration, scaling engine, storage
> detection, token-bucket throttler, and the WHPX / KVM partition setup are
> implemented and unit-tested. Booting a real `vmlinux` end-to-end requires a
> Linux kernel image and rootfs; the VirtIO block device wiring is structurally
> in place but awaits interrupt-injection integration before a guest can mount
> a rootfs through it. See **Validation status** below for the precise
> bar each component meets today.

---

## Why HyperPod

* **Native on Windows.** Most lightweight Rust VMMs (Firecracker, Cloud
  Hypervisor) target Linux/KVM only. HyperPod runs natively on Windows by
  driving the Hyper-V `WinHvPlatform.dll` API directly. No QEMU, no Docker
  Desktop, no WSL detour.
* **Burstable scaling.** A `rayon`-powered monitor watches host CPU load and
  re-tunes the VMM's CPU quota in real time:
  * Windows: JobObject `CpuRate` (hundredths of a percent, 1..=10_000).
  * Linux: cgroup-v2 `cpu.weight` (1..=10_000).
* **Small, readable, safe Rust.** All `unsafe` is wrapped in thin RAII helpers
  (`Partition`, `GuestRam`). The hot paths use only safe APIs.
* **One config, multiple disk formats.** `HyperPod.toml` declares the kernel,
  the rootfs path, RAM / CPU limits, and an optional GPU policy. Rootfs can be
  `.raw` / `.img` / `.ext4` (raw block image) or a **fixed** `.vhd`. VHDX and
  dynamic VHD are rejected with a clear, actionable error.

---

## Architecture

```
                        ┌───────────────────────┐
                        │   hyperpod (CLI)      │
                        └──────────┬────────────┘
                                   │
                   ┌───────────────┴──────────────┐
                   ▼                              ▼
          ┌────────────────┐             ┌─────────────────┐
          │  vmm::common   │             │     scaling     │
          │  (cross-plat)  │             │  Monitor + Probe│
          │ ─ GuestRam     │             │  (rayon worker) │
          │ ─ ELF loader   │             └────────┬────────┘
          │ ─ Boot tables  │                      │
          │ ─ 8250 serial  │              ┌───────┴────────┐
          │ ─ Storage      │              ▼                ▼
          │ ─ Device layer │      Windows                Linux
          │   (virtio-mmio │   GetSystemTimes         /proc/loadavg
          │    + token-   │   JobObject              cgroup-v2
          │    bucket)    │   CpuRateControl         cpu.weight
          └────────┬───────┘
                   │
        ┌──────────┴──────────┐
        ▼                     ▼
 ┌──────────────┐      ┌──────────────┐
 │ vmm::windows │      │ vmm::linux   │
 │   (WHPX)     │      │    (KVM)     │
 │  Partition / │      │  Kvm / VmFd /│
 │  vCPU / IO   │      │  VcpuFd      │
 └──────────────┘      └──────────────┘
```

### Source layout

```
hyperpod/
├── Cargo.toml
└── src/
    ├── main.rs / cli.rs / commands/      # CLI surface
    ├── config.rs                         # HyperPod.toml parsing + validation
    ├── scaling/
    │   ├── mod.rs                        # Monitor (CPU-share tuning)
    │   └── probes.rs                     # Win: GetSystemTimes + JobObject
    │                                     # Linux: /proc/loadavg + cgroup v2
    └── vmm/
        ├── common/                       # cross-platform building blocks
        │   ├── ram.rs                    # GuestRam (VirtualAlloc / mmap)
        │   ├── elf.rs                    # vmlinux ELF64 loader
        │   ├── boot.rs                   # GDT / page tables / boot_params
        │   ├── serial.rs                 # 8250 UART
        │   ├── device/                   # VirtIO-MMIO + token-bucket
        │   └── storage/                  # raw + fixed-VHD backends
        ├── windows/                      # WHPX backend (Primary)
        └── linux/                        # KVM backend (Secondary)
```

---

## Quick start

### Prerequisites

**Windows host (primary):**
1. Windows 10 / 11 Pro, Enterprise, or IoT Enterprise. (Home requires extra
   steps to enable the hypervisor platform.)
2. Intel VT-x or AMD-V enabled in firmware.
3. The **Windows Hypervisor Platform** optional feature enabled:
   ```powershell
   # Run as administrator, then reboot.
   Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
   ```
4. Rust toolchain (stable or nightly, edition 2021+).

**Linux host (secondary):**
1. KVM available (`/dev/kvm` readable/writable by the running user; usually the
   `kvm` group).
2. cgroup v2 unified hierarchy mounted at `/sys/fs/cgroup` (default on most
   modern distros).
3. Rust toolchain.

### Build

```bash
cargo build --release
```

The binary lands at `target/release/hyperpod`.

### Run

Author a `HyperPod.toml`:

```toml
rootfs_path = "C:/vm/rootfs.raw"
kernel_path = "C:/vm/vmlinux"
cmdline     = "console=ttyS0 reboot=k panic=1 root=/dev/vda rw"

[limits]
min_ram        = 128      # MiB
max_ram        = 1024
min_cpu_shares = 100      # 1.00%
max_cpu_shares = 5000     # 50.00%

# Optional. Captures GPU intent in config; enforcement is not yet implemented.
[gpu]
mode               = "credits"
credits_per_second = 500
max_credits        = 5000
# Or: mode = "full"
```

Launch:

```bash
hyperpod start ./HyperPod.toml
```

`hyperpod status` reports on the local runtime (currently a placeholder while
the runtime daemonization story is being designed).

---

## `HyperPod.toml` schema

| Field                       | Required | Description                                                                            |
|----------------------------|----------|----------------------------------------------------------------------------------------|
| `rootfs_path`              | yes      | Path to the rootfs image. See **Disk formats** below.                                 |
| `kernel_path`              | yes      | Path to an uncompressed Linux `vmlinux` ELF64 (x86_64, little-endian).                 |
| `cmdline`                  | yes      | Kernel command line passed via `boot_params.hdr.cmd_line_ptr`.                         |
| `limits.min_ram`           | yes      | Guest RAM floor, in MiB.                                                               |
| `limits.max_ram`           | yes      | Guest RAM ceiling (burstable target).                                                  |
| `limits.min_cpu_shares`    | yes      | Initial CPU quota. Range 1..=10_000 (Linux cgroup v2 `cpu.weight` semantics).          |
| `limits.max_cpu_shares`    | yes      | Quota ceiling the scaler may raise to on bursts.                                       |
| `gpu`                      | no       | See **GPU policy** below.                                                              |

### Disk formats

`rootfs_path` is opened via the `storage::open` dispatcher:

| Extension / magic              | Backend                  | Status                          |
|--------------------------------|--------------------------|---------------------------------|
| `.raw`, `.img`, `.ext4`        | `RawBackend`             | ✅ Supported                    |
| Fixed `.vhd` (cookie `conectix`, `disk_type=2`) | `VhdFixedBackend` | ✅ Supported                    |
| Dynamic `.vhd` (`disk_type=3,4`) | —                      | ❌ Hard-error, convert via `qemu-img convert -O vpc -o subformat=fixed` |
| `.vhdx`                        | —                        | ❌ Hard-error, convert via `qemu-img convert -O raw` |

Format is detected by extension first, then verified by magic (`vhdxfile` head
for VHDX, `conectix` footer for VHD).

### GPU policy

`[gpu]` declares intent for the host's GPU admission layer:

```toml
[gpu]
mode               = "credits"
credits_per_second = 500
max_credits        = 5000
```

or

```toml
[gpu]
mode = "full"
```

**Status:** the policy is parsed and validated; **enforcement is not
implemented** in 0.1.x. Surfacing GPU acceleration into the guest requires
either (a) a paravirt graphics device (`virtio-gpu` + Venus / VirGL with a
guest driver) or (b) PCIe passthrough (VFIO on Linux, DDA on Windows server
SKUs). Neither is shipped here. The field exists so configs can declare intent
ahead of the upcoming graphics-device work.

---

## Scaling engine

A `rayon::ThreadPool` runs one worker that polls host load every 500 ms and
adjusts the VMM's CPU quota inside the configured `[min_cpu_shares,
max_cpu_shares]` range:

* **Load probe:**
  * Windows: `GetSystemTimes` (system-wide busy fraction since last sample).
  * Linux: `/proc/loadavg` divided by core count from `/proc/cpuinfo`.
* **Quota sink:**
  * Windows: `SetInformationJobObject` with
    `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` (`ControlFlags = ENABLE |
    HARD_CAP`, `CpuRate = shares`). The VMM process is assigned to the
    auto-created job object on startup.
  * Linux: writes the integer share value to the current process's cgroup-v2
    `cpu.weight` file (discovered by parsing `/proc/self/cgroup`).
* **Thresholds:** `load ≥ 0.80` triggers a 25%-of-headroom step up,
  `load ≤ 0.20` triggers a 25%-of-slack step down. Inert otherwise.

There are no synthetic-load fallbacks. If the OS-specific probe / sink cannot
be constructed (`/proc/loadavg` unreadable, cgroup v2 not mounted, job object
nesting refused), `hyperpod start` reports the error and exits — no fake
metrics.

---

## Phase 4 device layer

* **`device::throttle::TokenBucket`** — full implementation with adjustable
  capacity and refill rate. Six unit tests cover initial consumption, refill
  over time, zero-rate behaviour, rate updates, and capacity clamping. The
  scaling monitor can call `set_rate` to widen the throttle on a burst.
* **`device::mmio::VirtioMmio`** — VirtIO-MMIO transport (Version 2). Handles
  the magic / version / device-id reads, feature-bit negotiation, queue
  configuration (`QueueDescLow/High`, `QueueAvailLow/High`, `QueueUsedLow/High`,
  `QueueNum`, `QueueReady`), and signals queue-notify writes.
* **`device::mmio::DescriptorChain`** — walks the available ring, follows the
  `VRING_DESC_F_NEXT` chain in guest memory, and bumps `last_avail_idx`. Unit
  tested against a synthetic `GuestRam`.

**Not yet wired to a run loop.** The Linux `virtio-blk` driver requires
interrupts when a request completes; HyperPod does not yet inject interrupts
into either backend (WHPX `WHvRequestInterrupt` / KVM `irqfd`). Until that
lands, the device pieces above are exercised only by their unit tests. The
hard-error policy (no `LoggingSink`, no skeleton balloon) means we don't ship
a placeholder block device that pretends to work.

---

## WHPX backend (Windows)

* `WHvCreatePartition` → `WHvSetPartitionProperty(ProcessorCount=1)` →
  `WHvSetPartitionProperty(LocalApicEmulationMode=XApic)` → `WHvSetupPartition`.
* Guest RAM is allocated with `VirtualAlloc(MEM_COMMIT | MEM_RESERVE,
  PAGE_READWRITE)` and handed to `WHvMapGpaRange` with R/W/X flags.
* vCPU registers are programmed for 64-bit long mode:
  CS (`L=1`, G=1, `0xa09b`), DS/ES/FS/GS/SS (`0xc093`), TR (`0x008b`), LDTR
  (`0x0082`), GDTR (base `0x500`, limit 31), IDTR (base `0x520`, limit 7),
  `CR0 = PE|PG`, `CR3 = 0x9000`, `CR4 = PAE`, `EFER = LME|LMA`,
  `RFLAGS = 0x2`, RIP = ELF entry, RSP = `0x8ff0`, RSI = `0x7000`
  (boot_params).
* Run loop handles `X64Halt`, `UnrecoverableException`, `X64IoPortAccess`
  (serial PIO → host stdout, advancing RIP by `InstructionByteCount`),
  `MemoryAccess` (logged, RIP advanced), and `Canceled`.

## KVM backend (Linux)

Mirrors the WHPX flow on top of `kvm-ioctls`: `Kvm::new` → `create_vm` →
`set_tss_address(0xfffbd000)` → `set_user_memory_region` (zero-copy, points
into the same `GuestRam` as the Windows path) → `create_vcpu` → `set_cpuid2`
(pass-through from `get_supported_cpuid`) → `set_sregs` / `set_regs` mirroring
the same long-mode constants → `vcpu.run()`.

---

## Validation status

| Component                              | Built | Unit-tested | End-to-end on real hardware |
|----------------------------------------|-------|-------------|-----------------------------|
| `HyperPod.toml` parse + validation     | ✅    | ✅ (8 tests) | n/a                         |
| Scaling Monitor (decision logic)       | ✅    | ✅ (2 tests) | n/a                         |
| Linux probes (`/proc/loadavg`, cgroup) | ✅    | —           | pending Linux host          |
| Windows probes (GetSystemTimes, JobObject CpuRate) | ✅ | — | smoke-passes on this Windows host |
| `GuestRam` (`VirtualAlloc` / `mmap`)   | ✅    | ✅ (2 tests) | works on this Windows host  |
| ELF loader                             | ✅    | —           | header validation works     |
| Boot artefacts (GDT, page tables, boot_params) | ✅ | —     | runs to vCPU configure      |
| Storage `raw`                          | ✅    | ✅ (3 tests) | n/a                         |
| Storage fixed-VHD                      | ✅    | ✅ (2 tests) | n/a                         |
| Storage detection (`.vhdx` → error)    | ✅    | ✅ (3 tests) | n/a                         |
| `TokenBucket`                          | ✅    | ✅ (6 tests) | n/a                         |
| VirtIO-MMIO register file              | ✅    | ✅ (3 tests) | not wired                   |
| Descriptor-chain walker                | ✅    | ✅ (1 test)  | not wired                   |
| WHPX partition + memory + vCPU setup   | ✅    | —           | reaches ELF load on this host |
| WHPX run loop (PIO / MMIO / HLT)       | ✅    | —           | needs a real `vmlinux`      |
| KVM run loop                           | ✅    | —           | needs a Linux host          |
| Interrupt injection (for virtio-blk)   | —     | —           | future work                  |
| GPU enforcement                        | —     | —           | future work                  |

`cargo test` on Windows: **29 passed, 0 failed**.

To validate the WHPX boot path end-to-end:

```powershell
hyperpod start .\HyperPod.toml
# Expected sequence with a real vmlinux + empty rootfs:
#   HyperPod configuration loaded from ...
#   hyperpod[whpx]: entering vCPU at 0x... with 128 MiB RAM
#   monitor: starting (shares 100..5000, every 500ms)
#   <kernel boot messages via emulated 8250>
#   <kernel panic: VFS: Unable to mount root fs>  ← expected with no virtio-blk
#   hyperpod[whpx]: guest unrecoverable exception
```

---

## Roadmap

* **0.1.x → 0.2.0** — interrupt injection (`WHvRequestInterrupt` + KVM
  `irqfd`) wire virtio-blk into the run loops. Promote `device/mmio` from
  tested-but-not-wired to wired.
* **0.2.x** — `vmm-status` daemon: track running VMs across CLI invocations.
* **0.3.x** — virtio-net.
* **0.4.x** — paravirt graphics (start with `virtio-gpu` + VirGL). At that
  point the GPU policy starts enforcing.
* **Later** — dynamic VHD, VHDX, multi-vCPU, SMP scheduling.

---

## Contributing

Issues and PRs welcome on
[github.com/turtle170/HyperPod](https://github.com/turtle170/HyperPod).
By contributing you agree your work is licensed under Apache-2.0.

---

## License

Licensed under the **Apache License, Version 2.0**. See [LICENSE](LICENSE).
