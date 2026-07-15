use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use rdev::EventType;
use serde::{Deserialize, Serialize};

use crate::model::{ScreenEdge, Side};

pub(crate) const ALPN: &[u8] = b"meow/remote-input/0";
pub(crate) const MAX_MSG_SIZE: usize = 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AuthRequest {
    pub(crate) secret: String,
    pub(crate) side: Side,
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AuthResponse {
    pub(crate) ok: bool,
    pub(crate) message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum WireMessage {
    Input { event: EventType },
    MouseMoveRelative { dx: i32, dy: i32 },
    ClientEdgeReached { edge: ScreenEdge },
}

pub(crate) async fn send_wire_message(
    connection: &Connection,
    message: &WireMessage,
) -> Result<()> {
    let bytes = bincode::serialize(message)?;
    let mut send = connection.open_uni().await?;
    send.write_all(&bytes).await?;
    send.finish()?;
    Ok(())
}

pub(crate) async fn write_framed<T: Serialize>(
    stream: &mut iroh::endpoint::SendStream,
    value: &T,
) -> Result<()> {
    let bytes = bincode::serialize(value)?;
    let len = bytes.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

pub(crate) async fn read_framed<T: for<'de> Deserialize<'de>>(
    stream: &mut iroh::endpoint::RecvStream,
) -> Result<T> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    ensure_frame_len(len)?;
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes).await?;
    Ok(bincode::deserialize(&bytes)?)
}

fn ensure_frame_len(len: usize) -> Result<()> {
    if len > MAX_MSG_SIZE {
        bail!("framed message too large: {len}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_frame_len_accepts_limits() {
        assert!(ensure_frame_len(0).is_ok());
        assert!(ensure_frame_len(MAX_MSG_SIZE).is_ok());
    }

    #[test]
    fn ensure_frame_len_rejects_oversize() {
        let err = ensure_frame_len(MAX_MSG_SIZE + 1).expect_err("must reject oversize frame");
        assert!(err.to_string().contains("framed message too large"));
    }

    #[test]
    fn wire_message_round_trip_serde() {
        let msg = WireMessage::MouseMoveRelative { dx: -42, dy: 17 };
        let bytes = bincode::serialize(&msg).expect("serialize");
        let round_trip: WireMessage = bincode::deserialize(&bytes).expect("deserialize");
        match round_trip {
            WireMessage::MouseMoveRelative { dx, dy } => {
                assert_eq!(dx, -42);
                assert_eq!(dy, 17);
            }
            _ => panic!("unexpected wire message variant"),
        }
    }
}
