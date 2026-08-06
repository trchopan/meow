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
    model::{ActiveTarget, HostState, RemotePointerMode, Side, TARGET_TRANSITION_LOCK},
    presentation::print_status_response,
    state::{host_state_path, load_or_create_host_state, socket_path, write_host_state_file},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub(crate) enum IpcCommand {
    Switch { target: ActiveTarget },
    PointerMode { mode: RemotePointerMode },
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
    pub(crate) pointer_mode: RemotePointerMode,
    pub(crate) attached: Vec<Side>,
    pub(crate) captured_events: u64,
    pub(crate) normalized_events: u64,
    pub(crate) replay_failures: u64,
    pub(crate) capture_tap_user_disabled: u64,
    pub(crate) recovery_events: u64,
    pub(crate) captured_queue_full_mouse_dropped: u64,
    pub(crate) captured_queue_full_non_mouse_dropped: u64,
    pub(crate) writer_queue_full_dropped: u64,
    pub(crate) writer_queue_full_forced_local: u64,
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
        let maybe_stream = tokio::select! {
            _ = state.shutdown_notify.notified() => {
                break;
            }
            accepted = listener.accept() => Some(accepted?),
        };

        if let Some((mut stream, _)) = maybe_stream {
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_control_request(&mut stream, state).await {
                    error!("control request error: {err:#}");
                }
            });
        }
    }

    let _ = std::fs::remove_file(socket);
    Ok(())
}

async fn handle_control_request(stream: &mut UnixStream, state: HostState) -> Result<()> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let command: IpcCommand = serde_json::from_slice(&bytes)?;

    let response = match command {
        IpcCommand::Switch { target } => switch_target(&state, target).await,
        IpcCommand::PointerMode { mode } => set_pointer_mode(&state, mode).await,
        IpcCommand::Status => IpcResponse {
            ok: true,
            message: "host daemon is running".to_string(),
            status: Some(status_payload(&state).await),
        },
        IpcCommand::Stop => {
            state.shutdown_requested.store(true, Ordering::Relaxed);
            state.shutdown_notify.notify_waiters();
            apply_target_change(&state, ActiveTarget::Local, "daemon stop");
            ensure_pointer_restored();
            let response = IpcResponse {
                ok: true,
                message: "stopping host daemon".to_string(),
                status: None,
            };
            let payload = serde_json::to_vec(&response)?;
            stream.write_all(&payload).await?;
            return Ok(());
        }
    };

    let payload = serde_json::to_vec(&response)?;
    stream.write_all(&payload).await?;
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn ensure_pointer_restored() {
    let _transition_guard = TARGET_TRANSITION_LOCK
        .lock()
        .expect("target transition mutex poisoned");
    if let Err(err) = host_mouse::set_pointer_dissociation(false) {
        warn!("failed to disable pointer dissociation during shutdown: {err:#}");
    }
    if let Err(err) = host_mouse::set_pointer_visible(true) {
        warn!("failed to show pointer during shutdown: {err:#}");
    }
}

#[cfg(test)]
pub(crate) fn ensure_pointer_restored() {}

pub(crate) async fn switch_target(state: &HostState, target: ActiveTarget) -> IpcResponse {
    if !switch_target_if_attached(state, target, "control command").await {
        let side = target
            .to_side()
            .expect("detached target validation only applies to remote targets");
        return IpcResponse {
            ok: false,
            message: format!("no remote attached on {side:?}").to_lowercase(),
            status: Some(status_payload(state).await),
        };
    }

    IpcResponse {
        ok: true,
        message: format!("switched target to {target}"),
        status: Some(status_payload(state).await),
    }
}

pub(crate) async fn switch_target_if_attached(
    state: &HostState,
    target: ActiveTarget,
    context: &str,
) -> bool {
    if state.shutdown_requested.load(Ordering::Acquire) && target != ActiveTarget::Local {
        return false;
    }
    let remotes = state.remotes.write().await;
    if target
        .to_side()
        .is_some_and(|side| !remotes.contains_key(&side))
    {
        return false;
    }

    apply_target_change(state, target, context);
    true
}

async fn set_pointer_mode(state: &HostState, mode: RemotePointerMode) -> IpcResponse {
    state
        .remote_pointer_mode
        .store(mode.to_u8(), Ordering::Relaxed);

    match persist_pointer_mode(state.endpoint_id, mode) {
        Ok(()) => IpcResponse {
            ok: true,
            message: format!("set pointer mode to {mode}"),
            status: Some(status_payload(state).await),
        },
        Err(err) => IpcResponse {
            ok: false,
            message: format!("failed to persist pointer mode: {err:#}"),
            status: Some(status_payload(state).await),
        },
    }
}

