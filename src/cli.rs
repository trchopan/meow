use clap::{Parser, Subcommand};

use crate::{
    input::{DEFAULT_EDGE_DWELL_MS, DEFAULT_EDGE_ZONE_PX},
    model::{RemotePointerMode, Side},
};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum OverlayPosition {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

#[derive(Parser, Debug)]
#[command(name = "meow", version, about = "Control nearby machines with iroh")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    Host(HostArgs),
    Attach(AttachArgs),
    #[command(hide = true)]
    DevSmoke(DevSmokeArgs),
    #[command(hide = true)]
    ProbePointerLock(ProbePointerLockArgs),
    ResetIdentity,
    RotateSecret,
    #[command(hide = true)]
    TestInject,
    #[command(hide = true)]
    BenchFlush(BenchFlushArgs),
    #[command(hide = true)]
    OverlayUi(OverlayUiArgs),
    Local,
    Right,
    Left,
    Up,
    Down,
    PointerMode(PointerModeArgs),
    Status,
    Stop,
}

#[derive(Debug, clap::Args)]
pub(crate) struct HostArgs {
    /// Width of the screen edge activation zone in pixels.
    #[arg(long, default_value_t = DEFAULT_EDGE_ZONE_PX, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub(crate) edge_zone_px: u32,
    /// Time the pointer must remain in the edge zone before switching.
    #[arg(long, default_value_t = DEFAULT_EDGE_DWELL_MS, value_parser = clap::value_parser!(u64).range(0..=10_000))]
    pub(crate) edge_dwell_ms: u64,
}

#[derive(Debug, clap::Args)]
pub(crate) struct DevSmokeArgs {
    #[arg(long, default_value_t = 5)]
    pub(crate) duration_secs: u64,
    #[arg(long, value_enum, default_value_t = Side::Right)]
    pub(crate) side: Side,
}

#[derive(Debug, clap::Args)]
pub(crate) struct BenchFlushArgs {
    #[arg(long, default_value_t = 30)]
    pub(crate) duration_secs: u64,
    #[arg(long, value_enum, default_value_t = Side::Right)]
    pub(crate) side: Side,
    #[arg(long, default_value_t = 1000)]
    pub(crate) event_rate_hz: u64,
    #[arg(long, default_value_t = 5)]
    pub(crate) runs: u64,
    #[arg(long, default_value_t = 6)]
    pub(crate) flush_a_ms: u64,
    #[arg(long, default_value_t = 2)]
    pub(crate) flush_b_ms: u64,
    #[arg(long, default_value_t = 3)]
    pub(crate) dx: i32,
    #[arg(long, default_value_t = 2)]
    pub(crate) dy: i32,
}

#[derive(Debug, clap::Args)]
pub(crate) struct PointerModeArgs {
    #[arg(value_enum)]
    pub(crate) mode: RemotePointerMode,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AttachArgs {
    pub(crate) host_id: String,
    pub(crate) secret: String,
    #[arg(long, value_enum)]
    pub(crate) side: Side,
    #[arg(long, default_value_t = false, hide = true)]
    pub(crate) probe_received: bool,
    #[arg(long, default_value_t = 0, hide = true)]
    pub(crate) probe_duration_secs: u64,
    #[arg(long, default_value_t = false, hide = true)]
    pub(crate) no_inject: bool,
    #[arg(long, default_value_t = false, hide = true)]
    pub(crate) probe_summary_only: bool,
    #[arg(long, hide = true)]
    pub(crate) test_drop_sequence: Option<u64>,
    #[arg(long, default_value_t = false)]
    pub(crate) input_overlay: bool,
    #[arg(long, value_enum, default_value_t = OverlayPosition::TopRight)]
    pub(crate) input_overlay_position: OverlayPosition,
    #[arg(long, default_value_t = 1500)]
    pub(crate) input_overlay_idle_ms: u64,
}

#[derive(Debug, clap::Args)]
pub(crate) struct OverlayUiArgs {
    #[arg(long, value_enum, default_value_t = OverlayPosition::TopRight)]
    pub(crate) position: OverlayPosition,
    #[arg(long, default_value_t = 1500)]
    pub(crate) idle_ms: u64,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ProbePointerLockArgs {
    #[arg(long, default_value_t = 10)]
    pub(crate) duration_secs: u64,
}
