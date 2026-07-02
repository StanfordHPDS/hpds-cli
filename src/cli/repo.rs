//! `hpds repo` — GitHub repository helpers (spec §9).

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct RepoArgs {
    #[command(subcommand)]
    pub command: RepoCommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Create a GitHub repo for the current project (lab-manual gh flow)
    Create,
}

pub fn run(args: RepoArgs) -> anyhow::Result<()> {
    match args.command {
        // Stub until M7.2.
        RepoCommand::Create => Err(super::not_yet_implemented("repo create")),
    }
}
