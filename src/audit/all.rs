//! The org-sweep core behind `hpds audit all`: resolve a list of repos
//! (GitHub slugs or local paths), audit each one in isolation, and render
//! the combined results as an aligned terminal table, a markdown report,
//! and a stable JSON document.
//!
//! Like the rest of the audit core, this module returns data and rendered
//! strings only; the command layer (`cli::audit_all`) prints, shows
//! progress, and writes the report file.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use serde::Serialize;

use super::{AuditCtx, Check, Finding, Severity, github};
use crate::config::{self, Config, Layer};
use crate::ui::{ERROR_STYLE, WARN_STYLE, paint};

/// One entry in the sweep's repo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSpec {
    /// `owner/name` on github.com, cloned through `gh repo clone` (which
    /// reuses gh's authentication for private repos).
    Slug(String),
    /// A local path to a git repository, cloned with plain `git clone`.
    Local(PathBuf),
}

impl RepoSpec {
    /// How the repo is named in every report: the slug itself, or a local
    /// path's basename.
    pub fn display_name(&self) -> String {
        match self {
            RepoSpec::Slug(slug) => slug.clone(),
            RepoSpec::Local(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        }
    }
}

/// Parse a `--repos-from` file: one repo per line, blank lines and `#`
/// comments skipped. A line naming an existing path is a local repo; any
/// other slug-shaped line (`owner/name`) is a GitHub slug; everything else
/// is treated as a path so the sweep reports a clear per-repo clone
/// failure instead of aborting.
pub fn parse_repos_from(text: &str) -> Vec<RepoSpec> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(parse_repo_line)
        .collect()
}

fn parse_repo_line(line: &str) -> RepoSpec {
    let path = Path::new(line);
    if !path.exists() && is_slug(line) {
        RepoSpec::Slug(line.to_string())
    } else {
        RepoSpec::Local(path.to_path_buf())
    }
}

/// `owner/name` with GitHub's identifier charset on both sides.
fn is_slug(line: &str) -> bool {
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    line.split_once('/')
        .is_some_and(|(owner, name)| valid(owner) && valid(name))
}

/// Repo slugs out of `gh repo list <org> --json nameWithOwner` output
/// (a JSON array of `{"nameWithOwner": "owner/name"}` objects).
pub fn parse_repo_list(json: &str) -> anyhow::Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    let entries: Vec<Entry> =
        serde_json::from_str(json).context("could not parse `gh repo list` output as JSON")?;
    Ok(entries
        .into_iter()
        .map(|entry| entry.name_with_owner)
        .collect())
}

/// The audit result for one repo in the sweep.
#[derive(Debug)]
pub struct RepoReport {
    /// Report name: the slug, or a local path's basename.
    pub repo: String,
    pub outcome: RepoOutcome,
}

/// What happened to one repo. A failure never aborts the sweep; it is
/// reported alongside the audited repos.
#[derive(Debug)]
pub enum RepoOutcome {
    /// The audit ran; the findings may be empty.
    Audited { findings: Vec<Finding> },
    /// The repo could not be cloned or set up for auditing.
    Failed { error: String },
}

/// Clone `spec` into `dest` (which must not exist yet) and run the
/// standard audit on the clone. Every failure (clone, config, anything)
/// lands in the returned report; this function never aborts the sweep.
pub fn audit_spec(spec: &RepoSpec, dest: &Path) -> RepoReport {
    let repo = spec.display_name();
    let outcome = match clone_and_audit(spec, dest) {
        Ok(findings) => RepoOutcome::Audited { findings },
        Err(err) => RepoOutcome::Failed {
            error: one_line(&format!("{err:#}")),
        },
    };
    RepoReport { repo, outcome }
}

/// The standard audit against a fresh clone: layered config from the
/// clone, the local checks, and, when the clone's `origin` points at
/// github.com and `gh` is authenticated, the GitHub checks too.
/// Config *warnings* (unknown keys) are dropped here: in a sweep across
/// many repos they are noise for someone who cannot fix them in place.
fn clone_and_audit(spec: &RepoSpec, dest: &Path) -> anyhow::Result<Vec<Finding>> {
    clone(spec, dest)?;
    let loaded = config::load(dest, None, Layer::default())?;
    let (github_ctx, notice) = match github::probe(dest) {
        github::GithubStatus::Ready(ctx) => (Some(ctx), None),
        github::GithubStatus::NoRemote => (None, None),
        github::GithubStatus::Skipped(finding) => (None, Some(finding)),
    };
    let mut checks = super::registry();
    if github_ctx.is_some() {
        checks.extend(github::registry());
    }
    let ctx = AuditCtx {
        repo: dest.to_path_buf(),
        config: loaded.config,
        github: github_ctx,
    };
    let mut findings = super::run_checks(&checks, &ctx);
    findings.extend(notice);
    Ok(findings)
}

