//! `hpds audit` — repo and org audits, plus the bot reporter.
//!
//! The audit core (`crate::audit`) returns data; this layer loads config,
//! runs the registered checks, prints the report through `ui/`, and turns
//! failing findings into the process exit code.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Args, Subcommand, ValueEnum};

use crate::audit::{self, AuditCtx, Summary, report_github};
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
    ///
    /// Enumerates the org's repos via `gh`, audits each (shallow clone, or a
    /// metadata-only pass with --no-clone), and emits a combined report: a
    /// terminal summary table plus a per-repo markdown file. A failure on one
    /// repo is reported, not fatal.
    All(super::audit_all::AllArgs),
    /// Post audit results to GitHub (sticky PR comment, dedup'd issues)
    ///
    /// Consumes audit JSON (from `hpds audit --format json`) and mirrors the
    /// findings to GitHub via `gh`: a single sticky comment on a pull
    /// request, or one deduplicated issue per new error finding on a
    /// schedule. Meant to run inside the audit-bot workflow; see
    /// docs/audit-bot.md.
    ReportGithub(ReportGithubArgs),
}

/// Flags for `hpds audit report-github`. Everything defaults from the
/// GitHub Actions environment; the flags exist so the bot can be run (and
/// tested) anywhere `gh` is authenticated.
#[derive(Debug, Args)]
pub struct ReportGithubArgs {
    /// Audit JSON from `hpds audit --format json` (stdin when omitted)
    #[arg(long, value_name = "FILE")]
    input: Option<PathBuf>,

    /// Repository to report to (default: $GITHUB_REPOSITORY)
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,

    /// Pull request to comment on (default: $GITHUB_REF / event payload)
    #[arg(long, value_name = "NUMBER")]
    pr: Option<u64>,

    /// What to post (default: from $GITHUB_EVENT_NAME)
    #[arg(long, value_enum, value_name = "MODE")]
    mode: Option<ReportMode>,
}

/// CLI face of [`report_github::Mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReportMode {
    /// Upsert the sticky findings comment on a pull request
    Pr,
    /// File issues for new error findings and close resolved ones
    Schedule,
}

impl From<ReportMode> for report_github::Mode {
    fn from(mode: ReportMode) -> Self {
        match mode {
            ReportMode::Pr => report_github::Mode::Pr,
            ReportMode::Schedule => report_github::Mode::Schedule,
        }
    }
}

pub fn run(args: AuditArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    match args.command {
        None => audit_current_repo(&args, global),
        Some(AuditCommand::All(all_args)) => super::audit_all::run(all_args),
        Some(AuditCommand::ReportGithub(args)) => report_to_github(&args),
    }
}

/// Run the audit bot: read the audit JSON, resolve the GitHub context
/// (flags first, Actions environment second), and mirror the findings to
/// a PR comment or the issue tracker.
fn report_to_github(args: &ReportGithubArgs) -> anyhow::Result<()> {
    let json = read_report_input(args.input.as_deref())?;
    let report = report_github::parse_report(&json)
        .context("could not parse the audit report")
        .hint(
            "report-github consumes `hpds audit --format json` output, \
             via --input <file> or stdin",
        )?;

    let repo = report_github::resolve_repo(
        args.repo.as_deref(),
        env_var("GITHUB_REPOSITORY").as_deref(),
    )
    .map_err(usage)?;
    let mode = report_github::resolve_mode(
        args.mode.map(Into::into),
        env_var("GITHUB_EVENT_NAME").as_deref(),
    )
    .map_err(usage)?;

    let bot = report_github::GhCliBot::new(repo.clone());
    let lines = match mode {
        report_github::Mode::Pr => {
            let payload =
                env_var("GITHUB_EVENT_PATH").and_then(|path| std::fs::read_to_string(path).ok());
            let pr = report_github::resolve_pr(
                args.pr,
                env_var("GITHUB_REF").as_deref(),
                payload.as_deref(),
            )
            .map_err(usage)?;
            report_github::run_pr(&bot, pr, &report)
        }
        report_github::Mode::Schedule => report_github::run_schedule(&bot, &repo, &report),
    }
    .with_context(|| format!("could not report the audit findings to {repo}"))
    .hint(
        "the bot needs an authenticated gh with write access to the repo \
         (GITHUB_TOKEN on Actions; `gh auth login` elsewhere); fix the access and rerun",
    )?;

    for line in &lines {
        ui::println(line);
    }
    Ok(())
}

/// The audit JSON: from `--input <file>`, or stdin when piped. An
/// interactive terminal on stdin means the caller forgot both.
fn read_report_input(input: Option<&Path>) -> anyhow::Result<String> {
    match input {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("could not read the audit report at `{}`", path.display()))
            .hint("generate it with `hpds audit --format json > audit.json` and pass --input audit.json"),
        None if std::io::stdin().is_terminal() => Err(super::usage_error(
            "no audit report on stdin",
            "pipe `hpds audit --format json` in, or pass --input <file>",
        )),
        None => std::io::read_to_string(std::io::stdin())
            .context("could not read the audit report from stdin")
            .hint("pipe `hpds audit --format json` in, or pass --input <file>"),
    }
}

/// A GitHub Actions environment value, with empty strings treated as
/// unset (Actions defines some variables as empty outside their event).
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Map a bot [`report_github::ContextError`] onto the CLI's usage-error
/// type so it renders with its hint and exits 2.
fn usage(err: report_github::ContextError) -> anyhow::Error {
    let hint = err.hint().to_string();
    super::usage_error(err.to_string(), hint)
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
    // any repo, fall back to the cwd, run only the checks that never ask
    // git, and report the missing repository once.
    let (root, is_repo) = match audit::repo_root(&cwd) {
        Some(root) => (root, true),
        None => (cwd, false),
    };
    let repo = repo_display_name(&root);

    // GitHub checks run only against a github.com origin with an
    // authenticated gh; whenever they do not run, the report carries an
    // Info notice saying why (no origin remote, or gh unavailable).
    // Without a repo there is no origin to probe.
    let (github, notice) = if is_repo {
        match audit::github::probe(&root) {
            audit::github::GithubStatus::Ready(ctx) => (Some(ctx), None),
            audit::github::GithubStatus::NoRemote => {
                (None, Some(audit::github::no_remote_notice()))
            }
            audit::github::GithubStatus::Skipped(finding) => (None, Some(finding)),
        }
    } else {
        (None, None)
    };
    let ctx = AuditCtx {
        repo: root,
        config: loaded.config,
        github,
    };
    let mut checks = audit::registry();
    if !is_repo {
        // One "not a git repository" finding stands in for the git-backed
        // checks; running them would only repeat it per check.
        checks.retain(|check| !check.needs_repo());
    }
    if ctx.github.is_some() {
        checks.extend(audit::github::registry());
    }
    // The summary line counts checks actually run; the appended not-a-repo
    // and gh-skip notices are findings about the run, not checks.
    let checks_run = checks.len();
    let mut findings = Vec::new();
    if !is_repo {
        findings.push(audit::not_a_repo_finding());
    }
    findings.extend(audit::run_checks(&checks, &ctx));
    findings.extend(notice);

    match args.format {
        OutputFormat::Text => ui::println(&audit::render_text(
            &repo,
            &findings,
            checks_run,
            ui::stdout_colors(),
            global.verbose,
        )),
        OutputFormat::Json => ui::data(&audit::render_json(&repo, &findings)?),
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
