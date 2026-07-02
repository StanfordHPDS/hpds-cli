//! Integration tests for `hpds git vaccinate`.
//!
//! Every test runs against a sandboxed HOME + GIT_CONFIG_GLOBAL so the real
//! user's git config and global ignore file are NEVER touched.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const MARKER_BEGIN: &str = "# >>> hpds vaccinate >>>";
const MARKER_END: &str = "# <<< hpds vaccinate <<<";

/// Isolated HOME + global git config for one test. Dropping it removes the
/// temp dirs; no test may run git or hpds without going through this.
struct Sandbox {
    home: TempDir,
    work: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("temp HOME"),
            work: TempDir::new().expect("temp workdir"),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn work(&self) -> &Path {
        self.work.path()
    }

    fn gitconfig(&self) -> PathBuf {
        self.home.path().join(".gitconfig")
    }

    /// `hpds` command with the environment fully pointed at the sandbox.
    fn hpds(&self, cwd: &Path) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        self.isolate(&mut cmd, cwd);
        cmd
    }

    /// Raw `git` command with the same sandboxed environment, for test setup.
    fn git(&self, cwd: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        self.isolate(&mut cmd, cwd);
        cmd.args(args).assert().success();
    }

    fn isolate(&self, cmd: &mut Command, cwd: &Path) {
        cmd.current_dir(cwd)
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("GIT_CONFIG_GLOBAL", self.gitconfig())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
    }

    /// Value of `core.excludesFile` in the sandboxed global config, if set.
    fn excludes_file_config(&self) -> Option<String> {
        let mut cmd = Command::new("git");
        self.isolate(&mut cmd, self.work());
        let output = cmd
            .args(["config", "--global", "--get", "core.excludesFile"])
            .output()
            .expect("git should run");
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack
        .lines()
        .filter(|line| line.trim() == needle)
        .count()
}

#[test]
fn global_vaccinate_creates_home_gitignore_and_sets_config() {
    let sb = Sandbox::new();
    sb.hpds(sb.work())
        .args(["git", "vaccinate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added"));

    let ignore = sb.home().join(".gitignore");
    let content = read(&ignore);
    // Marker block present.
    assert_eq!(count_occurrences(&content, MARKER_BEGIN), 1);
    assert_eq!(count_occurrences(&content, MARKER_END), 1);
    // R patterns.
    for pat in [
        ".Rhistory",
        ".RData",
        ".Rproj.user",
        ".Rdata",
        ".httr-oauth",
        ".DS_Store",
    ] {
        assert_eq!(count_occurrences(&content, pat), 1, "missing {pat}");
    }
    // Curated Python patterns.
    for pat in [
        "__pycache__/",
        "*.py[cod]",
        ".venv/",
        ".ipynb_checkpoints/",
        ".env",
        "*.egg-info/",
        ".pytest_cache/",
        ".mypy_cache/",
        ".ruff_cache/",
    ] {
        assert_eq!(count_occurrences(&content, pat), 1, "missing {pat}");
    }
    // core.excludesFile now points at the file we created.
    let configured = sb.excludes_file_config().expect("config should be set");
    assert_eq!(PathBuf::from(configured), ignore);
}

#[test]
fn global_vaccinate_is_idempotent() {
    let sb = Sandbox::new();
    for _ in 0..2 {
        sb.hpds(sb.work())
            .args(["git", "vaccinate"])
            .assert()
            .success();
    }

    let content = read(&sb.home().join(".gitignore"));
    assert_eq!(count_occurrences(&content, MARKER_BEGIN), 1);
    assert_eq!(count_occurrences(&content, MARKER_END), 1);
    assert_eq!(count_occurrences(&content, ".Rhistory"), 1);
    assert_eq!(count_occurrences(&content, "__pycache__/"), 1);
}

#[test]
fn global_vaccinate_second_run_reports_nothing_to_add() {
    let sb = Sandbox::new();
    sb.hpds(sb.work())
        .args(["git", "vaccinate"])
        .assert()
        .success();
    sb.hpds(sb.work())
        .args(["git", "vaccinate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to add"));
}

