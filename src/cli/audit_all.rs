//! `hpds audit all` — command layer for the org sweep: resolve the repo
//! list (gh enumeration or `--repos-from`), drive the per-repo audits
//! behind a progress bar, print the table or JSON, write the markdown
//! report, and turn the results into the exit code.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use clap::{Args, ValueEnum};

use crate::audit::all::{self, RepoSpec};
use crate::config::{self, Layer};
use crate::gitx::{self, GhAuth, repo::DEFAULT_ORG};
use crate::ui::{self, HintExt};

#[derive(Debug, Args)]
pub struct AllArgs {
    /// GitHub organization to sweep
    #[arg(long, value_name = "ORG", default_value = DEFAULT_ORG)]
    org: String,

    /// Audit at most this many repos when enumerating the org
    #[arg(long, value_name = "N", default_value_t = 100,
          value_parser = clap::value_parser!(u32).range(1..))]
    limit: u32,

    /// Skip cloning: audit GitHub metadata only (runs just the watchers,
    /// contributors, and stale-remote-branches checks; the other checks
    /// need a working tree)
    #[arg(long)]
    no_clone: bool,

    /// Audit the repos listed in FILE (one `owner/name` slug or local
    /// path per line, `#` comments allowed) instead of enumerating the org
    #[arg(long, value_name = "FILE")]
    repos_from: Option<PathBuf>,

    /// Write the markdown report to PATH
    #[arg(long, value_name = "PATH", default_value = "hpds-audit-report.md")]
    output: PathBuf,

    /// Output format for the terminal summary
    #[arg(long, value_enum, default_value_t = SweepFormat::Text)]
    format: SweepFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SweepFormat {
    Text,
    Json,
}

pub fn run(args: AllArgs) -> anyhow::Result<()> {
    let (specs, source) = resolve_specs(&args)?;
    if specs.is_empty() {
        return Err(anyhow::anyhow!("no repositories to audit ({source})")).hint(
            "list at least one `owner/name` slug or local path, or check the org name \
             with `gh repo list <org>`",
        );
    }

    let scratch = tempfile::tempdir()
        .context("could not create a temporary directory for the repo clones")?;

    // Metadata-only audits have no working tree to read hpds.toml from;
    // user-level config (e.g. required watchers) still applies. Loading
    // from the scratch dir guarantees no project file is picked up.
    let no_clone_config = if args.no_clone {
        ensure_gh_ready("audit GitHub metadata")?;
        let loaded = config::load(scratch.path(), None, Layer::default())?;
        for warning in &loaded.warnings {
            ui::warn(warning);
        }
        Some(loaded.config)
    } else {
        None
    };

    let bar = ui::progress_bar(specs.len() as u64, "auditing repos");
    let mut reports = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        bar.set_message(format!("auditing {}", spec.display_name()));
        let report = match (&no_clone_config, spec) {
            (Some(config), RepoSpec::Slug(slug)) => {
                all::audit_metadata(slug, config, scratch.path())
            }
            (Some(_), RepoSpec::Local(path)) => all::local_path_needs_clone(path),
            (None, _) => all::audit_spec(spec, &scratch.path().join(format!("repo-{index}"))),
        };
        bar.inc(1);
        reports.push(report);
    }
    bar.finish_and_clear();

    write_markdown(&args, &source, &reports)?;
    match args.format {
        SweepFormat::Text => {
            ui::println(&all::render_table(&reports, ui::stdout_colors()));
            ui::println(&format!("report written to {}", args.output.display()));
        }
        SweepFormat::Json => {
            // --repos-from bypasses the org, so the JSON claims none.
            let org = args.repos_from.is_none().then_some(args.org.as_str());
            ui::println(&all::render_sweep_json(org, &reports)?);
        }
    }

    let (with_errors, failed) = all::failure_counts(&reports);
    if with_errors == 0 && failed == 0 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(sweep_failure_message(with_errors, failed))).hint(format!(
            "see {} for per-repo findings and fixes",
            args.output.display()
        ))
    }
}

