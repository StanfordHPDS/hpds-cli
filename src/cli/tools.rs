//! `hpds tools` — manage hpds-installed formatter/linter tools (spec §4).

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct ToolsArgs {
    #[command(subcommand)]
    pub command: ToolsCommand,
}

#[derive(Debug, Subcommand)]
pub enum ToolsCommand {
    /// List installed tools and their versions
    List,
    /// Refresh tools to release defaults or config pins
    Update,
    /// Remove the tool cache
    Clean,
}

pub fn run(args: ToolsArgs) -> anyhow::Result<()> {
    match args.command {
        // Stubs until M1.4.
        ToolsCommand::List => Err(super::not_yet_implemented("tools list")),
        ToolsCommand::Update => Err(super::not_yet_implemented("tools update")),
        ToolsCommand::Clean => Err(super::not_yet_implemented("tools clean")),
    }
}
