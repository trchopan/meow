use clap::Parser;
use meow::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = meow::logging::init_logging("cli", false);
    meow::logging::install_panic_hook(false);

    let cli = Cli::parse();
    meow::run_cli(cli).await
}
