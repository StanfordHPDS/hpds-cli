//! Integration tests for `hpds audit`: end-to-end wiring of the check
//! framework, the JSON report schema, and exit codes.
//!
//! Every test pins `HPDS_CONFIG_DIR` (internal test override for the user
//! config directory) to an isolated temp dir so the developer's real user
//! config can never leak into assertions.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;

/// A throwaway repo directory plus an isolated user-config directory.
struct Sandbox {
    _root: tempfile::TempDir,
    repo: std::path::PathBuf,
    user_dir: std::path::PathBuf,
}

impl Sandbox {
    /// A real git repo that passes every local check (committed README
    /// with the lab-manual sections, complete `hpds.toml`), plus an empty
    /// user-config dir so the developer's real config never leaks in.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox tempdir");
        let repo = root.path().join("demo-repo");
        let user_dir = root.path().join("user-config");
        fs::create_dir_all(&repo).expect("create repo dir");
        fs::create_dir_all(&user_dir).expect("create user config dir");

        let sandbox = Sandbox {
            _root: root,
            repo,
            user_dir,
        };
        sandbox.git(&["init", "--quiet"]);
        sandbox.write(
            "README.md",
            "# demo\n\n## Description\n\n## File structure\n\n## How to run\n\n## Dependencies\n",
        );
        sandbox.write(
            "hpds.toml",
            "[project]\nstatus = \"active\"\nprimary-author = \"malcolm\"\n",
        );
        sandbox.git(&["add", "-A"]);
        sandbox.git(&["commit", "--quiet", "-m", "initial"]);
        sandbox
    }

    /// Run git in the sandbox repo with an isolated identity/config.
    fn git(&self, args: &[&str]) {
        let excludes = format!(
            "core.excludesFile={}",
            self.repo.join("no-such-excludes").display()
        );
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            // The default excludes file (~/.config/git/ignore) applies even
            // with GIT_CONFIG_GLOBAL unset, so pin it somewhere empty too.
            .args(["-c", &excludes])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", self.repo.join("no-such-global-config"))
            .env("GIT_CONFIG_SYSTEM", self.repo.join("no-such-system-config"))
            .output()
            .expect("run git in sandbox");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Write a file inside the sandbox repo.
    fn write(&self, rel: &str, content: &str) {
        fs::write(self.repo.join(rel), content).expect("write sandbox file");
    }

    /// `hpds audit <args...>` invoked from the sandbox repo.
    fn audit_cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(&self.repo)
            .env("HPDS_CONFIG_DIR", &self.user_dir)
            .arg("audit")
            .args(args);
        cmd
    }
}

#[test]
fn audit_on_a_clean_repo_exits_0_and_reports_no_findings() {
    let sb = Sandbox::new();
    sb.audit_cmd(&[])
        .assert()
        .success()
        .stdout(predicate::str::contains("no findings").and(predicate::str::contains("demo-repo")))
        .stderr(predicate::str::is_empty());
}

#[test]
fn audit_strict_still_exits_0_when_there_are_no_findings() {
    let sb = Sandbox::new();
    sb.audit_cmd(&["--strict"]).assert().success();
}

#[test]
fn audit_json_emits_the_stable_report_schema() {
    let sb = Sandbox::new();
    let assert = sb.audit_cmd(&["--format", "json"]).assert().success();
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");

    // Wire order of the top-level fields is part of the stable schema;
    // parsed `serde_json::Value` maps sort keys, so check the raw text.
    assert!(
        stdout.trim_start().starts_with("{\n  \"repo\""),
        "repo comes first: {stdout}"
    );

    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");
    let object = value.as_object().expect("report is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["findings", "repo", "summary"]);

    assert_eq!(value["repo"], "demo-repo");
    assert_eq!(value["findings"], serde_json::json!([]));
    assert_eq!(
        value["summary"],
        serde_json::json!({ "errors": 0, "warnings": 0, "infos": 0 })
    );
}

#[test]
fn audit_json_stdout_is_pure_json_with_warnings_on_stderr() {
    let sb = Sandbox::new();
    // An unknown config key makes the loader emit a warning; in JSON mode
    // that warning must land on stderr so piped stdout stays parseable.
    sb.write(
        "hpds.toml",
        "[project]\nstatus = \"active\"\nprimary-author = \"malcolm\"\nfuture-key = 1\n",
    );
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "--quiet", "-m", "config with unknown key"]);

    let assert = sb.audit_cmd(&["--format", "json"]).assert().success();
    let output = assert.get_output();

    // The entire piped stdout must be one JSON document — nothing before,
    // nothing after (a trailing newline aside).
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout should be UTF-8");
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("entire stdout parses as one JSON document");
    assert_eq!(value["repo"], "demo-repo");
    assert_eq!(value["summary"]["errors"], 0);

    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr should be UTF-8");
    assert!(
        stderr.contains("warning:") && stderr.contains("future-key"),
        "config warning goes to stderr: {stderr}"
    );
}

