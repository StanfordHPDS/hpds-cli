//! Clap derive command tree: one file per top-level command.
//!
//! Commands not yet implemented are stubbed with a
//! typed [`NotYetImplemented`] error, rendered by `main` with exit code 2.

mod audit;
mod completions;
mod config;
mod format;
mod git;
mod init;
mod install;
mod lint;
mod project;
mod repo;
mod setup;
mod tools;
mod upgrade;
mod r#use;
mod version;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Unified tooling for the Stanford HPDS lab: format, lint, templates,
/// setup, and repo audits.
#[derive(Debug, Parser)]
#[command(name = "hpds", version, arg_required_else_help = true)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every command.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Show more detail (underlying commands, tool names)
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Suppress all output except errors
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Use this config file instead of discovering hpds.toml
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Format project files in place (R, Python, Quarto, SQL, Markdown)
    Format,
    /// Report lint violations across the project
    Lint,
    /// Set up a new or existing project interactively
    Init,
    /// Project commands (`hpds project init` is an alias for `hpds init`)
    Project(project::ProjectArgs),
    /// Apply a template component to the current project
    Use,
    /// Install external software (r, quarto, uv, gh, rig, tinytex, duckdb)
    Install,
    /// Set up a fresh machine with the lab toolchain
    Setup,
    /// Git helpers: sensible defaults and global ignore vaccination
    Git(git::GitArgs),
    /// GitHub repository helpers
    Repo(repo::RepoArgs),
    /// Audit the current repo (or the whole org) against lab standards
    Audit(audit::AuditArgs),
    /// Manage hpds-installed formatter/linter tools (advanced)
    Tools(tools::ToolsArgs),
    /// Print the resolved configuration and where each value came from
    Config(config::ConfigArgs),
    /// Generate shell completions
    Completions(completions::CompletionsArgs),
    /// Print the hpds version
    Version,
    /// Upgrade hpds to the latest release
    Upgrade,
}

/// Dispatch a parsed CLI invocation to its command module.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    let global = cli.global;
    match cli.command {
        Command::Format => format::run(),
        Command::Lint => lint::run(),
        Command::Init => init::run(),
        Command::Project(args) => project::run(args),
        Command::Use => r#use::run(),
        Command::Install => install::run(),
        Command::Setup => setup::run(),
        Command::Git(args) => git::run(args),
        Command::Repo(args) => repo::run(args),
        Command::Audit(args) => audit::run(args),
        Command::Tools(args) => tools::run(args),
        Command::Config(args) => config::run(args, &global),
        Command::Completions(args) => completions::run(args),
        Command::Version => version::run(),
        Command::Upgrade => upgrade::run(),
    }
}

/// Typed error for command stubs whose implementation lands in a later
/// `main` renders it cleanly and exits 2.
#[derive(Debug, thiserror::Error)]
#[error("`hpds {command}` is not yet implemented")]
pub struct NotYetImplemented {
    /// Full subcommand path, e.g. `"git vaccinate"`.
    command: &'static str,
}

impl NotYetImplemented {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        "this command is planned but not built yet; run `hpds --help` to see what works today"
            .to_string()
    }
}

/// Convenience constructor for stubbed commands.
pub(crate) fn not_yet_implemented(command: &'static str) -> anyhow::Error {
    anyhow::Error::new(NotYetImplemented { command })
}
