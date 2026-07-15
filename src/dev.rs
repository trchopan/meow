use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use uuid::Uuid;

use crate::cli::DevSmokeArgs;

pub(crate) async fn run_dev_smoke(args: DevSmokeArgs) -> Result<()> {
    if args.duration_secs == 0 {
        bail!("duration must be > 0");
    }

    let temp_dir = std::env::temp_dir().join(format!("meow-dev-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("failed to create {}", temp_dir.display()))?;

    println!("dev smoke state dir: {}", temp_dir.display());

    let mut host = match Command::new(std::env::current_exe()?)
        .arg("host")
        .env("MEOW_STATE_DIR", &temp_dir)
        .env("MEOW_DEV_SMOKE", "1")
        .env("MEOW_DEV_SIDE", side_arg(args.side))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(err).context("failed to spawn host for dev smoke");
        }
    };

    let host_stdout = match host.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = cleanup_dev_smoke(&temp_dir, &mut host, None);
            return Err(anyhow!("host stdout not available"));
        }
    };

    let (ready_tx, ready_rx) = mpsc::sync_channel::<(String, String)>(1);
    let mut host_reader = Some(thread::spawn(move || {
        let reader = BufReader::new(host_stdout);
        let mut endpoint_id = None;
        let mut secret = None;
        let mut sent_ready = false;
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    println!("[host] {line}");
                    if let Some(id) = line.strip_prefix("Host endpoint id: ") {
                        endpoint_id = Some(id.trim().to_string());
                    } else if let Some(s) = line.strip_prefix("Session secret: ") {
                        secret = Some(s.trim().to_string());
                    }
                    if !sent_ready && let (Some(id), Some(secret)) = (&endpoint_id, &secret) {
                        let _ = ready_tx.send((id.clone(), secret.clone()));
                        sent_ready = true;
                    }
                }
                Err(_) => break,
            }
        }
    }));

    let run_result = (|| -> Result<()> {
        let (host_id, secret) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .context("timed out waiting for host endpoint id and session secret")?;

        let attach_status = Command::new(std::env::current_exe()?)
            .arg("attach")
            .arg(&host_id)
            .arg(&secret)
            .arg("--side")
            .arg(side_arg(args.side))
            .arg("--probe-received")
            .arg("--probe-duration-secs")
            .arg(args.duration_secs.to_string())
            .arg("--no-inject")
            .env("MEOW_STATE_DIR", &temp_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run attach probe in dev smoke")?;

        if !attach_status.success() {
            bail!("dev smoke attach probe failed with status {attach_status}");
        }

        Ok(())
    })();

    let cleanup_result = cleanup_dev_smoke(&temp_dir, &mut host, host_reader.take());

    match (run_result, cleanup_result) {
        (Ok(()), Ok(())) => {}
        (Err(run_err), Ok(())) => return Err(run_err),
        (Ok(()), Err(cleanup_err)) => return Err(cleanup_err),
        (Err(run_err), Err(cleanup_err)) => {
            return Err(run_err.context(format!("cleanup failed: {cleanup_err:#}")));
        }
    }

    println!("dev smoke completed successfully");
    Ok(())
}

fn side_arg(side: crate::model::Side) -> &'static str {
    match side {
        crate::model::Side::Left => "left",
        crate::model::Side::Right => "right",
        crate::model::Side::Up => "up",
        crate::model::Side::Down => "down",
    }
}

fn stop_host_process(state_dir: &Path, host: &mut std::process::Child) -> Result<()> {
    let stop_status = Command::new(std::env::current_exe()?)
        .arg("stop")
        .env("MEOW_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if let Ok(status) = stop_status
        && status.success()
    {
        let _ = host.wait();
        return Ok(());
    }

    let _ = host.kill();
    let _ = host.wait();
    Ok(())
}

fn cleanup_dev_smoke(
    state_dir: &Path,
    host: &mut std::process::Child,
    host_reader: Option<std::thread::JoinHandle<()>>,
) -> Result<()> {
    stop_host_process(state_dir, host).context("failed to stop dev smoke host")?;

    if let Some(reader) = host_reader {
        reader
            .join()
            .map_err(|_| anyhow!("host output reader thread panicked"))?;
    }

    match std::fs::remove_dir_all(state_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to remove dev smoke state dir {}",
                state_dir.display()
            )
        }),
    }
}
