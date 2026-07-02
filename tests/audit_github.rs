//! Integration tests for the GitHub side of `hpds audit`: the auth-gated
//! skip notice and the end-to-end path through a shimmed `gh` that serves
//! the recorded fixtures from `tests/fixtures/tool-output/gh/`.
//!
//! These tests NEVER call the real `gh`: a shim script earlier on `PATH`
//! answers `gh auth status` per `GH_AUTH_EXIT` and `gh api <endpoint>` from
//! fixture files in `GH_FIXTURES`. Real `git` runs only inside temp repos,
//! isolated via `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`.
//!
//! Unix-only: on Windows `Command::new("gh")` resolves only `gh.exe`, so a
//! script shim cannot intercept the call. The check logic itself is
//! platform-independent and unit-tested in `src/audit/github/`.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// The fake `gh`: `auth status` obeys `GH_AUTH_EXIT`; `api <endpoint>`
/// serves the recorded fixture matching the endpoint.
const GH_SHIM: &str = r#"#!/bin/sh
case "$1" in
  auth)
    exit "${GH_AUTH_EXIT:-0}"
    ;;
  api)
    case "$2" in
      repos/acme/demo/subscribers)              cat "$GH_FIXTURES/subscribers.json" ;;
      repos/acme/demo/contributors)             cat "$GH_FIXTURES/contributors.json" ;;
      orgs/acme/members)                        cat "$GH_FIXTURES/org-members.json" ;;
      repos/acme/demo/branches/old-analysis)    cat "$GH_FIXTURES/branch-old.json" ;;
      repos/acme/demo/branches/fresh-idea)      cat "$GH_FIXTURES/branch-fresh.json" ;;
      repos/acme/demo/branches)                 cat "$GH_FIXTURES/branches.json" ;;
      repos/acme/demo/compare/main...old-analysis) cat "$GH_FIXTURES/compare-ahead.json" ;;
      repos/acme/demo/compare/*...main)         cat "$GH_FIXTURES/compare-identical.json" ;;
      repos/acme/demo/releases)                 cat "$GH_FIXTURES/releases-empty.json" ;;
      repos/acme/demo)                          cat "$GH_FIXTURES/repo.json" ;;
      *)                                        echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
    esac
    ;;
esac
exit 0
"#;

struct Sandbox {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    user_dir: PathBuf,
    gitconfig: PathBuf,
    path: std::ffi::OsString,
}

/// A temp git repo with one commit on `main` and an `origin` remote
/// pointing at github.com, plus the `gh` shim first on PATH. The committed
/// README and hpds.toml satisfy every local check, so tests observe the
/// GitHub side without local-check noise (`malcolmbarrett` is in both the
/// subscribers and contributors fixtures, keeping those checks green too).
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

    let gitconfig = tmp.path().join("gitconfig");
    fs::write(
        &gitconfig,
        "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n",
    )
    .expect("write test gitconfig");

    let repo = tmp.path().join("demo");
    fs::create_dir(&repo).expect("create repo dir");
    let user_dir = tmp.path().join("user-config");
    fs::create_dir(&user_dir).expect("create user config dir");

    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init"]);
    fs::write(
        repo.join("README.md"),
        "# demo\n\n## Description\n\n## File structure\n\n## How to run\n\n## Dependencies\n",
    )
    .expect("write README");
    fs::write(
        repo.join("hpds.toml"),
        "[project]\nstatus = \"active\"\nprimary-author = \"malcolmbarrett\"\n",
    )
    .expect("write hpds.toml");
    git(&["add", "."]);
    git(&["commit", "-m", "Initial commit"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/demo.git",
    ]);

    let orig_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(shim_dir).chain(std::env::split_paths(&orig_path)))
            .expect("join PATH");

    Sandbox {
        _tmp: tmp,
        repo,
        user_dir,
        gitconfig,
        path,
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tool-output/gh")
}

fn audit(sb: &Sandbox) -> Command {
    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    cmd.current_dir(&sb.repo)
        .env("PATH", &sb.path)
        .env("HPDS_CONFIG_DIR", &sb.user_dir)
        .env("GH_FIXTURES", fixtures_dir())
        .env("GIT_CONFIG_GLOBAL", &sb.gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("audit");
    cmd
}

#[test]
fn unauthenticated_gh_skips_github_checks_with_the_documented_notice() {
    let sb = setup();
    audit(&sb)
        .env("GH_AUTH_EXIT", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "GitHub checks skipped: gh not authenticated",
        ));
}

#[test]
fn unauthenticated_skip_notice_is_info_severity_in_json() {
    let sb = setup();
    let assert = audit(&sb)
        .env("GH_AUTH_EXIT", "1")
        .args(["--format", "json"])
        .assert()
        .success();
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    let notice = report["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .find(|f| f["check_id"] == "github")
        .expect("skip notice finding")
        .clone();
    assert_eq!(notice["severity"], "info");
    assert_eq!(
        notice["message"],
        "GitHub checks skipped: gh not authenticated"
    );
}

#[test]
fn repo_without_a_github_remote_runs_no_github_checks_and_stays_clean() {
    let sb = setup();
    let out = std::process::Command::new("git")
        .args(["remote", "remove", "origin"])
        .current_dir(&sb.repo)
        .output()
        .expect("run git");
    assert!(out.status.success());

    audit(&sb)
        .assert()
        .success()
        .stdout(predicate::str::contains("no findings"));
}

#[test]
fn project_config_cannot_change_the_required_watcher_list() {
    // The audited repo must not be able to rewrite the watcher requirement
    // for whoever audits it, so the key is honored only from user config.
    // If the project-layer key were honored, `ghost-watcher` would show up
    // in a watchers finding; instead the key is ignored with a warning and
    // the default lab leads (who ARE in subscribers.json) keep the check
    // green.
    let sb = setup();
    fs::write(
        sb.repo.join("hpds.toml"),
        "[project]\nstatus = \"active\"\nprimary-author = \"malcolmbarrett\"\n\n\
         [audit]\nrequired-watchers = [\"ghost-watcher\"]\n",
    )
    .expect("write hpds.toml");

    audit(&sb)
        .assert()
        .success()
        .stdout(predicate::str::contains("ghost-watcher").not())
        .stderr(
            predicate::str::contains("warning:")
                .and(predicate::str::contains("audit.required-watchers")),
        );
}

#[test]
fn user_config_required_watchers_reach_the_watchers_check() {
    let sb = setup();
    fs::write(
        sb.user_dir.join("config.toml"),
        "[audit]\nrequired-watchers = [\"ghost-watcher\"]\n",
    )
    .expect("write user config.toml");

    audit(&sb).assert().success().stdout(
        predicate::str::contains("ghost-watcher")
            .and(predicate::str::contains("not watching the repo")),
    );
}

#[test]
fn authenticated_audit_reports_github_findings_end_to_end() {
    let sb = setup();
    // submitted + releases-empty.json makes the releases check an Error,
    // so the audit exits 1; researcher1 is not in subscribers.json, so the
    // watchers check warns; branches/compare fixtures make old-analysis a
    // stale unmerged branch.
    fs::write(
        sb.repo.join("hpds.toml"),
        "[project]\nstatus = \"submitted\"\nprimary-author = \"researcher1\"\n",
    )
    .expect("write hpds.toml");

    let assert = audit(&sb).args(["--format", "json"]).assert().code(1);
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    let findings = report["findings"].as_array().expect("findings array");

    let by_check = |id: &str| -> Vec<&serde_json::Value> {
        findings.iter().filter(|f| f["check_id"] == id).collect()
    };

    let watchers = by_check("watchers");
    assert_eq!(watchers.len(), 1, "watchers findings: {findings:?}");
    assert!(
        watchers[0]["message"]
            .as_str()
            .unwrap()
            .contains("researcher1")
    );

    let releases = by_check("releases");
    assert_eq!(releases.len(), 1, "releases findings: {findings:?}");
    assert_eq!(releases[0]["severity"], "error");

    let stale = by_check("stale-remote-branches");
    assert_eq!(stale.len(), 1, "stale branch findings: {findings:?}");
    assert!(
        stale[0]["message"]
            .as_str()
            .unwrap()
            .contains("old-analysis")
    );

    // The shim answers the local-sha compare with `identical`, so the
    // default branch is in sync; contributors and lifecycle are consistent
    // in the fixtures too, and there must be no skip notice.
    assert!(
        by_check("default-branch-staleness").is_empty(),
        "{findings:?}"
    );
    assert!(by_check("contributors").is_empty(), "{findings:?}");
    assert!(by_check("lifecycle-consistency").is_empty(), "{findings:?}");
    assert!(by_check("github").is_empty(), "{findings:?}");
}
