//! Integration tests for `hpds config`: discovery, layering,
//! unknown-key warnings, `--config`, and `--format json`.
//!
//! Every test pins `HPDS_CONFIG_DIR` (internal test override for the user
//! config directory) to an isolated temp dir so the developer's real user
//! config can never leak into assertions.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

/// A throwaway project directory plus an isolated user-config directory.
struct Sandbox {
    _root: tempfile::TempDir,
    project: std::path::PathBuf,
    user_dir: std::path::PathBuf,
}

impl Sandbox {
    /// Project dir containing a `.git` marker (so discovery never walks out
    /// of the sandbox) and an empty user-config dir.
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create sandbox tempdir");
        let project = root.path().join("project");
        let user_dir = root.path().join("user-config");
        fs::create_dir_all(project.join(".git")).expect("create project/.git");
        fs::create_dir_all(&user_dir).expect("create user config dir");
        Sandbox {
            _root: root,
            project,
            user_dir,
        }
    }

    fn write_project_config(&self, contents: &str) {
        fs::write(self.project.join("hpds.toml"), contents).expect("write hpds.toml");
    }

    fn write_user_config(&self, contents: &str) {
        fs::write(self.user_dir.join("config.toml"), contents).expect("write user config.toml");
    }

    /// `hpds config` invoked from `dir` with the sandboxed user config.
    fn config_cmd_in(&self, dir: &Path) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(dir)
            .env("HPDS_CONFIG_DIR", &self.user_dir)
            .arg("config");
        cmd
    }

    fn config_cmd(&self) -> Command {
        self.config_cmd_in(&self.project)
    }
}

