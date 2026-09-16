use clap::Parser;
use meow::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meow=info".into()),
        )
        .init();

    let cli = Cli::parse();
    meow::run_cli(cli).await
}
