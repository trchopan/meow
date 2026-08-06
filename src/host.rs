use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint,
    endpoint::{Incoming, presets},
};
use rdev::{EventType, Key};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::{
    cli::HostArgs,
    clipboard,
    input::{
        normalize_non_motion_event, parse_clipboard_chord, parse_detach_chord,
        parse_directional_chord, run_input_grab,
    },
    ipc::{
        IpcCommand, apply_target_change, cleanup_stale_socket, ensure_pointer_restored,
        run_control_socket, send_ipc, switch_target_if_attached,
    },
    macos_keyboard::LayoutTranslator,
    macos_mouse_delta::run_macos_mouse_delta_capture,
    macos_permissions::ensure_host_permissions_on_startup,
    model::{
        ActiveTarget, CapturedEvent, CapturedInput, HostState, PeerMessage,
        PendingClipboardRequest, RemotePeer, RemotePointerMode, RuntimeStats, ScreenEdge, Side,
    },
    presentation::print_host_ready,
    protocol::{
        ALPN, AuthRequest, AuthResponse, ClientToHostMessage, HostToClientMessage, KeyAction,
        MAX_AUTH_MSG_SIZE, MAX_CLIPBOARD_MSG_SIZE, MAX_FEEDBACK_MSG_SIZE, ModifierFlags, WireEvent,
        WireKey, read_framed_with_limit, write_framed,
    },
    state::{host_state_path, load_or_create_host_secret_key, load_or_create_host_state},
};

const MAX_REPLAY_FAILURE_REPORT_COUNT: u64 = 1_000_000;

pub(crate) async fn run_host(args: HostArgs) -> Result<()> {
    if !skip_permissions_for_synthetic_mode() {
        ensure_host_permissions_on_startup()?;
    } else {
        info!("running host in synthetic input mode");
    }

    if crate::ipc::is_daemon_running().await {
        println!("meow host daemon already running");
        send_ipc(IpcCommand::Status).await?;
        return Ok(());
    }

    cleanup_stale_socket().await?;

    let host_secret_key = load_or_create_host_secret_key()?;

    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(host_secret_key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("failed to create iroh endpoint")?;

    let endpoint_id = endpoint.id();
    let state_path = host_state_path()?;
    let persisted_state = load_or_create_host_state(endpoint_id)?;
    let secret = persisted_state.attach_secret;
    let detach_chord = parse_detach_chord(&persisted_state.detach_key).with_context(|| {
        format!(
            "invalid detach_key {:?} in {}",
            persisted_state.detach_key,
            state_path.display()
        )
    })?;
    let clipboard_chord =
        parse_clipboard_chord(&persisted_state.clipboard_key).with_context(|| {
            format!(
                "invalid clipboard_key {:?} in {}",
                persisted_state.clipboard_key,
                state_path.display()
            )
        })?;
    let up_chord = parse_directional_chord(&persisted_state.up_key, "up_key", "ctrl+alt+cmd+k")?;
    let down_chord =
        parse_directional_chord(&persisted_state.down_key, "down_key", "ctrl+alt+cmd+j")?;
    let left_chord =
        parse_directional_chord(&persisted_state.left_key, "left_key", "ctrl+alt+cmd+h")?;
    let right_chord =
        parse_directional_chord(&persisted_state.right_key, "right_key", "ctrl+alt+cmd+l")?;

    let active_target = Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8()));
    let remote_pointer_mode = Arc::new(AtomicU8::new(persisted_state.remote_pointer_mode.to_u8()));
    let pointer_lock_active = Arc::new(AtomicBool::new(false));
    let pointer_hidden = Arc::new(AtomicBool::new(false));
    let pinned_pointer_pos = Arc::new(Mutex::new(None));
    let remotes = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
    let next_remote_generation = Arc::new(AtomicU64::new(1));
    let pending_release_sides = Arc::new(AtomicU8::new(0));
    let pending_center_target = Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8()));
    let last_remote_target = Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8()));
    let target_epoch = Arc::new(AtomicU64::new(0));
    let next_clipboard_request = Arc::new(AtomicU64::new(1));
    let pending_clipboard_request = Arc::new(Mutex::new(None));
    let runtime_stats = Arc::new(RuntimeStats::default());
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());

    print_host_ready(&endpoint_id.to_string(), &secret);

    let state = HostState {
        endpoint_id,
        active_target: active_target.clone(),
        remote_pointer_mode: remote_pointer_mode.clone(),
        pointer_lock_active: pointer_lock_active.clone(),
        pointer_hidden: pointer_hidden.clone(),
        pinned_pointer_pos: pinned_pointer_pos.clone(),
        remotes: remotes.clone(),
        next_remote_generation: next_remote_generation.clone(),
        pending_release_sides: pending_release_sides.clone(),
        pending_center_target: pending_center_target.clone(),
        last_remote_target: last_remote_target.clone(),
        target_epoch: target_epoch.clone(),
        next_clipboard_request: next_clipboard_request.clone(),
        pending_clipboard_request: pending_clipboard_request.clone(),
        runtime_stats: runtime_stats.clone(),
        shutdown_requested: shutdown_requested.clone(),
        shutdown_notify: shutdown_notify.clone(),
    };

    let (input_tx, input_rx) = mpsc::channel::<CapturedInput>(captured_input_channel_capacity());
    let (directional_switch_tx, directional_switch_rx) = mpsc::unbounded_channel();
    if bench_flush_enabled() {
        tokio::spawn(run_bench_synthetic_input(input_tx.clone(), state.clone()));
    } else if dev_smoke_enabled() {
        tokio::spawn(run_dev_synthetic_input(input_tx.clone(), state.clone()));
    } else {
        let mouse_delta_tx = input_tx.clone();

        let input_active_target = active_target.clone();
        let input_target_epoch = target_epoch.clone();
        let input_pointer_lock_active = pointer_lock_active.clone();
        let input_pointer_hidden = pointer_hidden.clone();
        let input_pinned_pointer_pos = pinned_pointer_pos.clone();
        let input_pending_release_sides = pending_release_sides.clone();
        let input_pending_clipboard_request = pending_clipboard_request.clone();
        let input_detach_chord = detach_chord.clone();
        let input_clipboard_chord = clipboard_chord.clone();
        let input_runtime_stats = runtime_stats.clone();
        let input_edge_config =
            crate::input::HostEdgeConfig::new(args.edge_zone_px, args.edge_dwell_ms);
        std::thread::spawn(move || {
            if let Err(err) = run_input_grab(
                input_tx,
                input_runtime_stats,
                input_active_target,
                input_target_epoch,
                input_pointer_lock_active,
                input_pointer_hidden,
                input_pinned_pointer_pos,
                input_pending_release_sides,
                input_pending_clipboard_request,
                directional_switch_tx,
                input_detach_chord,
                input_clipboard_chord,
                up_chord,
                down_chord,
                left_chord,
                right_chord,
                input_edge_config,
            ) {
                error!("input grab stopped: {err:#}");
            }
        });

        let mouse_delta_active_target = active_target.clone();
        let mouse_delta_pointer_lock_active = pointer_lock_active.clone();
        let mouse_delta_pointer_hidden = pointer_hidden.clone();
        let mouse_delta_pinned_pointer_pos = pinned_pointer_pos.clone();
        let mouse_delta_pending_release_sides = pending_release_sides.clone();
        let mouse_delta_runtime_stats = runtime_stats.clone();
        std::thread::spawn(move || {
            if let Err(err) = run_macos_mouse_delta_capture(
                mouse_delta_tx,
                mouse_delta_runtime_stats,
                mouse_delta_active_target,
                mouse_delta_pointer_lock_active,
                mouse_delta_pointer_hidden,
                mouse_delta_pinned_pointer_pos,
                mouse_delta_pending_release_sides,
            ) {
                error!("macOS mouse delta capture stopped: {err:#}");
            }
        });
    }

    let forward_task = tokio::spawn(run_forward_loop(
        input_rx,
        directional_switch_rx,
        state.clone(),
    ));

    let control_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = run_control_socket(control_state).await {
            error!("control socket failed: {err:#}");
        }
    });

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        if state.shutdown_requested.load(Ordering::Relaxed) {
            break;
        }
        let incoming = tokio::select! {
            signal = &mut ctrl_c => {
                match signal {
                    Ok(()) => info!("Ctrl+C received, shutting down host"),
                    Err(err) => warn!("failed waiting for Ctrl+C; shutting down host: {err}"),
                }
                state.shutdown_requested.store(true, Ordering::Relaxed);
                state.shutdown_notify.notify_waiters();
                break;
            }
            _ = state.shutdown_notify.notified() => {
                break;
            }
            incoming = endpoint.accept() => incoming,
        };

        let Some(incoming) = incoming else {
            warn!("host endpoint closed, shutting down");
            state.shutdown_requested.store(true, Ordering::Relaxed);
            state.shutdown_notify.notify_waiters();
            break;
        };

        let state = state.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, state, &secret).await {
                warn!("incoming peer rejected/failed: {err:#}");
            }
        });
    }

    let forward_completed = forward_task.await.is_ok();
    apply_target_change(&state, ActiveTarget::Local, "daemon shutdown");
    if !forward_completed {
        release_all_remote_inputs(&state).await;
    }
    ensure_pointer_restored();
    let _ = std::fs::remove_file(crate::state::socket_path()?);
    Ok(())
}

