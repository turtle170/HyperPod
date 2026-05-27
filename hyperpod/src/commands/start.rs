use anyhow::Result;

use crate::cli::StartArgs;
use crate::config::{GpuPolicy, HyperPodFile};
use crate::vmm;
use crate::vmm::common::storage;

pub fn run(args: StartArgs) -> Result<()> {
    let cfg = HyperPodFile::load(&args.config)?;
    let fmt = storage::detect_format(&cfg.rootfs_path)?;

    println!("HyperPod configuration loaded from {}", args.config.display());
    println!("  kernel   : {}", cfg.kernel_path.display());
    println!("  rootfs   : {} ({:?})", cfg.rootfs_path.display(), fmt);
    println!("  cmdline  : {}", cfg.cmdline);
    println!(
        "  ram      : {} MiB .. {} MiB",
        cfg.limits.min_ram, cfg.limits.max_ram
    );
    println!(
        "  cpu      : {} .. {} shares",
        cfg.limits.min_cpu_shares, cfg.limits.max_cpu_shares
    );
    match cfg.gpu {
        None => println!("  gpu      : (none configured)"),
        Some(GpuPolicy::Full) => println!("  gpu      : full (unrestricted)"),
        Some(GpuPolicy::Credits {
            credits_per_second,
            max_credits,
        }) => println!(
            "  gpu      : credits ({credits_per_second}/s, burst {max_credits})"
        ),
    }

    vmm::run_vm(&cfg)
}