#[test]
fn audit_reports_error_findings_and_exits_1() {
    let sb = Sandbox::new();
    sb.write(".env", "SECRET=hunter2\n");
    sb.git(&["add", "-f", ".env"]);
    sb.git(&["commit", "--quiet", "-m", "oops"]);
    sb.audit_cmd(&[])
        .assert()
        .code(1)
        .stdout(
            predicate::str::contains("junk-files")
                .and(predicate::str::contains(".env"))
                .and(predicate::str::contains("fix:")),
        )
        .stderr(predicate::str::contains("error:"));
}

#[test]
fn audit_warnings_exit_0_normally_and_1_under_strict() {
    let sb = Sandbox::new();
    // A modified tracked file is a warning-severity finding.
    sb.write("README.md", "# demo (edited)\n");
    sb.audit_cmd(&[])
        .assert()
        .success()
        .stdout(predicate::str::contains("dirty-files"));
    sb.audit_cmd(&["--strict"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--strict"));
}

#[test]
fn audit_from_a_subdirectory_audits_the_repo_root() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.repo.join("analysis")).expect("create subdir");
    sb.write("analysis/notes.txt", "n\n");
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "--quiet", "-m", "add analysis"]);

    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    cmd.current_dir(sb.repo.join("analysis"))
        .env("HPDS_CONFIG_DIR", &sb.user_dir)
        .arg("audit");
    // The README and hpds.toml live at the root; auditing from a subdir
    // must still see them (and name the report after the root).
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("no findings").and(predicate::str::contains("demo-repo")));
}

#[test]
fn audit_reads_the_project_config() {
    // A broken hpds.toml must surface as a clean error, proving the audit
    // actually loads the layered config rather than ignoring it.
    let sb = Sandbox::new();
    fs::write(sb.repo.join("hpds.toml"), "not valid toml [").expect("write hpds.toml");
    sb.audit_cmd(&[])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("hpds.toml")));
}

#[test]
fn audit_outside_a_git_repo_reports_one_not_a_repo_error() {
    // Regression: every git-backed check used to report its own
    // "could not inspect the repo" error — seven near-identical findings.
    // Outside a repo the report must carry exactly one Error saying so.
    let root = tempfile::tempdir().expect("create tempdir");
    let dir = root.path().join("plain-dir");
    let user_dir = root.path().join("user-config");
    fs::create_dir_all(&dir).expect("create plain dir");
    fs::create_dir_all(&user_dir).expect("create user config dir");

    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    let assert = cmd
        .current_dir(&dir)
        .env("HPDS_CONFIG_DIR", &user_dir)
        .arg("audit")
        .assert()
        .code(1);
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");
    assert_eq!(
        stdout.matches("not a git repository").count(),
        1,
        "exactly one not-a-repo finding:\n{stdout}"
    );
    assert!(
        stdout.contains("git init"),
        "remediation points at git init:\n{stdout}"
    );
    assert!(
        !stdout.contains("could not inspect the repo"),
        "the git-backed checks are skipped, not failed:\n{stdout}"
    );
}

#[test]
fn audit_outside_a_git_repo_still_runs_the_git_free_checks() {
    // The checks that never touch git (README, lifecycle metadata) still
    // inspect the directory.
    let root = tempfile::tempdir().expect("create tempdir");
    let dir = root.path().join("plain-dir");
    let user_dir = root.path().join("user-config");
    fs::create_dir_all(&dir).expect("create plain dir");
    fs::create_dir_all(&user_dir).expect("create user config dir");

    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    cmd.current_dir(&dir)
        .env("HPDS_CONFIG_DIR", &user_dir)
        .arg("audit")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("readme").and(predicate::str::contains("lifecycle")));
}

#[test]
fn audit_rejects_an_unknown_format_value() {
    let sb = Sandbox::new();
    sb.audit_cmd(&["--format", "yaml"]).assert().code(2);
}

#[test]
fn audit_help_documents_format_and_strict() {
    Command::cargo_bin("hpds")
        .expect("hpds binary should build")
        .args(["audit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--format").and(predicate::str::contains("--strict")));
}