async fn handle_incoming(incoming: Incoming, state: HostState, secret: &str) -> Result<()> {
    let connection = incoming.accept()?.await?;
    let remote_id = connection.remote_id();
    let (mut send, mut recv) = connection.accept_bi().await?;

    let auth: AuthRequest = match time::timeout(
        Duration::from_secs(5),
        read_framed_with_limit(&mut recv, MAX_AUTH_MSG_SIZE),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => bail!("timed out waiting for auth request"),
    };
    if auth.secret != secret {
        let res = AuthResponse {
            ok: false,
            message: "invalid secret".to_string(),
        };
        write_framed(&mut send, &res).await?;
        bail!("invalid secret from {remote_id}")
    }

    if state.shutdown_requested.load(Ordering::Acquire) {
        bail!("host is shutting down");
    }

    write_framed(
        &mut send,
        &AuthResponse {
            ok: true,
            message: format!("attached as {:?}", auth.side),
        },
    )
    .await?;

    let (input_tx, mut input_rx) = mpsc::channel::<PeerMessage>(peer_writer_channel_capacity());
    tokio::spawn(async move {
        while let Some(message) = input_rx.recv().await {
            let written = write_framed(&mut send, &message.message).await.is_ok();
            if let Some(complete) = message.complete {
                let _ = complete.send(written);
            }
            if !written {
                debug!("host->client writer stream ended");
                break;
            }
        }
    });

    let generation = state.next_remote_generation.fetch_add(1, Ordering::Relaxed);
    {
        let mut remotes = state.remotes.write().await;
        let previous = remotes.insert(
            auth.side,
            RemotePeer {
                input_tx,
                next_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                remote_id,
                generation,
                name: auth.name.clone(),
            },
        );
        if let Some(previous) = previous {
            clear_pending_clipboard_request(&state, auth.side, previous.generation);
            let seq = previous.next_seq.fetch_add(1, Ordering::Relaxed);
            if !send_release_request(&previous, HostToClientMessage::ReleaseAll { seq }).await {
                warn!(
                    "failed delivering release-all to replaced remote {:?}",
                    auth.side
                );
            }
            info!(
                "replaced existing remote on {:?}: old={} new={}",
                auth.side, previous.remote_id, remote_id
            );
        }
    }

    info!(
        "remote attached: {:?} ({remote_id}) name={}",
        auth.side, auth.name
    );

    let feedback_state = state.clone();
    let feedback_connection = connection.clone();
    let feedback_side = auth.side;
    let feedback_generation = generation;
    let feedback_name = auth.name.clone();
    tokio::spawn(async move {
        if let Err(err) = run_client_feedback_loop(
            feedback_state,
            feedback_connection,
            feedback_side,
            feedback_generation,
            &feedback_name,
        )
        .await
        {
            debug!(
                "client feedback loop exited for {:?} ({}): {err:#}",
                feedback_side, feedback_name
            );
        }
    });

    let remotes = state.remotes.clone();
    tokio::spawn(async move {
        connection.closed().await;
        let mut remotes = remotes.write().await;
        let removed_current =
            is_current_remote(remotes.get(&auth.side), remote_id, feedback_generation);
        if removed_current {
            remotes.remove(&auth.side);
            clear_pending_clipboard_request(&state, auth.side, feedback_generation);
        }

        if removed_current
            && ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed)).to_side()
                == Some(auth.side)
        {
            apply_target_change(&state, ActiveTarget::Local, "remote disconnect");
        }
        info!("remote disconnected: {:?} ({remote_id})", auth.side);
    });

    Ok(())
}

fn is_current_remote(
    existing: Option<&RemotePeer>,
    remote_id: iroh::EndpointId,
    generation: u64,
) -> bool {
    existing.is_some_and(|existing| {
        existing.remote_id == remote_id && existing.generation == generation
    })
}