#[test]
fn global_vaccinate_respects_existing_excludes_file_config() {
    let sb = Sandbox::new();
    let custom = sb.home().join("custom-ignore");
    sb.git(
        sb.work(),
        &[
            "config",
            "--global",
            "core.excludesFile",
            custom.to_str().unwrap(),
        ],
    );

    sb.hpds(sb.work())
        .args(["git", "vaccinate"])
        .assert()
        .success();

    let content = read(&custom);
    assert_eq!(count_occurrences(&content, ".Rhistory"), 1);
    // ~/.gitignore must not be created when config already points elsewhere.
    assert!(!sb.home().join(".gitignore").exists());
    // Config value untouched.
    assert_eq!(PathBuf::from(sb.excludes_file_config().unwrap()), custom);
}

#[test]
fn global_vaccinate_expands_tilde_in_excludes_file_config() {
    let sb = Sandbox::new();
    sb.git(
        sb.work(),
        &["config", "--global", "core.excludesFile", "~/tilde-ignore"],
    );

    sb.hpds(sb.work())
        .args(["git", "vaccinate"])
        .assert()
        .success();

    let content = read(&sb.home().join("tilde-ignore"));
    assert_eq!(count_occurrences(&content, ".Rhistory"), 1);
}

#[test]
fn global_vaccinate_preserves_existing_content_and_skips_present_patterns() {
    let sb = Sandbox::new();
    let ignore = sb.home().join(".gitignore");
    std::fs::write(&ignore, "my-junk.txt\n.Rhistory\n").unwrap();
    sb.git(
        sb.work(),
        &[
            "config",
            "--global",
            "core.excludesFile",
            ignore.to_str().unwrap(),
        ],
    );

    sb.hpds(sb.work())
        .args(["git", "vaccinate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already present"));

    let content = read(&ignore);
    // Existing content preserved, pattern not duplicated.
    assert!(content.starts_with("my-junk.txt\n"));
    assert_eq!(count_occurrences(&content, ".Rhistory"), 1);
    assert_eq!(count_occurrences(&content, "__pycache__/"), 1);
}

#[test]
fn project_vaccinate_appends_to_repo_gitignore() {
    let sb = Sandbox::new();
    sb.git(sb.work(), &["init"]);
    std::fs::write(sb.work().join(".gitignore"), "target/\n").unwrap();

    sb.hpds(sb.work())
        .args(["git", "vaccinate", "--project"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added"));

    let content = read(&sb.work().join(".gitignore"));
    assert!(content.starts_with("target/\n"));
    assert_eq!(count_occurrences(&content, MARKER_BEGIN), 1);
    assert_eq!(count_occurrences(&content, ".Rhistory"), 1);
    assert_eq!(count_occurrences(&content, "__pycache__/"), 1);
    // The global ignore is untouched in --project mode.
    assert!(!sb.home().join(".gitignore").exists());
    assert_eq!(sb.excludes_file_config(), None);
}

#[test]
fn project_vaccinate_creates_gitignore_when_missing() {
    let sb = Sandbox::new();
    sb.git(sb.work(), &["init"]);

    sb.hpds(sb.work())
        .args(["git", "vaccinate", "--project"])
        .assert()
        .success();

    let content = read(&sb.work().join(".gitignore"));
    assert_eq!(count_occurrences(&content, MARKER_BEGIN), 1);
    assert_eq!(count_occurrences(&content, ".ruff_cache/"), 1);
}

#[test]
fn project_vaccinate_is_idempotent() {
    let sb = Sandbox::new();
    sb.git(sb.work(), &["init"]);
    for _ in 0..2 {
        sb.hpds(sb.work())
            .args(["git", "vaccinate", "--project"])
            .assert()
            .success();
    }

    let content = read(&sb.work().join(".gitignore"));
    assert_eq!(count_occurrences(&content, MARKER_BEGIN), 1);
    assert_eq!(count_occurrences(&content, MARKER_END), 1);
    assert_eq!(count_occurrences(&content, ".Rhistory"), 1);
}

#[test]
fn project_vaccinate_works_from_a_subdirectory() {
    let sb = Sandbox::new();
    sb.git(sb.work(), &["init"]);
    let sub = sb.work().join("analysis");
    std::fs::create_dir(&sub).unwrap();

    sb.hpds(&sub)
        .args(["git", "vaccinate", "--project"])
        .assert()
        .success();

    // Patterns land in the repo root .gitignore, not analysis/.gitignore.
    assert!(sb.work().join(".gitignore").exists());
    assert!(!sub.join(".gitignore").exists());
}

#[test]
fn project_vaccinate_outside_a_repo_fails_with_guidance() {
    let sb = Sandbox::new();
    sb.hpds(sb.work())
        .args(["git", "vaccinate", "--project"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("not inside a git repository")
                .and(predicate::str::contains("git init")),
        );
}
