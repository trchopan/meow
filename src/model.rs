use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64},
    },
};

use clap::ValueEnum;
use iroh::EndpointId;
use rdev::EventType;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::sync::mpsc;

use crate::protocol::HostToClientMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl Side {
    pub(crate) fn release_bit(self) -> u8 {
        match self {
            Self::Left => 1 << 0,
            Self::Right => 1 << 1,
            Self::Up => 1 << 2,
            Self::Down => 1 << 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemotePointerMode {
    #[serde(alias = "return_on_edge")]
    #[value(alias = "return-on-edge")]
    EdgeToEdge,
    Confine,
}

impl RemotePointerMode {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::EdgeToEdge => 0,
            Self::Confine => 1,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Confine,
            _ => Self::EdgeToEdge,
        }
    }
}

impl fmt::Display for RemotePointerMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::EdgeToEdge => "edge-to-edge",
            Self::Confine => "confine",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScreenEdge {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActiveTarget {
    Local,
    Left,
    Right,
    Up,
    Down,
}

impl ActiveTarget {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Left => 1,
            Self::Right => 2,
            Self::Up => 3,
            Self::Down => 4,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Left,
            2 => Self::Right,
            3 => Self::Up,
            4 => Self::Down,
            _ => Self::Local,
        }
    }

    pub(crate) fn to_side(self) -> Option<Side> {
        match self {
            Self::Local => None,
            Self::Left => Some(Side::Left),
            Self::Right => Some(Side::Right),
            Self::Up => Some(Side::Up),
            Self::Down => Some(Side::Down),
        }
    }
}

impl From<Side> for ActiveTarget {
    fn from(value: Side) -> Self {
        match value {
            Side::Left => Self::Left,
            Side::Right => Self::Right,
            Side::Up => Self::Up,
            Side::Down => Self::Down,
        }
    }
}

impl fmt::Display for ActiveTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Local => "local",
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug)]
pub(crate) struct CapturedInput {
    pub(crate) target: ActiveTarget,
    pub(crate) event: CapturedEvent,
}

#[derive(Debug)]
pub(crate) enum CapturedEvent {
    Raw(EventType),
    MouseButton { button: rdev::Button, pressed: bool },
    MouseWheel { delta_x: i64, delta_y: i64 },
    MouseMoveRelative { dx: i32, dy: i32 },
    HostEdgeReached { edge: ScreenEdge },
}

#[derive(Clone)]
pub(crate) struct RemotePeer {
    pub(crate) input_tx: mpsc::Sender<HostToClientMessage>,
    pub(crate) next_seq: Arc<AtomicU64>,
    pub(crate) remote_id: EndpointId,
    pub(crate) generation: u64,
    pub(crate) name: String,
}

#[derive(Clone)]
pub(crate) struct HostState {
    pub(crate) endpoint_id: EndpointId,
    pub(crate) active_target: Arc<AtomicU8>,
    pub(crate) remote_pointer_mode: Arc<AtomicU8>,
    pub(crate) pointer_lock_active: Arc<AtomicBool>,
    pub(crate) pointer_hidden: Arc<AtomicBool>,
    pub(crate) pinned_pointer_pos: Arc<Mutex<Option<(f64, f64)>>>,
    pub(crate) pointer_transition_lock: Arc<Mutex<()>>,
    pub(crate) pointer_lock_recovery_running: Arc<AtomicBool>,
    pub(crate) pointer_lock_recovery_target: Arc<AtomicU8>,
    pub(crate) pointer_lock_recovery_generation: Arc<AtomicU64>,
    pub(crate) remotes: Arc<RwLock<HashMap<Side, RemotePeer>>>,
    pub(crate) next_remote_generation: Arc<AtomicU64>,
    pub(crate) pending_release_sides: Arc<AtomicU8>,
    pub(crate) pending_center_target: Arc<AtomicU8>,
    pub(crate) runtime_stats: Arc<RuntimeStats>,
    pub(crate) shutdown_requested: Arc<AtomicBool>,
    pub(crate) shutdown_notify: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
pub(crate) struct RuntimeStats {
    pub(crate) captured_events: AtomicU64,
    pub(crate) normalized_events: AtomicU64,
    pub(crate) replay_failures: AtomicU64,
    pub(crate) capture_tap_user_disabled: AtomicU64,
    pub(crate) recovery_events: AtomicU64,
    pub(crate) captured_queue_full_mouse_dropped: AtomicU64,
    pub(crate) captured_queue_full_non_mouse_dropped: AtomicU64,
    pub(crate) writer_queue_full_dropped: AtomicU64,
    pub(crate) writer_queue_full_forced_local: AtomicU64,
}