fn persist_pointer_mode(endpoint_id: iroh::EndpointId, mode: RemotePointerMode) -> Result<()> {
    let path = host_state_path()?;
    let mut persisted = load_or_create_host_state(endpoint_id)?;
    persisted.remote_pointer_mode = mode;
    write_host_state_file(&path, &persisted)
}

pub(crate) fn apply_target_change(state: &HostState, target: ActiveTarget, context: &str) {
    let _transition_guard = TARGET_TRANSITION_LOCK
        .lock()
        .expect("target transition mutex poisoned");
    if state.shutdown_requested.load(Ordering::Acquire) && target != ActiveTarget::Local {
        return;
    }
    let previous_target = ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed));
    if previous_target != target {
        state.target_epoch.fetch_add(1, Ordering::AcqRel);
        state
            .pending_clipboard_request
            .lock()
            .expect("clipboard request mutex poisoned")
            .take();
    }
    if let Some(side) = target.to_side() {
        state
            .last_remote_target
            .store(ActiveTarget::from(side).to_u8(), Ordering::Relaxed);
    }
    if let Some(previous_side) = previous_target.to_side()
        && target.to_side() != Some(previous_side)
    {
        state
            .pending_release_sides
            .fetch_or(previous_side.release_bit(), Ordering::AcqRel);
    }
    state.active_target.store(target.to_u8(), Ordering::Relaxed);

    let should_lock = target.to_side().is_some();
    let was_locked = state.pointer_lock_active.load(Ordering::Relaxed);

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

    let lock_active = if should_lock {
        if !was_locked {
            match host_mouse::set_pointer_dissociation(true) {
                Ok(()) => true,
                Err(err) => {
                    warn!("failed to enable pointer dissociation: {err:#}");
                    false
                }
            }
        } else {
            true
        }
    } else {
        if (was_locked || previous_target.to_side().is_some())
            && let Err(err) = host_mouse::set_pointer_dissociation(false)
        {
            warn!("failed to disable pointer dissociation: {err:#}");
        }
        false
    };
    state
        .pointer_lock_active
        .store(lock_active, Ordering::Relaxed);

    let should_hide = lock_active;
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
        pointer_mode: RemotePointerMode::from_u8(state.remote_pointer_mode.load(Ordering::Relaxed)),
        attached,
        captured_events: state.runtime_stats.captured_events.load(Ordering::Relaxed),
        normalized_events: state
            .runtime_stats
            .normalized_events
            .load(Ordering::Relaxed),
        replay_failures: state.runtime_stats.replay_failures.load(Ordering::Relaxed),
        capture_tap_user_disabled: state
            .runtime_stats
            .capture_tap_user_disabled
            .load(Ordering::Relaxed),
        recovery_events: state.runtime_stats.recovery_events.load(Ordering::Relaxed),
        captured_queue_full_mouse_dropped: state
            .runtime_stats
            .captured_queue_full_mouse_dropped
            .load(Ordering::Relaxed),
        captured_queue_full_non_mouse_dropped: state
            .runtime_stats
            .captured_queue_full_non_mouse_dropped
            .load(Ordering::Relaxed),
        writer_queue_full_dropped: state
            .runtime_stats
            .writer_queue_full_dropped
            .load(Ordering::Relaxed),
        writer_queue_full_forced_local: state
            .runtime_stats
            .writer_queue_full_forced_local
            .load(Ordering::Relaxed),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64};

    use iroh::{EndpointId, SecretKey};
    use tokio::net::UnixStream;
    use tokio::sync::{Notify, RwLock, mpsc};

    use crate::model::{HostState, PeerMessage, PendingClipboardRequest, RemotePeer, RuntimeStats};

    fn test_host_state() -> HostState {
        HostState {
            endpoint_id: EndpointId::from(SecretKey::generate().public()),
            active_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            remote_pointer_mode: Arc::new(AtomicU8::new(RemotePointerMode::EdgeToEdge.to_u8())),
            pointer_lock_active: Arc::new(AtomicBool::new(false)),
            pointer_hidden: Arc::new(AtomicBool::new(false)),
            pinned_pointer_pos: Arc::new(std::sync::Mutex::new(None)),
            remotes: Arc::new(RwLock::new(std::collections::HashMap::new())),
            next_remote_generation: Arc::new(AtomicU64::new(1)),
            pending_release_sides: Arc::new(AtomicU8::new(0)),
            last_remote_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            target_epoch: Arc::new(AtomicU64::new(0)),
            next_clipboard_request: Arc::new(AtomicU64::new(1)),
            pending_clipboard_request: Arc::new(std::sync::Mutex::new(None)),
            runtime_stats: Arc::new(RuntimeStats::default()),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
        }
    }

    #[tokio::test]
    async fn stop_command_sets_shutdown_flag_and_returns_ok() {
        let state = test_host_state();
        let (mut client, mut server) = UnixStream::pair().expect("pair");

        let request = serde_json::to_vec(&IpcCommand::Stop).expect("serialize stop request");
        client.write_all(&request).await.expect("write request");
        client.shutdown().await.expect("shutdown client write");

        handle_control_request(&mut server, state.clone())
            .await
            .expect("handle stop request");

        let mut response_bytes = vec![0u8; 512];
        let read_len = client
            .read(&mut response_bytes)
            .await
            .expect("read response bytes");
        let response: IpcResponse =
            serde_json::from_slice(&response_bytes[..read_len]).expect("parse response");

        assert!(response.ok);
        assert!(state.shutdown_requested.load(Ordering::Relaxed));
    }

    #[test]
    fn target_change_records_release_for_previous_remote_side() {
        let state = test_host_state();
        *state
            .pending_clipboard_request
            .lock()
            .expect("clipboard request mutex poisoned") = Some(PendingClipboardRequest {
            request_id: 1,
            side: Side::Right,
            generation: 1,
            target_epoch: 0,
        });
        apply_target_change(&state, ActiveTarget::Right, "test attach");
        assert_eq!(state.pending_release_sides.load(Ordering::Acquire), 0);
        assert!(state.pointer_lock_active.load(Ordering::Acquire));
        assert_eq!(state.target_epoch.load(Ordering::Acquire), 1);
        assert!(
            state
                .pending_clipboard_request
                .lock()
                .expect("clipboard request mutex poisoned")
                .is_none()
        );

        apply_target_change(&state, ActiveTarget::Local, "test detach");
        assert!(!state.pointer_lock_active.load(Ordering::Acquire));
        assert_eq!(
            state.pending_release_sides.load(Ordering::Acquire),
            Side::Right.release_bit()
        );

        let state = test_host_state();
        apply_target_change(&state, ActiveTarget::Right, "test first side");
        apply_target_change(&state, ActiveTarget::Left, "test second side");
        apply_target_change(&state, ActiveTarget::Right, "test return side");
        assert_eq!(
            state.pending_release_sides.load(Ordering::Acquire),
            Side::Right.release_bit() | Side::Left.release_bit()
        );
    }

    #[test]
    fn shutdown_rejects_new_remote_target_changes() {
        let state = test_host_state();
        state.shutdown_requested.store(true, Ordering::Release);

        apply_target_change(&state, ActiveTarget::Right, "late remote switch");

        assert_eq!(
            ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire)),
            ActiveTarget::Local
        );
        assert!(!state.pointer_lock_active.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn switch_target_rejects_detached_remote() {
        let state = test_host_state();

        let response = switch_target(&state, ActiveTarget::Right).await;

        assert!(!response.ok);
        assert_eq!(response.message, "no remote attached on right");
        assert_eq!(
            ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire)),
            ActiveTarget::Local
        );
    }

    #[tokio::test]
    async fn switch_target_validates_and_activates_under_one_lock() {
        let state = test_host_state();
        let (input_tx, _input_rx) = mpsc::channel::<PeerMessage>(1);
        state.remotes.write().await.insert(
            Side::Right,
            RemotePeer {
                input_tx,
                next_seq: Arc::new(AtomicU64::new(1)),
                remote_id: EndpointId::from(SecretKey::generate().public()),
                generation: 1,
                name: "test-peer".to_string(),
            },
        );

        let response = switch_target(&state, ActiveTarget::Right).await;

        assert!(response.ok);
        assert_eq!(
            ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire)),
            ActiveTarget::Right
        );
    }

    #[test]
    fn status_payload_round_trip_includes_runtime_counters() {
        let payload = StatusPayload {
            endpoint_id: "endpoint".to_string(),
            active: ActiveTarget::Right,
            pointer_mode: RemotePointerMode::Confine,
            attached: vec![Side::Right],
            captured_events: 1,
            normalized_events: 2,
            replay_failures: 3,
            capture_tap_user_disabled: 4,
            recovery_events: 4,
            captured_queue_full_mouse_dropped: 11,
            captured_queue_full_non_mouse_dropped: 7,
            writer_queue_full_dropped: 5,
            writer_queue_full_forced_local: 3,
        };

        let encoded = serde_json::to_vec(&payload).expect("serialize payload");
        let decoded: StatusPayload = serde_json::from_slice(&encoded).expect("deserialize payload");

        assert_eq!(decoded.captured_queue_full_mouse_dropped, 11);
        assert_eq!(decoded.captured_queue_full_non_mouse_dropped, 7);
        assert_eq!(decoded.writer_queue_full_dropped, 5);
        assert_eq!(decoded.writer_queue_full_forced_local, 3);
        assert_eq!(decoded.capture_tap_user_disabled, 4);
    }
}