#[test]
fn config_with_no_files_prints_builtin_defaults() {
    let sb = Sandbox::new();
    sb.config_cmd()
        .assert()
        .success()
        .stdout(
            predicate::str::contains("built-in defaults")
                .and(predicate::str::contains(r#"status = "active""#))
                .and(predicate::str::contains("stale-days = 90")),
        )
        .stderr(predicate::str::is_empty());
}

#[test]
fn config_discovers_project_file_walking_up_from_subdirectory() {
    let sb = Sandbox::new();
    sb.write_project_config("[audit]\nstale-days = 30\n");
    let nested = sb.project.join("analysis").join("deep");
    fs::create_dir_all(&nested).expect("create nested dirs");

    sb.config_cmd_in(&nested).assert().success().stdout(
        predicate::str::contains("stale-days = 30").and(predicate::str::contains("hpds.toml")),
    );
}

#[test]
fn discovery_stops_at_git_root() {
    // hpds.toml above the git root must NOT be picked up.
    let sb = Sandbox::new();
    fs::write(
        sb.project.parent().unwrap().join("hpds.toml"),
        "[audit]\nstale-days = 30\n",
    )
    .expect("write outer hpds.toml");

    sb.config_cmd()
        .assert()
        .success()
        .stdout(predicate::str::contains("stale-days = 90"));
}

#[test]
fn user_config_overrides_defaults_and_project_overrides_user() {
    let sb = Sandbox::new();
    sb.write_user_config("[audit]\nstale-days = 30\n[project]\nstatus = \"submitted\"\n");
    sb.write_project_config("[audit]\nstale-days = 45\n");

    sb.config_cmd().assert().success().stdout(
        // project wins for audit.stale-days; user's project.status survives.
        predicate::str::contains("stale-days = 45")
            .and(predicate::str::contains(r#"status = "submitted""#)),
    );
}

#[test]
fn explicit_config_flag_overrides_discovery() {
    let sb = Sandbox::new();
    sb.write_project_config("[project]\nstatus = \"submitted\"\n");
    let other = sb.project.join("other.toml");
    fs::write(&other, "[project]\nstatus = \"published\"\n").expect("write other.toml");

    let mut cmd = sb.config_cmd();
    cmd.arg("--config").arg(&other);
    cmd.assert().success().stdout(
        predicate::str::contains(r#"status = "published""#)
            .and(predicate::str::contains("other.toml")),
    );
}

#[test]
fn explicit_config_flag_with_missing_file_is_a_usage_error() {
    // A bad `--config` value is a usage error (exit 2), like any other
    // bad flag value — not a runtime failure.
    let sb = Sandbox::new();
    let mut cmd = sb.config_cmd();
    cmd.arg("--config").arg("no-such-file.toml");
    cmd.assert().failure().code(2).stderr(
        predicate::str::contains("error:")
            .and(predicate::str::contains("no-such-file.toml"))
            .and(predicate::str::contains("hint:"))
            .and(predicate::str::contains("--config")),
    );
}

#[test]
fn unknown_keys_warn_but_do_not_error() {
    let sb = Sandbox::new();
    sb.write_project_config(
        "typo-section = 1\n[project]\nstatus = \"active\"\nfrobnicate = true\n",
    );

    sb.config_cmd()
        .assert()
        .success()
        .stdout(predicate::str::contains(r#"status = "active""#))
        .stderr(
            predicate::str::contains("warning:")
                .and(predicate::str::contains("typo-section"))
                .and(predicate::str::contains("project.frobnicate"))
                .and(predicate::str::contains("hpds.toml")),
        );
}

#[test]
fn removed_formatting_sections_warn_as_unknown_keys() {
    // `[format]`, `[lint]`, `[sql]`, and `[tools]` belong to togi now; an
    // hpds.toml still carrying them loads fine, with a warning per table.
    let sb = Sandbox::new();
    sb.write_project_config(
        "[project]\nstatus = \"active\"\n\n[format]\nlanguages = [\"r\"]\n\n[lint]\nexclude = []\n\n[sql]\ndialect = \"bigquery\"\n\n[tools]\nair = \"0.10.0\"\n",
    );

    sb.config_cmd()
        .assert()
        .success()
        .stdout(predicate::str::contains(r#"status = "active""#))
        .stderr(
            predicate::str::contains("warning:")
                .and(predicate::str::contains("format"))
                .and(predicate::str::contains("lint"))
                .and(predicate::str::contains("sql"))
                .and(predicate::str::contains("tools")),
        );
}

#[test]
fn project_config_cannot_set_audit_required_watchers() {
    // `[audit].required-watchers` is honored only from *user* config: a
    // repo must not be able to exempt itself from the lab-lead watcher
    // requirement for everyone who audits it.
    let sb = Sandbox::new();
    sb.write_project_config("[audit]\nrequired-watchers = []\n");

    sb.config_cmd().assert().success().stderr(
        predicate::str::contains("warning:")
            .and(predicate::str::contains("audit.required-watchers"))
            .and(predicate::str::contains("hpds.toml"))
            .and(predicate::str::contains("user config")),
    );
}

#[test]
fn user_config_may_set_audit_required_watchers_without_warning() {
    let sb = Sandbox::new();
    sb.write_user_config("[audit]\nrequired-watchers = [\"lead1\"]\n");

    sb.config_cmd()
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn project_config_may_still_set_audit_stale_days() {
    // Only required-watchers is user-only; the staleness threshold is an
    // ordinary per-project knob that any repo may tune for itself.
    let sb = Sandbox::new();
    sb.write_project_config("[audit]\nstale-days = 30\n");

    sb.config_cmd()
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

#[test]
fn invalid_toml_errors_with_path_and_hint() {
    let sb = Sandbox::new();
    sb.write_project_config("[audit\nstale-days = 30\n");

    sb.config_cmd().assert().failure().code(1).stderr(
        predicate::str::contains("error:")
            .and(predicate::str::contains("hpds.toml"))
            .and(predicate::str::contains("hint:")),
    );
}

#[test]
fn json_format_emits_resolved_config_and_sources() {
    let sb = Sandbox::new();
    sb.write_user_config("[project]\nprimary-author = \"malcolm\"\n");
    sb.write_project_config("[audit]\nstale-days = 30\n");

    let assert = sb
        .config_cmd()
        .arg("--format")
        .arg("json")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout should be JSON");

    assert_eq!(value["config"]["audit"]["stale-days"], 30);
    assert_eq!(value["config"]["project"]["primary-author"], "malcolm");
    assert_eq!(value["config"]["project"]["status"], "active");
    let project_source = value["sources"]["project"]
        .as_str()
        .expect("project source should be a path");
    assert!(project_source.ends_with("hpds.toml"));
    let user_source = value["sources"]["user"]
        .as_str()
        .expect("user source should be a path");
    assert!(user_source.ends_with("config.toml"));
}

#[test]
fn json_sources_are_null_when_no_files_contribute() {
    let sb = Sandbox::new();
    let assert = sb
        .config_cmd()
        .arg("--format")
        .arg("json")
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    assert!(value["sources"]["user"].is_null());
    assert!(value["sources"]["project"].is_null());
    assert_eq!(value["config"]["audit"]["stale-days"], 90);
}
