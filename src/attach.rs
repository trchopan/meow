use std::str::FromStr;

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets};
use rdev::{EventType, Key, simulate};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    cli::AttachArgs,
    protocol::{
        ALPN, AuthRequest, AuthResponse, MAX_MSG_SIZE, WireMessage, read_framed, write_framed,
    },
};

pub(crate) async fn run_attach(args: AttachArgs) -> Result<()> {
    let host_id = EndpointId::from_str(&args.host_id).context("invalid host endpoint id")?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::generate())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("failed to create iroh endpoint")?;

    let connection = endpoint
        .connect(host_id, ALPN)
        .await
        .context("failed to connect to host")?;

    let (mut send, mut recv) = connection.open_bi().await?;
    let auth = AuthRequest {
        secret: args.secret,
        side: args.side,
        name: format!("remote-{}", Uuid::new_v4().simple()),
    };
    write_framed(&mut send, &auth).await?;

    let response: AuthResponse = read_framed(&mut recv).await?;
    if !response.ok {
        bail!("host denied attach: {}", response.message);
    }

    println!("Attached to host as {:?}", args.side);
    info!("client attach complete, waiting for forwarded events");

    loop {
        let mut recv = connection.accept_uni().await?;
        let bytes = recv.read_to_end(MAX_MSG_SIZE).await?;
        debug!(
            "client received {} byte(s) on forwarded stream",
            bytes.len()
        );
        let message: WireMessage = bincode::deserialize(&bytes)?;
        match message {
            WireMessage::Input { event } => {
                debug!("client received input event: {:?}", event);
                if let Err(err) = simulate(&event) {
                    warn!("failed injecting input event: {err:?}");
                } else {
                    debug!("client simulate ok for event: {:?}", event);
                }
            }
        }
    }
}

pub(crate) async fn run_test_inject() -> Result<()> {
    println!("running local injection test in 2 seconds...");
    println!("this will move mouse slightly and type 'meowtest'");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let (x, y) = rdev::display_size().unwrap_or((0, 0));
    let center_x = (x as f64 / 2.0).max(20.0);
    let center_y = (y as f64 / 2.0).max(20.0);

    let sequence = [
        EventType::MouseMove {
            x: center_x,
            y: center_y,
        },
        EventType::MouseMove {
            x: center_x + 20.0,
            y: center_y + 20.0,
        },
        EventType::MouseMove {
            x: center_x,
            y: center_y,
        },
        EventType::KeyPress(Key::KeyM),
        EventType::KeyRelease(Key::KeyM),
        EventType::KeyPress(Key::KeyE),
        EventType::KeyRelease(Key::KeyE),
        EventType::KeyPress(Key::KeyO),
        EventType::KeyRelease(Key::KeyO),
        EventType::KeyPress(Key::KeyW),
        EventType::KeyRelease(Key::KeyW),
        EventType::KeyPress(Key::KeyT),
        EventType::KeyRelease(Key::KeyT),
        EventType::KeyPress(Key::KeyE),
        EventType::KeyRelease(Key::KeyE),
        EventType::KeyPress(Key::KeyS),
        EventType::KeyRelease(Key::KeyS),
        EventType::KeyPress(Key::KeyT),
        EventType::KeyRelease(Key::KeyT),
    ];

    for event in sequence {
        debug!("test-inject event: {:?}", event);
        match simulate(&event) {
            Ok(()) => debug!("test-inject simulate ok: {:?}", event),
            Err(err) => {
                warn!("test-inject simulate failed for {:?}: {err:?}", event);
                bail!("local input injection test failed; check Accessibility permission")
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(35)).await;
    }

    println!("test-inject finished");
    Ok(())
}
