use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use rdev::EventType;
use serde::{Deserialize, Serialize};

use crate::model::Side;

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
    if len > MAX_MSG_SIZE {
        bail!("framed message too large: {len}");
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes).await?;
    Ok(bincode::deserialize(&bytes)?)
}