async fn run_client_feedback_loop(
    state: HostState,
    connection: iroh::endpoint::Connection,
    side: Side,
    generation: u64,
    peer_name: &str,
) -> Result<()> {
    loop {
        let mut recv = connection.accept_uni().await?;
        let bytes = recv
            .read_to_end(MAX_CLIPBOARD_MSG_SIZE.max(MAX_FEEDBACK_MSG_SIZE))
            .await?;
        let message: ClientToHostMessage = bincode::deserialize(&bytes)?;
        let is_current = {
            let remotes = state.remotes.read().await;
            remotes
                .get(&side)
                .is_some_and(|remote| remote.generation == generation)
        };
        if !is_current {
            return Ok(());
        }
        match message {
            ClientToHostMessage::ClientEdgeReached { edge } => {
                maybe_switch_to_local_on_edge(&state, side, generation, edge, peer_name).await;
            }
            ClientToHostMessage::ReplayFailure { kind, count } => {
                let count = count.min(MAX_REPLAY_FAILURE_REPORT_COUNT);
                state
                    .runtime_stats
                    .replay_failures
                    .fetch_add(count, Ordering::Relaxed);
                warn!(
                    "remote replay failure on {:?} ({}): kind={kind:?} count={count}",
                    side, peer_name
                );
            }
            ClientToHostMessage::ClipboardData { request_id, text } => {
                let mut pending_guard = state
                    .pending_clipboard_request
                    .lock()
                    .expect("clipboard request mutex poisoned");
                let valid = pending_guard.as_ref().is_some_and(|pending| {
                    clipboard_response_matches(
                        *pending,
                        request_id,
                        side,
                        generation,
                        state.target_epoch.load(Ordering::Acquire),
                        ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire)),
                    )
                });
                if !valid {
                    debug!("ignoring stale clipboard response {request_id}");
                    continue;
                }
                pending_guard.take();
                drop(pending_guard);
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        clipboard::write_text(&text)
                            .and_then(|_| crate::macos_inject::inject_paste())
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            warn!("failed to paste clipboard response {request_id}: {err:#}");
                        }
                        Err(err) => {
                            warn!("clipboard paste task failed {request_id}: {err:#}");
                        }
                    }
                });
            }
        }
    }
}

async fn maybe_switch_to_local_on_edge(
    state: &HostState,
    side: Side,
    generation: u64,
    edge: ScreenEdge,
    peer_name: &str,
) {
    let remotes = state.remotes.read().await;
    if remotes
        .get(&side)
        .is_none_or(|remote| remote.generation != generation)
    {
        return;
    }
    let mode = RemotePointerMode::from_u8(state.remote_pointer_mode.load(Ordering::Relaxed));
    if mode != RemotePointerMode::EdgeToEdge {
        return;
    }

    let active = ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed));
    if active.to_side() != Some(side) {
        return;
    }

    if !is_host_facing_edge(side, edge) {
        return;
    }

    drop(remotes);
    apply_target_change(state, ActiveTarget::Local, "client edge reached");
    let _ = send_to_side(
        state,
        side,
        HostToClientMessage::CenterPointer { seq: 0 },
        false,
    )
    .await;
    info!(
        "switched to local after host-facing edge {:?} from {:?} ({})",
        edge, side, peer_name
    );
}

fn is_host_facing_edge(side: Side, edge: ScreenEdge) -> bool {
    matches!(
        (side, edge),
        (Side::Right, ScreenEdge::Left)
            | (Side::Left, ScreenEdge::Right)
            | (Side::Up, ScreenEdge::Down)
            | (Side::Down, ScreenEdge::Up)
    )
}

fn resolve_directional_target(active: ActiveTarget, requested: ActiveTarget) -> ActiveTarget {
    match (active, requested) {
        (ActiveTarget::Right, ActiveTarget::Left)
        | (ActiveTarget::Left, ActiveTarget::Right)
        | (ActiveTarget::Up, ActiveTarget::Down)
        | (ActiveTarget::Down, ActiveTarget::Up) => ActiveTarget::Local,
        _ => requested,
    }
}

async fn run_forward_loop(
    mut rx: mpsc::Receiver<CapturedInput>,
    mut directional_switch_rx: mpsc::UnboundedReceiver<ActiveTarget>,
    state: HostState,
) {
    use tokio::time::{self, MissedTickBehavior};

    let mut pending_relative: HashMap<Side, (i32, i32)> = HashMap::new();
    let mut modifier_flags = ModifierFlags::default();
    let mut pressed_keys = HashSet::new();
    let mut layout = LayoutTranslator::new();
    let flush_tick_ms = configured_flush_tick_ms();
    let mut flush_tick = time::interval(Duration::from_millis(flush_tick_ms));
    flush_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    debug!("relative flush tick configured to {}ms", flush_tick_ms);

    loop {
        reconcile_target_transition(
            &state,
            &mut pending_relative,
            &mut modifier_flags,
            &mut pressed_keys,
        )
        .await;

        tokio::select! {
            _ = state.shutdown_notify.notified() => {
                flush_pending_relative(&state, &mut pending_relative).await;
                release_all_remote_inputs(&state).await;
                break;
            }
            _ = flush_tick.tick() => {
                flush_pending_relative(&state, &mut pending_relative).await;
            }
            Some(target) = directional_switch_rx.recv() => {
                let active = ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire));
                let target = resolve_directional_target(active, target);
                let response = crate::ipc::switch_target(&state, target).await;
                if !response.ok {
                    warn!("directional shortcut failed: {}", response.message);
                }
            }
                    maybe_captured = rx.recv() => {
                        let Some(captured) = maybe_captured else {
                            flush_pending_relative(&state, &mut pending_relative).await;
                            release_all_remote_inputs(&state).await;
                            break;
                        };

                    let current_target = reconcile_target_transition(
                        &state,
                        &mut pending_relative,
                        &mut modifier_flags,
                        &mut pressed_keys,
                    )
                    .await;
                    if is_stale_captured_target(captured.target, current_target) {
                        debug!(
                            "discarding input captured for stale target {} (current target {})",
                            captured.target, current_target
                        );
                        continue;
                    }

                    if let Some(side) = side_requiring_ordered_flush(&captured) {
                        flush_relative_for_side(&state, &mut pending_relative, side).await;
                    }

                    match captured.event {
                    CapturedEvent::HostEdgeReached { edge } => {
                        maybe_switch_to_remote_on_host_edge(&state, edge).await;
                    }
                    CapturedEvent::ClipboardPaste => {
                        handle_clipboard_paste(&state, captured.target).await;
                    }
                    CapturedEvent::Raw(event) => {
                        let Some(side) = captured.target.to_side() else {
                            continue;
                        };
                        let message = HostToClientMessage::Event {
                            seq: 0,
                            event: wire_event_from_rdev(
                                event,
                                &mut modifier_flags,
                                &mut pressed_keys,
                                &mut layout,
                            ),
                        };
                        if !send_to_side(&state, side, message, false).await {
                            continue;
                        }
                    }
                    CapturedEvent::MouseButton { button, pressed } => {
                        let Some(side) = captured.target.to_side() else {
                            continue;
                        };
                        let message = HostToClientMessage::Event {
                            seq: 0,
                            event: WireEvent::MouseButton {
                                button: button_to_wire(button),
                                pressed,
                            },
                        };
                        let _ = send_to_side(&state, side, message, false).await;
                    }
                    CapturedEvent::MouseWheel { delta_x, delta_y } => {
                        let Some(side) = captured.target.to_side() else {
                            continue;
                        };
                        let message = HostToClientMessage::Event {
                            seq: 0,
                            event: WireEvent::Wheel { delta_x, delta_y },
                        };
                        let _ = send_to_side(&state, side, message, false).await;
                    }
                    CapturedEvent::MouseMoveRelative { dx, dy } => {
                        let Some(side) = captured.target.to_side() else {
                            continue;
                        };
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let entry = pending_relative.entry(side).or_insert((0, 0));
                        entry.0 = saturating_add_i32(entry.0, dx);
                        entry.1 = saturating_add_i32(entry.1, dy);
                    }
                }
            }
        }
    }
}

