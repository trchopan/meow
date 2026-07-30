#![allow(unexpected_cfgs)]

use anyhow::Result;
use clap::Parser;

mod attach;
mod cli;
mod clipboard;
mod dev;
mod host;
mod host_mouse;
mod input;
mod input_overlay;
mod ipc;
mod macos_inject;
mod macos_keyboard;
mod macos_mouse_delta;
mod macos_permissions;
mod model;
mod presentation;
mod probe;
mod protocol;
mod state;

use attach::{run_attach, run_test_inject};
use cli::{Cli, Command};
use dev::{run_bench_flush, run_dev_smoke};
use host::run_host;
use input_overlay::run_overlay_ui;
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
        Command::Host(args) => run_host(args).await,
        Command::Attach(args) => run_attach(args).await,
        Command::DevSmoke(args) => run_dev_smoke(args).await,
        Command::ProbePointerLock(args) => run_probe_pointer_lock(args).await,
        Command::ResetIdentity => reset_identity().await,
        Command::RotateSecret => rotate_secret().await,
        Command::TestInject => run_test_inject().await,
        Command::BenchFlush(args) => run_bench_flush(args).await,
        Command::OverlayUi(args) => run_overlay_ui(args),
        Command::Local => send_switch(ActiveTarget::Local).await,
        Command::Right => send_switch(ActiveTarget::Right).await,
        Command::Left => send_switch(ActiveTarget::Left).await,
        Command::Up => send_switch(ActiveTarget::Up).await,
        Command::Down => send_switch(ActiveTarget::Down).await,
        Command::PointerMode(args) => send_ipc(IpcCommand::PointerMode { mode: args.mode }).await,
        Command::Status => send_ipc(IpcCommand::Status).await,
        Command::Stop => send_ipc(IpcCommand::Stop).await,
    }
}
