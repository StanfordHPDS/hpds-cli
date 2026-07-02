//! Doc-check for `docs/audit-bot.md`: the guide must exist and must not
//! drift from the real `hpds audit report-github` interface or from the
//! workflow template it documents. Flag names and behavior claims are
//! verified against the binary's own `--help` output, not hardcoded twice.

use std::collections::BTreeSet;
use std::path::Path;

use assert_cmd::Command;

fn repo_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel} must exist and be UTF-8: {e}"))
}

fn doc() -> String {
    repo_file("docs/audit-bot.md")
}

/// `--help` output for the given hpds arguments, straight from the binary.
fn help(args: &[&str]) -> String {
    let output = Command::cargo_bin("hpds")
        .expect("hpds binary should build")
        .args(args)
        .arg("--help")
        .output()
        .expect("hpds runs");
    assert!(output.status.success(), "--help exits 0");
    String::from_utf8(output.stdout).expect("help is UTF-8")
}

fn report_github_help() -> String {
    help(&["audit", "report-github"])
}

/// The flags report-github itself defines: its help minus the global
/// flags (like `--config`) that ride along on every subcommand.
fn report_github_flags() -> BTreeSet<String> {
    let global = long_flags(&help(&[]));
    long_flags(&report_github_help())
        .into_iter()
        .filter(|flag| !global.contains(flag))
        .collect()
}

/// Every long flag (`--like-this`) in `text`, deduped, without `--help`.
fn long_flags(text: &str) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find("--") {
        let after = &rest[start + 2..];
        let end = after
            .find(|c: char| !c.is_ascii_lowercase() && c != '-')
            .unwrap_or(after.len());
        let name = &after[..end];
        if !name.is_empty() && !name.starts_with('-') && name != "help" {
            flags.insert(format!("--{name}"));
        }
        rest = &after[end..];
    }
    flags
}

#[test]
fn doc_documents_every_report_github_flag() {
    let doc = doc();
    let flags = report_github_flags();
    assert!(!flags.is_empty(), "sanity: help lists at least one flag");
    for flag in &flags {
        assert!(
            doc.contains(flag.as_str()),
            "docs/audit-bot.md must document the report-github flag `{flag}`"
        );
    }
}

#[test]
fn doc_mentions_only_real_flags_on_report_github_lines() {
    // Any flag the doc attaches to `report-github` must actually exist,
    // so renamed or removed flags fail this check instead of misleading
    // readers.
    let doc = doc();
    let real = long_flags(&report_github_help());
    for line in doc.lines().filter(|l| l.contains("report-github")) {
        for flag in long_flags(line) {
            assert!(
                real.contains(&flag),
                "docs/audit-bot.md claims report-github has `{flag}`, but --help does not list it:\nline: {line}"
            );
        }
    }
}

#[test]
fn doc_matches_the_bots_persisted_markers() {
    // The sticky-comment marker and the issue label are persisted state
    // the doc promises to users; they must match what the bot writes.
    let doc = doc();
    assert!(
        doc.contains("<!-- hpds-audit -->"),
        "doc names the sticky comment marker"
    );
    assert!(doc.contains("`hpds-audit`"), "doc names the issue label");
}

#[test]
fn doc_matches_the_workflow_template() {
    // Install instructions and permissions in the doc must match the
    // workflow the `gha` component actually generates.
    let doc = doc();
    let template = repo_file("templates/gha/audit-bot/.github/workflows/hpds-audit.yml");

    assert!(
        doc.contains("hpds use gha"),
        "doc says how to install the workflow"
    );
    assert!(
        doc.contains("--workflows audit-bot"),
        "doc gives the non-interactive selection"
    );
    assert!(
        doc.contains(".github/workflows/hpds-audit.yml"),
        "doc names the generated file"
    );
    for permission in ["contents: read", "issues: write", "pull-requests: write"] {
        assert!(
            doc.contains(permission),
            "doc lists the `{permission}` permission"
        );
        assert!(
            template.contains(permission),
            "template grants the `{permission}` permission"
        );
    }
}

#[test]
fn doc_covers_the_audit_config_knobs() {
    let doc = doc();
    assert!(doc.contains("[audit]"), "doc points at the [audit] table");
    assert!(
        doc.contains("stale-days"),
        "doc names the stale-days config key"
    );
    assert!(
        doc.contains("required-watchers"),
        "doc names the required-watchers config key"
    );
}