async fn handle_clipboard_paste(state: &HostState, target: ActiveTarget) {
    let request_id = state.next_clipboard_request.fetch_add(1, Ordering::Relaxed);
    let side = match target.to_side() {
        Some(side) => side,
        None => match connected_clipboard_source(state).await {
            Some((side, _)) => side,
            None => {
                warn!("clipboard paste requested locally without a connected remote source");
                return;
            }
        },
    };

    if target.to_side().is_some() {
        let target_epoch = state.target_epoch.load(Ordering::Acquire);
        let text = match tokio::task::spawn_blocking(clipboard::read_text).await {
            Ok(Ok(text)) => text,
            Ok(Err(err)) => {
                warn!("failed to read host clipboard: {err:#}");
                return;
            }
            Err(err) => {
                warn!("host clipboard read task failed: {err:#}");
                return;
            }
        };
        if ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire)) != target
            || state.target_epoch.load(Ordering::Acquire) != target_epoch
        {
            debug!("discarding clipboard paste after target changed");
            return;
        }
        let _ = send_to_side(
            state,
            side,
            HostToClientMessage::ClipboardPaste { request_id, text },
            false,
        )
        .await;
    } else {
        let Some((_, peer)) = connected_clipboard_source(state).await else {
            return;
        };
        let pending = PendingClipboardRequest {
            request_id,
            side,
            generation: peer.generation,
            target_epoch: state.target_epoch.load(Ordering::Acquire),
        };
        *state
            .pending_clipboard_request
            .lock()
            .expect("clipboard request mutex poisoned") = Some(pending);
        if !send_to_side(
            state,
            side,
            HostToClientMessage::ClipboardRequest { request_id },
            false,
        )
        .await
        {
            state
                .pending_clipboard_request
                .lock()
                .expect("clipboard request mutex poisoned")
                .take();
        }
    }
}

fn clear_pending_clipboard_request(state: &HostState, side: Side, generation: u64) {
    let mut pending = state
        .pending_clipboard_request
        .lock()
        .expect("clipboard request mutex poisoned");
    if pending.is_some_and(|request| request.side == side && request.generation == generation) {
        pending.take();
    }
}

async fn connected_clipboard_source(state: &HostState) -> Option<(Side, RemotePeer)> {
    let preferred =
        ActiveTarget::from_u8(state.last_remote_target.load(Ordering::Acquire)).to_side();
    let remotes = state.remotes.read().await;
    let generations = remotes
        .iter()
        .map(|(side, peer)| (*side, peer.generation))
        .collect::<HashMap<_, _>>();
    let side = choose_clipboard_side(ActiveTarget::Local, preferred, &generations)?;
    remotes.get(&side).cloned().map(|peer| (side, peer))
}

fn choose_clipboard_side(
    target: ActiveTarget,
    last_remote_target: Option<Side>,
    remote_generations: &HashMap<Side, u64>,
) -> Option<Side> {
    target
        .to_side()
        .filter(|side| remote_generations.contains_key(side))
        .or_else(|| last_remote_target.filter(|side| remote_generations.contains_key(side)))
        .or_else(|| {
            remote_generations
                .iter()
                .max_by_key(|(_, generation)| *generation)
                .map(|(side, _)| *side)
        })
}

fn clipboard_response_matches(
    pending: PendingClipboardRequest,
    request_id: u64,
    side: Side,
    generation: u64,
    target_epoch: u64,
    active_target: ActiveTarget,
) -> bool {
    pending.request_id == request_id
        && pending.side == side
        && pending.generation == generation
        && pending.target_epoch == target_epoch
        && active_target == ActiveTarget::Local
}

fn is_stale_captured_target(captured_target: ActiveTarget, current_target: ActiveTarget) -> bool {
    captured_target != current_target
}

async fn reconcile_target_transition(
    state: &HostState,
    pending_relative: &mut HashMap<Side, (i32, i32)>,
    modifier_flags: &mut ModifierFlags,
    pressed_keys: &mut HashSet<Key>,
) -> ActiveTarget {
    let current_target = ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed));
    let pending = state.pending_release_sides.swap(0, Ordering::AcqRel);
    for side in [Side::Left, Side::Right, Side::Up, Side::Down] {
        if pending & side.release_bit() != 0 {
            flush_relative_for_side(state, pending_relative, side).await;
            send_release_all_to_side(state, side).await;
        }
    }
    if pending != 0 {
        // The remote no longer owns the host's previous key state. Do not carry
        // stale modifiers into the next target after a lost release or detach.
        *modifier_flags = ModifierFlags::default();
        pressed_keys.clear();
    }
    let pending_center = ActiveTarget::from_u8(state.pending_center_target.load(Ordering::Acquire));
    if pending_center == current_target
        && pending_center.to_side().is_some()
        && state
            .pending_center_target
            .compare_exchange(
                pending_center.to_u8(),
                ActiveTarget::Local.to_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    {
        let side = pending_center.to_side().expect("remote target has a side");
        let _ = send_to_side(
            state,
            side,
            HostToClientMessage::CenterPointer { seq: 0 },
            false,
        )
        .await;
    }
    current_target
}

