//! Render audit findings for humans (styled text, grouped by severity)
//! and machines (JSON with a stable schema).

use anyhow::Context;
use serde::Serialize;

use super::{Finding, Severity};
use crate::ui::{ERROR_STYLE, HINT_STYLE, SUCCESS_STYLE, WARN_STYLE, paint};

/// Per-severity finding counts.
///
/// Serialized field names are a STABLE schema consumed by the audit bot:
/// `errors`, `warnings`, `infos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Number of [`Severity::Error`] findings.
    pub errors: usize,
    /// Number of [`Severity::Warn`] findings.
    pub warnings: usize,
    /// Number of [`Severity::Info`] findings.
    pub infos: usize,
}

/// Count findings per severity.
pub fn summarize(findings: &[Finding]) -> Summary {
    let mut summary = Summary {
        errors: 0,
        warnings: 0,
        infos: 0,
    };
    for finding in findings {
        match finding.severity {
            Severity::Error => summary.errors += 1,
            Severity::Warn => summary.warnings += 1,
            Severity::Info => summary.infos += 1,
        }
    }
    summary
}

/// The whole machine-readable report.
///
/// Serialized field names are a STABLE schema consumed by the audit bot:
/// `repo` (repository name), `findings` (array of [`Finding`]), `summary`
/// ([`Summary`]).
#[derive(Debug, Serialize)]
struct JsonReport<'a> {
    repo: &'a str,
    findings: &'a [Finding],
    summary: Summary,
}

/// Render the report as pretty-printed JSON (see [`JsonReport`] for the
/// schema).
pub fn render_json(repo: &str, findings: &[Finding]) -> anyhow::Result<String> {
    let report = JsonReport {
        repo,
        findings,
        summary: summarize(findings),
    };
    serde_json::to_string_pretty(&report).context("could not serialize the audit report to JSON")
}

/// Render the report for the terminal: findings grouped by severity
/// (errors, then warnings, then info), each with its `fix:` remediation
/// line, followed by a one-line summary counting findings against the
/// `checks_run` checks that produced them.
pub fn render_text(repo: &str, findings: &[Finding], checks_run: usize, use_color: bool) -> String {
    if findings.is_empty() {
        return format!(
            "audit of {repo}\n\n{} no findings across {}\n",
            paint(SUCCESS_STYLE, "✓", use_color),
            count(checks_run, "check"),
        );
    }

    let mut out = format!("audit of {repo}\n");
    let sections = [
        (Severity::Error, "errors:", "✗", ERROR_STYLE),
        (Severity::Warn, "warnings:", "!", WARN_STYLE),
        (Severity::Info, "info:", "•", HINT_STYLE),
    ];
    for (severity, header, bullet, style) in sections {
        let group: Vec<&Finding> = findings.iter().filter(|f| f.severity == severity).collect();
        if group.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&paint(style, header, use_color));
        out.push('\n');
        for finding in group {
            out.push_str(&format!(
                "  {} [{}] {}\n    {} {}\n",
                paint(style, bullet, use_color),
                finding.check_id,
                finding.message,
                paint(HINT_STYLE, "fix:", use_color),
                finding.remediation,
            ));
        }
    }

    let summary = summarize(findings);
    out.push_str(&format!(
        "\n{}, {} across {}\n",
        count(summary.errors, "error"),
        count(summary.warnings, "warning"),
        count(checks_run, "check"),
    ));
    out
}

