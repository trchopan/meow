use clap::{Parser, Subcommand};

use crate::model::{RemotePointerMode, Side};

#[derive(Parser, Debug)]
#[command(name = "meow", version, about = "Control nearby machines with iroh")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    Host,
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
}

#[derive(Debug, clap::Args)]
pub(crate) struct ProbePointerLockArgs {
    #[arg(long, default_value_t = 10)]
    pub(crate) duration_secs: u64,
}
