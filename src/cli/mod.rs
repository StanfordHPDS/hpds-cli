//! Clap derive command tree: one file per top-level command.
//!
//! Commands not yet implemented are stubbed with a
//! typed [`NotYetImplemented`] error, rendered by `main` with exit code 2.

mod audit;
mod audit_all;
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
    Init(init::InitArgs),
    /// Project commands (`hpds project init` is an alias for `hpds init`)
    Project(project::ProjectArgs),
    /// Apply a template component to the current project
    Use(r#use::UseArgs),
    /// Install external software (r, quarto, uv, gh, rig, tinytex, duckdb)
    Install(install::InstallArgs),
    /// Set up a fresh machine with the lab toolchain
    Setup(setup::SetupArgs),
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
    apply_global_args(&cli.global);
    let global = cli.global;
    match cli.command {
        Command::Format => format::run(),
        Command::Lint => lint::run(),
        Command::Init(args) => init::run(args),
        Command::Project(args) => project::run(args),
        Command::Use(args) => r#use::run(args, &global),
        Command::Install(args) => install::run(args, &global),
        Command::Setup(args) => setup::run(args, &global),
        Command::Git(args) => git::run(args),
        Command::Repo(args) => repo::run(args),
        Command::Audit(args) => audit::run(args, &global),
        Command::Tools(args) => tools::run(args, &global),
        Command::Config(args) => config::run(args, &global),
        Command::Completions(args) => completions::run(args),
        Command::Version => version::run(),
        Command::Upgrade => upgrade::run(),
    }
}

/// Push the global flags into `ui`'s process-wide state before dispatch
///: `--quiet` gates informational stdout output, `--no-color`
/// forces color off. `--verbose` and `--config` are consumed by the
/// commands that need them.
fn apply_global_args(global: &GlobalArgs) {
    crate::ui::set_quiet(global.quiet);
    crate::ui::set_color_choice(color_choice_for(global.no_color));
}

/// Pure flag → color-choice mapping, factored out so it is unit-testable.
fn color_choice_for(no_color: bool) -> crate::ui::ColorChoice {
    if no_color {
        crate::ui::ColorChoice::Never
    } else {
        crate::ui::ColorChoice::Auto
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

/// Typed error for usage mistakes clap cannot catch (e.g. an unknown
/// `hpds use` component). `main` renders it with its hint and exits 2,
/// matching clap's own usage-error exit code.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct UsageError {
    message: String,
    hint: String,
}

impl UsageError {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        self.hint.clone()
    }
}

/// Convenience constructor for command-level usage errors.
pub(crate) fn usage_error(message: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError {
        message: message.into(),
        hint: hint.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ColorChoice;

    #[test]
    fn no_color_flag_maps_to_never() {
        assert_eq!(color_choice_for(true), ColorChoice::Never);
    }

    #[test]
    fn without_no_color_the_choice_stays_auto() {
        assert_eq!(color_choice_for(false), ColorChoice::Auto);
    }
}
