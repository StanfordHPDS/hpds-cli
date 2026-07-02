//! `hpds audit` — repo and org audits, plus the bot reporter.
//!
//! The audit core (`crate::audit`) returns data; this layer loads config,
//! runs the registered checks, prints the report through `ui/`, and turns
//! failing findings into the process exit code.

use std::path::Path;

use anyhow::Context;
use clap::{Args, Subcommand, ValueEnum};

use crate::audit::{self, AuditCtx, Summary};
use crate::config::{self, Layer};
use crate::ui::{self, HintExt};

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// With no subcommand, audit the current repo.
    #[command(subcommand)]
    pub command: Option<AuditCommand>,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,

    /// Exit 1 on warnings too (they stay warnings in the report)
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Audit every repo in the GitHub org
    All(super::audit_all::AllArgs),
    /// Post audit results to GitHub (sticky PR comment, dedup'd issues)
    ReportGithub,
}

pub fn run(args: AuditArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    match args.command {
        None => audit_current_repo(&args, global),
        Some(AuditCommand::All(all_args)) => super::audit_all::run(all_args),
        // Stub until the bot reporter is implemented.
        Some(AuditCommand::ReportGithub) => Err(super::not_yet_implemented("audit report-github")),
    }
}

/// Run every registered check against the current repo, print the report,
/// and fail (exit 1 via the returned error) when the findings warrant it.
fn audit_current_repo(args: &AuditArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let loaded = config::load(&cwd, global.config.as_deref(), Layer::default())?;
    for warning in &loaded.warnings {
        ui::warn(warning);
    }

    // Audit the whole repo even when started from a subdirectory; outside
    // any repo, fall back to the cwd and let the checks report that state.
    let root = audit::repo_root(&cwd).unwrap_or(cwd);
    let repo = repo_display_name(&root);

    // GitHub checks run only against a github.com origin with an
    // authenticated gh; when they apply but cannot run, the report carries
    // an Info notice saying so.
    let (github, notice) = match audit::github::probe(&root) {
        audit::github::GithubStatus::Ready(ctx) => (Some(ctx), None),
        audit::github::GithubStatus::NoRemote => (None, None),
        audit::github::GithubStatus::Skipped(finding) => (None, Some(finding)),
    };
    let ctx = AuditCtx {
        repo: root,
        config: loaded.config,
        github,
    };
    let mut checks = audit::registry();
    if ctx.github.is_some() {
        checks.extend(audit::github::registry());
    }
    // The summary line counts checks actually run; the appended gh-skip
    // notice is a finding about the run, not a check.
    let checks_run = checks.len();
    let mut findings = audit::run_checks(&checks, &ctx);
    findings.extend(notice);

    match args.format {
        OutputFormat::Text => ui::println(&audit::render_text(
            &repo,
            &findings,
            checks_run,
            ui::stdout_colors(),
        )),
        OutputFormat::Json => ui::println(&audit::render_json(&repo, &findings)?),
    }

    if audit::exit_code(&findings, args.strict) == 0 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(failure_message(
            &audit::summarize(&findings),
            args.strict
        )))
        .hint(
            "each finding in the report has a fix suggestion; address them and rerun `hpds audit`",
        )
    }
}

/// How the repo is named in reports: the directory's basename, falling back
/// to the full path when there is none (e.g. a filesystem root).
fn repo_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// One line saying why the audit failed, matching [`audit::exit_code`]:
/// real errors always count; warnings count only under `--strict`.
fn failure_message(summary: &Summary, strict: bool) -> String {
    let mut parts = Vec::new();
    if summary.errors > 0 {
        parts.push(count(summary.errors, "error"));
    }
    if strict && summary.warnings > 0 {
        parts.push(format!(
            "{} (fail the audit because of --strict)",
            count(summary.warnings, "warning")
        ));
    }
    format!("audit found {}", parts.join(" and "))
}

/// `1 error` / `2 errors` — for [`failure_message`].
fn count(n: usize, noun: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{s}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn repo_display_name_is_the_directory_basename() {
        let path = PathBuf::from("/home/x/projects").join("demo-repo");
        assert_eq!(repo_display_name(&path), "demo-repo");
    }

    #[test]
    fn failure_message_counts_errors() {
        let summary = Summary {
            errors: 2,
            warnings: 1,
            infos: 0,
        };
        assert_eq!(failure_message(&summary, false), "audit found 2 errors");
    }

    #[test]
    fn failure_message_singular_error() {
        let summary = Summary {
            errors: 1,
            warnings: 0,
            infos: 0,
        };
        assert_eq!(failure_message(&summary, false), "audit found 1 error");
    }

    #[test]
    fn failure_message_under_strict_names_the_promoted_warnings() {
        let summary = Summary {
            errors: 0,
            warnings: 3,
            infos: 1,
        };
        assert_eq!(
            failure_message(&summary, true),
            "audit found 3 warnings (fail the audit because of --strict)"
        );
    }

    #[test]
    fn failure_message_with_both_errors_and_strict_warnings() {
        let summary = Summary {
            errors: 1,
            warnings: 2,
            infos: 0,
        };
        assert_eq!(
            failure_message(&summary, true),
            "audit found 1 error and 2 warnings (fail the audit because of --strict)"
        );
    }
}
