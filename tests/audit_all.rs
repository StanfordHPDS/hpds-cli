//! Integration tests for `hpds audit all`, driven through the
//! `--repos-from` seam: local fixture repos (one clean, one messy) are
//! cloned and audited exactly like org repos, without touching the
//! network or `gh`.
//!
//! Every test pins `HPDS_CONFIG_DIR` (internal test override for the user
//! config directory) to an isolated temp dir so the developer's real user
//! config can never leak into assertions.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

/// A run directory (cwd for the sweep), an isolated user-config dir, and
/// two fixture repos: `clean-repo` passes every local check and `messy-repo`
/// has a committed `.env` plus other audit findings. Every sweep runs with
/// `HPDS_GH` pointed at a shim that reports an authenticated session, so
/// no test outcome can depend on the developer's real gh login.
struct OrgSandbox {
    _root: tempfile::TempDir,
    run_dir: PathBuf,
    user_dir: PathBuf,
    clean: PathBuf,
    messy: PathBuf,
    gh: PathBuf,
}

impl OrgSandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox tempdir");
        let run_dir = root.path().join("run");
        let user_dir = root.path().join("user-config");
        let clean = root.path().join("clean-repo");
        let messy = root.path().join("messy-repo");
        for dir in [&run_dir, &user_dir, &clean, &messy] {
            fs::create_dir_all(dir).expect("create sandbox dir");
        }

        let gh = write_gh_shim(root.path());
        let sandbox = OrgSandbox {
            _root: root,
            run_dir,
            user_dir,
            clean,
            messy,
            gh,
        };

        sandbox.git(&sandbox.clean, &["init", "--quiet"]);
        write(&sandbox.clean, "README.md", "# demo\n");
        write(
            &sandbox.clean,
            "hpds.toml",
            "[project]\nstatus = \"active\"\nprimary-author = \"malcolm\"\n",
        );
        sandbox.git(&sandbox.clean, &["add", "-A"]);
        sandbox.git(&sandbox.clean, &["commit", "--quiet", "-m", "initial"]);

        sandbox.git(&sandbox.messy, &["init", "--quiet"]);
        write(&sandbox.messy, "README.md", "# messy\n");
        write(&sandbox.messy, ".env", "SECRET=hunter2\n");
        sandbox.git(&sandbox.messy, &["add", "-f", "-A"]);
        sandbox.git(&sandbox.messy, &["commit", "--quiet", "-m", "oops"]);

        sandbox
    }

    /// Run git in a fixture repo with an isolated identity/config.
    fn git(&self, repo: &Path, args: &[&str]) {
        let excludes = format!(
            "core.excludesFile={}",
            repo.join("no-such-excludes").display()
        );
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", &excludes])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", repo.join("no-such-global-config"))
            .env("GIT_CONFIG_SYSTEM", repo.join("no-such-system-config"))
            .output()
            .expect("run git in sandbox");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Write a `--repos-from` file listing the given entries.
    fn repos_file(&self, entries: &[&str]) -> PathBuf {
        let path = self.run_dir.join("repos.txt");
        fs::write(&path, entries.join("\n") + "\n").expect("write repos file");
        path
    }

    /// `hpds audit all --repos-from <file> <args...>` run from `run_dir`.
    fn sweep_cmd(&self, repos_file: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(&self.run_dir)
            .env("HPDS_CONFIG_DIR", &self.user_dir)
            .env("HPDS_GH", &self.gh)
            .args(["audit", "all", "--repos-from"])
            .arg(repos_file)
            .args(args);
        cmd
    }

    fn clean_line(&self) -> String {
        self.clean.display().to_string()
    }

    fn messy_line(&self) -> String {
        self.messy.display().to_string()
    }
}

fn write(repo: &Path, rel: &str, content: &str) {
    fs::write(repo.join(rel), content).expect("write fixture file");
}

/// A fake `gh` that answers every invocation (including `auth status`)
/// with success, i.e. an authenticated session.
#[cfg(unix)]
fn write_gh_shim(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let gh = dir.join("gh");
    fs::write(&gh, "#!/bin/sh\nexit 0\n").expect("write gh shim");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("make gh shim executable");
    gh
}

