//! `hpds audit report-github` — the audit bot core: consumes the JSON
//! emitted by `hpds audit --format json` and mirrors it to GitHub via `gh`.
//!
//! Two modes, one per workflow trigger:
//!
//! - **PR**: upsert a single sticky comment (marked [`COMMENT_MARKER`])
//!   carrying the findings table; subsequent runs edit it in place.
//! - **Schedule**: keep one open issue per error finding, keyed by a stable
//!   [`fingerprint`] embedded in an HTML marker comment. Existing open
//!   issues with the same fingerprint are never duplicated; issues whose
//!   fingerprint no longer occurs are closed with a comment.
//!
//! Like the rest of the audit core this module returns data — plans and
//! progress lines — and never prints. All `gh` calls sit behind the
//! [`GhBot`] trait so every decision is tested against a fake.

use std::process::Command;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::github::model::{self, ModelError};
use super::{Finding, Severity};

/// HTML marker identifying the sticky PR comment; invisible in rendered
/// Markdown. Part of the bot's persisted state — changing it orphans
/// existing comments.
pub const COMMENT_MARKER: &str = "<!-- hpds-audit -->";

/// Label carried by every bot-filed issue; the schedule pass only ever
/// lists (and therefore only ever closes) issues with this label.
pub const ISSUE_LABEL: &str = "hpds-audit";

/// Marker prefix embedding a finding fingerprint in an issue body. Also
/// persisted state — see [`COMMENT_MARKER`].
const FINGERPRINT_PREFIX: &str = "<!-- hpds-audit:fingerprint:";
const FINGERPRINT_SUFFIX: &str = "-->";

// ---------------------------------------------------------------------------
// Input: the audit JSON

/// The audit report as consumed from `hpds audit --format json` output.
/// Field names follow the stable schema documented on
/// [`super::render_json`].
#[derive(Debug, Deserialize)]
pub struct AuditReport {
    /// Repository display name as reported by the audit.
    pub repo: String,
    /// All findings, every severity.
    pub findings: Vec<Finding>,
}

/// Parse the JSON emitted by `hpds audit --format json`.
pub fn parse_report(json: &str) -> Result<AuditReport, serde_json::Error> {
    serde_json::from_str(json)
}

// ---------------------------------------------------------------------------
// Fingerprints

/// Stable identity of a finding for issue dedup: hex-encoded SHA-256 of
/// the check id and the repo path, truncated to 16 characters. Message
/// wording may change between hpds releases; the fingerprint must not.
pub fn fingerprint(check_id: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(check_id.as_bytes());
    // NUL between the parts so (check, path) pairs cannot collide by
    // shifting characters across the boundary.
    hasher.update([0u8]);
    hasher.update(path.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    hex[..16].to_string()
}

/// The HTML marker line embedding `fingerprint` in an issue body.
fn fingerprint_marker(fingerprint: &str) -> String {
    format!("{FINGERPRINT_PREFIX}{fingerprint} {FINGERPRINT_SUFFIX}")
}

/// Extract the fingerprint from an issue body, or `None` when the body
/// carries no (or a malformed) marker.
pub fn extract_fingerprint(body: &str) -> Option<String> {
    let after = &body[body.find(FINGERPRINT_PREFIX)? + FINGERPRINT_PREFIX.len()..];
    let inside = after[..after.find(FINGERPRINT_SUFFIX)?].trim();
    (!inside.is_empty() && !inside.contains(char::is_whitespace)).then(|| inside.to_string())
}

// ---------------------------------------------------------------------------
// Rendered bodies

/// The sticky PR comment: marker line, findings table (or an all-clear
/// line), and a footer identifying the bot.
pub fn comment_body(report: &AuditReport) -> String {
    let mut out = format!(
        "{COMMENT_MARKER}\n### hpds audit: `{}`\n\n",
        table_cell(&report.repo)
    );

    if report.findings.is_empty() {
        out.push_str("✓ no findings — all checks passed\n");
    } else {
        out.push_str("| severity | check | finding | fix |\n");
        out.push_str("| --- | --- | --- | --- |\n");
        for finding in &report.findings {
            let severity = match finding.severity {
                Severity::Error => "error",
                Severity::Warn => "warn",
                Severity::Info => "info",
            };
            out.push_str(&format!(
                "| {severity} | `{}` | {} | {} |\n",
                table_cell(&finding.check_id),
                table_cell(&finding.message),
                table_cell(&finding.remediation),
            ));
        }
        let summary = super::summarize(&report.findings);
        out.push_str(&format!(
            "\n{}, {}, {} info\n",
            plural(summary.errors, "error"),
            plural(summary.warnings, "warning"),
            summary.infos,
        ));
    }

    out.push_str(
        "\n_Posted by `hpds audit report-github`; this comment is updated in place on every run._",
    );
    out
}

/// A finding string flattened into one Markdown table cell: pipes escaped,
/// newlines collapsed to spaces.
fn table_cell(text: &str) -> String {
    text.replace('|', r"\|").replace(['\r', '\n'], " ")
}

/// `1 error` / `2 errors` — for the comment summary line.
fn plural(n: usize, noun: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{s}")
}

/// Title of the issue filed for an error finding group.
pub fn issue_title(check_id: &str) -> String {
    format!("hpds audit: {check_id}")
}

/// Body of the issue filed for the error findings sharing `fingerprint`
/// (several when one check reports more than once). Carries the
/// fingerprint marker for later dedup and close decisions.
pub fn issue_body(repo: &str, findings: &[&Finding], fingerprint: &str) -> String {
    let mut out = format!(
        "{}\nThe scheduled `hpds audit` of `{repo}` reported an error:\n",
        fingerprint_marker(fingerprint)
    );
    for finding in findings {
        out.push_str(&format!(
            "\n- **check:** `{}`\n- **finding:** {}\n- **fix:** {}\n",
            finding.check_id, finding.message, finding.remediation,
        ));
    }
    out.push_str(
        "\n_Filed by `hpds audit report-github`; it will be closed automatically once a \
         scheduled audit no longer reports this finding._",
    );
    out
}

// ---------------------------------------------------------------------------
// Plans (pure decisions, executed by the run_* drivers)

/// One comment on a PR, as listed via `gh api`.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
}

