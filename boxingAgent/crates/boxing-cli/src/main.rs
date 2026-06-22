use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = boxing_cli::Cli::parse();
    boxing_cli::run(cli).await
}
