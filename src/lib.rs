#![allow(unexpected_cfgs)]

pub mod cli;
pub mod diagnostics;
pub mod doctor;
pub mod logging;
pub mod menubar;
pub mod monitor;

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
        Command::Doctor(args) => {
            let report = doctor::run_doctor_checks().await;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if args.markdown {
                println!("{}", doctor::format_doctor_markdown(&report));
            } else {
                print!("{}", doctor::format_doctor_terminal(&report));
            }
            Ok(())
        }
        Command::Diagnose(args) => {
            println!("Exporting sanitized diagnostics bundle...");
            let zip_path = diagnostics::export_diagnostic_bundle().await?;
            println!("Diagnostics bundle created at: {}", zip_path.display());
            if args.open {
                let _ = diagnostics::open_in_finder(&zip_path);
            }
            Ok(())
        }
        Command::Logs(args) => {
            if args.follow {
                let log_path = state::log_file_path()?;
                println!(
                    "Streaming logs from {} (press Ctrl+C to stop)...",
                    log_path.display()
                );
                let _ = std::process::Command::new("/usr/bin/tail")
                    .args(["-n", &args.lines.to_string(), "-f"])
                    .arg(&log_path)
                    .status();
            } else {
                let lines = logging::read_recent_logs(args.lines)?;
                for line in lines {
                    println!("{line}");
                }
            }
            Ok(())
        }
        Command::Monitor(args) => {
            monitor::run_monitor(monitor::MonitorArgs {
                duration_secs: args.duration_secs,
            })
            .await
        }
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
