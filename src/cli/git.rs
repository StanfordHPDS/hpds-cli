//! `hpds git` — git helpers: defaults + ignore vaccination.

use clap::{Args, Subcommand};

use crate::gitx;
use crate::ui;

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
    Vaccinate(VaccinateArgs),
}

#[derive(Debug, Args)]
pub struct VaccinateArgs {
    /// Append to this repo's .gitignore instead of the global git ignore
    #[arg(long)]
    pub project: bool,
}

pub fn run(args: GitArgs) -> anyhow::Result<()> {
    match args.command {
        // Stub.
        GitCommand::Setup => Err(super::not_yet_implemented("git setup")),
        GitCommand::Vaccinate(args) => vaccinate(args),
    }
}

fn vaccinate(args: VaccinateArgs) -> anyhow::Result<()> {
    let report = if args.project {
        gitx::vaccinate_project()?
    } else {
        gitx::vaccinate_global()?
    };

    if report.set_excludes_file {
        ui::println(&format!(
            "set global git core.excludesFile to {}",
            report.path.display()
        ));
    }
    let path = report.path.display();
    if report.added.is_empty() {
        ui::success(&format!(
            "{path} already has all {} hpds ignore patterns; nothing to add",
            report.already_present.len()
        ));
    } else if report.already_present.is_empty() {
        ui::success(&format!(
            "added {} ignore pattern(s) to {path}",
            report.added.len()
        ));
    } else {
        ui::success(&format!(
            "added {} ignore pattern(s) to {path} ({} already present)",
            report.added.len(),
            report.already_present.len()
        ));
    }
    Ok(())
}
