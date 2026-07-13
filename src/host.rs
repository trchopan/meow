use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
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
    ipc::{IpcCommand, cleanup_stale_socket, run_control_socket, send_ipc},
    model::{ActiveTarget, CapturedInput, HostState, RemotePeer},
    presentation::print_host_ready,
    protocol::{
        ALPN, AuthRequest, AuthResponse, WireMessage, read_framed, send_wire_message, write_framed,
    },
    state::{host_state_path, load_or_create_host_secret_key, load_or_create_host_state},
};

pub(crate) async fn run_host() -> Result<()> {
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
    let remotes = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    print_host_ready(&endpoint_id.to_string(), &secret);

    let state = HostState {
        endpoint_id,
        active_target: active_target.clone(),
        remotes: remotes.clone(),
    };

    let (input_tx, input_rx) = mpsc::unbounded_channel::<CapturedInput>();

    let input_active_target = active_target.clone();
    let input_detach_chord = detach_chord.clone();
    std::thread::spawn(move || {
        if let Err(err) = run_input_grab(input_tx, input_active_target, input_detach_chord) {
            error!("input grab stopped: {err:#}");
        }
    });

    tokio::spawn(run_forward_loop(
        input_rx,
        state.clone(),
        active_target.clone(),
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
        let active_target = active_target.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_incoming(incoming, state, &secret, active_target).await {
                warn!("incoming peer rejected/failed: {err:#}");
            }
        });
    }
}

async fn handle_incoming(
    incoming: Incoming,
    state: HostState,
    secret: &str,
    active_target: Arc<AtomicU8>,
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

        if ActiveTarget::from_u8(active_target.load(Ordering::Relaxed)).to_side() == Some(auth.side)
        {
            active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
        }
        info!("remote disconnected: {:?} ({remote_id})", auth.side);
    });

    Ok(())
}

async fn run_forward_loop(
    mut rx: mpsc::UnboundedReceiver<CapturedInput>,
    state: HostState,
    active_target: Arc<AtomicU8>,
) {
    while let Some(captured) = rx.recv().await {
        let Some(side) = captured.target.to_side() else {
            continue;
        };

        let peer = {
            let remotes = state.remotes.read().await;
            remotes.get(&side).cloned()
        };

        let Some(peer) = peer else {
            warn!(
                "no remote attached for target {:?}; falling back to local control",
                side
            );
            active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
            continue;
        };

        let message = WireMessage::Input {
            event: captured.event,
        };
        debug!("forwarding input event to {:?} ({})", side, peer.name);
        if let Err(err) = send_wire_message(&peer.connection, &message).await {
            warn!("failed forwarding to {:?} ({}): {err:#}", side, peer.name);
            active_target.store(ActiveTarget::Local.to_u8(), Ordering::Relaxed);
        }
    }
}