/// One issue, as listed via `gh api`. The issues endpoint also returns
/// pull requests; those carry a `pull_request` key and are skipped.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueSummary {
    pub number: u64,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

/// Whether the PR pass creates the sticky comment or edits the existing
/// one in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentAction {
    Create,
    Update { comment_id: u64 },
}

/// Upsert decision: edit the first existing comment carrying
/// [`COMMENT_MARKER`], create when none does.
pub fn plan_comment(existing: &[IssueComment]) -> CommentAction {
    existing
        .iter()
        .find(|comment| {
            comment
                .body
                .as_deref()
                .is_some_and(|body| body.contains(COMMENT_MARKER))
        })
        .map_or(CommentAction::Create, |comment| CommentAction::Update {
            comment_id: comment.id,
        })
}

/// An issue the schedule pass will file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIssue {
    pub fingerprint: String,
    pub title: String,
    pub body: String,
}

/// What the schedule pass will do: file issues for new error fingerprints,
/// close open ones whose fingerprint no longer occurs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulePlan {
    pub file: Vec<NewIssue>,
    pub close: Vec<u64>,
}

/// Decide the schedule pass against the currently open bot issues:
///
/// - error findings are grouped by [`fingerprint`] (first-seen order); a
///   group whose fingerprint already marks an open issue files nothing;
/// - open issues (never PRs) whose marked fingerprint no longer occurs are
///   closed; issues without a parseable marker are left alone — the bot
///   only retires state it created.
pub fn plan_schedule(repo: &str, findings: &[Finding], open: &[IssueSummary]) -> SchedulePlan {
    // Group error findings by fingerprint, keeping first-seen order so
    // issues are filed in report order.
    let mut groups: Vec<(String, Vec<&Finding>)> = Vec::new();
    for finding in findings {
        if finding.severity != Severity::Error {
            continue;
        }
        let fp = fingerprint(&finding.check_id, repo);
        match groups.iter_mut().find(|(existing, _)| *existing == fp) {
            Some((_, members)) => members.push(finding),
            None => groups.push((fp, vec![finding])),
        }
    }

    // The bot's live state: fingerprints marked on currently open issues
    // (PRs share the issues endpoint but are not issues).
    let marked_open: Vec<(u64, String)> = open
        .iter()
        .filter(|issue| issue.pull_request.is_none())
        .filter_map(|issue| {
            let fp = extract_fingerprint(issue.body.as_deref()?)?;
            Some((issue.number, fp))
        })
        .collect();

    let file = groups
        .iter()
        .filter(|(fp, _)| !marked_open.iter().any(|(_, open_fp)| open_fp == fp))
        .map(|(fp, members)| NewIssue {
            fingerprint: fp.clone(),
            title: issue_title(&members[0].check_id),
            body: issue_body(repo, members, fp),
        })
        .collect();

    let close = marked_open
        .iter()
        .filter(|(_, open_fp)| !groups.iter().any(|(fp, _)| fp == open_fp))
        .map(|(number, _)| *number)
        .collect();

    SchedulePlan { file, close }
}

// ---------------------------------------------------------------------------
// The gh seam

/// Errors from talking to GitHub through `gh`. The drivers stop on the
/// first failure: a half-applied pass self-heals on the next run because
/// every decision re-derives from live GitHub state.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    #[error("gh is not installed or not on PATH")]
    GhMissing,

    #[error("`gh api {endpoint}` failed{}", render_detail(detail))]
    Failed { endpoint: String, detail: String },

    #[error("unexpected JSON from `gh api {endpoint}`: {detail}")]
    Json { endpoint: String, detail: String },
}

