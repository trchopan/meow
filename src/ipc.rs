use std::{io::ErrorKind, path::PathBuf, sync::atomic::Ordering, thread, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StatusPayload {
    pub(crate) endpoint_id: String,
    #[serde(default)]
    pub(crate) runtime_id: String,
    pub(crate) active: ActiveTarget,
    pub(crate) pointer_mode: RemotePointerMode,
    pub(crate) attached: Vec<Side>,
    #[serde(default)]
    pub(crate) attached_peers: Vec<AttachedPeerStatus>,
    pub(crate) captured_events: u64,
    pub(crate) normalized_events: u64,
    pub(crate) replay_failures: u64,
    pub(crate) capture_tap_user_disabled: u64,
    pub(crate) recovery_events: u64,
    pub(crate) captured_queue_full_mouse_dropped: u64,
    pub(crate) captured_queue_full_non_mouse_dropped: u64,
    pub(crate) writer_queue_full_dropped: u64,
    pub(crate) writer_queue_full_forced_local: u64,
    pub(crate) pointer_lock_active: bool,
    pub(crate) pointer_tap_healthy: bool,
    pub(crate) capture_tap_stopped: u64,
}

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IPC_MESSAGE_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AttachedPeerStatus {
    pub(crate) side: Side,
    pub(crate) endpoint_id: String,
    pub(crate) name: String,
}

pub(crate) async fn send_switch(target: ActiveTarget) -> Result<()> {
    send_ipc(IpcCommand::Switch { target }).await
}

pub(crate) async fn send_ipc(command: IpcCommand) -> Result<()> {
    let response = request_ipc(command).await?;

    if response.ok {
        print_status_response(&response.message, response.status.as_ref());
        Ok(())
    } else {
        bail!(response.message)
    }
}

pub(crate) async fn request_ipc(command: IpcCommand) -> Result<IpcResponse> {
    let socket = socket_path()?;
    let mut stream = tokio::time::timeout(IPC_TIMEOUT, UnixStream::connect(&socket))
        .await
        .with_context(|| format!("timed out connecting to host daemon ({})", socket.display()))?
        .with_context(|| format!("host daemon is not running ({})", socket.display()))?;
    let bytes = serde_json::to_vec(&command)?;
    tokio::time::timeout(IPC_TIMEOUT, stream.write_all(&bytes))
        .await
        .context("timed out writing host control request")??;
    tokio::time::timeout(IPC_TIMEOUT, stream.shutdown())
        .await
        .context("timed out closing host control request")??;

    let response_bytes = read_ipc_message(&mut stream).await?;
    let response: IpcResponse = serde_json::from_slice(&response_bytes)?;

    Ok(response)
}

pub(crate) async fn bind_control_socket() -> Result<(UnixListener, PathBuf)> {
    let socket = socket_path()?;
    if socket.exists() {
        match tokio::time::timeout(IPC_TIMEOUT, UnixStream::connect(&socket)).await {
            Ok(Ok(_)) => bail!(
                "host control socket is already in use ({})",
                socket.display()
            ),
            Ok(Err(err)) if stale_socket_error(&err) => {
                std::fs::remove_file(&socket).with_context(|| {
                    format!("failed to remove stale socket {}", socket.display())
                })?;
            }
            Ok(Err(err)) => {
                bail!(
                    "host control socket is unavailable and cannot be proven stale ({}): {err}",
                    socket.display()
                );
            }
            Err(_) => {
                bail!(
                    "host control socket did not respond and cannot be proven stale ({})",
                    socket.display()
                );
            }
        }
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind {}", socket.display()))?;
    info!("control socket ready: {}", socket.display());
    Ok((listener, socket))
}

pub(crate) async fn run_control_socket(
    listener: UnixListener,
    socket: PathBuf,
    state: HostState,
) -> Result<()> {
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
    let bytes = read_ipc_message(stream).await?;
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
            tokio::time::timeout(IPC_TIMEOUT, stream.write_all(&payload))
                .await
                .context("timed out writing host stop response")??;
            return Ok(());
        }
    };

    let payload = serde_json::to_vec(&response)?;
    tokio::time::timeout(IPC_TIMEOUT, stream.write_all(&payload))
        .await
        .context("timed out writing host control response")??;
    Ok(())
}

