//! `hpds project`: project commands; `project init` aliases `hpds init`
//!.

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
    ///
    /// Identical to `hpds init`: the interactive project-setup wizard. See
    /// `hpds init --help` for every option.
    Init(init::InitArgs),
}

pub fn run(args: ProjectArgs) -> anyhow::Result<()> {
    match args.command {
        ProjectCommand::Init(args) => init::run(args),
    }
}
