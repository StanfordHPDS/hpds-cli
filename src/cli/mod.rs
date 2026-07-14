//! Clap derive command tree: one file per top-level command.
//!
//! Commands not yet implemented are stubbed with a
//! typed [`NotYetImplemented`] error, rendered by `main` with exit code 2.

mod audit;
mod audit_all;
mod completions;
mod config;
mod git;
mod init;
mod install;
mod project;
mod repo;
mod setup;
mod upgrade;
mod r#use;
mod version;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Unified tooling for the Stanford HPDS lab: project templates, machine
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
    /// Set up a new or existing project interactively
    ///
    /// Walks through the project name, language(s), and components (pipeline,
    /// readme, container, slurm, gha), then writes hpds.toml with the
    /// [project] metadata and optionally initializes git and creates the
    /// GitHub repo. Pass --yes to accept every default without prompting.
    /// Formatting and linting the scaffolded project is the separate togi
    /// tool's job (`hpds install togi`). `hpds project init` is an alias
    /// for this command.
    Init(init::InitArgs),
    /// Project commands (`hpds project init` is an alias for `hpds init`)
    ///
    /// Groups project-scoped subcommands. Currently just `init`; run
    /// `hpds project init` for the setup wizard.
    Project(project::ProjectArgs),
    /// Apply a template component to the current project
    ///
    /// Drops a single lab template into the current project: hpds.toml,
    /// pipeline, readme, container, slurm, or gha. Omit the component to list
    /// what is available. Existing files are left untouched unless --force
    /// is given.
    Use(r#use::UseArgs),
    /// Install external software (r, quarto, uv, gh, rig, tinytex, duckdb, togi)
    ///
    /// Installs developer tooling onto the machine. Prints exactly what
    /// will run and asks for confirmation before changing anything; pass
    /// --yes to skip the prompt.
    Install(install::InstallArgs),
    /// Set up a fresh machine with the lab toolchain
    ///
    /// Runs the machine-setup bundle. The `dev` profile provisions this
    /// machine's toolchain; the `server` profile does full lab-server
    /// provisioning (Linux only). Use --plan to print the numbered steps
    /// without running or prompting.
    Setup(setup::SetupArgs),
    /// Git helpers: sensible defaults and global ignore vaccination
    ///
    /// `setup` configures sensible git defaults and points you at gh auth;
    /// `vaccinate` adds R/Python/editor junk patterns to your global git
    /// ignore so they can never be committed.
    Git(git::GitArgs),
    /// GitHub repository helpers
    ///
    /// Subcommands that shell out to `gh` (which must be authenticated).
    /// `create` makes a GitHub repo for the current project following the
    /// lab-manual flow.
    Repo(repo::RepoArgs),
    /// Audit the current repo (or the whole org) against lab standards
    ///
    /// With no subcommand, audits the current repo and exits 1 on any
    /// error-severity finding (--strict promotes warnings to failures).
    /// `all` sweeps every repo in the org; `report-github` posts results back
    /// to GitHub as a sticky PR comment or deduplicated issues. --format json
    /// emits the stable schema the audit bot consumes.
    Audit(audit::AuditArgs),
    /// Print the resolved configuration and where each value came from
    ///
    /// Prints the fully layered configuration (built-in defaults, then user
    /// config, then the project hpds.toml) and the path of each contributing
    /// file, the answer to "why did it do that?". --format json for a
    /// machine-readable dump. See docs/hpds.toml.md for every key.
    Config(config::ConfigArgs),
    /// Generate shell completions
    ///
    /// Prints a completion script for the given shell to stdout; redirect it
    /// into the location your shell loads completions from.
    Completions(completions::CompletionsArgs),
    /// Print the hpds version
    ///
    /// The same value as `hpds --version`, provided as a subcommand for
    /// scripts.
    Version,
    /// Upgrade hpds to the latest release
    ///
    /// Downloads the latest release for your platform and replaces the
    /// running binary in place. Does nothing if you already have the latest
    /// version.
    Upgrade,
    /// Hidden intercept for the former `hpds format` command, which moved
    /// to the standalone togi tool: parses (swallowing any arguments the
    /// old command took) and errors with a redirect instead of clap's
    /// generic "unrecognized subcommand".
    #[command(hide = true, disable_help_flag = true)]
    Format(MovedToTogiArgs),
    /// Hidden intercept for the former `hpds lint` command; see `Format`.
    #[command(hide = true, disable_help_flag = true)]
    Lint(MovedToTogiArgs),
}

/// Swallows whatever arguments the former format/lint commands were
/// invoked with, so the redirect error is reached instead of a clap
/// "unexpected argument" complaint.
#[derive(Debug, Args)]
pub struct MovedToTogiArgs {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        hide = true,
        value_name = "ARGS"
    )]
    pub args: Vec<String>,
}

/// The usage error for a former format/lint invocation: name the removed
/// command and point at its togi replacement.
fn moved_to_togi(command: &str) -> anyhow::Error {
    usage_error(
        format!("`hpds {command}` has moved to togi, the lab's standalone formatter and linter"),
        format!("install it with `hpds install togi`, then run `togi {command}`"),
    )
}

/// Dispatch a parsed CLI invocation to its command module.
pub fn run(cli: Cli) -> anyhow::Result<()> {
    apply_global_args(&cli.global);
    let global = cli.global;
    match cli.command {
        Command::Init(args) => init::run(args),
        Command::Project(args) => project::run(args),
        Command::Use(args) => r#use::run(args, &global),
        Command::Install(args) => install::run(args, &global),
        Command::Setup(args) => setup::run(args, &global),
        Command::Git(args) => git::run(args),
        Command::Repo(args) => repo::run(args, &global),
        Command::Audit(args) => audit::run(args, &global),
        Command::Config(args) => config::run(args, &global),
        Command::Completions(args) => completions::run(args),
        Command::Version => version::run(),
        Command::Upgrade => upgrade::run(&global),
        Command::Format(_) => Err(moved_to_togi("format")),
        Command::Lint(_) => Err(moved_to_togi("lint")),
    }
}

/// Push the global flags into `ui`'s process-wide state before dispatch:
/// `--quiet` gates informational stdout output and `--no-color` forces
/// color off. `--verbose` and `--config` are consumed by the commands
/// that need them.
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