async fn read_ipc_message(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let result = tokio::time::timeout(IPC_TIMEOUT, async {
        let mut bytes = Vec::new();
        let mut limited = stream.take((MAX_IPC_MESSAGE_SIZE + 1) as u64);
        limited.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    })
    .await
    .context("timed out reading host control request")??;
    if result.len() > MAX_IPC_MESSAGE_SIZE {
        bail!("host control message is too large");
    }
    Ok(result)
}

fn stale_socket_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::NotFound
    )
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
    state
        .pointer_lock_recovery_target
        .store(target.to_u8(), Ordering::Release);
    state
        .pointer_lock_recovery_generation
        .fetch_add(1, Ordering::AcqRel);
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

    let lock_active = if should_lock {
        if let Err(err) = ensure_pointer_pinned(state) {
            warn!("failed to pin pointer before remote switch: {err:#}");
            false
        } else {
            match host_mouse::set_pointer_dissociation(true) {
                Ok(()) => match warp_to_pinned_pointer(state) {
                    Ok(()) => true,
                    Err(err) => {
                        warn!("failed to warp pointer after remote switch: {err:#}");
                        if let Err(restore_err) = host_mouse::set_pointer_dissociation(false) {
                            warn!(
                                "failed to restore pointer association after lock activation failure: {restore_err:#}"
                            );
                        }
                        false
                    }
                },
                Err(err) => {
                    warn!("failed to enable pointer dissociation: {err:#}");
                    false
                }
            }
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
    } else if !lock_active {
        schedule_pointer_lock_recovery(state, target);
    }

    info!("switched active target to {} via {}", target, context);
}

const POINTER_LOCK_RECOVERY_ATTEMPTS: u8 = 10;
const POINTER_LOCK_RECOVERY_DELAY: Duration = Duration::from_millis(10);

fn ensure_pointer_pinned(state: &HostState) -> Result<()> {
    let mut pinned = state
        .pinned_pointer_pos
        .lock()
        .expect("pinned pointer mutex poisoned");
    if pinned.is_none() {
        *pinned = Some(
            host_mouse::current_pointer_position()
                .context("failed reading current pointer position")?,
        );
    }
    Ok(())
}

fn warp_to_pinned_pointer(state: &HostState) -> Result<()> {
    let position = *state
        .pinned_pointer_pos
        .lock()
        .expect("pinned pointer mutex poisoned");
    let Some((x, y)) = position else {
        return Err(anyhow!("pointer position was not pinned"));
    };
    host_mouse::warp_pointer(x, y)
        .with_context(|| format!("failed to warp pointer to pinned position ({x:.2},{y:.2})"))
}

fn should_continue_pointer_lock_recovery(
    active_target: ActiveTarget,
    expected_target: ActiveTarget,
) -> bool {
    active_target == expected_target && expected_target.to_side().is_some()
}

