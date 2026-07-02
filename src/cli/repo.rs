//! `hpds repo` — GitHub repository helpers.

use clap::{Args, Subcommand};

use crate::gitx;

#[derive(Debug, Args)]
pub struct RepoArgs {
    #[command(subcommand)]
    pub command: RepoCommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Create a GitHub repo for the current project (lab-manual gh flow)
    Create(CreateArgs),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Repository name [default: the current directory's name]
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// GitHub organization (or user) to create the repo under
    #[arg(long, value_name = "ORG", help = format!("GitHub organization (or user) to create the repo under [default: {}]", gitx::repo::DEFAULT_ORG))]
    pub org: Option<String>,

    /// Repository visibility [default: private]
    #[arg(long, value_enum, value_name = "VISIBILITY")]
    pub visibility: Option<gitx::repo::Visibility>,

    /// Accept the default for every unanswered question; never prompt
    #[arg(short = 'y', long)]
    pub yes: bool,
}

pub fn run(args: RepoArgs) -> anyhow::Result<()> {
    match args.command {
        RepoCommand::Create(args) => gitx::repo::create(gitx::repo::CreateOptions {
            name: args.name,
            org: args.org,
            visibility: args.visibility,
            yes: args.yes,
        }),
    }
}
