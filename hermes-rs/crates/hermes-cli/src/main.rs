use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = hermes_cli::Cli::parse();
    hermes_cli::run(cli)
}
