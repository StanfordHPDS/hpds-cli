//! `hpds git` — git helpers: defaults + ignore vaccination.

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
        // Stubs until the setup and vaccinate implementations land.
        GitCommand::Setup => Err(super::not_yet_implemented("git setup")),
        GitCommand::Vaccinate => Err(super::not_yet_implemented("git vaccinate")),
    }
}
