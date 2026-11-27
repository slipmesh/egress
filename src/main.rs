//! The egress agent.

mod crd;

use anyhow::Result;
use clap::{Parser, Subcommand};
use kube::CustomResourceExt;

#[derive(Parser)]
#[command(version, about = "The egress agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the CRD of `Egress`, as `deploy/` holds it.
    Crd,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Crd => print!("{}", yaml_serde::to_string(&crd::Egress::crd())?),
    }
    Ok(())
}
