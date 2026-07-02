//! `hpds project` — project commands; `project init` aliases `hpds init`
//! (spec §6).

use clap::{Args, Subcommand};

use super::init;

#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Set up a new or existing project interactively (alias for `hpds init`)
    Init,
}

pub fn run(args: ProjectArgs) -> anyhow::Result<()> {
    match args.command {
        ProjectCommand::Init => init::run(),
    }
}
