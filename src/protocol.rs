use anyhow::{Result, bail};
use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize};

use crate::model::{ScreenEdge, Side};

pub(crate) const ALPN: &[u8] = b"meow/remote-input/1";
pub(crate) const MAX_AUTH_MSG_SIZE: usize = 16 * 1024;
pub(crate) const MAX_INPUT_MSG_SIZE: usize = 64 * 1024;
pub(crate) const MAX_FEEDBACK_MSG_SIZE: usize = 4 * 1024;

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
pub(crate) enum HostToClientMessage {
    Event { seq: u64, event: WireEvent },
    RelativeMotion { seq: u64, dx: i32, dy: i32 },
    ReleaseAll { seq: u64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ModifierFlags {
    pub(crate) left_shift: bool,
    pub(crate) right_shift: bool,
    pub(crate) left_control: bool,
    pub(crate) right_control: bool,
    pub(crate) left_alt: bool,
    pub(crate) right_alt: bool,
    pub(crate) left_meta: bool,
    pub(crate) right_meta: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WireKey {
    pub(crate) physical_code: Option<u16>,
    pub(crate) logical: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum KeyAction {
    Down,
    Repeat,
    Up,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum WireEvent {
    Key {
        action: KeyAction,
        key: WireKey,
        modifiers: ModifierFlags,
    },
    ModifierChanged {
        modifiers: ModifierFlags,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    RelativeMotion {
        dx: i32,
        dy: i32,
    },
    Wheel {
        delta_x: i64,
        delta_y: i64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ClientToHostMessage {
    ClientEdgeReached { edge: ScreenEdge },
}

pub(crate) async fn send_client_feedback(
    connection: &Connection,
    message: &ClientToHostMessage,
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

pub(crate) async fn read_framed_with_size<T: for<'de> Deserialize<'de>>(
    stream: &mut iroh::endpoint::RecvStream,
) -> Result<(T, usize)> {
    read_framed_with_size_limit(stream, MAX_INPUT_MSG_SIZE).await
}

pub(crate) async fn read_framed_with_limit<T: for<'de> Deserialize<'de>>(
    stream: &mut iroh::endpoint::RecvStream,
    max_size: usize,
) -> Result<T> {
    let (value, _) = read_framed_with_size_limit(stream, max_size).await?;
    Ok(value)
}

pub(crate) async fn read_framed_with_size_limit<T: for<'de> Deserialize<'de>>(
    stream: &mut iroh::endpoint::RecvStream,
    max_size: usize,
) -> Result<(T, usize)> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    ensure_frame_len(len, max_size)?;
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes).await?;
    Ok((bincode::deserialize(&bytes)?, len + 4))
}

fn ensure_frame_len(len: usize, max_size: usize) -> Result<()> {
    if len > max_size {
        bail!("framed message too large: {len}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_frame_len_accepts_limits() {
        assert!(ensure_frame_len(0, MAX_INPUT_MSG_SIZE).is_ok());
        assert!(ensure_frame_len(MAX_INPUT_MSG_SIZE, MAX_INPUT_MSG_SIZE).is_ok());
    }

    #[test]
    fn ensure_frame_len_rejects_oversize() {
        let err = ensure_frame_len(MAX_INPUT_MSG_SIZE + 1, MAX_INPUT_MSG_SIZE)
            .expect_err("must reject oversize frame");
        assert!(err.to_string().contains("framed message too large"));
    }

    #[test]
    fn wire_message_round_trip_serde() {
        let msg = HostToClientMessage::Event {
            seq: 7,
            event: WireEvent::RelativeMotion { dx: -42, dy: 17 },
        };
        let bytes = bincode::serialize(&msg).expect("serialize");
        let round_trip: HostToClientMessage = bincode::deserialize(&bytes).expect("deserialize");
        match round_trip {
            HostToClientMessage::Event {
                seq,
                event: WireEvent::RelativeMotion { dx, dy },
            } => {
                assert_eq!(seq, 7);
                assert_eq!(dx, -42);
                assert_eq!(dy, 17);
            }
            _ => panic!("unexpected wire message variant"),
        }
    }

    #[test]
    fn host_release_all_round_trip_serde() {
        let msg = HostToClientMessage::ReleaseAll { seq: 99 };
        let bytes = bincode::serialize(&msg).expect("serialize");
        let round_trip: HostToClientMessage = bincode::deserialize(&bytes).expect("deserialize");
        match round_trip {
            HostToClientMessage::ReleaseAll { seq } => assert_eq!(seq, 99),
            _ => panic!("unexpected wire message variant"),
        }
    }

    #[test]
    fn client_edge_feedback_round_trip_serde() {
        let msg = ClientToHostMessage::ClientEdgeReached {
            edge: ScreenEdge::Left,
        };
        let bytes = bincode::serialize(&msg).expect("serialize");
        let round_trip: ClientToHostMessage = bincode::deserialize(&bytes).expect("deserialize");
        match round_trip {
            ClientToHostMessage::ClientEdgeReached { edge } => {
                assert_eq!(edge, ScreenEdge::Left);
            }
        }
    }

    #[test]
    fn semantic_events_round_trip_serde() {
        let events = [
            WireEvent::Key {
                action: KeyAction::Down,
                key: WireKey {
                    physical_code: Some(0),
                    logical: "KeyA".to_string(),
                },
                modifiers: ModifierFlags::default(),
            },
            WireEvent::Key {
                action: KeyAction::Repeat,
                key: WireKey {
                    physical_code: Some(0),
                    logical: "KeyA".to_string(),
                },
                modifiers: ModifierFlags::default(),
            },
            WireEvent::Key {
                action: KeyAction::Up,
                key: WireKey {
                    physical_code: Some(0),
                    logical: "KeyA".to_string(),
                },
                modifiers: ModifierFlags::default(),
            },
            WireEvent::ModifierChanged {
                modifiers: ModifierFlags {
                    left_shift: true,
                    ..ModifierFlags::default()
                },
            },
            WireEvent::MouseButton {
                button: 2,
                pressed: true,
            },
            WireEvent::RelativeMotion { dx: -4, dy: 8 },
            WireEvent::Wheel {
                delta_x: 1,
                delta_y: -2,
            },
        ];

        for event in events {
            let bytes = bincode::serialize(&event).expect("serialize semantic event");
            let decoded: WireEvent = bincode::deserialize(&bytes).expect("deserialize event");
            assert_eq!(decoded, event);
        }
    }
}
