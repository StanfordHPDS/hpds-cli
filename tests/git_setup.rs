//! Integration tests for `hpds git setup`.
//!
//! Every test runs against a sandboxed HOME + GIT_CONFIG_GLOBAL so the real
//! user's git config is NEVER touched. `gh` is never the real binary either:
//! a shim script earlier on PATH records its argv to `$GH_SHIM_LOG` and
//! answers with the exit code in `$GH_AUTH_EXIT`; the "gh not installed" case
//! uses a PATH containing only a symlink to the real `git`.
//!
//! Unix-only: on Windows `std::process::Command::new("gh")` resolves only
//! `gh.exe`, so a script shim on PATH cannot intercept the call there.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// The fake `gh`: logs `gh <argv...>` one line per invocation, then answers
/// `auth status` with the canned exit code.
const GH_SHIM: &str = r#"#!/bin/sh
printf 'gh %s\n' "$*" >> "$GH_SHIM_LOG"
case "$1" in
  auth)
    exit "${GH_AUTH_EXIT:-0}"
    ;;
esac
exit 0
"#;

/// Isolated HOME + global git config + PATH-shimmed `gh` for one test.
struct Sandbox {
    tmp: TempDir,
    /// PATH the spawned `hpds` sees.
    path: std::ffi::OsString,
}

