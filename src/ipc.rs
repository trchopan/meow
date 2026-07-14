use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use tracing::{error, info, warn};

use crate::{
    host_mouse,
    model::{ActiveTarget, HostState, Side},
    presentation::print_status_response,
    state::socket_path,
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub(crate) enum IpcCommand {
    Switch { target: ActiveTarget },
    Status,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct IpcResponse {
    pub(crate) ok: bool,
    pub(crate) message: String,
    pub(crate) status: Option<StatusPayload>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StatusPayload {
    pub(crate) endpoint_id: String,
    pub(crate) active: ActiveTarget,
    pub(crate) attached: Vec<Side>,
}

pub(crate) async fn send_switch(target: ActiveTarget) -> Result<()> {
    send_ipc(IpcCommand::Switch { target }).await
}

pub(crate) async fn send_ipc(command: IpcCommand) -> Result<()> {
    let socket = socket_path()?;
    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("host daemon is not running ({})", socket.display()))?;
    let bytes = serde_json::to_vec(&command)?;
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;

    let mut response_bytes = Vec::new();
    stream.read_to_end(&mut response_bytes).await?;
    let response: IpcResponse = serde_json::from_slice(&response_bytes)?;

    if response.ok {
        print_status_response(&response.message, response.status.as_ref());
        Ok(())
    } else {
        bail!(response.message)
    }
}

pub(crate) async fn run_control_socket(state: HostState) -> Result<()> {
    let socket = socket_path()?;
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind {}", socket.display()))?;
    info!("control socket ready: {}", socket.display());

    loop {
        let (mut stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_control_request(&mut stream, state).await {
                error!("control request error: {err:#}");
            }
        });
    }
}

async fn handle_control_request(stream: &mut UnixStream, state: HostState) -> Result<()> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let command: IpcCommand = serde_json::from_slice(&bytes)?;

    let response = match command {
        IpcCommand::Switch { target } => switch_target(&state, target).await,
        IpcCommand::Status => IpcResponse {
            ok: true,
            message: "host daemon is running".to_string(),
            status: Some(status_payload(&state).await),
        },
        IpcCommand::Stop => {
            apply_target_change(&state, ActiveTarget::Local, "daemon stop");
            let response = IpcResponse {
                ok: true,
                message: "stopping host daemon".to_string(),
                status: None,
            };
            let payload = serde_json::to_vec(&response)?;
            stream.write_all(&payload).await?;
            let _ = std::fs::remove_file(socket_path()?);
            std::process::exit(0);
        }
    };

    let payload = serde_json::to_vec(&response)?;
    stream.write_all(&payload).await?;
    Ok(())
}

async fn switch_target(state: &HostState, target: ActiveTarget) -> IpcResponse {
    if let Some(side) = target.to_side() {
        let side_exists = {
            let remotes = state.remotes.read().await;
            remotes.contains_key(&side)
        };
        if !side_exists {
            return IpcResponse {
                ok: false,
                message: format!("no remote attached on {side:?}").to_lowercase(),
                status: Some(status_payload(state).await),
            };
        }
    }

    apply_target_change(state, target, "control command");
    IpcResponse {
        ok: true,
        message: format!("switched target to {target}"),
        status: Some(status_payload(state).await),
    }
}

pub(crate) fn apply_target_change(state: &HostState, target: ActiveTarget, context: &str) {
    state.active_target.store(target.to_u8(), Ordering::Relaxed);

    let should_lock = target.to_side().is_some();
    let was_locked = state
        .pointer_lock_active
        .swap(should_lock, Ordering::Relaxed);

    if should_lock && !was_locked {
        match host_mouse::current_pointer_position() {
            Ok((x, y)) => {
                let mut pinned = state
                    .pinned_pointer_pos
                    .lock()
                    .expect("pinned pointer mutex poisoned");
                *pinned = Some((x, y));
            }
            Err(err) => {
                warn!("failed reading current pointer position: {err:#}");
            }
        }
    }

    if was_locked != should_lock
        && let Err(err) = host_mouse::set_pointer_dissociation(should_lock)
    {
        warn!("failed to update pointer dissociation enabled={should_lock}: {err:#}");
    }

    let should_hide = should_lock;
    let was_hidden = state.pointer_hidden.swap(should_hide, Ordering::Relaxed);
    if was_hidden != should_hide
        && let Err(err) = host_mouse::set_pointer_visible(!should_hide)
    {
        warn!("failed to update pointer visibility hidden={should_hide}: {err:#}");
        state.pointer_hidden.store(was_hidden, Ordering::Relaxed);
    }

    if !should_lock {
        let mut pinned = state
            .pinned_pointer_pos
            .lock()
            .expect("pinned pointer mutex poisoned");
        *pinned = None;
    }

    info!("switched active target to {} via {}", target, context);
}

async fn status_payload(state: &HostState) -> StatusPayload {
    let attached = {
        let remotes = state.remotes.read().await;
        remotes.keys().copied().collect::<Vec<_>>()
    };
    StatusPayload {
        endpoint_id: state.endpoint_id.to_string(),
        active: ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed)),
        attached,
    }
}

pub(crate) async fn is_daemon_running() -> bool {
    let Ok(path) = socket_path() else {
        return false;
    };
    UnixStream::connect(path).await.is_ok()
}

pub(crate) async fn cleanup_stale_socket() -> Result<()> {
    let path = socket_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}
