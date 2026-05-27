use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "hyperpod",
    version,
    about = "Lightweight VMM with burstable scaling. Primary backend: WHPX on Windows. Secondary backend: KVM on Linux."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a HyperPod VM from a HyperPod.toml definition.
    Start(StartArgs),
    /// Report on the local HyperPod runtime.
    Status(StatusArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Path to the HyperPod.toml file describing the VM.
    pub config: PathBuf,
}

#[derive(Debug, Args)]
pub struct StatusArgs {}