impl Sandbox {
    /// Sandbox whose PATH has the `gh` shim first (shadowing any real gh).
    fn new() -> Self {
        let (tmp, bin) = Self::base_dirs();
        let gh = bin.join("gh");
        fs::write(&gh, GH_SHIM).expect("write gh shim");
        make_executable(&gh);
        let orig = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&orig)))
            .expect("join PATH");
        Self { tmp, path }
    }

    /// Sandbox whose PATH contains ONLY real `git` (via a symlink) — no `gh`
    /// anywhere, so spawning it fails with NotFound.
    fn without_gh() -> Self {
        let (tmp, bin) = Self::base_dirs();
        let real_git = find_on_path("git").expect("git must be installed to run these tests");
        std::os::unix::fs::symlink(real_git, bin.join("git")).expect("symlink git");
        Self {
            path: bin.into_os_string(),
            tmp,
        }
    }

    fn base_dirs() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("create tempdir");
        for dir in ["home", "work", "bin"] {
            fs::create_dir(tmp.path().join(dir)).expect("create sandbox dir");
        }
        let bin = tmp.path().join("bin");
        (tmp, bin)
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn work(&self) -> PathBuf {
        self.tmp.path().join("work")
    }

    fn gitconfig(&self) -> PathBuf {
        self.home().join(".gitconfig")
    }

    fn gh_log_path(&self) -> PathBuf {
        self.tmp.path().join("gh.log")
    }

    /// `hpds` command with the environment fully pointed at the sandbox.
    fn hpds(&self) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        self.isolate(&mut cmd);
        cmd
    }

    /// Raw `git` command with the same sandboxed environment, for test setup.
    fn git(&self, args: &[&str]) {
        let mut cmd = Command::new("git");
        self.isolate(&mut cmd);
        cmd.args(args).assert().success();
    }

    fn isolate(&self, cmd: &mut Command) {
        cmd.current_dir(self.work())
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("GIT_CONFIG_GLOBAL", self.gitconfig())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PATH", &self.path)
            .env("GH_SHIM_LOG", self.gh_log_path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE");
    }

    /// Value of `key` in the sandboxed global git config, if set.
    fn config(&self, key: &str) -> Option<String> {
        let mut cmd = Command::new("git");
        self.isolate(&mut cmd);
        let output = cmd
            .args(["config", "--global", "--get", key])
            .output()
            .expect("git should run");
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }

    fn gh_log(&self) -> Vec<String> {
        fs::read_to_string(self.gh_log_path())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

/// Resolve a program on the test process's own PATH.
fn find_on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[test]
fn fresh_setup_sets_default_branch_identity_and_vaccinates() {
    let sb = Sandbox::new();
    sb.hpds()
        .args([
            "git",
            "setup",
            "--name",
            "Ada Lovelace",
            "--email",
            "ada@example.com",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("set init.defaultBranch to main")
                .and(predicate::str::contains("set user.name to Ada Lovelace"))
                .and(predicate::str::contains(
                    "set user.email to ada@example.com",
                ))
                .and(predicate::str::contains("added")),
        );

    assert_eq!(sb.config("init.defaultBranch").as_deref(), Some("main"));
    assert_eq!(sb.config("user.name").as_deref(), Some("Ada Lovelace"));
    assert_eq!(sb.config("user.email").as_deref(), Some("ada@example.com"));

    // --yes ran the vaccinate step without prompting.
    let ignore = fs::read_to_string(sb.home().join(".gitignore")).expect("global ignore written");
    assert!(ignore.contains("# >>> hpds vaccinate >>>"));
    assert!(ignore.contains(".Rhistory"));

    // gh was only ever probed for auth state — nothing else.
    assert_eq!(sb.gh_log(), vec!["gh auth status".to_string()]);
}

#[test]
fn default_branch_already_main_is_reported_not_reset() {
    let sb = Sandbox::new();
    sb.git(&["config", "--global", "init.defaultBranch", "main"]);

    sb.hpds()
        .args(["git", "setup", "--name", "A", "--email", "a@b.c", "--yes"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("init.defaultBranch").and(predicate::str::contains("already")),
        );

    assert_eq!(sb.config("init.defaultBranch").as_deref(), Some("main"));
}

#[test]
fn other_default_branch_is_kept_and_reported() {
    let sb = Sandbox::new();
    sb.git(&["config", "--global", "init.defaultBranch", "trunk"]);

    sb.hpds()
        .args(["git", "setup", "--name", "A", "--email", "a@b.c", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("trunk"));

    // The existing value is never clobbered.
    assert_eq!(sb.config("init.defaultBranch").as_deref(), Some("trunk"));
}

#[test]
fn existing_identity_is_kept_and_reported() {
    let sb = Sandbox::new();
    sb.git(&["config", "--global", "user.name", "Old Name"]);
    sb.git(&["config", "--global", "user.email", "old@example.com"]);

    sb.hpds()
        .args(["git", "setup", "--yes"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Old Name")
                .and(predicate::str::contains("old@example.com"))
                .and(predicate::str::contains("already")),
        );

    assert_eq!(sb.config("user.name").as_deref(), Some("Old Name"));
    assert_eq!(sb.config("user.email").as_deref(), Some("old@example.com"));
}

#[test]
fn identity_flags_override_existing_config() {
    let sb = Sandbox::new();
    sb.git(&["config", "--global", "user.name", "Old Name"]);
    sb.git(&["config", "--global", "user.email", "old@example.com"]);

    sb.hpds()
        .args([
            "git",
            "setup",
            "--name",
            "New Name",
            "--email",
            "new@example.com",
            "--yes",
        ])
        .assert()
        .success();

    assert_eq!(sb.config("user.name").as_deref(), Some("New Name"));
    assert_eq!(sb.config("user.email").as_deref(), Some("new@example.com"));
}

#[test]
fn yes_without_identity_flags_errors_actionably_when_unset() {
    let sb = Sandbox::new();
    sb.hpds()
        .args(["git", "setup", "--yes"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("user.name")
                .and(predicate::str::contains("--name"))
                .and(predicate::str::contains("hint:")),
        );

    assert_eq!(sb.config("user.name"), None);
}

#[test]
fn non_interactive_identity_prompt_refuses_with_actionable_error() {
    let sb = Sandbox::new();
    // stdin is not a TTY under assert_cmd, so the name prompt must refuse
    // with a hint instead of hanging or panicking.
    sb.hpds().args(["git", "setup"]).assert().code(1).stderr(
        predicate::str::contains("non-interactively").and(predicate::str::contains("hint:")),
    );
}

#[test]
fn non_interactive_vaccinate_offer_without_yes_errors_actionably() {
    let sb = Sandbox::new();
    sb.hpds()
        .args(["git", "setup", "--name", "A", "--email", "a@b.c"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("non-interactively").and(predicate::str::contains("hint:")),
        );

    // Everything before the vaccinate offer was still applied.
    assert_eq!(sb.config("init.defaultBranch").as_deref(), Some("main"));
    assert_eq!(sb.config("user.name").as_deref(), Some("A"));
}

#[test]
fn unauthenticated_gh_prints_login_guidance_and_continues() {
    let sb = Sandbox::new();
    sb.hpds()
        .args(["git", "setup", "--name", "A", "--email", "a@b.c", "--yes"])
        .env("GH_AUTH_EXIT", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("gh auth login"));

    // Guidance only: the auth state never aborts setup, and the shim shows
    // hpds never tried to run the login itself.
    assert_eq!(sb.gh_log(), vec!["gh auth status".to_string()]);
    let ignore = fs::read_to_string(sb.home().join(".gitignore")).expect("global ignore written");
    assert!(ignore.contains("# >>> hpds vaccinate >>>"));
}

#[test]
fn missing_gh_suggests_hpds_install_gh() {
    let sb = Sandbox::without_gh();
    sb.hpds()
        .args(["git", "setup", "--name", "A", "--email", "a@b.c", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hpds install gh"));

    // Setup still completed: defaults, identity, and vaccination all landed.
    assert_eq!(sb.config("init.defaultBranch").as_deref(), Some("main"));
    assert!(sb.home().join(".gitignore").exists());
}

#[test]
fn empty_name_flag_errors_actionably() {
    let sb = Sandbox::new();
    sb.hpds()
        .args(["git", "setup", "--name", "  ", "--email", "a@b.c", "--yes"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("user.name").and(predicate::str::contains("hint:")));

    assert_eq!(sb.config("user.name"), None);
}