/// A fake `gh` that answers every invocation (including `auth status`)
/// with success, i.e. an authenticated session.
#[cfg(windows)]
fn write_gh_shim(dir: &Path) -> PathBuf {
    let gh = dir.join("gh.bat");
    fs::write(&gh, "@echo off\r\nexit /b 0\r\n").expect("write gh shim");
    gh
}

#[test]
fn sweep_table_reports_both_repos_and_exits_1_on_errors() {
    let sb = OrgSandbox::new();
    let file = sb.repos_file(&[&sb.messy_line(), &sb.clean_line()]);
    sb.sweep_cmd(&file, &[])
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("repo")
                .and(predicate::str::contains("errors"))
                .and(predicate::str::contains("warnings"))
                .and(predicate::str::contains("messy-repo"))
                .and(predicate::str::contains("clean-repo"))
                .and(predicate::str::contains("1 with errors")),
        )
        .stderr(predicate::str::contains("error:"));
}

#[test]
fn sweep_writes_a_markdown_report_with_per_repo_sections() {
    let sb = OrgSandbox::new();
    let file = sb.repos_file(&[&sb.messy_line(), &sb.clean_line()]);
    sb.sweep_cmd(&file, &[]).assert().code(1);

    let report = fs::read_to_string(sb.run_dir.join("hpds-audit-report.md"))
        .expect("default markdown report is written to the cwd");
    assert!(report.starts_with("# hpds audit report"), "{report}");
    assert!(report.contains("## messy-repo"), "{report}");
    assert!(report.contains("## clean-repo"), "{report}");
    assert!(report.contains("junk-files"), "{report}");
    assert!(report.contains("fix:"), "remediation present: {report}");
    assert!(report.contains("No findings."), "{report}");
}

