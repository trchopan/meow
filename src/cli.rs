use clap::{Parser, Subcommand};

use crate::model::Side;

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
    ResetIdentity,
    RotateSecret,
    TestInject,
    Local,
    Right,
    Left,
    Up,
    Down,
    Status,
    Stop,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AttachArgs {
    pub(crate) host_id: String,
    pub(crate) secret: String,
    #[arg(long, value_enum)]
    pub(crate) side: Side,
}
