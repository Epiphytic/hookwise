use captain_hook::Commands;
use clap::Parser;

#[derive(Parser)]
#[command(name = "captain-hook")]
#[command(about = "Intelligent permission gating for AI coding assistants")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    captain_hook::cli::dispatch(cli.command).await?;
    Ok(())
}