fn schedule_pointer_lock_recovery(state: &HostState, expected_target: ActiveTarget) {
    state
        .pointer_lock_recovery_target
        .store(expected_target.to_u8(), Ordering::Release);
    let generation = state
        .pointer_lock_recovery_generation
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    if state
        .pointer_lock_recovery_running
        .swap(true, Ordering::AcqRel)
    {
        return;
    }

    let state = state.clone();
    thread::spawn(move || {
        for attempt in 1..=POINTER_LOCK_RECOVERY_ATTEMPTS {
            thread::sleep(POINTER_LOCK_RECOVERY_DELAY);
            let _transition_guard = TARGET_TRANSITION_LOCK
                .lock()
                .expect("target transition mutex poisoned");
            let expected_target =
                ActiveTarget::from_u8(state.pointer_lock_recovery_target.load(Ordering::Acquire));
            let active_target = ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire));
            if state.shutdown_requested.load(Ordering::Acquire)
                || !should_continue_pointer_lock_recovery(active_target, expected_target)
            {
                break;
            }
            if state.pointer_lock_active.load(Ordering::Acquire) {
                break;
            }

            if let Err(err) = ensure_pointer_pinned(&state)
                .and_then(|()| host_mouse::set_pointer_dissociation(true))
                .and_then(|()| warp_to_pinned_pointer(&state))
            {
                warn!(
                    "pointer lock recovery attempt {attempt}/{POINTER_LOCK_RECOVERY_ATTEMPTS} failed: {err:#}"
                );
                let _ = host_mouse::set_pointer_dissociation(false);
                continue;
            }

            state.pointer_lock_active.store(true, Ordering::Release);
            let was_hidden = state.pointer_hidden.swap(true, Ordering::AcqRel);
            if !was_hidden && let Err(err) = host_mouse::set_pointer_visible(false) {
                warn!("failed to hide pointer during lock recovery: {err:#}");
                state.pointer_hidden.store(false, Ordering::Release);
            }
            info!("pointer lock recovered for remote target {expected_target}");
            break;
        }

        state
            .pointer_lock_recovery_running
            .store(false, Ordering::Release);
        let newer_recovery_requested = state
            .pointer_lock_recovery_generation
            .load(Ordering::Acquire)
            != generation;
        let active_target = ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire));
        let expected_target =
            ActiveTarget::from_u8(state.pointer_lock_recovery_target.load(Ordering::Acquire));
        if newer_recovery_requested
            && should_continue_pointer_lock_recovery(active_target, expected_target)
            && !state.pointer_lock_active.load(Ordering::Acquire)
        {
            schedule_pointer_lock_recovery(&state, expected_target);
        }
    });
}

async fn status_payload(state: &HostState) -> StatusPayload {
    let (attached, attached_peers) = {
        let remotes = state.remotes.read().await;
        let mut peers = remotes
            .iter()
            .map(|(side, remote)| AttachedPeerStatus {
                side: *side,
                endpoint_id: remote.remote_id.to_string(),
                name: remote.name.clone(),
            })
            .collect::<Vec<_>>();
        peers.sort_by_key(|peer| match peer.side {
            Side::Left => 0,
            Side::Right => 1,
            Side::Up => 2,
            Side::Down => 3,
        });
        let attached = peers.iter().map(|peer| peer.side).collect::<Vec<_>>();
        (attached, peers)
    };
    StatusPayload {
        endpoint_id: state.endpoint_id.to_string(),
        runtime_id: state.runtime_id.clone(),
        active: ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed)),
        pointer_mode: RemotePointerMode::from_u8(state.remote_pointer_mode.load(Ordering::Relaxed)),
        attached,
        attached_peers,
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
        pointer_lock_active: state.pointer_lock_active.load(Ordering::Acquire),
        pointer_tap_healthy: state.pointer_tap_healthy.load(Ordering::Acquire),
        capture_tap_stopped: state
            .runtime_stats
            .capture_tap_stopped
            .load(Ordering::Acquire),
    }
}