/// Clone the repo into `dest`: slugs through `gh repo clone` (shallow, and
/// authenticated the same way as every other GitHub interaction), local
/// paths through plain `git clone` (where `--depth` would not apply).
fn clone(spec: &RepoSpec, dest: &Path) -> anyhow::Result<()> {
    let (tool, mut cmd) = match spec {
        RepoSpec::Slug(slug) => {
            let mut cmd = Command::new(crate::gitx::gh_program());
            cmd.args(["repo", "clone", slug])
                .arg(dest)
                .args(["--", "--depth", "1", "--quiet"]);
            ("gh", cmd)
        }
        RepoSpec::Local(path) => {
            let mut cmd = Command::new("git");
            cmd.args(["clone", "--quiet"]).arg(path).arg(dest);
            ("git", cmd)
        }
    };
    let out = cmd.output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow::anyhow!("could not clone: {tool} is not installed or not on PATH")
        }
        _ => anyhow::anyhow!("could not clone: failed to run {tool}: {err}"),
    })?;
    if out.status.success() {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "could not clone: {}",
        one_line(&String::from_utf8_lossy(&out.stderr))
    ))
}

/// Collapse a multi-line error into one report-friendly line.
fn one_line(text: &str) -> String {
    text.trim()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Check ids that run in `--no-clone` (metadata-only) mode: the
/// GitHub-side checks whose signal needs no working tree. The others are
/// excluded deliberately: `default-branch-staleness` compares against a
/// local checkout, and `releases` / `lifecycle-consistency` read the
/// project status from `hpds.toml`, which only exists in a working tree.
pub const NO_CLONE_CHECK_IDS: &[&str] = &["watchers", "contributors", "stale-remote-branches"];

/// The [`NO_CLONE_CHECK_IDS`] subset of the GitHub check registry.
fn metadata_registry() -> Vec<Box<dyn Check>> {
    github::registry()
        .into_iter()
        .filter(|check| NO_CLONE_CHECK_IDS.contains(&check.id()))
        .collect()
}

/// `--no-clone`: audit one slug's GitHub metadata only, without a working
/// tree, running just [`NO_CLONE_CHECK_IDS`]. `config` supplies user-level
/// settings (e.g. required watchers); project-level settings are unknown
/// without a checkout.
pub fn audit_metadata(slug: &str, config: &Config, scratch: &Path) -> RepoReport {
    let repo = slug.to_string();
    let Some((owner, name)) = slug.split_once('/') else {
        return RepoReport {
            repo,
            outcome: RepoOutcome::Failed {
                error: format!("`{slug}` is not an owner/name GitHub slug"),
            },
        };
    };
    let ctx = AuditCtx {
        // No working tree exists; the metadata checks never read it.
        repo: scratch.to_path_buf(),
        config: config.clone(),
        github: Some(github::ctx_without_checkout(github::RepoSlug {
            owner: owner.to_string(),
            repo: name.to_string(),
        })),
    };
    let findings = super::run_checks(&metadata_registry(), &ctx);
    RepoReport {
        repo,
        outcome: RepoOutcome::Audited { findings },
    }
}

/// The per-repo failure reported when `--no-clone` meets a local path:
/// the metadata pass audits GitHub repos only.
pub fn local_path_needs_clone(path: &Path) -> RepoReport {
    RepoReport {
        repo: RepoSpec::Local(path.to_path_buf()).display_name(),
        outcome: RepoOutcome::Failed {
            error: "--no-clone audits GitHub metadata only; drop --no-clone to audit \
                    local paths"
                .to_string(),
        },
    }
}

/// Aligned terminal table: one row per repo with error/warning counts, a
/// `failed:` note for repos that could not be audited, and a closing
/// summary line.
pub fn render_table(reports: &[RepoReport], use_color: bool) -> String {
    let width = reports
        .iter()
        .map(|report| report.repo.chars().count())
        .chain(std::iter::once("repo".len()))
        .max()
        .unwrap_or(4);
    let mut out = format!("{:<width$}  {:>6}  {:>8}\n", "repo", "errors", "warnings");
    for report in reports {
        match &report.outcome {
            RepoOutcome::Audited { findings } => {
                let summary = super::summarize(findings);
                // Pad first, paint after: ANSI codes would break `{:>6}`.
                let errors = paint(
                    ERROR_STYLE,
                    &format!("{:>6}", summary.errors),
                    use_color && summary.errors > 0,
                );
                let warnings = paint(
                    WARN_STYLE,
                    &format!("{:>8}", summary.warnings),
                    use_color && summary.warnings > 0,
                );
                out.push_str(&format!("{:<width$}  {errors}  {warnings}\n", report.repo));
            }
            RepoOutcome::Failed { error } => {
                out.push_str(&format!(
                    "{:<width$}  {} {error}\n",
                    report.repo,
                    paint(ERROR_STYLE, "failed:", use_color),
                ));
            }
        }
    }
    out.push('\n');
    out.push_str(&summary_line(reports));
    out.push('\n');
    out
}

/// The table's closing line: repo count plus whatever went wrong.
fn summary_line(reports: &[RepoReport]) -> String {
    let totals = totals(reports);
    let mut problems = Vec::new();
    if totals.repos_with_errors > 0 {
        problems.push(format!("{} with errors", totals.repos_with_errors));
    }
    if totals.failed > 0 {
        problems.push(format!("{} failed to audit", totals.failed));
    }
    let repos = count(reports.len(), "repo");
    if problems.is_empty() {
        format!("{repos} audited: no errors")
    } else {
        format!("{repos} audited: {}", problems.join(", "))
    }
}

/// `1 repo` / `2 repos`, for summary lines.
fn count(n: usize, noun: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{s}")
}

/// Sweep-wide tallies shared by the renderers and the exit decision.
struct Totals {
    audited: usize,
    failed: usize,
    repos_with_errors: usize,
    errors: usize,
    warnings: usize,
    infos: usize,
}

fn totals(reports: &[RepoReport]) -> Totals {
    let mut totals = Totals {
        audited: 0,
        failed: 0,
        repos_with_errors: 0,
        errors: 0,
        warnings: 0,
        infos: 0,
    };
    for report in reports {
        match &report.outcome {
            RepoOutcome::Audited { findings } => {
                totals.audited += 1;
                let summary = super::summarize(findings);
                totals.errors += summary.errors;
                totals.warnings += summary.warnings;
                totals.infos += summary.infos;
                if summary.errors > 0 {
                    totals.repos_with_errors += 1;
                }
            }
            RepoOutcome::Failed { .. } => totals.failed += 1,
        }
    }
    totals
}

/// The markdown report: a header with totals, then one section per repo
/// listing each finding with its remediation (or the failure that kept
/// the repo from being audited). `source` says where the repo list came
/// from, e.g. `org StanfordHPDS` or `file repos.txt`.
pub fn render_markdown(source: &str, reports: &[RepoReport]) -> String {
    let totals = totals(reports);
    let mut out = String::from("# hpds audit report\n\n");
    out.push_str(&format!("Source: {source}\n"));
    out.push_str(&format!(
        "Repos: {} audited, {} failed\n",
        totals.audited, totals.failed
    ));
    out.push_str(&format!(
        "Findings: {}, {}, {} info\n",
        count(totals.errors, "error"),
        count(totals.warnings, "warning"),
        totals.infos,
    ));
    for report in reports {
        out.push_str(&format!("\n## {}\n\n", report.repo));
        match &report.outcome {
            RepoOutcome::Audited { findings } if findings.is_empty() => {
                out.push_str("No findings.\n");
            }
            RepoOutcome::Audited { findings } => {
                for finding in findings {
                    out.push_str(&format!(
                        "- **{}** `{}`: {}\n  - fix: {}\n",
                        severity_word(finding.severity),
                        finding.check_id,
                        finding.message,
                        finding.remediation,
                    ));
                }
            }
            RepoOutcome::Failed { error } => {
                out.push_str(&format!("Could not audit: {error}\n\n"));
                out.push_str(
                    "- fix: check that the repo slug or path is correct and that you \
                     can clone it, then re-run `hpds audit all`\n",
                );
            }
        }
    }
    out
}

/// The severity label used in markdown, matching the JSON serialization.
fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warn => "warn",
        Severity::Info => "info",
    }
}

