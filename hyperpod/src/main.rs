mod cli;
mod commands;
mod config;
mod scaling;
mod vmm;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start(args) => commands::start::run(args),
        Command::Status(args) => commands::status::run(args),
    }
}
