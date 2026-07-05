//! Integration tests for `hpds use gha`.
//!
//! The gha component adds GitHub Actions scaffolding: a pull request
//! template, a lint workflow, and the audit-bot workflow. Non-interactively
//! the selection comes
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
const LINT_WORKFLOW: &str = ".github/workflows/togi-lint.yml";
const AUDIT_WORKFLOW: &str = ".github/workflows/hpds-audit.yml";

#[test]
fn use_gha_with_every_workflow_creates_all_files() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "pr-template,lint,audit-bot"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("pull_request_template.md")
                .and(predicate::str::contains("togi-lint.yml"))
                .and(predicate::str::contains("hpds-audit.yml")),
        );

    assert!(sandbox.path(PR_TEMPLATE).is_file(), "PR template written");
    assert!(
        sandbox.path(LINT_WORKFLOW).is_file(),
        "lint workflow written"
    );
    assert!(
        sandbox.path(AUDIT_WORKFLOW).is_file(),
        "audit workflow written"
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

    assert!(body.contains("togi lint"), "runs togi lint: {body}");
    assert!(
        body.contains("togi format --check"),
        "runs togi format --check: {body}"
    );
    assert!(
        body.contains(TOGI_INSTALLER_ONE_LINER),
        "installs togi via its release installer script: {body}"
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

#[test]
fn generated_audit_workflow_is_valid_yaml_with_the_bot_steps() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "audit-bot"])
        .assert()
        .success();

    let body = sandbox.read(AUDIT_WORKFLOW);

    // Binding acceptance criterion: the workflow YAML must parse.
    let doc: serde_yaml::Value = serde_yaml::from_str(&body).expect("workflow YAML parses");
    assert!(
        doc.get("jobs").is_some(),
        "workflow has a jobs section: {body}"
    );

    // Triggers: weekly cron + pull_request.
    let on = doc.get("on").expect("workflow has triggers");
    assert!(
        on.get("schedule")
            .and_then(|s| s.get(0))
            .and_then(|e| e.get("cron"))
            .is_some(),
        "schedule trigger with a cron expression: {body}"
    );
    assert!(
        on.get("pull_request").is_some(),
        "pull_request trigger: {body}"
    );

    // The bot's least-privilege permissions block.
    let perms = doc.get("permissions").expect("permissions block");
    let perm = |key: &str| perms.get(key).and_then(|v| v.as_str());
    assert_eq!(perm("contents"), Some("read"), "{body}");
    assert_eq!(perm("issues"), Some("write"), "{body}");
    assert_eq!(perm("pull-requests"), Some("write"), "{body}");

    // Steps: install, audit to JSON without dying on findings, report.
    assert!(
        body.contains(&installer_one_liner()),
        "installs hpds via the release installer script: {body}"
    );
    assert!(
        body.contains("hpds audit --format json > audit.json"),
        "writes the audit JSON: {body}"
    );
    assert!(
        body.contains("hpds audit report-github --input audit.json"),
        "feeds the JSON to the reporter: {body}"
    );
    assert!(
        body.contains("GITHUB_TOKEN"),
        "reporter step gets the Actions token: {body}"
    );
}

#[test]
fn generated_audit_workflow_passes_actionlint_when_available() {
    // actionlint is an optional local dependency: run it when it is on
    // PATH, otherwise skip (CI and dev machines without it stay green).
    let Ok(found) = which_actionlint() else {
        eprintln!("skipping: actionlint not found on PATH");
        return;
    };

    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["gha", "--workflows", "audit-bot"])
        .assert()
        .success();

    let output = StdCommand::new(found)
        .arg(sandbox.path(AUDIT_WORKFLOW))
        .output()
        .expect("actionlint runs");
    assert!(
        output.status.success(),
        "actionlint reported problems:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The name of the shell installer artifact dist publishes for this crate:
/// `<package>-installer.sh`. Kept in lockstep with the crate name so the
/// workflows and the published artifact can never drift apart.
fn installer_artifact() -> String {
    format!("{}-installer.sh", env!("CARGO_PKG_NAME"))
}

/// The exact install command the generated audit workflow runs: curl the
/// latest shell installer from this repo's releases and pipe it to `sh`.
fn installer_one_liner() -> String {
    format!(
        "curl --proto '=https' --tlsv1.2 -LsSf {}/releases/latest/download/{} | sh",
        env!("CARGO_PKG_REPOSITORY"),
        installer_artifact(),
    )
}

/// The exact install command the generated lint workflow runs. The
/// `togi-installer.sh` artifact name tracks the togi project's cargo-dist
/// config (its shell installer), hardcoded here on purpose — togi is
/// separate software, so there is no config in this repo to parse it from.
const TOGI_INSTALLER_ONE_LINER: &str = "curl --proto '=https' --tlsv1.2 -LsSf \
     https://github.com/StanfordHPDS/togi/releases/latest/download/togi-installer.sh | sh";

/// Read a file from the crate root (the source templates live here).
fn repo_read(rel: &[&str]) -> String {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for part in rel {
        path.push(part);
    }
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The generated audit workflow must point at the very artifact dist is
/// configured to publish. dist emits `<package>-installer.sh` only when the
/// `shell` installer is enabled; if that installer is ever dropped or the
/// crate renamed, this test fails so the workflow and dist config stay in
/// sync.
#[test]
fn audit_workflow_installer_matches_dist_shell_installer_artifact() {
    let dist = repo_read(&["dist-workspace.toml"]);
    let parsed: toml::Value = toml::from_str(&dist).expect("dist-workspace.toml is valid TOML");
    let installers = parsed
        .get("dist")
        .and_then(|d| d.get("installers"))
        .and_then(|i| i.as_array())
        .expect("dist-workspace.toml declares [dist] installers");
    assert!(
        installers.iter().any(|i| i.as_str() == Some("shell")),
        "the shell installer must be enabled to publish {}",
        installer_artifact()
    );

    let one_liner = installer_one_liner();
    let body = repo_read(&[
        "templates",
        "gha",
        "audit-bot",
        ".github",
        "workflows",
        "hpds-audit.yml",
    ]);
    assert!(
        body.contains(&one_liner),
        "the audit workflow must install hpds via `{one_liner}`, got:\n{body}"
    );
}

/// The lint workflow installs togi, not hpds: its installer line is the
/// hardcoded togi one-liner (which tracks togi's own cargo-dist config),
/// and no hpds installer step may linger.
#[test]
fn lint_workflow_installs_togi_not_hpds() {
    let body = repo_read(&[
        "templates",
        "gha",
        "lint",
        ".github",
        "workflows",
        "togi-lint.yml",
    ]);
    assert!(
        body.contains(TOGI_INSTALLER_ONE_LINER),
        "the lint workflow must install togi via `{TOGI_INSTALLER_ONE_LINER}`, got:\n{body}"
    );
    assert!(
        !body.contains(&installer_artifact()),
        "the lint workflow must not install hpds anymore:\n{body}"
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
                .and(predicate::str::contains("lint"))
                .and(predicate::str::contains("audit-bot")),
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
        .hpds_use(&["gha", "--workflows", "pr-template,lint,audit-bot"])
        .assert()
        .success();
    let entries: Vec<_> = fs::read_dir(sandbox.project())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from(".github")]);
    assert!(sandbox.path(".github").is_dir());
}