/// Render the sweep as pretty-printed JSON.
///
/// STABLE schema (like the single-repo report, consumed by tooling):
/// `org` (string or null), `repos` (array of per-repo objects; audited
/// repos use the single-audit shape `{repo, findings, summary}`, failed
/// repos `{repo, error}`), and `summary` with `repos`, `audited`,
/// `failed`, `errors`, `warnings`, `infos`.
pub fn render_sweep_json(org: Option<&str>, reports: &[RepoReport]) -> anyhow::Result<String> {
    #[derive(Serialize)]
    #[serde(untagged)]
    enum RepoJson<'a> {
        Audited {
            repo: &'a str,
            findings: &'a [Finding],
            summary: super::Summary,
        },
        Failed {
            repo: &'a str,
            error: &'a str,
        },
    }

    #[derive(Serialize)]
    struct SweepSummary {
        repos: usize,
        audited: usize,
        failed: usize,
        errors: usize,
        warnings: usize,
        infos: usize,
    }

    #[derive(Serialize)]
    struct SweepJson<'a> {
        org: Option<&'a str>,
        repos: Vec<RepoJson<'a>>,
        summary: SweepSummary,
    }

    let repos = reports
        .iter()
        .map(|report| match &report.outcome {
            RepoOutcome::Audited { findings } => RepoJson::Audited {
                repo: &report.repo,
                findings,
                summary: super::summarize(findings),
            },
            RepoOutcome::Failed { error } => RepoJson::Failed {
                repo: &report.repo,
                error,
            },
        })
        .collect();
    let totals = totals(reports);
    let sweep = SweepJson {
        org,
        repos,
        summary: SweepSummary {
            repos: reports.len(),
            audited: totals.audited,
            failed: totals.failed,
            errors: totals.errors,
            warnings: totals.warnings,
            infos: totals.infos,
        },
    };
    serde_json::to_string_pretty(&sweep).context("could not serialize the sweep report to JSON")
}