pub(crate) async fn is_daemon_running() -> bool {
    request_ipc(IpcCommand::Status)
        .await
        .is_ok_and(|response| response.ok && response.status.is_some())
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
            blob_runtime: crate::blob::BlobRuntime::disabled(),
            endpoint_id: EndpointId::from(SecretKey::generate().public()),
            runtime_id: "test-runtime".to_string(),
            active_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            remote_pointer_mode: Arc::new(AtomicU8::new(RemotePointerMode::EdgeToEdge.to_u8())),
            pointer_lock_active: Arc::new(AtomicBool::new(false)),
            pointer_hidden: Arc::new(AtomicBool::new(false)),
            pinned_pointer_pos: Arc::new(std::sync::Mutex::new(None)),
            pointer_lock_recovery_running: Arc::new(AtomicBool::new(false)),
            pointer_lock_recovery_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            pointer_lock_recovery_generation: Arc::new(AtomicU64::new(0)),
            pointer_tap_healthy: Arc::new(AtomicBool::new(false)),
            remotes: Arc::new(RwLock::new(std::collections::HashMap::new())),
            next_remote_generation: Arc::new(AtomicU64::new(1)),
            pending_release_sides: Arc::new(AtomicU8::new(0)),
            last_remote_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            target_epoch: Arc::new(AtomicU64::new(0)),
            next_clipboard_request: Arc::new(AtomicU64::new(1)),
            pending_clipboard_request: Arc::new(std::sync::Mutex::new(None)),
            transfer_registry: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
        assert!(
            state
                .pinned_pointer_pos
                .lock()
                .expect("pinned pointer mutex poisoned")
                .is_some()
        );
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
        assert!(
            state
                .pinned_pointer_pos
                .lock()
                .expect("pinned pointer mutex poisoned")
                .is_none()
        );
        assert_eq!(
            state.pending_release_sides.load(Ordering::Acquire),
            Side::Right.release_bit()
        );

        let state = test_host_state();
        apply_target_change(&state, ActiveTarget::Right, "test first side");
        apply_target_change(&state, ActiveTarget::Left, "test second side");
        apply_target_change(&state, ActiveTarget::Right, "test return side");
        assert_eq!(
            ActiveTarget::from_u8(state.pointer_lock_recovery_target.load(Ordering::Acquire)),
            ActiveTarget::Right
        );
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

    #[test]
    fn pointer_lock_recovery_stops_for_local_or_stale_targets() {
        assert!(should_continue_pointer_lock_recovery(
            ActiveTarget::Right,
            ActiveTarget::Right
        ));
        assert!(!should_continue_pointer_lock_recovery(
            ActiveTarget::Local,
            ActiveTarget::Right
        ));
        assert!(!should_continue_pointer_lock_recovery(
            ActiveTarget::Right,
            ActiveTarget::Local
        ));
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
                connection: None,
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
            runtime_id: "runtime".to_string(),
            active: ActiveTarget::Right,
            pointer_mode: RemotePointerMode::Confine,
            attached: vec![Side::Right],
            attached_peers: vec![AttachedPeerStatus {
                side: Side::Right,
                endpoint_id: "peer-endpoint".to_string(),
                name: "peer".to_string(),
            }],
            captured_events: 1,
            normalized_events: 2,
            replay_failures: 3,
            capture_tap_user_disabled: 4,
            recovery_events: 4,
            captured_queue_full_mouse_dropped: 11,
            captured_queue_full_non_mouse_dropped: 7,
            writer_queue_full_dropped: 5,
            writer_queue_full_forced_local: 3,
            pointer_lock_active: true,
            pointer_tap_healthy: true,
            capture_tap_stopped: 2,
        };

        let encoded = serde_json::to_vec(&payload).expect("serialize payload");
        let decoded: StatusPayload = serde_json::from_slice(&encoded).expect("deserialize payload");

        assert_eq!(decoded.captured_queue_full_mouse_dropped, 11);
        assert_eq!(decoded.captured_queue_full_non_mouse_dropped, 7);
        assert_eq!(decoded.writer_queue_full_dropped, 5);
        assert_eq!(decoded.writer_queue_full_forced_local, 3);
        assert_eq!(decoded.attached_peers.len(), 1);
        assert_eq!(decoded.runtime_id, "runtime");
        assert_eq!(decoded.capture_tap_user_disabled, 4);
        assert!(decoded.pointer_lock_active);
        assert!(decoded.pointer_tap_healthy);
        assert_eq!(decoded.capture_tap_stopped, 2);
    }

    #[test]
    fn stale_socket_cleanup_only_accepts_proven_connection_failures() {
        assert!(stale_socket_error(&std::io::Error::from(
            ErrorKind::ConnectionRefused,
        )));
        assert!(stale_socket_error(&std::io::Error::from(
            ErrorKind::NotFound
        )));
        assert!(!stale_socket_error(&std::io::Error::from(
            ErrorKind::TimedOut
        )));
        assert!(!stale_socket_error(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
    }
}
