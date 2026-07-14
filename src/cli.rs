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
    ProbePointerLock(ProbePointerLockArgs),
    ResetIdentity,
    RotateSecret,
    #[command(hide = true)]
    TestInject,
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
}

#[derive(Debug, clap::Args)]
pub(crate) struct ProbePointerLockArgs {
    #[arg(long, default_value_t = 10)]
    pub(crate) duration_secs: u64,
}