/// How many repos had Error findings, and how many failed to audit. The
/// sweep fails (exit 1) when either count is nonzero.
pub fn failure_counts(reports: &[RepoReport]) -> (usize, usize) {
    let totals = totals(reports);
    (totals.repos_with_errors, totals.failed)
}

#[cfg(test)]
mod tests {
    use super::super::checks::testutil;
    use super::*;

    const ESC: &str = "\x1b[";

    fn finding(check_id: &str, severity: Severity) -> Finding {
        Finding {
            check_id: check_id.to_string(),
            severity,
            message: format!("{check_id} went wrong"),
            remediation: format!("fix {check_id}"),
        }
    }

    fn audited(repo: &str, findings: Vec<Finding>) -> RepoReport {
        RepoReport {
            repo: repo.to_string(),
            outcome: RepoOutcome::Audited { findings },
        }
    }

    fn failed(repo: &str, error: &str) -> RepoReport {
        RepoReport {
            repo: repo.to_string(),
            outcome: RepoOutcome::Failed {
                error: error.to_string(),
            },
        }
    }

    /// One audited repo with an error and a warning, one clean repo, one
    /// failure: the shapes every renderer must handle.
    fn mixed_reports() -> Vec<RepoReport> {
        vec![
            audited(
                "acme/messy",
                vec![
                    finding("junk-files", Severity::Error),
                    finding("gitignore-hygiene", Severity::Warn),
                ],
            ),
            audited("acme/clean", vec![]),
            failed("gone", "could not clone: no such path"),
        ]
    }

    // ---- repo list parsing -------------------------------------------

    #[test]
    fn parse_repos_from_skips_blank_lines_and_comments() {
        let specs = parse_repos_from("# lab repos\n\nacme/demo\n  \n# done\n");
        assert_eq!(specs, vec![RepoSpec::Slug("acme/demo".to_string())]);
    }

    #[test]
    fn slug_shaped_lines_parse_as_slugs() {
        let specs = parse_repos_from("StanfordHPDS/grant_sil.repro-ai\n");
        assert_eq!(
            specs,
            vec![RepoSpec::Slug(
                "StanfordHPDS/grant_sil.repro-ai".to_string()
            )]
        );
    }

