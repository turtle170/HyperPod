use anyhow::Result;

use crate::cli::StatusArgs;

pub fn run(_args: StatusArgs) -> Result<()> {
    println!("hyperpod: no running pods (runtime not implemented yet).");
    Ok(())
}