fn side_requiring_ordered_flush(captured: &CapturedInput) -> Option<Side> {
    match &captured.event {
        CapturedEvent::Raw(_)
        | CapturedEvent::MouseButton { .. }
        | CapturedEvent::MouseWheel { .. } => captured.target.to_side(),
        CapturedEvent::MouseMoveRelative { .. }
        | CapturedEvent::HostEdgeReached { .. }
        | CapturedEvent::ClipboardPaste => None,
    }
}

async fn flush_pending_relative(
    state: &HostState,
    pending_relative: &mut HashMap<Side, (i32, i32)>,
) {
    let sides = pending_relative.keys().copied().collect::<Vec<_>>();
    for side in sides {
        flush_relative_for_side(state, pending_relative, side).await;
    }
}

async fn flush_relative_for_side(
    state: &HostState,
    pending_relative: &mut HashMap<Side, (i32, i32)>,
    side: Side,
) {
    let Some((dx, dy)) = pending_relative.remove(&side) else {
        return;
    };
    if dx == 0 && dy == 0 {
        return;
    }
    let message = HostToClientMessage::RelativeMotion { seq: 0, dx, dy };
    let _ = send_to_side(state, side, message, true).await;
}

async fn send_to_side(
    state: &HostState,
    side: Side,
    message: HostToClientMessage,
    drop_if_full: bool,
) -> bool {
    let peer = {
        let remotes = state.remotes.read().await;
        remotes.get(&side).cloned()
    };

    let Some(peer) = peer else {
        warn!(
            "no remote attached for target {:?}; falling back to local control",
            side
        );
        apply_target_change(state, ActiveTarget::Local, "missing remote peer");
        return false;
    };

    let seq = peer.next_seq.fetch_add(1, Ordering::Relaxed);
    let with_seq = assign_sequence(message, seq);

    debug!("forwarding input event to {:?} ({})", side, peer.name);
    match peer.input_tx.try_send(PeerMessage {
        message: with_seq,
        complete: None,
    }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            if drop_if_full {
                debug!(
                    "dropping forwarded event for {:?} ({}) due to saturated writer queue",
                    side, peer.name
                );
            } else {
                warn!(
                    "writer queue saturated for {:?} ({}); falling back to local control",
                    side, peer.name
                );
                state
                    .runtime_stats
                    .writer_queue_full_forced_local
                    .fetch_add(1, Ordering::Relaxed);
                state
                    .runtime_stats
                    .recovery_events
                    .fetch_add(1, Ordering::Relaxed);
                apply_target_change(state, ActiveTarget::Local, "writer queue saturated");
            }
            state
                .runtime_stats
                .writer_queue_full_dropped
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        Err(TrySendError::Closed(_)) => {
            warn!(
                "failed forwarding to {:?} ({}): writer channel closed",
                side, peer.name
            );
            apply_target_change(state, ActiveTarget::Local, "forwarding failure");
            state
                .runtime_stats
                .recovery_events
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
    }

    true
}

fn assign_sequence(message: HostToClientMessage, seq: u64) -> HostToClientMessage {
    match message {
        HostToClientMessage::Event { event, .. } => HostToClientMessage::Event { seq, event },
        HostToClientMessage::RelativeMotion { dx, dy, .. } => {
            HostToClientMessage::RelativeMotion { seq, dx, dy }
        }
        HostToClientMessage::ReleaseAll { .. } => HostToClientMessage::ReleaseAll { seq },
        HostToClientMessage::CenterPointer { .. } => HostToClientMessage::CenterPointer { seq },
        message @ HostToClientMessage::ClipboardPaste { .. }
        | message @ HostToClientMessage::ClipboardRequest { .. } => message,
    }
}

fn wire_event_from_rdev(
    event: EventType,
    modifiers: &mut ModifierFlags,
    pressed_keys: &mut HashSet<Key>,
    layout: &mut LayoutTranslator,
) -> WireEvent {
    match event {
        EventType::KeyPress(key) => {
            let action = if pressed_keys.insert(key) {
                KeyAction::Down
            } else {
                KeyAction::Repeat
            };
            update_modifier_flags(modifiers, key, true);
            WireEvent::Key {
                action,
                key: wire_key(key, layout, modifiers, true),
                modifiers: modifiers.clone(),
            }
        }
        EventType::KeyRelease(key) => {
            pressed_keys.remove(&key);
            update_modifier_flags(modifiers, key, false);
            WireEvent::Key {
                action: KeyAction::Up,
                key: wire_key(key, layout, modifiers, false),
                modifiers: modifiers.clone(),
            }
        }
        EventType::Wheel { delta_x, delta_y } => WireEvent::Wheel { delta_x, delta_y },
        EventType::ButtonPress(button) => WireEvent::MouseButton {
            button: button_to_wire(button),
            pressed: true,
        },
        EventType::ButtonRelease(button) => WireEvent::MouseButton {
            button: button_to_wire(button),
            pressed: false,
        },
        EventType::MouseMove { x, y } => WireEvent::RelativeMotion {
            dx: x.round() as i32,
            dy: y.round() as i32,
        },
    }
}

fn update_modifier_flags(flags: &mut ModifierFlags, key: Key, down: bool) {
    let slot = match key {
        Key::ShiftLeft => &mut flags.left_shift,
        Key::ShiftRight => &mut flags.right_shift,
        Key::ControlLeft => &mut flags.left_control,
        Key::ControlRight => &mut flags.right_control,
        Key::Alt => &mut flags.left_alt,
        Key::AltGr => &mut flags.right_alt,
        Key::MetaLeft => &mut flags.left_meta,
        Key::MetaRight => &mut flags.right_meta,
        _ => return,
    };
    *slot = down;
}

fn wire_key(
    key: Key,
    layout: &mut LayoutTranslator,
    modifiers: &ModifierFlags,
    translate: bool,
) -> WireKey {
    let physical_code = crate::macos_inject::keycode_for_key(key);
    WireKey {
        logical: physical_code
            .filter(|_| translate)
            .and_then(|code| layout.translate(code, modifiers))
            .unwrap_or_else(|| format!("{key:?}")),
        physical_code,
    }
}

#[cfg(test)]
fn wire_key_identity(key: &WireKey) -> String {
    key.physical_code
        .map(|code| format!("physical:{code}"))
        .unwrap_or_else(|| format!("logical:{}", key.logical))
}

fn button_to_wire(button: rdev::Button) -> u8 {
    match button {
        rdev::Button::Left => 1,
        rdev::Button::Middle => 2,
        rdev::Button::Right => 3,
        rdev::Button::Unknown(value) => value,
    }
}

async fn send_release_all_to_side(state: &HostState, side: Side) {
    let peer = {
        let remotes = state.remotes.read().await;
        remotes.get(&side).cloned()
    };

    let Some(peer) = peer else {
        return;
    };

    let seq = peer.next_seq.fetch_add(1, Ordering::Relaxed);
    if !send_release_request(&peer, HostToClientMessage::ReleaseAll { seq }).await {
        warn!("failed sending release-all to {:?} ({})", side, peer.name);
        state
            .runtime_stats
            .recovery_events
            .fetch_add(1, Ordering::Relaxed);
        if ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed)).to_side()
            == Some(side)
        {
            apply_target_change(state, ActiveTarget::Local, "release-all delivery failure");
        }
    }
}

