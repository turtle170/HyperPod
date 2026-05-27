use anyhow::{bail, Result};

use crate::config::HyperPodFile;

pub fn run_vm(_cfg: &HyperPodFile) -> Result<()> {
    bail!(
        "HyperPod's VMM requires a Linux host with /dev/kvm (current OS: {}).",
        std::env::consts::OS
    );
}