    #[test]
    fn an_existing_directory_parses_as_a_local_path_even_if_slug_shaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let line = tmp.path().display().to_string();
        let specs = parse_repos_from(&line);
        assert_eq!(specs, vec![RepoSpec::Local(tmp.path().to_path_buf())]);
    }

    #[test]
    fn non_slug_nonexistent_lines_fall_back_to_local_paths() {
        // Reported as a per-repo clone failure later, never a parse abort.
        let specs = parse_repos_from("/no/such/deep/path\nnot a slug\n");
        assert_eq!(
            specs,
            vec![
                RepoSpec::Local(PathBuf::from("/no/such/deep/path")),
                RepoSpec::Local(PathBuf::from("not a slug")),
            ]
        );
    }

    #[test]
    fn display_name_is_the_slug_or_the_path_basename() {
        assert_eq!(
            RepoSpec::Slug("acme/demo".to_string()).display_name(),
            "acme/demo"
        );
        let path = PathBuf::from("/tmp/fixtures").join("messy-repo");
        assert_eq!(RepoSpec::Local(path).display_name(), "messy-repo");
    }

    // ---- gh repo list parsing ----------------------------------------

    #[test]
    fn parse_repo_list_reads_recorded_gh_output() {
        let json = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/tool-output/gh/repo-list.json"),
        )
        .expect("read recorded gh repo list output");
        let slugs = parse_repo_list(&json).expect("parse recorded output");
        assert_eq!(
            slugs,
            [
                "StanfordHPDS/ruff",
                "StanfordHPDS/uv",
                "StanfordHPDS/grant_sil_repro_ai",
            ]
        );
    }

    #[test]
    fn parse_repo_list_rejects_non_json_output_with_context() {
        let err = parse_repo_list("gh: something went sideways").expect_err("not JSON");
        assert!(
            format!("{err:#}").contains("gh repo list"),
            "error names the command: {err:#}"
        );
    }

    // ---- per-repo audit -----------------------------------------------

    #[test]
    fn audit_spec_on_a_compliant_local_repo_reports_no_errors() {
        let (_tmp, repo) = testutil::compliant_repo();
        let scratch = tempfile::tempdir().expect("scratch dir");
        let report = audit_spec(&RepoSpec::Local(repo), &scratch.path().join("clone"));
        match &report.outcome {
            RepoOutcome::Audited { findings } => {
                assert!(
                    findings.iter().all(|f| f.severity != Severity::Error),
                    "unexpected errors: {findings:?}"
                );
            }
            RepoOutcome::Failed { error } => panic!("audit failed: {error}"),
        }
    }

    #[test]
    fn audit_spec_reports_a_clone_failure_instead_of_aborting() {
        let scratch = tempfile::tempdir().expect("scratch dir");
        let spec = RepoSpec::Local(PathBuf::from("/no/such/repo-anywhere"));
        let report = audit_spec(&spec, &scratch.path().join("clone"));
        assert_eq!(report.repo, "repo-anywhere");
        match &report.outcome {
            RepoOutcome::Failed { error } => {
                assert!(error.contains("could not clone"), "error was: {error}");
            }
            RepoOutcome::Audited { .. } => panic!("clone of a missing path cannot succeed"),
        }
    }

    #[test]
    fn local_path_needs_clone_names_the_repo_and_says_what_to_do() {
        let report = local_path_needs_clone(Path::new("/tmp/fixtures/messy-repo"));
        assert_eq!(report.repo, "messy-repo");
        match &report.outcome {
            RepoOutcome::Failed { error } => {
                assert!(error.contains("--no-clone"), "error was: {error}");
            }
            RepoOutcome::Audited { .. } => panic!("must be a failure report"),
        }
    }

    // ---- no-clone registry --------------------------------------------

    #[test]
    fn metadata_registry_is_exactly_the_documented_subset() {
        let ids: Vec<String> = metadata_registry()
            .iter()
            .map(|c| c.id().to_string())
            .collect();
        assert_eq!(ids, NO_CLONE_CHECK_IDS);
    }

    #[test]
    fn no_clone_check_ids_all_exist_in_the_github_registry() {
        let github_ids: Vec<String> = github::registry()
            .iter()
            .map(|c| c.id().to_string())
            .collect();
        for id in NO_CLONE_CHECK_IDS {
            assert!(github_ids.iter().any(|g| g == id), "unknown check id {id}");
        }
    }

    // ---- table ----------------------------------------------------------

    #[test]
    fn table_has_a_header_and_aligned_count_columns() {
        let out = render_table(&mixed_reports(), false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "repo        errors  warnings");
        assert_eq!(lines[1], "acme/messy       1         1");
        assert_eq!(lines[2], "acme/clean       0         0");
    }

    #[test]
    fn table_reports_a_failed_repo_with_its_error() {
        let out = render_table(&mixed_reports(), false);
        assert!(
            out.contains("gone        failed: could not clone: no such path"),
            "failed row:\n{out}"
        );
    }

    #[test]
    fn table_ends_with_a_summary_line() {
        let out = render_table(&mixed_reports(), false);
        assert!(
            out.trim_end()
                .ends_with("3 repos audited: 1 with errors, 1 failed to audit"),
            "summary line:\n{out}"
        );
    }

    #[test]
    fn table_summary_reports_no_errors_when_everything_passed() {
        let reports = vec![audited("acme/clean", vec![])];
        let out = render_table(&reports, false);
        assert!(
            out.trim_end().ends_with("1 repo audited: no errors"),
            "summary line:\n{out}"
        );
    }

    #[test]
    fn uncolored_table_has_no_ansi_codes() {
        assert!(!render_table(&mixed_reports(), false).contains(ESC));
    }

    #[test]
    fn colored_table_styles_nonzero_counts_and_failures() {
        let out = render_table(&mixed_reports(), true);
        assert!(out.contains(ESC));
        // Zero counts stay plain so problems stand out.
        let clean = render_table(&[audited("a", vec![])], true);
        assert!(!clean.lines().nth(1).expect("row").contains(ESC));
    }

    // ---- markdown -------------------------------------------------------

    #[test]
    fn markdown_has_a_title_source_and_totals() {
        let out = render_markdown("file repos.txt", &mixed_reports());
        assert!(out.starts_with("# hpds audit report\n"), "title:\n{out}");
        assert!(out.contains("Source: file repos.txt"), "source:\n{out}");
        assert!(out.contains("Repos: 2 audited, 1 failed"), "repos:\n{out}");
        assert!(
            out.contains("Findings: 1 error, 1 warning, 0 info"),
            "findings:\n{out}"
        );
    }

    #[test]
    fn markdown_has_a_section_per_repo_with_findings_and_remediation() {
        let out = render_markdown("org acme", &mixed_reports());
        assert!(out.contains("\n## acme/messy\n"), "section:\n{out}");
        assert!(
            out.contains("- **error** `junk-files`: junk-files went wrong"),
            "finding line:\n{out}"
        );
        assert!(out.contains("fix: fix junk-files"), "remediation:\n{out}");
    }

    #[test]
    fn markdown_marks_clean_repos_and_failed_repos() {
        let out = render_markdown("org acme", &mixed_reports());
        assert!(out.contains("\n## acme/clean\n\nNo findings.\n"), "{out}");
        assert!(
            out.contains("Could not audit: could not clone: no such path"),
            "failure section:\n{out}"
        );
        assert!(
            out.contains("re-run `hpds audit all`"),
            "failure remediation:\n{out}"
        );
    }

    // ---- JSON -----------------------------------------------------------

    #[test]
    fn sweep_json_schema_is_exactly_the_documented_shape() {
        // Tooling consumes this schema; assert the exact serialized bytes
        // so any change to field names, order, or casing fails loudly.
        let reports = vec![
            audited("acme/messy", vec![finding("junk-files", Severity::Error)]),
            failed("gone", "could not clone: no such path"),
        ];
        let json = render_sweep_json(None, &reports).expect("render json");
        assert_eq!(
            json,
            r#"{
  "org": null,
  "repos": [
    {
      "repo": "acme/messy",
      "findings": [
        {
          "check_id": "junk-files",
          "severity": "error",
          "message": "junk-files went wrong",
          "remediation": "fix junk-files"
        }
      ],
      "summary": {
        "errors": 1,
        "warnings": 0,
        "infos": 0
      }
    },
    {
      "repo": "gone",
      "error": "could not clone: no such path"
    }
  ],
  "summary": {
    "repos": 2,
    "audited": 1,
    "failed": 1,
    "errors": 1,
    "warnings": 0,
    "infos": 0
  }
}"#
        );
    }

    #[test]
    fn sweep_json_carries_the_org_when_enumerated_from_one() {
        let json = render_sweep_json(Some("StanfordHPDS"), &[audited("StanfordHPDS/x", vec![])])
            .expect("render json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["org"], "StanfordHPDS");
        assert_eq!(value["summary"]["repos"], 1);
        assert_eq!(value["summary"]["failed"], 0);
    }

    // ---- exit semantics ---------------------------------------------------

    #[test]
    fn failure_counts_count_error_repos_and_failed_repos() {
        assert_eq!(failure_counts(&mixed_reports()), (1, 1));
        assert_eq!(failure_counts(&[audited("a", vec![])]), (0, 0));
        let warn_only = vec![audited(
            "a",
            vec![finding("gitignore-hygiene", Severity::Warn)],
        )];
        assert_eq!(failure_counts(&warn_only), (0, 0));
    }
}