#[test]
fn sweep_json_emits_the_stable_schema() {
    let sb = OrgSandbox::new();
    let file = sb.repos_file(&[&sb.messy_line(), &sb.clean_line()]);
    let assert = sb.sweep_cmd(&file, &["--format", "json"]).assert().code(1);
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");

    // Wire order of the top-level fields is part of the stable schema.
    assert!(
        stdout.trim_start().starts_with("{\n  \"org\""),
        "org comes first: {stdout}"
    );

    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");
    let object = value.as_object().expect("sweep report is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["org", "repos", "summary"]);

    // --repos-from bypasses org enumeration, so no org is claimed.
    assert_eq!(value["org"], serde_json::Value::Null);

    let repos = value["repos"].as_array().expect("repos is an array");
    assert_eq!(repos.len(), 2, "file order and count preserved");
    assert_eq!(repos[0]["repo"], "messy-repo");
    assert_eq!(repos[1]["repo"], "clean-repo");
    let messy_checks: Vec<&str> = repos[0]["findings"]
        .as_array()
        .expect("findings is an array")
        .iter()
        .map(|f| f["check_id"].as_str().expect("check_id is a string"))
        .collect();
    assert!(messy_checks.contains(&"junk-files"), "{messy_checks:?}");
    assert_eq!(repos[1]["findings"], serde_json::json!([]));
    assert_eq!(
        repos[1]["summary"],
        serde_json::json!({ "errors": 0, "warnings": 0, "infos": 0 })
    );

    assert_eq!(value["summary"]["repos"], 2);
    assert_eq!(value["summary"]["audited"], 2);
    assert_eq!(value["summary"]["failed"], 0);
    assert!(
        value["summary"]["errors"].as_u64().expect("errors count") >= 1,
        "messy repo contributes errors: {}",
        value["summary"]
    );
}

#[test]
fn sweep_reports_render_in_input_order() {
    let sb = OrgSandbox::new();
    let bogus = sb._root.path().join("no-such-repo").display().to_string();
    let file = sb.repos_file(&[&sb.messy_line(), &sb.clean_line(), &bogus]);
    let assert = sb.sweep_cmd(&file, &[]).assert().code(1);
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");

    // The table lists one row per repo, in the order the repos file gave
    // them, regardless of how the audits were scheduled.
    let position = |haystack: &str, needle: &str| {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{haystack}"))
    };
    let messy = position(&stdout, "messy-repo");
    let clean = position(&stdout, "clean-repo");
    let failed = position(&stdout, "no-such-repo");
    assert!(
        messy < clean && clean < failed,
        "table rows out of input order:\n{stdout}"
    );

    // The markdown sections keep the same order.
    let report = fs::read_to_string(sb.run_dir.join("hpds-audit-report.md"))
        .expect("markdown report is written");
    let messy = position(&report, "## messy-repo");
    let clean = position(&report, "## clean-repo");
    let failed = position(&report, "## no-such-repo");
    assert!(
        messy < clean && clean < failed,
        "markdown sections out of input order:\n{report}"
    );
}

#[test]
fn sweep_of_only_clean_repos_exits_0() {
    let sb = OrgSandbox::new();
    let file = sb.repos_file(&[&sb.clean_line()]);
    sb.sweep_cmd(&file, &[])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 repo audited: no errors"));
}

#[test]
fn a_repo_that_fails_to_clone_is_reported_and_does_not_abort_the_sweep() {
    let sb = OrgSandbox::new();
    let bogus = sb._root.path().join("no-such-repo").display().to_string();
    let file = sb.repos_file(&[&bogus, &sb.clean_line()]);
    sb.sweep_cmd(&file, &[]).assert().code(1).stdout(
        predicate::str::contains("failed:")
            .and(predicate::str::contains("no-such-repo"))
            // The clean repo after the failure is still audited.
            .and(predicate::str::contains("clean-repo"))
            .and(predicate::str::contains("1 failed to audit")),
    );

    let report = fs::read_to_string(sb.run_dir.join("hpds-audit-report.md"))
        .expect("markdown report still written");
    assert!(report.contains("Could not audit:"), "{report}");
    assert!(report.contains("No findings."), "{report}");
}

#[test]
fn sweep_respects_the_output_flag() {
    let sb = OrgSandbox::new();
    let file = sb.repos_file(&[&sb.clean_line()]);
    // The confirmation echoes the path exactly as the user spelled it,
    // forward slashes included, on every platform.
    sb.sweep_cmd(&file, &["--output", "reports/org.md"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reports/org.md"));
    assert!(sb.run_dir.join("reports/org.md").is_file());
    assert!(!sb.run_dir.join("hpds-audit-report.md").exists());
}

#[test]
fn sweep_with_a_missing_repos_from_file_is_a_clean_error() {
    let sb = OrgSandbox::new();
    let missing = sb.run_dir.join("nowhere.txt");
    sb.sweep_cmd(&missing, &[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("repos")));
}

#[test]
fn no_clone_rejects_local_paths_per_repo_without_aborting() {
    let sb = OrgSandbox::new();
    let file = sb.repos_file(&[&sb.clean_line()]);
    // A local path cannot be metadata-audited; it is reported per-repo,
    // and with no auditable repos left the sweep fails rather than
    // pretending the org is healthy.
    sb.sweep_cmd(&file, &["--no-clone"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("--no-clone"));
}

#[test]
fn audit_all_help_documents_the_sweep_flags() {
    Command::cargo_bin("hpds")
        .expect("hpds binary should build")
        .args(["audit", "all", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--org")
                .and(predicate::str::contains("--limit"))
                .and(predicate::str::contains("--no-clone"))
                .and(predicate::str::contains("--repos-from"))
                .and(predicate::str::contains("--output"))
                .and(predicate::str::contains("--format")),
        );
}

/// Online end-to-end sweep against the real org. Needs network plus an
/// authenticated `gh`, so it is opt-in twice over:
/// `cargo test --features online-tests -- --ignored`.
#[cfg(feature = "online-tests")]
#[test]
#[ignore = "network + authenticated gh required; run with --features online-tests -- --ignored"]
fn online_sweep_audits_two_real_org_repos() {
    let sb = OrgSandbox::new();
    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    let assert = cmd
        .current_dir(&sb.run_dir)
        .env("HPDS_CONFIG_DIR", &sb.user_dir)
        .args(["audit", "all", "--limit", "2", "--format", "json"])
        .timeout(std::time::Duration::from_secs(300))
        .assert()
        // Real repos may or may not have error findings; both exits are
        // legitimate. Anything else (2 = usage error, panic) is a bug.
        .code(predicate::in_iter([0, 1]));

    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");
    assert_eq!(value["org"], "StanfordHPDS");
    let repos = value["repos"].as_array().expect("repos is an array");
    assert!(
        !repos.is_empty() && repos.len() <= 2,
        "respects --limit 2: {}",
        repos.len()
    );
    assert_eq!(value["summary"]["repos"], repos.len());
    assert!(sb.run_dir.join("hpds-audit-report.md").is_file());
}
