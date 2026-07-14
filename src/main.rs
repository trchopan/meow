use anyhow::Result;
use clap::Parser;

mod attach;
mod cli;
mod host;
mod host_mouse;
mod input;
mod ipc;
mod macos_permissions;
mod macos_mouse_delta;
mod model;
mod probe;
mod presentation;
mod protocol;
mod state;

use attach::{run_attach, run_test_inject};
use cli::{Cli, Command};
use host::run_host;
use ipc::{IpcCommand, send_ipc, send_switch};
use model::ActiveTarget;
use probe::run_probe_pointer_lock;
use state::{reset_identity, rotate_secret};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "meow=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Host => run_host().await,
        Command::Attach(args) => run_attach(args).await,
        Command::ProbePointerLock(args) => run_probe_pointer_lock(args).await,
        Command::ResetIdentity => reset_identity().await,
        Command::RotateSecret => rotate_secret().await,
        Command::TestInject => run_test_inject().await,
        Command::Local => send_switch(ActiveTarget::Local).await,
        Command::Right => send_switch(ActiveTarget::Right).await,
        Command::Left => send_switch(ActiveTarget::Left).await,
        Command::Up => send_switch(ActiveTarget::Up).await,
        Command::Down => send_switch(ActiveTarget::Down).await,
        Command::Status => send_ipc(IpcCommand::Status).await,
        Command::Stop => send_ipc(IpcCommand::Stop).await,
    }
}
