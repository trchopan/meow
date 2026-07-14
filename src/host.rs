use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint,
    endpoint::{Incoming, presets},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    input::{parse_detach_chord, run_input_grab},
    ipc::{IpcCommand, apply_target_change, cleanup_stale_socket, run_control_socket, send_ipc},
    macos_permissions::ensure_host_permissions_on_startup,
    macos_mouse_delta::run_macos_mouse_delta_capture,
    model::{ActiveTarget, CapturedEvent, CapturedInput, HostState, RemotePeer, Side},
    presentation::print_host_ready,
    protocol::{
        ALPN, AuthRequest, AuthResponse, WireMessage, read_framed, send_wire_message, write_framed,
    },
    state::{host_state_path, load_or_create_host_secret_key, load_or_create_host_state},
};

pub(crate) async fn run_host() -> Result<()> {
    ensure_host_permissions_on_startup()?;

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

    let active_target = Arc::new(AtomicU8::new(ActiveTarget::Local.to_u8()));
    let pointer_lock_active = Arc::new(AtomicBool::new(false));
    let pinned_pointer_pos = Arc::new(Mutex::new(None));
    let remotes = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    print_host_ready(&endpoint_id.to_string(), &secret);

    let state = HostState {
        endpoint_id,
        active_target: active_target.clone(),
        pointer_lock_active: pointer_lock_active.clone(),
        pinned_pointer_pos: pinned_pointer_pos.clone(),
        remotes: remotes.clone(),
    };

    let (input_tx, input_rx) = mpsc::unbounded_channel::<CapturedInput>();
    let mouse_delta_tx = input_tx.clone();

    let input_active_target = active_target.clone();
    let input_pointer_lock_active = pointer_lock_active.clone();
    let input_pinned_pointer_pos = pinned_pointer_pos.clone();
    let input_detach_chord = detach_chord.clone();
    std::thread::spawn(move || {
        if let Err(err) = run_input_grab(
            input_tx,
            input_active_target,
            input_pointer_lock_active,
            input_pinned_pointer_pos,
            input_detach_chord,
        ) {
            error!("input grab stopped: {err:#}");
        }
    });

    let mouse_delta_active_target = active_target.clone();
    let mouse_delta_pointer_lock_active = pointer_lock_active.clone();
    std::thread::spawn(move || {
        if let Err(err) = run_macos_mouse_delta_capture(
            mouse_delta_tx,
            mouse_delta_active_target,
            mouse_delta_pointer_lock_active,
        ) {
            error!("macOS mouse delta capture stopped: {err:#}");
        }
    });

    tokio::spawn(run_forward_loop(
        input_rx,
        state.clone(),
    ));

    let control_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = run_control_socket(control_state).await {
            error!("control socket failed: {err:#}");
        }
    });

    loop {
        let Some(incoming) = endpoint.accept().await else {
            bail!("endpoint closed")
        };

        let state = state.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, state, &secret).await {
                warn!("incoming peer rejected/failed: {err:#}");
            }
        });
    }
}

async fn handle_incoming(
    incoming: Incoming,
    state: HostState,
    secret: &str,
) -> Result<()> {
    let connection = incoming.accept()?.await?;
    let remote_id = connection.remote_id();
    let (mut send, mut recv) = connection.accept_bi().await?;

    let auth: AuthRequest = read_framed(&mut recv).await?;
    if auth.secret != secret {
        let res = AuthResponse {
            ok: false,
            message: "invalid secret".to_string(),
        };
        write_framed(&mut send, &res).await?;
        bail!("invalid secret from {remote_id}")
    }

    {
        let mut remotes = state.remotes.write().await;
        remotes.insert(
            auth.side,
            RemotePeer {
                connection: connection.clone(),
                remote_id,
                name: auth.name.clone(),
            },
        );
    }

    write_framed(
        &mut send,
        &AuthResponse {
            ok: true,
            message: format!("attached as {:?}", auth.side),
        },
    )
    .await?;

    info!(
        "remote attached: {:?} ({remote_id}) name={}",
        auth.side, auth.name
    );

    let remotes = state.remotes.clone();
    tokio::spawn(async move {
        connection.closed().await;
        let mut remotes = remotes.write().await;
        if let Some(existing) = remotes.get(&auth.side)
            && existing.remote_id == remote_id
        {
            remotes.remove(&auth.side);
        }

        if ActiveTarget::from_u8(state.active_target.load(Ordering::Relaxed)).to_side()
            == Some(auth.side)
        {
            apply_target_change(&state, ActiveTarget::Local, "remote disconnect");
        }
        info!("remote disconnected: {:?} ({remote_id})", auth.side);
    });

    Ok(())
}

async fn run_forward_loop(
    mut rx: mpsc::UnboundedReceiver<CapturedInput>,
    state: HostState,
) {
    use tokio::time::{self, MissedTickBehavior};

    let mut pending_relative: HashMap<Side, (i32, i32)> = HashMap::new();
    let mut flush_tick = time::interval(Duration::from_millis(6));
    flush_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = flush_tick.tick() => {
                flush_pending_relative(&state, &mut pending_relative).await;
            }
            maybe_captured = rx.recv() => {
                let Some(captured) = maybe_captured else {
                    flush_pending_relative(&state, &mut pending_relative).await;
                    break;
                };

                let Some(side) = captured.target.to_side() else {
                    continue;
                };

                match captured.event {
                    CapturedEvent::Raw(event) => {
                        flush_relative_for_side(&state, &mut pending_relative, side).await;
                        let message = WireMessage::Input { event };
                        if !send_to_side(&state, side, &message).await {
                            continue;
                        }
                    }
                    CapturedEvent::MouseMoveRelative { dx, dy } => {
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

async fn flush_pending_relative(state: &HostState, pending_relative: &mut HashMap<Side, (i32, i32)>) {
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
    let message = WireMessage::MouseMoveRelative { dx, dy };
    let _ = send_to_side(state, side, &message).await;
}

async fn send_to_side(state: &HostState, side: Side, message: &WireMessage) -> bool {
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

    debug!("forwarding input event to {:?} ({})", side, peer.name);
    if let Err(err) = send_wire_message(&peer.connection, message).await {
        warn!("failed forwarding to {:?} ({}): {err:#}", side, peer.name);
        apply_target_change(state, ActiveTarget::Local, "forwarding failure");
        return false;
    }

    true
}

fn saturating_add_i32(lhs: i32, rhs: i32) -> i32 {
    lhs.saturating_add(rhs)
}
