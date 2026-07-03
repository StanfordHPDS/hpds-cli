//! Integration tests for `hpds audit report-github`: flag/environment
//! resolution and the end-to-end path through a shimmed `gh`.
//!
//! These tests NEVER call the real `gh`: the internal `HPDS_GH` override
//! points hpds straight at a shim script (no PATH lookup can ever fall
//! through to a real gh), the same shim also sits first on `PATH` as a
//! second line of defense, and every invocation is logged to `GH_LOG` with
//! list endpoints answered from the recorded fixtures in
//! `tests/fixtures/tool-output/gh/`. GitHub Actions environment variables
//! are cleared (or set) explicitly per test so runs inside real CI stay
//! deterministic.
//!
//! Unix-only: on Windows `Command::new("gh")` resolves only `gh.exe`, so a
//! script shim cannot intercept the call. The bot logic itself is
//! platform-independent and unit-tested in `src/audit/report_github.rs`.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// The fake `gh`: logs `$*` to `GH_LOG`, serves list endpoints from the
/// fixture files named by `GH_PR_COMMENTS`/`GH_ISSUES`, and answers writes
/// with minimal JSON.
const GH_SHIM: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$GH_LOG"
[ "$1" = "api" ] || exit 0
case "$2" in
  repos/acme/demo/issues/7/comments)
    case "$*" in
      *--paginate*) cat "$GH_FIXTURES/${GH_PR_COMMENTS:-pr-comments-none.json}" ;;
      *) echo '{"id": 999}' ;;
    esac ;;
  repos/acme/demo/issues/comments/*) echo '{}' ;;
  "repos/acme/demo/issues?state=open&labels=hpds-audit&per_page=100")
    cat "$GH_FIXTURES/${GH_ISSUES:-pr-comments-none.json}" ;;
  repos/acme/demo/issues) echo '{"number": 42}' ;;
  repos/acme/demo/issues/*/comments) echo '{}' ;;
  repos/acme/demo/issues/*) echo '{}' ;;
  *) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
esac
exit 0
"#;

/// Audit JSON as emitted by `hpds audit --format json`: one error finding.
const AUDIT_JSON: &str = r#"{
  "repo": "demo",
  "findings": [
    {
      "check_id": "dirty-files",
      "severity": "error",
      "message": "2 tracked files have uncommitted changes",
      "remediation": "commit or stash them"
    }
  ],
  "summary": { "errors": 1, "warnings": 0, "infos": 0 }
}"#;

struct Sandbox {
    tmp: tempfile::TempDir,
    path: std::ffi::OsString,
    /// The shim executable itself, wired in via `HPDS_GH`.
    gh: PathBuf,
    log: PathBuf,
    input: PathBuf,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tool-output/gh")
}

fn setup() -> Sandbox {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let shim_dir = tmp.path().join("bin");
    fs::create_dir(&shim_dir).expect("create shim dir");
    let gh = shim_dir.join("gh");
    fs::write(&gh, GH_SHIM).expect("write gh shim");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod gh shim");
    }

    let log = tmp.path().join("gh.log");
    fs::write(&log, "").expect("create gh log");
    let input = tmp.path().join("audit.json");
    fs::write(&input, AUDIT_JSON).expect("write audit json");

    let orig_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(shim_dir).chain(std::env::split_paths(&orig_path)))
            .expect("join PATH");

    Sandbox {
        tmp,
        path,
        gh,
        log,
        input,
    }
}

/// `hpds audit report-github` with the shim on PATH and every GitHub
/// Actions variable cleared; tests opt back in per variable.
fn report_github(sb: &Sandbox) -> Command {
    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    cmd.arg("audit")
        .arg("report-github")
        .current_dir(sb.tmp.path())
        .env("PATH", &sb.path)
        .env("HPDS_GH", &sb.gh)
        .env("GH_LOG", &sb.log)
        .env("GH_FIXTURES", fixtures_dir())
        .env_remove("GITHUB_REPOSITORY")
        .env_remove("GITHUB_EVENT_NAME")
        .env_remove("GITHUB_REF")
        .env_remove("GITHUB_EVENT_PATH");
    cmd
}

fn gh_log(sb: &Sandbox) -> String {
    fs::read_to_string(&sb.log).expect("read gh log")
}

#[test]
fn pr_mode_creates_the_sticky_comment_when_none_exists() {
    let sb = setup();
    report_github(&sb)
        .args(["--input"])
        .arg(&sb.input)
        .args(["--repo", "acme/demo", "--pr", "7", "--mode", "pr"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "posted the audit comment to PR #7",
        ));

    let log = gh_log(&sb);
    assert!(
        log.contains("api repos/acme/demo/issues/7/comments --paginate"),
        "lists existing comments:\n{log}"
    );
    assert!(
        log.contains(
            "api repos/acme/demo/issues/7/comments --method POST -f body=<!-- hpds-audit -->"
        ),
        "creates the marked comment:\n{log}"
    );
    assert!(
        log.contains("dirty-files"),
        "comment body carries the finding:\n{log}"
    );
}

#[test]
fn pr_mode_updates_the_existing_sticky_comment_in_place() {
    let sb = setup();
    report_github(&sb)
        .env("GH_PR_COMMENTS", "pr-comments-sticky.json")
        .args(["--input"])
        .arg(&sb.input)
        .args(["--repo", "acme/demo", "--pr", "7", "--mode", "pr"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "updated the audit comment on PR #7",
        ));

    let log = gh_log(&sb);
    assert!(
        log.contains("api repos/acme/demo/issues/comments/201 --method PATCH"),
        "edits the marked comment, not a new one:\n{log}"
    );
}

#[test]
fn pr_mode_resolves_everything_from_the_actions_environment_and_stdin() {
    let sb = setup();
    report_github(&sb)
        .env("GITHUB_REPOSITORY", "acme/demo")
        .env("GITHUB_EVENT_NAME", "pull_request")
        .env("GITHUB_REF", "refs/pull/7/merge")
        .write_stdin(AUDIT_JSON)
        .assert()
        .success()
        .stdout(predicate::str::contains("PR #7"));
}

#[test]
fn schedule_mode_files_a_labeled_issue_for_a_new_error() {
    let sb = setup();
    report_github(&sb)
        .args(["--input"])
        .arg(&sb.input)
        .args(["--repo", "acme/demo", "--mode", "schedule"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "opened issue #42: hpds audit: dirty-files",
        ));

    let log = gh_log(&sb);
    assert!(
        log.contains("labels[]=hpds-audit"),
        "issue carries the bot label:\n{log}"
    );
    assert!(
        log.contains("<!-- hpds-audit:fingerprint:"),
        "issue body carries the fingerprint marker:\n{log}"
    );
}

#[test]
fn schedule_mode_dedups_open_issues_and_closes_resolved_ones() {
    // The fixture has open issues for dirty-files (#31, still failing)
    // and junk-files (#32, resolved), plus a human issue (#33). The run
    // must file nothing, close #32 with a comment, and spare #33.
    let sb = setup();
    report_github(&sb)
        .env("GH_ISSUES", "issues-open-audit.json")
        .args(["--input"])
        .arg(&sb.input)
        .args(["--repo", "acme/demo", "--mode", "schedule"])
        .assert()
        .success()
        .stdout(predicate::str::contains("closed issue #32"))
        .stdout(predicate::str::contains("opened").not());

    let log = gh_log(&sb);
    assert!(
        log.contains("api repos/acme/demo/issues/32/comments --method POST"),
        "closing comment on #32:\n{log}"
    );
    assert!(
        log.contains("api repos/acme/demo/issues/32 --method PATCH -f state=closed"),
        "closes #32:\n{log}"
    );
    assert!(
        !log.contains("repos/acme/demo/issues --method POST"),
        "no duplicate issue for the still-open finding:\n{log}"
    );
    assert!(
        !log.contains("repos/acme/demo/issues/33"),
        "the human issue is left alone:\n{log}"
    );
}

#[test]
fn gh_runs_the_explicit_override_and_never_falls_through_to_path() {
    // Regression: a test binary was once observed spawning a REAL
    // `gh api repos/acme/demo/issues` child. Every gh spawn must go
    // through the explicit HPDS_GH invoker; a decoy `gh` planted first on
    // PATH stands in for the real one and must never run.
    let sb = setup();
    let decoy_dir = sb.tmp.path().join("decoy-bin");
    fs::create_dir(&decoy_dir).expect("create decoy dir");
    let sentinel = sb.tmp.path().join("real-gh-was-invoked");
    fs::write(
        decoy_dir.join("gh"),
        format!("#!/bin/sh\ntouch \"{}\"\nexit 1\n", sentinel.display()),
    )
    .expect("write decoy gh");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(decoy_dir.join("gh"), fs::Permissions::from_mode(0o755))
            .expect("chmod decoy gh");
    }
    let orig_path = std::env::var_os("PATH").unwrap_or_default();
    let decoy_path =
        std::env::join_paths(std::iter::once(decoy_dir).chain(std::env::split_paths(&orig_path)))
            .expect("join decoy PATH");

    report_github(&sb)
        .env("PATH", &decoy_path)
        .args(["--input"])
        .arg(&sb.input)
        .args(["--repo", "acme/demo", "--mode", "schedule"])
        .assert()
        .success();

    assert!(
        !sentinel.exists(),
        "gh must never be resolved through PATH when HPDS_GH is set"
    );
    let log = gh_log(&sb);
    assert!(
        log.contains("api repos/acme/demo/issues"),
        "the shim handled the gh calls:\n{log}"
    );
}

#[test]
fn missing_repo_context_is_a_usage_error_naming_the_flag() {
    let sb = setup();
    report_github(&sb)
        .args(["--input"])
        .arg(&sb.input)
        .args(["--mode", "schedule"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--repo"));
}

#[test]
fn unparseable_input_says_where_audit_json_comes_from() {
    let sb = setup();
    report_github(&sb)
        .args(["--repo", "acme/demo", "--mode", "pr", "--pr", "7"])
        .write_stdin("this is not json")
        .assert()
        .failure()
        .stderr(predicate::str::contains("hpds audit --format json"));
}