async fn send_release_request(peer: &RemotePeer, message: HostToClientMessage) -> bool {
    let (complete_tx, complete_rx) = tokio::sync::oneshot::channel();
    let request = PeerMessage {
        message,
        complete: Some(complete_tx),
    };
    let sent = tokio::time::timeout(Duration::from_millis(500), peer.input_tx.send(request))
        .await
        .is_ok_and(|result| result.is_ok());
    if !sent {
        return false;
    }

    match tokio::time::timeout(Duration::from_millis(500), complete_rx).await {
        Ok(Ok(written)) => written,
        Ok(Err(_)) | Err(_) => false,
    }
}

async fn release_all_remote_inputs(state: &HostState) {
    for side in [Side::Left, Side::Right, Side::Up, Side::Down] {
        send_release_all_to_side(state, side).await;
    }
}

async fn maybe_switch_to_remote_on_host_edge(state: &HostState, edge: ScreenEdge) {
    let mode = RemotePointerMode::from_u8(state.remote_pointer_mode.load(Ordering::Relaxed));
    if mode != RemotePointerMode::EdgeToEdge {
        return;
    }

    let active = ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed));
    if !matches!(active, ActiveTarget::Local) {
        return;
    }

    let side = side_from_edge(edge);

    switch_target_if_attached(state, ActiveTarget::from(side), "host edge reached").await;
}

fn saturating_add_i32(lhs: i32, rhs: i32) -> i32 {
    lhs.saturating_add(rhs)
}

fn side_from_edge(edge: ScreenEdge) -> Side {
    match edge {
        ScreenEdge::Left => Side::Left,
        ScreenEdge::Right => Side::Right,
        ScreenEdge::Up => Side::Up,
        ScreenEdge::Down => Side::Down,
    }
}

