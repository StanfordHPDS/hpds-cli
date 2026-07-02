//! `hpds git` — git helpers: defaults + ignore vaccination (spec §7, §9).

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct GitArgs {
    #[command(subcommand)]
    pub command: GitCommand,
}

#[derive(Debug, Subcommand)]
pub enum GitCommand {
    /// Configure sensible git defaults and gh auth guidance
    Setup,
    /// Add R/Python/editor junk patterns to the global git ignore
    Vaccinate,
}

pub fn run(args: GitArgs) -> anyhow::Result<()> {
    match args.command {
        // Stub until M4.4 (setup) and M7.1 (vaccinate).
        GitCommand::Setup => Err(super::not_yet_implemented("git setup")),
        GitCommand::Vaccinate => Err(super::not_yet_implemented("git vaccinate")),
    }
}
