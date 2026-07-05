//! `hpds repo` — GitHub repository helpers.

use clap::{Args, Subcommand};

use crate::gitx;
use crate::ui;

use super::GlobalArgs;

#[derive(Debug, Args)]
pub struct RepoArgs {
    #[command(subcommand)]
    pub command: RepoCommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Create a GitHub repo for the current project (lab-manual gh flow)
    ///
    /// Creates the repository under the lab org (private by default) via
    /// `gh` and wires it up as the origin remote, following the lab-manual
    /// flow. `gh` must be authenticated. Pass --yes to accept the defaults
    /// without prompting.
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

pub fn run(args: RepoArgs, global: &GlobalArgs) -> anyhow::Result<()> {
    match args.command {
        RepoCommand::Create(args) => {
            gitx::repo::create(gitx::repo::CreateOptions {
                name: args.name,
                org: args.org,
                visibility: args.visibility,
                yes: args.yes,
            })?;
            offer_gha(args.yes, global)
        }
    }
}

/// How to follow up a created repo with the gha component.
#[derive(Debug, PartialEq, Eq)]
enum GhaOffer {
    /// Ask whether to run `hpds use gha` right now.
    Ask,
    /// Print a one-line next step instead (under `--yes`, or when the
    /// session cannot prompt).
    Suggest,
}

/// Pure decision, factored out of the ambient prompt probing so it is
/// unit-testable: `--yes` means "accept defaults, never prompt", and a
/// session that cannot prompt gets the suggestion instead of a failed
/// prompt after the repo was already created.
fn gha_offer(yes: bool, can_prompt: bool) -> GhaOffer {
    if !yes && can_prompt {
        GhaOffer::Ask
    } else {
        GhaOffer::Suggest
    }
}

/// The next-step line printed whenever the gha offer cannot (or should
/// not) prompt.
const GHA_NEXT_STEP: &str = "next: run `hpds use gha` to add GitHub Actions workflows to this repo";

/// Offer to apply the gha component to the freshly created repo, per the
/// design: `hpds repo create` offers `hpds use gha` afterward.
fn offer_gha(yes: bool, global: &GlobalArgs) -> anyhow::Result<()> {
    match gha_offer(yes, ui::can_prompt()) {
        GhaOffer::Suggest => {
            ui::println(GHA_NEXT_STEP);
            Ok(())
        }
        GhaOffer::Ask => {
            if ui::confirm("Add GitHub Actions workflows now (`hpds use gha`)?", true)? {
                super::r#use::run(
                    super::r#use::UseArgs {
                        component: Some("gha".to_string()),
                        kind: None,
                        force: false,
                        language: None,
                        workflows: None,
                    },
                    global,
                )
            } else {
                ui::println(GHA_NEXT_STEP);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_suggests_instead_of_prompting_even_on_a_tty() {
        assert_eq!(gha_offer(true, true), GhaOffer::Suggest);
        assert_eq!(gha_offer(true, false), GhaOffer::Suggest);
    }

    #[test]
    fn interactive_sessions_are_asked() {
        assert_eq!(gha_offer(false, true), GhaOffer::Ask);
    }

    #[test]
    fn sessions_that_cannot_prompt_get_the_suggestion() {
        assert_eq!(gha_offer(false, false), GhaOffer::Suggest);
    }
}