fn dev_smoke_enabled() -> bool {
    std::env::var("MEOW_DEV_SMOKE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn bench_flush_enabled() -> bool {
    std::env::var("MEOW_BENCH_FLUSH")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn skip_permissions_for_synthetic_mode() -> bool {
    dev_smoke_enabled() || bench_flush_enabled()
}

fn configured_flush_tick_ms() -> u64 {
    const DEFAULT_FLUSH_TICK_MS: u64 = 2;
    match std::env::var("MEOW_FLUSH_TICK_MS") {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(parsed) if parsed > 0 => parsed,
            Ok(_) | Err(_) => {
                warn!(
                    "invalid MEOW_FLUSH_TICK_MS value {:?}; using default {}ms",
                    raw, DEFAULT_FLUSH_TICK_MS
                );
                DEFAULT_FLUSH_TICK_MS
            }
        },
        Err(_) => DEFAULT_FLUSH_TICK_MS,
    }
}

fn captured_input_channel_capacity() -> usize {
    std::env::var("MEOW_CAPTURED_INPUT_CHAN_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(8192)
}

fn peer_writer_channel_capacity() -> usize {
    std::env::var("MEOW_PEER_WRITER_CHAN_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(2048)
}

fn dev_smoke_side() -> Side {
    match std::env::var("MEOW_DEV_SIDE") {
        Ok(v) if v.eq_ignore_ascii_case("left") => Side::Left,
        Ok(v) if v.eq_ignore_ascii_case("up") => Side::Up,
        Ok(v) if v.eq_ignore_ascii_case("down") => Side::Down,
        _ => Side::Right,
    }
}

async fn run_dev_synthetic_input(tx: mpsc::Sender<CapturedInput>, state: HostState) {
    use tokio::time::{Duration, sleep};

    let side = dev_smoke_side();
    info!("dev smoke synthetic input active for side {:?}", side);

    let mut seq_idx: u64 = 0;
    let mut target_initialized = false;
    loop {
        let attached = {
            let remotes = state.remotes.read().await;
            remotes.contains_key(&side)
        };

        if attached {
            let target = ActiveTarget::from(side);
            if should_initialize_synthetic_target(attached, target_initialized) {
                apply_target_change(&state, target, "dev smoke synthetic input");
                target_initialized = true;
            }
            let event = match seq_idx % 16 {
                0 => normalize_non_motion_event(EventType::KeyPress(Key::KeyM)),
                1 => normalize_non_motion_event(EventType::KeyRelease(Key::KeyM)),
                2 => normalize_non_motion_event(EventType::KeyPress(Key::KeyE)),
                3 => normalize_non_motion_event(EventType::KeyRelease(Key::KeyE)),
                _ => CapturedEvent::MouseMoveRelative {
                    dx: if seq_idx.is_multiple_of(2) { 4 } else { -3 },
                    dy: if seq_idx.is_multiple_of(3) { 2 } else { -2 },
                },
            };

            if tx.send(CapturedInput { target, event }).await.is_err() {
                break;
            }
            seq_idx = seq_idx.saturating_add(1);
            sleep(Duration::from_millis(12)).await;
        } else {
            target_initialized = false;
            sleep(Duration::from_millis(20)).await;
        }
    }
}

fn bench_side() -> Side {
    match std::env::var("MEOW_BENCH_SIDE") {
        Ok(v) if v.eq_ignore_ascii_case("left") => Side::Left,
        Ok(v) if v.eq_ignore_ascii_case("up") => Side::Up,
        Ok(v) if v.eq_ignore_ascii_case("down") => Side::Down,
        _ => Side::Right,
    }
}

fn bench_event_rate_hz() -> u64 {
    std::env::var("MEOW_BENCH_EVENT_RATE_HZ")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1000)
}

fn bench_axis_delta(var_name: &str, default: i32) -> i32 {
    std::env::var(var_name)
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(default)
}

async fn run_bench_synthetic_input(tx: mpsc::Sender<CapturedInput>, state: HostState) {
    use tokio::time::{Duration, sleep};

    let side = bench_side();
    let event_rate_hz = bench_event_rate_hz();
    let interval = Duration::from_nanos((1_000_000_000u64 / event_rate_hz).max(1));
    let dx = bench_axis_delta("MEOW_BENCH_DX", 3);
    let dy = bench_axis_delta("MEOW_BENCH_DY", 2);
    info!(
        "bench synthetic input active side={:?} rate_hz={} dx={} dy={}",
        side, event_rate_hz, dx, dy
    );

    let mut seq_idx: u64 = 0;
    let mut target_initialized = false;
    loop {
        let attached = {
            let remotes = state.remotes.read().await;
            remotes.contains_key(&side)
        };

        if attached {
            let target = ActiveTarget::from(side);
            if should_initialize_synthetic_target(attached, target_initialized) {
                apply_target_change(&state, target, "flush benchmark synthetic input");
                target_initialized = true;
            }
            let signed_dx = if seq_idx.is_multiple_of(2) { dx } else { -dx };
            let signed_dy = if seq_idx.is_multiple_of(3) { dy } else { -dy };
            let event = CapturedEvent::MouseMoveRelative {
                dx: signed_dx,
                dy: signed_dy,
            };
            if tx.send(CapturedInput { target, event }).await.is_err() {
                break;
            }
            seq_idx = seq_idx.saturating_add(1);
            sleep(interval).await;
        } else {
            target_initialized = false;
            sleep(Duration::from_millis(10)).await;
        }
    }
}

fn should_initialize_synthetic_target(attached: bool, initialized: bool) -> bool {
    attached && !initialized
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    fn test_forward_state(side: Side, input_tx: mpsc::Sender<PeerMessage>) -> HostState {
        HostState {
            endpoint_id: iroh::EndpointId::from(iroh::SecretKey::generate().public()),
            active_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            remote_pointer_mode: Arc::new(AtomicU8::new(RemotePointerMode::EdgeToEdge.to_u8())),
            pointer_lock_active: Arc::new(AtomicBool::new(false)),
            pointer_hidden: Arc::new(AtomicBool::new(false)),
            pinned_pointer_pos: Arc::new(Mutex::new(None)),
            remotes: Arc::new(tokio::sync::RwLock::new(HashMap::from([(
                side,
                RemotePeer {
                    input_tx,
                    next_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    remote_id: iroh::EndpointId::from(iroh::SecretKey::generate().public()),
                    generation: 1,
                    name: "test-peer".to_string(),
                },
            )]))),
            next_remote_generation: Arc::new(AtomicU64::new(2)),
            pending_release_sides: Arc::new(AtomicU8::new(0)),
            pending_center_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            last_remote_target: Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8())),
            target_epoch: Arc::new(AtomicU64::new(0)),
            next_clipboard_request: Arc::new(AtomicU64::new(1)),
            pending_clipboard_request: Arc::new(Mutex::new(None)),
            runtime_stats: Arc::new(RuntimeStats::default()),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
        }
    }

    #[test]
    fn stale_remote_disconnect_does_not_match_replacement() {
        let (input_tx, _input_rx) = mpsc::channel(1);
        let current_id = iroh::SecretKey::generate().public();
        let stale_id = iroh::SecretKey::generate().public();
        let current = RemotePeer {
            input_tx,
            next_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            remote_id: current_id,
            generation: 2,
            name: "current-peer".to_string(),
        };

        assert!(is_current_remote(Some(&current), current_id, 2));
        assert!(!is_current_remote(Some(&current), current_id, 1));
        assert!(!is_current_remote(Some(&current), stale_id, 2));
    }

    #[test]
    fn host_facing_edge_matches_layout() {
        assert!(is_host_facing_edge(Side::Right, ScreenEdge::Left));
        assert!(is_host_facing_edge(Side::Left, ScreenEdge::Right));
        assert!(is_host_facing_edge(Side::Up, ScreenEdge::Down));
        assert!(is_host_facing_edge(Side::Down, ScreenEdge::Up));
        assert!(!is_host_facing_edge(Side::Right, ScreenEdge::Right));
        assert!(!is_host_facing_edge(Side::Up, ScreenEdge::Up));
    }

    #[test]
    fn opposite_directional_shortcut_returns_to_local() {
        assert_eq!(
            resolve_directional_target(ActiveTarget::Right, ActiveTarget::Left),
            ActiveTarget::Local
        );
        assert_eq!(
            resolve_directional_target(ActiveTarget::Left, ActiveTarget::Right),
            ActiveTarget::Local
        );
        assert_eq!(
            resolve_directional_target(ActiveTarget::Up, ActiveTarget::Down),
            ActiveTarget::Local
        );
        assert_eq!(
            resolve_directional_target(ActiveTarget::Down, ActiveTarget::Up),
            ActiveTarget::Local
        );
    }

    #[test]
    fn directional_shortcut_keeps_requested_target_when_not_opposite() {
        assert_eq!(
            resolve_directional_target(ActiveTarget::Local, ActiveTarget::Right),
            ActiveTarget::Right
        );
        assert_eq!(
            resolve_directional_target(ActiveTarget::Right, ActiveTarget::Right),
            ActiveTarget::Right
        );
        assert_eq!(
            resolve_directional_target(ActiveTarget::Right, ActiveTarget::Up),
            ActiveTarget::Up
        );
    }

    #[test]
    fn clipboard_uses_active_remote_or_last_connected_remote() {
        let remotes = HashMap::from([(Side::Left, 1), (Side::Right, 2)]);
        assert_eq!(
            choose_clipboard_side(ActiveTarget::Right, Some(Side::Left), &remotes),
            Some(Side::Right)
        );
        assert_eq!(
            choose_clipboard_side(ActiveTarget::Local, Some(Side::Left), &remotes),
            Some(Side::Left)
        );
        assert_eq!(
            choose_clipboard_side(ActiveTarget::Local, Some(Side::Down), &remotes),
            Some(Side::Right)
        );
        assert_eq!(
            choose_clipboard_side(ActiveTarget::Local, None, &HashMap::new()),
            None
        );
    }

    #[test]
    fn clipboard_response_rejects_focus_or_generation_changes() {
        let pending = PendingClipboardRequest {
            request_id: 7,
            side: Side::Right,
            generation: 3,
            target_epoch: 2,
        };
        assert!(clipboard_response_matches(
            pending,
            7,
            Side::Right,
            3,
            2,
            ActiveTarget::Local
        ));
        assert!(!clipboard_response_matches(
            pending,
            7,
            Side::Right,
            4,
            2,
            ActiveTarget::Local
        ));
        assert!(!clipboard_response_matches(
            pending,
            7,
            Side::Right,
            3,
            3,
            ActiveTarget::Local
        ));
        assert!(!clipboard_response_matches(
            pending,
            7,
            Side::Right,
            3,
            2,
            ActiveTarget::Right
        ));
    }

    #[test]
    fn saturating_add_protects_overflow() {
        assert_eq!(saturating_add_i32(i32::MAX, 10), i32::MAX);
        assert_eq!(saturating_add_i32(i32::MIN, -10), i32::MIN);
    }

    #[test]
    fn screen_edge_maps_to_side() {
        assert_eq!(side_from_edge(ScreenEdge::Left), Side::Left);
        assert_eq!(side_from_edge(ScreenEdge::Right), Side::Right);
        assert_eq!(side_from_edge(ScreenEdge::Up), Side::Up);
        assert_eq!(side_from_edge(ScreenEdge::Down), Side::Down);
    }

    #[test]
    fn physical_key_identity_is_stable_across_logical_release_changes() {
        let press = WireKey {
            physical_code: Some(37),
            logical: "A".to_string(),
        };
        let release = WireKey {
            physical_code: Some(37),
            logical: "KeyL".to_string(),
        };
        assert_eq!(wire_key_identity(&press), wire_key_identity(&release));
    }

    #[test]
    fn control_press_and_release_update_forwarded_modifier_state() {
        let mut modifiers = ModifierFlags::default();
        let mut pressed_keys = HashSet::new();
        let mut layout = LayoutTranslator::new();

        let press = wire_event_from_rdev(
            EventType::KeyPress(Key::ControlLeft),
            &mut modifiers,
            &mut pressed_keys,
            &mut layout,
        );
        assert!(matches!(
            press,
            WireEvent::Key {
                action: KeyAction::Down,
                modifiers: ModifierFlags {
                    left_control: true,
                    ..
                },
                ..
            }
        ));

        let release = wire_event_from_rdev(
            EventType::KeyRelease(Key::ControlLeft),
            &mut modifiers,
            &mut pressed_keys,
            &mut layout,
        );
        assert!(matches!(
            release,
            WireEvent::Key {
                action: KeyAction::Up,
                modifiers: ModifierFlags {
                    left_control: false,
                    ..
                },
                ..
            }
        ));
        assert!(pressed_keys.is_empty());
        assert_eq!(modifiers, ModifierFlags::default());
    }

    #[test]
    fn standard_mouse_buttons_have_stable_wire_values() {
        assert_eq!(button_to_wire(rdev::Button::Left), 1);
        assert_eq!(button_to_wire(rdev::Button::Middle), 2);
        assert_eq!(button_to_wire(rdev::Button::Right), 3);
    }

    #[test]
    fn ordered_discrete_events_flush_only_for_remote_targets() {
        let remote = CapturedInput {
            target: ActiveTarget::Right,
            event: CapturedEvent::MouseWheel {
                delta_x: 1,
                delta_y: -1,
            },
        };
        let local_motion = CapturedInput {
            target: ActiveTarget::Local,
            event: CapturedEvent::MouseMoveRelative { dx: 1, dy: 1 },
        };
        let edge = CapturedInput {
            target: ActiveTarget::Local,
            event: CapturedEvent::HostEdgeReached {
                edge: ScreenEdge::Right,
            },
        };

        assert_eq!(side_requiring_ordered_flush(&remote), Some(Side::Right));
        assert_eq!(side_requiring_ordered_flush(&local_motion), None);
        assert_eq!(side_requiring_ordered_flush(&edge), None);
    }

    #[test]
    fn captured_events_from_previous_target_are_discarded() {
        assert!(is_stale_captured_target(
            ActiveTarget::Right,
            ActiveTarget::Local
        ));
        assert!(!is_stale_captured_target(
            ActiveTarget::Right,
            ActiveTarget::Right
        ));
    }

    #[test]
    fn synthetic_target_is_initialized_once_per_attachment() {
        assert!(should_initialize_synthetic_target(true, false));
        assert!(!should_initialize_synthetic_target(true, true));
        assert!(!should_initialize_synthetic_target(false, false));
    }

    #[tokio::test]
    async fn transition_flushes_motion_before_release_all() {
        let (input_tx, mut input_rx) = mpsc::channel(4);
        let state = test_forward_state(Side::Right, input_tx);
        let order_task = tokio::spawn(async move {
            assert!(matches!(
                input_rx.recv().await,
                Some(PeerMessage {
                    message: HostToClientMessage::RelativeMotion { dx: 3, dy: 4, .. },
                    ..
                })
            ));
            let release = input_rx.recv().await.expect("release-all message");
            assert!(matches!(
                release.message,
                HostToClientMessage::ReleaseAll { .. }
            ));
            release
                .complete
                .expect("release completion")
                .send(true)
                .expect("release completion receiver");
        });
        state
            .pending_release_sides
            .store(Side::Right.release_bit(), Ordering::Release);
        let mut pending_relative = HashMap::from([(Side::Right, (3, 4))]);
        let mut modifier_flags = ModifierFlags {
            left_control: true,
            ..ModifierFlags::default()
        };
        let mut pressed_keys = HashSet::from([Key::ControlLeft]);

        reconcile_target_transition(
            &state,
            &mut pending_relative,
            &mut modifier_flags,
            &mut pressed_keys,
        )
        .await;

        assert_eq!(modifier_flags, ModifierFlags::default());
        assert!(pressed_keys.is_empty());
        order_task.await.expect("writer order task");
    }

    #[tokio::test]
    async fn closed_writer_records_release_recovery_and_returns_local() {
        let (input_tx, input_rx) = mpsc::channel(1);
        drop(input_rx);
        let state = test_forward_state(Side::Right, input_tx);
        state
            .active_target
            .store(ActiveTarget::Right.to_u8(), Ordering::Release);

        send_release_all_to_side(&state, Side::Right).await;

        assert_eq!(
            ActiveTarget::from_u8(state.active_target.load(Ordering::Acquire)),
            ActiveTarget::Local
        );
        assert_eq!(
            state.runtime_stats.recovery_events.load(Ordering::Acquire),
            1
        );
    }
}