/// `1 error` / `2 errors` — for the summary line.
fn count(n: usize, noun: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {noun}{s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: &str = "\x1b[";

    fn finding(check_id: &str, severity: Severity, message: &str, remediation: &str) -> Finding {
        Finding {
            check_id: check_id.to_string(),
            severity,
            message: message.to_string(),
            remediation: remediation.to_string(),
        }
    }

    /// One finding of each severity, deliberately in scrambled order so
    /// grouping is observable.
    fn mixed_findings() -> Vec<Finding> {
        vec![
            finding(
                "lockfiles",
                Severity::Info,
                "no lockfile detected",
                "commit renv.lock or uv.lock if the project uses renv or uv",
            ),
            finding(
                "dirty-files",
                Severity::Error,
                "2 tracked files have uncommitted changes",
                "commit or stash them",
            ),
            finding(
                "readme",
                Severity::Warn,
                "README.md is missing required sections",
                "add the lab-manual minimum sections",
            ),
        ]
    }

    #[test]
    fn summarize_counts_each_severity() {
        let summary = summarize(&mixed_findings());
        assert_eq!(
            summary,
            Summary {
                errors: 1,
                warnings: 1,
                infos: 1,
            }
        );
    }

    #[test]
    fn summarize_of_nothing_is_all_zeros() {
        assert_eq!(
            summarize(&[]),
            Summary {
                errors: 0,
                warnings: 0,
                infos: 0,
            }
        );
    }

    #[test]
    fn json_schema_is_exactly_the_documented_shape() {
        // The bot consumes this schema; this asserts the exact serialized
        // bytes so any change to field names, order, or casing fails loudly.
        let findings = vec![finding(
            "dirty-files",
            Severity::Error,
            "2 tracked files have uncommitted changes",
            "commit or stash them",
        )];
        let json = render_json("demo-repo", &findings).expect("render json");
        assert_eq!(
            json,
            r#"{
  "repo": "demo-repo",
  "findings": [
    {
      "check_id": "dirty-files",
      "severity": "error",
      "message": "2 tracked files have uncommitted changes",
      "remediation": "commit or stash them"
    }
  ],
  "summary": {
    "errors": 1,
    "warnings": 0,
    "infos": 0
  }
}"#
        );
    }

    #[test]
    fn json_severities_serialize_lowercase() {
        let json = render_json("demo", &mixed_findings()).expect("render json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let severities: Vec<&str> = value["findings"]
            .as_array()
            .expect("findings is an array")
            .iter()
            .map(|f| f["severity"].as_str().expect("severity is a string"))
            .collect();
        assert_eq!(severities, ["info", "error", "warn"]);
    }

    #[test]
    fn json_with_no_findings_has_empty_array_and_zero_summary() {
        let json = render_json("demo", &[]).expect("render json");
        assert_eq!(
            json,
            r#"{
  "repo": "demo",
  "findings": [],
  "summary": {
    "errors": 0,
    "warnings": 0,
    "infos": 0
  }
}"#
        );
    }

    /// Checks-run count used by the text-rendering tests.
    const CHECKS_RUN: usize = 9;

    #[test]
    fn text_groups_findings_by_severity_errors_first() {
        let out = render_text("demo", &mixed_findings(), CHECKS_RUN, false);
        let errors_at = out.find("errors:").expect("has errors section");
        let warnings_at = out.find("warnings:").expect("has warnings section");
        let info_at = out.find("info:").expect("has info section");
        assert!(errors_at < warnings_at, "errors before warnings:\n{out}");
        assert!(warnings_at < info_at, "warnings before info:\n{out}");
    }

    #[test]
    fn text_shows_check_id_message_and_remediation_for_each_finding() {
        let out = render_text("demo", &mixed_findings(), CHECKS_RUN, false);
        for f in mixed_findings() {
            assert!(out.contains(&f.check_id), "missing check id:\n{out}");
            assert!(out.contains(&f.message), "missing message:\n{out}");
            assert!(out.contains(&f.remediation), "missing remediation:\n{out}");
        }
        assert!(out.contains("fix:"), "remediation rendered as fix hint");
    }

    #[test]
    fn text_omits_empty_severity_sections() {
        let only_warn = vec![finding("readme", Severity::Warn, "meh", "fix it")];
        let out = render_text("demo", &only_warn, CHECKS_RUN, false);
        assert!(!out.contains("errors:"), "no empty errors section:\n{out}");
        assert!(!out.contains("info:"), "no empty info section:\n{out}");
        assert!(out.contains("warnings:"));
    }

    #[test]
    fn text_ends_with_a_count_summary_naming_the_checks_run() {
        let out = render_text("demo", &mixed_findings(), CHECKS_RUN, false);
        assert!(
            out.trim_end()
                .ends_with("1 error, 1 warning across 9 checks"),
            "summary line:\n{out}"
        );
    }

    #[test]
    fn text_summary_pluralizes_counts() {
        let findings = vec![
            finding("a", Severity::Error, "m", "r"),
            finding("b", Severity::Error, "m", "r"),
        ];
        let out = render_text("demo", &findings, 1, false);
        assert!(
            out.trim_end()
                .ends_with("2 errors, 0 warnings across 1 check"),
            "summary line:\n{out}"
        );
    }

    #[test]
    fn text_with_no_findings_reports_a_clean_pass() {
        let out = render_text("demo", &[], CHECKS_RUN, false);
        assert!(out.contains("✓ no findings"), "clean report:\n{out}");
        assert!(out.contains("demo"), "names the repo:\n{out}");
        assert!(out.contains("9 checks"), "names the checks run:\n{out}");
    }

    #[test]
    fn text_names_the_repo() {
        let out = render_text("demo-repo", &mixed_findings(), CHECKS_RUN, false);
        assert!(out.contains("demo-repo"), "names the repo:\n{out}");
    }

    #[test]
    fn uncolored_text_has_no_ansi_codes() {
        assert!(!render_text("demo", &mixed_findings(), CHECKS_RUN, false).contains(ESC));
        assert!(!render_text("demo", &[], CHECKS_RUN, false).contains(ESC));
    }

    #[test]
    fn colored_text_styles_the_severity_sections() {
        let out = render_text("demo", &mixed_findings(), CHECKS_RUN, true);
        assert!(out.contains(ESC));
    }
}
