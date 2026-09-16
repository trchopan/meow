#![allow(unexpected_cfgs)]

pub mod cli;
pub mod menubar;

mod attach;
mod blob;
mod client_ipc;
mod clipboard;
mod dev;
mod display;
mod file_transfer;
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
mod transfer;

use anyhow::Result;
use cli::{Cli, Command};
use ipc::{IpcCommand, send_ipc, send_switch};
use model::ActiveTarget;
use state::{reset_identity, rotate_secret};
use transfer::receive_file;

pub async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Host(args) => host::run_host(args).await,
        Command::Attach(args) => attach::run_attach(args).await,
        Command::AttachProfile(args) => attach::run_attach_profile(args.profile).await,
        Command::CheckPermissions(args) => macos_permissions::print_permission_status(&args.role),
        Command::DevSmoke(args) => dev::run_dev_smoke(args).await,
        Command::ProbePointerLock(args) => probe::run_probe_pointer_lock(args).await,
        Command::ResetIdentity => reset_identity().await,
        Command::RotateSecret => rotate_secret().await,
        Command::TestInject => attach::run_test_inject().await,
        Command::BenchFlush(args) => dev::run_bench_flush(args).await,
        Command::OverlayUi(args) => input_overlay::run_overlay_ui(args),
        Command::Local => send_switch(ActiveTarget::Local).await,
        Command::Right => send_switch(ActiveTarget::Right).await,
        Command::Left => send_switch(ActiveTarget::Left).await,
        Command::Up => send_switch(ActiveTarget::Up).await,
        Command::Down => send_switch(ActiveTarget::Down).await,
        Command::PointerMode(args) => send_ipc(IpcCommand::PointerMode { mode: args.mode }).await,
        Command::Status => send_ipc(IpcCommand::Status).await,
        Command::Stop => send_ipc(IpcCommand::Stop).await,
        Command::Receive(args) => {
            let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
                .secret_key(iroh::SecretKey::generate())
                .alpns(vec![transfer::TRANSFER_ALPN.to_vec()])
                .bind()
                .await?;
            let result =
                receive_file(&endpoint, &args.reference, args.destination.as_deref()).await;
            endpoint.close().await;
            let path = result?;
            println!("received file at {}", path.display());
            Ok(())
        }
    }
}
