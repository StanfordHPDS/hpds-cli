//! Integration tests for `hpds use gha`.
//!
//! The gha component adds GitHub Actions scaffolding: a pull request
//! template and a lint workflow. Non-interactively the selection comes
//! from `--workflows`; interactively it is a multi-select (not testable
//! here — assert_cmd never has a TTY, so the no-flag path must fail with
//! an actionable error instead).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Isolated HOME + project directory for one test.
struct Sandbox {
    home: TempDir,
    project: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("temp HOME"),
            project: TempDir::new().expect("temp project dir"),
        }
    }

    fn project(&self) -> &Path {
        self.project.path()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.project().join(rel)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel))
            .unwrap_or_else(|e| panic!("read {rel} in the sandbox: {e}"))
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().expect("sandbox files have parents"))
            .expect("create sandbox dirs");
        fs::write(path, content).expect("write sandbox file");
    }

    /// `hpds use <args...>` run from the sandboxed project directory.
    fn hpds_use(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(self.project())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("HPDS_CONFIG_DIR", self.home.path().join("hpds-config"))
            .arg("use")
            .args(args);
        cmd
    }
}

const PR_TEMPLATE: &str = ".github/pull_request_template.md";
const LINT_WORKFLOW: &str = ".github/workflows/hpds-lint.yml";

#[test]
fn use_gha_with_both_workflows_creates_both_files() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template,lint"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("pull_request_template.md")
                .and(predicate::str::contains("hpds-lint.yml")),
        );

    assert!(sandbox.path(PR_TEMPLATE).is_file(), "PR template written");
    assert!(
        sandbox.path(LINT_WORKFLOW).is_file(),
        "lint workflow written"
    );
}

#[test]
fn workflows_flag_can_also_be_repeated() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template", "--workflows", "lint"])
        .assert()
        .success();
    assert!(sandbox.path(PR_TEMPLATE).is_file());
    assert!(sandbox.path(LINT_WORKFLOW).is_file());
}

#[test]
fn pr_template_asks_for_changes_and_decisions() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template"])
        .assert()
        .success();

    let body = sandbox.read(PR_TEMPLATE);
    assert!(
        body.to_lowercase().contains("describe"),
        "asks to describe the changes: {body}"
    );
    assert!(
        body.to_lowercase().contains("decision"),
        "asks to document important decisions: {body}"
    );
    // Only the requested workflow lands.
    assert!(
        !sandbox.path(LINT_WORKFLOW).exists(),
        "lint workflow was not requested"
    );
}

#[test]
fn generated_lint_workflow_is_valid_yaml_with_the_lint_steps() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "lint"])
        .assert()
        .success();

    let body = sandbox.read(LINT_WORKFLOW);

    // Binding acceptance criterion: the workflow YAML must parse.
    let doc: serde_yaml::Value = serde_yaml::from_str(&body).expect("workflow YAML parses");
    assert!(
        doc.get("jobs").is_some(),
        "workflow has a jobs section: {body}"
    );

    assert!(body.contains("hpds lint"), "runs hpds lint: {body}");
    assert!(
        body.contains("hpds format --check"),
        "runs hpds format --check: {body}"
    );
    assert!(
        body.contains("cargo install --git"),
        "installs hpds via the cargo fallback: {body}"
    );
    assert!(
        body.contains("release install script"),
        "comment flags the placeholder install step: {body}"
    );
}

#[test]
fn generated_lint_workflow_passes_actionlint_when_available() {
    // actionlint is an optional local dependency: run it when it is on
    // PATH, otherwise skip (CI and dev machines without it stay green).
    let Ok(found) = which_actionlint() else {
        eprintln!("skipping: actionlint not found on PATH");
        return;
    };

    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "lint"])
        .assert()
        .success();

    let output = StdCommand::new(found)
        .arg(sandbox.path(LINT_WORKFLOW))
        .output()
        .expect("actionlint runs");
    assert!(
        output.status.success(),
        "actionlint reported problems:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn which_actionlint() -> Result<PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        for name in ["actionlint", "actionlint.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(())
}

#[test]
fn unknown_workflow_exits_2_and_names_the_available_ones() {
    // An unknown --workflows value is a usage error, exactly like an
    // unknown component name: both exit 2.
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "does-not-exist"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("does-not-exist")
                .and(predicate::str::contains("pr-template"))
                .and(predicate::str::contains("lint")),
        );
}

#[test]
fn gha_without_workflows_flag_fails_actionably_when_not_a_tty() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--workflows"));
}

#[test]
fn gha_rejects_the_kind_flag() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "lint", "--kind", "make"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--kind"));
    assert!(!sandbox.path(LINT_WORKFLOW).exists());
}

#[test]
fn other_components_reject_the_workflows_flag() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["readme", "--language", "python", "--workflows", "lint"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--workflows"));
    assert!(!sandbox.path("README.md").exists());
}

#[test]
fn second_run_is_idempotent() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template,lint"])
        .assert()
        .success();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template,lint"])
        .assert()
        .success()
        .stdout(predicate::str::contains("is already up to date"));
    assert!(sandbox.path(PR_TEMPLATE).is_file());
    assert!(sandbox.path(LINT_WORKFLOW).is_file());
}

#[test]
fn conflicting_file_is_skipped_with_a_diff_and_force_hint() {
    let sandbox = Sandbox::new();
    sandbox.write(PR_TEMPLATE, "my own template\n");

    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("-my own template").and(predicate::str::contains("--force")),
        )
        .stderr(predicate::str::contains("skipped"));

    // The user's file is untouched.
    assert_eq!(sandbox.read(PR_TEMPLATE), "my own template\n");
}

#[test]
fn force_overwrites_a_conflicting_file() {
    let sandbox = Sandbox::new();
    sandbox.write(PR_TEMPLATE, "my own template\n");

    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template", "--force"])
        .assert()
        .success();

    let body = sandbox.read(PR_TEMPLATE);
    assert!(body.to_lowercase().contains("decision"), "body was: {body}");
}

#[test]
fn use_without_a_component_lists_gha() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&[])
        .assert()
        .success()
        .stdout(predicate::str::contains("gha"));
}

#[test]
fn unknown_component_errors_and_lists_gha_among_the_available_ones() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["no-such-component"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("`no-such-component` is not a template component")
                .and(predicate::str::contains("gha")),
        );
}

/// The generated tree never includes anything outside `.github/`.
#[test]
fn gha_only_writes_under_dot_github() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template,lint"])
        .assert()
        .success();
    let entries: Vec<_> = fs::read_dir(sandbox.project())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from(".github")]);
    assert!(sandbox.path(".github").is_dir());
}