fn render_detail(detail: &str) -> String {
    let trimmed = detail.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

/// The seam between the bot and GitHub: every remote read and write the
/// drivers perform, on one trait so tests fake the whole world in one
/// place (same pattern as [`super::github::GithubApi`]).
pub trait GhBot {
    fn list_pr_comments(&self, pr: u64) -> Result<Vec<IssueComment>, BotError>;
    fn create_pr_comment(&self, pr: u64, body: &str) -> Result<(), BotError>;
    fn update_pr_comment(&self, comment_id: u64, body: &str) -> Result<(), BotError>;
    fn list_open_audit_issues(&self) -> Result<Vec<IssueSummary>, BotError>;
    /// Returns the created issue's number.
    fn create_issue(&self, title: &str, body: &str, label: &str) -> Result<u64, BotError>;
    fn comment_on_issue(&self, number: u64, body: &str) -> Result<(), BotError>;
    fn close_issue(&self, number: u64) -> Result<(), BotError>;
}

/// The real [`GhBot`]: shells out to `gh api`, authenticated by gh itself
/// (`GITHUB_TOKEN` in Actions, `gh auth login` elsewhere).
pub struct GhCliBot {
    /// `owner/repo` slug all endpoints are built from.
    repo: String,
}

impl GhCliBot {
    pub fn new(repo: String) -> Self {
        GhCliBot { repo }
    }

    /// Run `gh api <endpoint> <args...>` and return stdout.
    fn gh(&self, endpoint: &str, args: &[&str]) -> Result<String, BotError> {
        let out = Command::new(crate::gitx::gh_program())
            .arg("api")
            .arg(endpoint)
            .args(args)
            .output()
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => BotError::GhMissing,
                _ => BotError::Failed {
                    endpoint: endpoint.to_string(),
                    detail: err.to_string(),
                },
            })?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(BotError::Failed {
                endpoint: endpoint.to_string(),
                detail: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Parse a `gh api` response, wrapping parse failures with the
    /// endpoint for the error message.
    fn parsed<T>(endpoint: &str, result: Result<T, ModelError>) -> Result<T, BotError> {
        result.map_err(|err| BotError::Json {
            endpoint: endpoint.to_string(),
            detail: err.to_string(),
        })
    }
}

/// `{"number": N}` — the only field read off a created issue.
#[derive(Debug, Deserialize)]
struct CreatedIssue {
    number: u64,
}

impl GhBot for GhCliBot {
    fn list_pr_comments(&self, pr: u64) -> Result<Vec<IssueComment>, BotError> {
        let endpoint = format!("repos/{}/issues/{pr}/comments", self.repo);
        let body = self.gh(&endpoint, &["--paginate"])?;
        Self::parsed(&endpoint, model::parse_pages(&body))
    }

    fn create_pr_comment(&self, pr: u64, body: &str) -> Result<(), BotError> {
        let endpoint = format!("repos/{}/issues/{pr}/comments", self.repo);
        self.gh(
            &endpoint,
            &["--method", "POST", "-f", &format!("body={body}")],
        )?;
        Ok(())
    }

    fn update_pr_comment(&self, comment_id: u64, body: &str) -> Result<(), BotError> {
        let endpoint = format!("repos/{}/issues/comments/{comment_id}", self.repo);
        self.gh(
            &endpoint,
            &["--method", "PATCH", "-f", &format!("body={body}")],
        )?;
        Ok(())
    }

    fn list_open_audit_issues(&self) -> Result<Vec<IssueSummary>, BotError> {
        let endpoint = format!(
            "repos/{}/issues?state=open&labels={ISSUE_LABEL}&per_page=100",
            self.repo
        );
        let body = self.gh(&endpoint, &["--paginate"])?;
        Self::parsed(&endpoint, model::parse_pages(&body))
    }

    fn create_issue(&self, title: &str, body: &str, label: &str) -> Result<u64, BotError> {
        let endpoint = format!("repos/{}/issues", self.repo);
        let out = self.gh(
            &endpoint,
            &[
                "--method",
                "POST",
                "-f",
                &format!("title={title}"),
                "-f",
                &format!("body={body}"),
                "-f",
                &format!("labels[]={label}"),
            ],
        )?;
        let created: CreatedIssue = Self::parsed(&endpoint, model::parse_one(&out))?;
        Ok(created.number)
    }

    fn comment_on_issue(&self, number: u64, body: &str) -> Result<(), BotError> {
        let endpoint = format!("repos/{}/issues/{number}/comments", self.repo);
        self.gh(
            &endpoint,
            &["--method", "POST", "-f", &format!("body={body}")],
        )?;
        Ok(())
    }

    fn close_issue(&self, number: u64) -> Result<(), BotError> {
        let endpoint = format!("repos/{}/issues/{number}", self.repo);
        self.gh(&endpoint, &["--method", "PATCH", "-f", "state=closed"])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Drivers

/// PR pass: upsert the sticky findings comment on `pr`. Returns progress
/// lines for the command layer to print.
pub fn run_pr(bot: &dyn GhBot, pr: u64, report: &AuditReport) -> Result<Vec<String>, BotError> {
    let body = comment_body(report);
    let line = match plan_comment(&bot.list_pr_comments(pr)?) {
        CommentAction::Create => {
            bot.create_pr_comment(pr, &body)?;
            format!("posted the audit comment to PR #{pr}")
        }
        CommentAction::Update { comment_id } => {
            bot.update_pr_comment(comment_id, &body)?;
            format!("updated the audit comment on PR #{pr}")
        }
    };
    Ok(vec![line])
}

/// Schedule pass: file issues for new error findings, close resolved
/// ones. `repo` is the `owner/repo` slug fingerprints hash over. Returns
/// progress lines for the command layer to print.
/// The comment left on an issue as it is closed.
const RESOLVED_COMMENT: &str =
    "The latest scheduled `hpds audit` no longer reports this finding — closing.";

pub fn run_schedule(
    bot: &dyn GhBot,
    repo: &str,
    report: &AuditReport,
) -> Result<Vec<String>, BotError> {
    let open = bot.list_open_audit_issues()?;
    let plan = plan_schedule(repo, &report.findings, &open);

    let mut lines = Vec::new();
    for issue in &plan.file {
        let number = bot.create_issue(&issue.title, &issue.body, ISSUE_LABEL)?;
        lines.push(format!("opened issue #{number}: {}", issue.title));
    }
    for number in &plan.close {
        bot.comment_on_issue(*number, RESOLVED_COMMENT)?;
        bot.close_issue(*number)?;
        lines.push(format!("closed issue #{number}: finding resolved"));
    }
    if lines.is_empty() {
        lines.push("nothing to do: no new error findings, no resolved issues".to_string());
    }
    Ok(lines)
}

// ---------------------------------------------------------------------------
// Run context (flags + GitHub Actions environment)

/// Which of the two bot passes to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pr,
    Schedule,
}

/// A flag/environment combination the bot cannot act on. Carries the
/// remediation hint separately so the command layer renders it as the
/// standard `hint:` line.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ContextError {
    message: String,
    hint: String,
}

impl ContextError {
    pub fn hint(&self) -> &str {
        &self.hint
    }
}

fn context_error(message: impl Into<String>, hint: impl Into<String>) -> ContextError {
    ContextError {
        message: message.into(),
        hint: hint.into(),
    }
}

/// Resolve the mode: explicit flag first, then the Actions event name
/// (`pull_request`/`pull_request_target` → PR, `schedule` → schedule).
pub fn resolve_mode(flag: Option<Mode>, event_name: Option<&str>) -> Result<Mode, ContextError> {
    if let Some(mode) = flag {
        return Ok(mode);
    }
    match event_name {
        Some("pull_request") | Some("pull_request_target") => Ok(Mode::Pr),
        Some("schedule") => Ok(Mode::Schedule),
        other => Err(context_error(
            match other {
                Some(event) if !event.is_empty() => {
                    format!("cannot pick a reporting mode for the `{event}` event")
                }
                _ => "cannot pick a reporting mode: GITHUB_EVENT_NAME is not set".to_string(),
            },
            "pass `--mode pr` (sticky PR comment) or `--mode schedule` (issue lifecycle)",
        )),
    }
}

/// Resolve the `owner/repo` slug: explicit flag first, then
/// `GITHUB_REPOSITORY`.
pub fn resolve_repo(flag: Option<&str>, env_repo: Option<&str>) -> Result<String, ContextError> {
    let slug = flag.or(env_repo).ok_or_else(|| {
        context_error(
            "no repository to report to: GITHUB_REPOSITORY is not set",
            "pass `--repo <owner/repo>` (GitHub Actions sets GITHUB_REPOSITORY automatically)",
        )
    })?;
    match slug.split_once('/') {
        Some((owner, repo)) if !owner.is_empty() && !repo.is_empty() && !repo.contains('/') => {
            Ok(slug.to_string())
        }
        _ => Err(context_error(
            format!("`{slug}` is not an owner/repo slug"),
            "pass the repository as `--repo <owner/repo>`, e.g. `--repo StanfordHPDS/demo`",
        )),
    }
}

/// Resolve the PR number: explicit flag, then `GITHUB_REF`
/// (`refs/pull/<n>/merge`), then the event payload (`pull_request.number`
/// or top-level `number`).
pub fn resolve_pr(
    flag: Option<u64>,
    github_ref: Option<&str>,
    event_payload: Option<&str>,
) -> Result<u64, ContextError> {
    flag.or_else(|| github_ref.and_then(pr_from_ref))
        .or_else(|| event_payload.and_then(pr_from_event))
        .ok_or_else(|| {
            context_error(
                "cannot determine which pull request to comment on",
                "pass `--pr <number>` (on Actions pull_request events, \
                 GITHUB_REF or the event payload carries it)",
            )
        })
}

/// The PR number in a `refs/pull/<n>/merge` (or `/head`) ref.
fn pr_from_ref(github_ref: &str) -> Option<u64> {
    let rest = github_ref.strip_prefix("refs/pull/")?;
    let (number, _) = rest.split_once('/')?;
    number.parse().ok()
}

/// The PR number in an Actions event payload: `pull_request.number`,
/// falling back to the top-level `number` (issue_comment-style payloads).
fn pr_from_event(payload: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value
        .pointer("/pull_request/number")
        .or_else(|| value.pointer("/number"))?
        .as_u64()
}

#[cfg(test)]
mod tests {
    use super::super::github::model::tests::fixture;
    use super::*;
    use std::cell::RefCell;

    fn finding(check_id: &str, severity: Severity, message: &str, remediation: &str) -> Finding {
        Finding {
            check_id: check_id.to_string(),
            severity,
            message: message.to_string(),
            remediation: remediation.to_string(),
        }
    }

    fn error_finding(check_id: &str) -> Finding {
        finding(
            check_id,
            Severity::Error,
            &format!("{check_id} went wrong"),
            &format!("fix {check_id}"),
        )
    }

    fn report(findings: Vec<Finding>) -> AuditReport {
        AuditReport {
            repo: "demo".to_string(),
            findings,
        }
    }

    // -- audit JSON input ---------------------------------------------------

    #[test]
    fn parses_the_json_emitted_by_the_audit() {
        // Round-trip through the real serializer so the two schemas can
        // never drift apart silently.
        let findings = vec![
            finding("dirty-files", Severity::Error, "2 dirty", "commit them"),
            finding(
                "gitignore-hygiene",
                Severity::Warn,
                "missing patterns",
                "add them",
            ),
        ];
        let json = super::super::render_json("demo", &findings).expect("render");
        let report = parse_report(&json).expect("parse back");
        assert_eq!(report.repo, "demo");
        assert_eq!(report.findings, findings);
    }

    #[test]
    fn rejects_json_that_is_not_an_audit_report() {
        assert!(parse_report("{}").is_err());
        assert!(parse_report("not json").is_err());
        assert!(parse_report("").is_err());
    }

    // -- fingerprints ---------------------------------------------------

    #[test]
    fn fingerprint_is_stable_across_runs() {
        assert_eq!(
            fingerprint("dirty-files", "acme/demo"),
            fingerprint("dirty-files", "acme/demo"),
        );
    }

    #[test]
    fn fingerprint_is_16_lowercase_hex_chars() {
        let fp = fingerprint("dirty-files", "acme/demo");
        assert_eq!(fp.len(), 16, "fingerprint {fp}");
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "fingerprint {fp}"
        );
    }

    #[test]
    fn fingerprint_changes_with_the_check_id() {
        assert_ne!(
            fingerprint("dirty-files", "acme/demo"),
            fingerprint("stale-branches", "acme/demo"),
        );
    }

    #[test]
    fn fingerprint_changes_with_the_path() {
        assert_ne!(
            fingerprint("dirty-files", "acme/demo"),
            fingerprint("dirty-files", "acme/other"),
        );
    }

    #[test]
    fn fingerprint_separates_check_id_from_path() {
        // Moving a character across the boundary must change the hash:
        // the two parts are separated, not concatenated.
        assert_ne!(fingerprint("ab", "cd"), fingerprint("abc", "d"));
    }

    #[test]
    fn issue_body_fingerprint_round_trips_through_extraction() {
        let fp = fingerprint("dirty-files", "acme/demo");
        let body = issue_body("acme/demo", &[&error_finding("dirty-files")], &fp);
        assert_eq!(extract_fingerprint(&body).as_deref(), Some(fp.as_str()));
    }

    #[test]
    fn extract_fingerprint_handles_missing_and_malformed_markers() {
        assert_eq!(extract_fingerprint(""), None);
        assert_eq!(extract_fingerprint("a plain issue body"), None);
        assert_eq!(extract_fingerprint(FINGERPRINT_PREFIX), None);
        // Marker never closed.
        assert_eq!(
            extract_fingerprint(&format!("{FINGERPRINT_PREFIX}abc123")),
            None
        );
        // Empty fingerprint.
        assert_eq!(
            extract_fingerprint(&format!("{FINGERPRINT_PREFIX} {FINGERPRINT_SUFFIX}")),
            None
        );
    }

    #[test]
    fn extract_fingerprint_finds_the_marker_anywhere_in_the_body() {
        let body = format!(
            "some preamble\n\n{}\nmore text",
            fingerprint_marker("abc123")
        );
        assert_eq!(extract_fingerprint(&body).as_deref(), Some("abc123"));
    }

    // -- comment body ---------------------------------------------------

    #[test]
    fn comment_body_starts_with_the_sticky_marker() {
        let with = comment_body(&report(vec![error_finding("dirty-files")]));
        let without = comment_body(&report(vec![]));
        assert!(with.starts_with(COMMENT_MARKER), "{with}");
        assert!(without.starts_with(COMMENT_MARKER), "{without}");
    }

    #[test]
    fn comment_body_tabulates_every_finding() {
        let body = comment_body(&report(vec![
            finding("dirty-files", Severity::Error, "2 dirty files", "commit"),
            finding(
                "gitignore-hygiene",
                Severity::Warn,
                "missing patterns",
                "add them",
            ),
            finding("lockfiles", Severity::Info, "no lockfile", "commit one"),
        ]));
        // A Markdown table: header, separator, one row per finding.
        assert!(
            body.contains("| severity | check | finding | fix |"),
            "{body}"
        );
        assert!(body.contains("| --- | --- | --- | --- |"), "{body}");
        assert!(
            body.contains("| error | `dirty-files` | 2 dirty files | commit |"),
            "{body}"
        );
        assert!(
            body.contains("| warn | `gitignore-hygiene` | missing patterns | add them |"),
            "{body}"
        );
        assert!(
            body.contains("| info | `lockfiles` | no lockfile | commit one |"),
            "{body}"
        );
    }

    #[test]
    fn comment_body_escapes_table_breaking_characters() {
        let body = comment_body(&report(vec![finding(
            "junk-files",
            Severity::Error,
            "found a | pipe\nand a newline",
            "remove | it",
        )]));
        assert!(body.contains(r"found a \| pipe and a newline"), "{body}");
        assert!(body.contains(r"remove \| it"), "{body}");
    }

    #[test]
    fn comment_body_without_findings_reports_all_clear() {
        let body = comment_body(&report(vec![]));
        assert!(body.contains("no findings"), "{body}");
        assert!(!body.contains("| --- |"), "no empty table:\n{body}");
    }

    #[test]
    fn comment_body_counts_findings_by_severity() {
        let body = comment_body(&report(vec![
            error_finding("dirty-files"),
            error_finding("junk-files"),
            finding("gitignore-hygiene", Severity::Warn, "m", "r"),
        ]));
        assert!(body.contains("2 errors, 1 warning"), "{body}");
    }

    #[test]
    fn comment_body_names_the_audited_repo() {
        let body = comment_body(&report(vec![]));
        assert!(body.contains("demo"), "{body}");
    }

    // -- issue body -------------------------------------------------------

    #[test]
    fn issue_title_names_the_check() {
        assert_eq!(issue_title("dirty-files"), "hpds audit: dirty-files");
    }

    #[test]
    fn issue_body_carries_marker_check_message_and_fix() {
        let f = finding("dirty-files", Severity::Error, "2 dirty files", "commit");
        let fp = fingerprint("dirty-files", "acme/demo");
        let body = issue_body("acme/demo", &[&f], &fp);
        assert!(body.contains(&fingerprint_marker(&fp)), "{body}");
        assert!(body.contains("`dirty-files`"), "{body}");
        assert!(body.contains("2 dirty files"), "{body}");
        assert!(body.contains("commit"), "{body}");
        assert!(body.contains("acme/demo"), "{body}");
    }

    #[test]
    fn issue_body_lists_every_finding_in_the_group() {
        let a = finding("junk-files", Severity::Error, ".DS_Store committed", "rm a");
        let b = finding("junk-files", Severity::Error, ".Rhistory committed", "rm b");
        let body = issue_body("acme/demo", &[&a, &b], "abc123");
        assert!(body.contains(".DS_Store committed"), "{body}");
        assert!(body.contains(".Rhistory committed"), "{body}");
    }

    // -- sticky comment upsert decision -----------------------------------

    fn comment(id: u64, body: &str) -> IssueComment {
        IssueComment {
            id,
            body: Some(body.to_string()),
        }
    }

    #[test]
    fn plan_comment_creates_when_no_comment_exists() {
        assert_eq!(plan_comment(&[]), CommentAction::Create);
    }

    #[test]
    fn plan_comment_creates_when_no_comment_carries_the_marker() {
        let existing = vec![comment(1, "LGTM"), comment(2, "unrelated bot output")];
        assert_eq!(plan_comment(&existing), CommentAction::Create);
    }

    #[test]
    fn plan_comment_updates_the_marked_comment_in_place() {
        let existing = vec![
            comment(1, "LGTM"),
            comment(2, &format!("{COMMENT_MARKER}\nold audit table")),
        ];
        assert_eq!(
            plan_comment(&existing),
            CommentAction::Update { comment_id: 2 }
        );
    }

    #[test]
    fn plan_comment_updates_the_first_of_several_marked_comments() {
        let existing = vec![
            comment(7, &format!("{COMMENT_MARKER} a")),
            comment(9, &format!("{COMMENT_MARKER} b")),
        ];
        assert_eq!(
            plan_comment(&existing),
            CommentAction::Update { comment_id: 7 }
        );
    }

    #[test]
    fn plan_comment_survives_a_null_body() {
        let existing = vec![IssueComment { id: 1, body: None }];
        assert_eq!(plan_comment(&existing), CommentAction::Create);
    }

    #[test]
    fn plan_comment_parses_recorded_pr_comment_fixtures() {
        let empty: Vec<IssueComment> =
            model::parse_pages(&fixture("pr-comments-none.json")).expect("parses");
        assert_eq!(plan_comment(&empty), CommentAction::Create);

        let sticky: Vec<IssueComment> =
            model::parse_pages(&fixture("pr-comments-sticky.json")).expect("parses");
        assert_eq!(
            plan_comment(&sticky),
            CommentAction::Update { comment_id: 201 }
        );
    }

    // -- schedule plan ------------------------------------------------------

    fn open_issue(number: u64, fingerprint: &str) -> IssueSummary {
        IssueSummary {
            number,
            body: Some(issue_body(
                "acme/demo",
                &[&error_finding("whatever")],
                fingerprint,
            )),
            pull_request: None,
        }
    }

    #[test]
    fn plan_schedule_files_one_issue_per_new_error_finding() {
        let findings = vec![error_finding("dirty-files"), error_finding("junk-files")];
        let plan = plan_schedule("acme/demo", &findings, &[]);
        assert_eq!(plan.file.len(), 2, "{plan:?}");
        assert_eq!(
            plan.file[0].fingerprint,
            fingerprint("dirty-files", "acme/demo")
        );
        assert_eq!(plan.file[0].title, issue_title("dirty-files"));
        assert_eq!(
            plan.file[1].fingerprint,
            fingerprint("junk-files", "acme/demo")
        );
        assert_eq!(plan.close, Vec::<u64>::new());
    }

    #[test]
    fn plan_schedule_ignores_warn_and_info_findings() {
        let findings = vec![
            finding("gitignore-hygiene", Severity::Warn, "m", "r"),
            finding("lockfiles", Severity::Info, "m", "r"),
        ];
        let plan = plan_schedule("acme/demo", &findings, &[]);
        assert_eq!(plan.file, Vec::new());
        assert_eq!(plan.close, Vec::<u64>::new());
    }

    #[test]
    fn plan_schedule_never_duplicates_an_open_issue_with_the_same_fingerprint() {
        let findings = vec![error_finding("dirty-files")];
        let open = vec![open_issue(3, &fingerprint("dirty-files", "acme/demo"))];
        let plan = plan_schedule("acme/demo", &findings, &open);
        assert_eq!(plan.file, Vec::new(), "must not duplicate issue #3");
        assert_eq!(plan.close, Vec::<u64>::new(), "finding still occurs");
    }

    #[test]
    fn plan_schedule_groups_same_check_findings_into_one_issue() {
        // Two findings from one check share a fingerprint: one issue,
        // both messages in its body.
        let a = finding("junk-files", Severity::Error, ".DS_Store committed", "rm");
        let b = finding("junk-files", Severity::Error, ".Rhistory committed", "rm");
        let plan = plan_schedule("acme/demo", &[a, b], &[]);
        assert_eq!(plan.file.len(), 1, "{plan:?}");
        assert!(plan.file[0].body.contains(".DS_Store committed"));
        assert!(plan.file[0].body.contains(".Rhistory committed"));
    }

    #[test]
    fn plan_schedule_closes_issues_whose_fingerprint_no_longer_occurs() {
        let open = vec![open_issue(3, &fingerprint("dirty-files", "acme/demo"))];
        let plan = plan_schedule("acme/demo", &[], &open);
        assert_eq!(plan.file, Vec::new());
        assert_eq!(plan.close, vec![3]);
    }

    #[test]
    fn plan_schedule_leaves_unmarked_and_pull_request_entries_alone() {
        let human = IssueSummary {
            number: 8,
            body: Some("a human filed this under the hpds-audit label".to_string()),
            pull_request: None,
        };
        let null_body = IssueSummary {
            number: 9,
            body: None,
            pull_request: None,
        };
        let pr = IssueSummary {
            number: 10,
            body: Some(issue_body(
                "acme/demo",
                &[&error_finding("dirty-files")],
                &fingerprint("dirty-files", "acme/demo"),
            )),
            pull_request: Some(serde_json::json!({"url": "https://example.invalid"})),
        };
        let plan = plan_schedule("acme/demo", &[], &[human, null_body, pr]);
        assert_eq!(plan.close, Vec::<u64>::new(), "{plan:?}");
    }

    #[test]
    fn plan_schedule_pr_entries_do_not_satisfy_dedup() {
        // A fingerprint seen only on a PR (not a real issue) must still
        // get its issue filed.
        let findings = vec![error_finding("dirty-files")];
        let pr = IssueSummary {
            number: 10,
            body: Some(fingerprint_marker(&fingerprint("dirty-files", "acme/demo"))),
            pull_request: Some(serde_json::json!({})),
        };
        let plan = plan_schedule("acme/demo", &findings, &[pr]);
        assert_eq!(plan.file.len(), 1, "{plan:?}");
    }

    #[test]
    fn plan_schedule_parses_the_recorded_open_issues_fixture() {
        // The fixture holds two bot issues (dirty-files, junk-files —
        // fingerprints hashed over acme/demo) and one unmarked human
        // issue. With only dirty-files still failing: junk-files closes,
        // nothing files, the human issue survives.
        let open: Vec<IssueSummary> =
            model::parse_pages(&fixture("issues-open-audit.json")).expect("parses");
        let findings = vec![error_finding("dirty-files")];
        let plan = plan_schedule("acme/demo", &findings, &open);
        assert_eq!(plan.file, Vec::new(), "{plan:?}");
        assert_eq!(plan.close, vec![32], "{plan:?}");
    }

    // -- drivers against the faked gh ---------------------------------------

    /// Every call the fake receives, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        ListPrComments {
            pr: u64,
        },
        CreatePrComment {
            pr: u64,
            body: String,
        },
        UpdatePrComment {
            comment_id: u64,
            body: String,
        },
        ListOpenAuditIssues,
        CreateIssue {
            title: String,
            body: String,
            label: String,
        },
        CommentOnIssue {
            number: u64,
            body: String,
        },
        CloseIssue {
            number: u64,
        },
    }

    /// A fake GitHub: canned list responses, recorded write calls.
    #[derive(Default)]
    struct FakeBot {
        pr_comments: Vec<IssueComment>,
        open_issues: Vec<IssueSummary>,
        calls: RefCell<Vec<Call>>,
    }

    impl FakeBot {
        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn record(&self, call: Call) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl GhBot for FakeBot {
        fn list_pr_comments(&self, pr: u64) -> Result<Vec<IssueComment>, BotError> {
            self.record(Call::ListPrComments { pr });
            Ok(self.pr_comments.clone())
        }

        fn create_pr_comment(&self, pr: u64, body: &str) -> Result<(), BotError> {
            self.record(Call::CreatePrComment {
                pr,
                body: body.to_string(),
            });
            Ok(())
        }

        fn update_pr_comment(&self, comment_id: u64, body: &str) -> Result<(), BotError> {
            self.record(Call::UpdatePrComment {
                comment_id,
                body: body.to_string(),
            });
            Ok(())
        }

        fn list_open_audit_issues(&self) -> Result<Vec<IssueSummary>, BotError> {
            self.record(Call::ListOpenAuditIssues);
            Ok(self.open_issues.clone())
        }

        fn create_issue(&self, title: &str, body: &str, label: &str) -> Result<u64, BotError> {
            self.record(Call::CreateIssue {
                title: title.to_string(),
                body: body.to_string(),
                label: label.to_string(),
            });
            Ok(42)
        }

        fn comment_on_issue(&self, number: u64, body: &str) -> Result<(), BotError> {
            self.record(Call::CommentOnIssue {
                number,
                body: body.to_string(),
            });
            Ok(())
        }

        fn close_issue(&self, number: u64) -> Result<(), BotError> {
            self.record(Call::CloseIssue { number });
            Ok(())
        }
    }

    #[test]
    fn run_pr_creates_the_sticky_comment_when_none_exists() {
        let bot = FakeBot::default();
        let report = report(vec![error_finding("dirty-files")]);
        let lines = run_pr(&bot, 7, &report).expect("run");
        let calls = bot.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], Call::ListPrComments { pr: 7 });
        match &calls[1] {
            Call::CreatePrComment { pr: 7, body } => {
                assert!(body.starts_with(COMMENT_MARKER), "{body}");
                assert!(body.contains("dirty-files"), "{body}");
            }
            other => panic!("expected create, got {other:?}"),
        }
        assert!(
            lines.iter().any(|l| l.contains("#7")),
            "lines name the PR: {lines:?}"
        );
    }

    #[test]
    fn run_pr_updates_the_existing_sticky_comment_in_place() {
        let bot = FakeBot {
            pr_comments: vec![
                comment(1, "unrelated"),
                comment(2, &format!("{COMMENT_MARKER}\nold table")),
            ],
            ..FakeBot::default()
        };
        let report = report(vec![]);
        run_pr(&bot, 7, &report).expect("run");
        let calls = bot.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        match &calls[1] {
            Call::UpdatePrComment {
                comment_id: 2,
                body,
            } => {
                assert!(body.starts_with(COMMENT_MARKER), "{body}");
            }
            other => panic!("expected update of comment 2, got {other:?}"),
        }
    }

    #[test]
    fn run_schedule_files_labeled_issues_for_new_errors_only() {
        let bot = FakeBot::default();
        let report = report(vec![
            error_finding("dirty-files"),
            finding("gitignore-hygiene", Severity::Warn, "m", "r"),
        ]);
        let lines = run_schedule(&bot, "acme/demo", &report).expect("run");
        let calls = bot.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], Call::ListOpenAuditIssues);
        match &calls[1] {
            Call::CreateIssue { title, body, label } => {
                assert_eq!(title, &issue_title("dirty-files"));
                assert_eq!(label, ISSUE_LABEL);
                assert_eq!(
                    extract_fingerprint(body).as_deref(),
                    Some(fingerprint("dirty-files", "acme/demo").as_str())
                );
            }
            other => panic!("expected issue creation, got {other:?}"),
        }
        assert!(
            lines.iter().any(|l| l.contains("#42")),
            "lines name the new issue: {lines:?}"
        );
    }

    #[test]
    fn run_schedule_skips_findings_that_already_have_an_open_issue() {
        let bot = FakeBot {
            open_issues: vec![open_issue(3, &fingerprint("dirty-files", "acme/demo"))],
            ..FakeBot::default()
        };
        let report = report(vec![error_finding("dirty-files")]);
        run_schedule(&bot, "acme/demo", &report).expect("run");
        assert_eq!(
            bot.calls(),
            vec![Call::ListOpenAuditIssues],
            "no writes: the issue already exists and still applies"
        );
    }

    #[test]
    fn run_schedule_closes_resolved_issues_with_a_comment() {
        let bot = FakeBot {
            open_issues: vec![open_issue(3, &fingerprint("dirty-files", "acme/demo"))],
            ..FakeBot::default()
        };
        let report = report(vec![]);
        let lines = run_schedule(&bot, "acme/demo", &report).expect("run");
        let calls = bot.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        match &calls[1] {
            Call::CommentOnIssue { number: 3, body } => {
                assert!(body.contains("no longer"), "{body}");
            }
            other => panic!("expected comment on #3, got {other:?}"),
        }
        assert_eq!(calls[2], Call::CloseIssue { number: 3 });
        assert!(
            lines.iter().any(|l| l.contains("#3")),
            "lines name the closed issue: {lines:?}"
        );
    }

    #[test]
    fn run_schedule_with_nothing_to_do_reports_that() {
        let bot = FakeBot::default();
        let lines = run_schedule(&bot, "acme/demo", &report(vec![])).expect("run");
        assert_eq!(bot.calls(), vec![Call::ListOpenAuditIssues]);
        assert!(!lines.is_empty(), "always report something: {lines:?}");
    }

    // -- context resolution -------------------------------------------------

    #[test]
    fn mode_flag_wins_over_the_event_name() {
        assert_eq!(
            resolve_mode(Some(Mode::Schedule), Some("pull_request")).unwrap(),
            Mode::Schedule
        );
    }

    #[test]
    fn mode_follows_the_actions_event_name() {
        assert_eq!(resolve_mode(None, Some("pull_request")).unwrap(), Mode::Pr);
        assert_eq!(
            resolve_mode(None, Some("pull_request_target")).unwrap(),
            Mode::Pr
        );
        assert_eq!(
            resolve_mode(None, Some("schedule")).unwrap(),
            Mode::Schedule
        );
    }

    #[test]
    fn unresolvable_mode_says_to_pass_the_flag() {
        for event in [None, Some("push"), Some("")] {
            let err = resolve_mode(None, event).expect_err("no mode");
            assert!(err.hint().contains("--mode"), "hint: {}", err.hint());
        }
    }

    #[test]
    fn repo_flag_wins_over_the_environment() {
        assert_eq!(
            resolve_repo(Some("acme/demo"), Some("other/repo")).unwrap(),
            "acme/demo"
        );
        assert_eq!(resolve_repo(None, Some("acme/demo")).unwrap(), "acme/demo");
    }

    #[test]
    fn repo_must_look_like_owner_slash_name() {
        for bad in ["demo", "acme/", "/demo", "a/b/c", ""] {
            let err = resolve_repo(Some(bad), None).expect_err("bad slug");
            assert!(
                err.to_string().contains("owner/repo"),
                "message names the shape: {err}"
            );
        }
    }

    #[test]
    fn missing_repo_says_to_pass_the_flag() {
        let err = resolve_repo(None, None).expect_err("no repo");
        assert!(err.hint().contains("--repo"), "hint: {}", err.hint());
    }

    #[test]
    fn pr_number_comes_from_flag_ref_or_event_payload_in_that_order() {
        let payload = r#"{"pull_request": {"number": 33}}"#;
        assert_eq!(
            resolve_pr(Some(5), Some("refs/pull/7/merge"), Some(payload)).unwrap(),
            5
        );
        assert_eq!(
            resolve_pr(None, Some("refs/pull/7/merge"), Some(payload)).unwrap(),
            7
        );
        assert_eq!(resolve_pr(None, None, Some(payload)).unwrap(), 33);
    }

    #[test]
    fn pr_number_falls_back_to_the_payloads_top_level_number() {
        assert_eq!(
            resolve_pr(None, None, Some(r#"{"number": 12}"#)).unwrap(),
            12
        );
    }

    #[test]
    fn non_pr_refs_do_not_yield_a_pr_number() {
        let payload = r#"{"number": 12}"#;
        for r in ["refs/heads/main", "refs/tags/v1.0.0", "refs/pull/x/merge"] {
            assert_eq!(
                resolve_pr(None, Some(r), Some(payload)).unwrap(),
                12,
                "ref {r} must fall through to the payload"
            );
        }
    }

    #[test]
    fn unresolvable_pr_number_says_to_pass_the_flag() {
        for payload in [None, Some("not json"), Some("{}")] {
            let err = resolve_pr(None, None, payload).expect_err("no pr");
            assert!(err.hint().contains("--pr"), "hint: {}", err.hint());
        }
    }
}