/// The repo list and a human-readable label for where it came from.
fn resolve_specs(args: &AllArgs) -> anyhow::Result<(Vec<RepoSpec>, String)> {
    match &args.repos_from {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("could not read repos file `{}`", path.display()))
                .hint(
                    "pass --repos-from an existing file listing one repo slug or \
                     local path per line",
                )?;
            Ok((
                all::parse_repos_from(&text),
                format!("file {}", path.display()),
            ))
        }
        None => {
            ensure_gh_ready("enumerate the org's repos")?;
            let slugs = list_org_repos(&args.org, args.limit)?;
            let specs = slugs.into_iter().map(RepoSpec::Slug).collect();
            Ok((specs, format!("org {}", args.org)))
        }
    }
}

/// `gh repo list <org> --json nameWithOwner`, parsed into slugs.
fn list_org_repos(org: &str, limit: u32) -> anyhow::Result<Vec<String>> {
    let out = Command::new("gh")
        .args(["repo", "list", org])
        .args(["--limit", &limit.to_string()])
        .args(["--json", "nameWithOwner"])
        .output()
        .context("could not run `gh repo list`")
        .hint("install the GitHub CLI from https://cli.github.com/ and run `gh auth login`")?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "`gh repo list {org}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
        .hint("check the org name and `gh auth status`, or use --repos-from <file>");
    }
    all::parse_repo_list(&String::from_utf8_lossy(&out.stdout))
}

/// Fail fast, before any per-repo work, when `gh` cannot serve the sweep.
fn ensure_gh_ready(purpose: &str) -> anyhow::Result<()> {
    match gitx::gh_auth()? {
        GhAuth::Authenticated => Ok(()),
        GhAuth::Unauthenticated(_) => Err(anyhow::anyhow!(
            "not logged in to GitHub (needed to {purpose})"
        ))
        .hint("run `gh auth login`, then re-run `hpds audit all`"),
        GhAuth::NotInstalled => Err(anyhow::anyhow!(
            "the GitHub CLI (`gh`) is not installed or not on PATH (needed to {purpose})"
        ))
        .hint(
            "install it from https://cli.github.com/ and run `gh auth login`, \
             then re-run `hpds audit all`",
        ),
    }
}

/// Write the markdown report to `--output`, creating parent directories.
fn write_markdown(args: &AllArgs, source: &str, reports: &[all::RepoReport]) -> anyhow::Result<()> {
    let markdown = all::render_markdown(source, reports);
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create `{}`", parent.display()))
            .hint("choose a writable location with --output <path>")?;
    }
    std::fs::write(&args.output, markdown)
        .with_context(|| format!("could not write the report to `{}`", args.output.display()))
        .hint("choose a writable location with --output <path>")
}

/// One line saying why the sweep failed: repos with error findings count,
/// and so do repos that could not be audited at all (an unauditable repo
/// is not a healthy one).
fn sweep_failure_message(with_errors: usize, failed: usize) -> String {
    let mut parts = Vec::new();
    if with_errors > 0 {
        parts.push(format!("{} with errors", count(with_errors, "repo")));
    }
    if failed > 0 {
        parts.push(format!("{} could not be audited", count(failed, "repo")));
    }
    format!("audit sweep found {}", parts.join(" and "))
}

/// `1 repo` / `2 repos` — for [`sweep_failure_message`].
fn count(n: usize, noun: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_message_counts_error_repos() {
        assert_eq!(
            sweep_failure_message(2, 0),
            "audit sweep found 2 repos with errors"
        );
    }

    #[test]
    fn failure_message_counts_unauditable_repos() {
        assert_eq!(
            sweep_failure_message(0, 1),
            "audit sweep found 1 repo could not be audited"
        );
    }

    #[test]
    fn failure_message_combines_both_causes() {
        assert_eq!(
            sweep_failure_message(1, 3),
            "audit sweep found 1 repo with errors and 3 repos could not be audited"
        );
    }
}
