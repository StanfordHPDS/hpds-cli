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
    /// Repo dir containing a `.git` marker (so config discovery never walks
    /// out of the sandbox) and an empty user-config dir.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox tempdir");
        let repo = root.path().join("demo-repo");
        let user_dir = root.path().join("user-config");
        fs::create_dir_all(repo.join(".git")).expect("create repo/.git");
        fs::create_dir_all(&user_dir).expect("create user config dir");
        Sandbox {
            _root: root,
            repo,
            user_dir,
        }
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
