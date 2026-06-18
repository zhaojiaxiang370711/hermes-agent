//! Command-line interface for hermes-rs (Phase 1a: scaffold).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "hermes-rs", version, about = "Faithful Rust port of the Hermes agent core")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Placeholder — full surface lands in later tasks.
    NotYetImplemented,
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let _ = cli;
    Ok(())
}
