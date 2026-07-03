//! Integration tests for `hpds repo create`.
//!
//! These tests NEVER call the real `gh` and never create real repositories:
//! the internal `HPDS_GH` override points hpds straight at a shim script
//! (no PATH lookup can ever fall through to a real gh; the same shim also
//! sits first on `PATH` as a second line of defense). The shim records its
//! argv to `$GH_SHIM_LOG` and returns canned results driven by env vars
//! (`GH_AUTH_EXIT`, `GH_REPO_CREATE_EXIT`). Real `git` is used, but only
//! against temp directories, isolated from the developer's configuration via
//! `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM`.
//!
//! Unix-only: on Windows `std::process::Command::new("gh")` resolves only
//! `gh.exe`, so a script shim on PATH cannot intercept the call there. The
//! logic under test is platform-independent and unit-tested in
//! `src/gitx/repo.rs`.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// The fake `gh`: logs `gh <argv...>` one line per invocation, then answers
/// with canned output/exit codes.
const GH_SHIM: &str = r#"#!/bin/sh
printf 'gh %s\n' "$*" >> "$GH_SHIM_LOG"
case "$1" in
  auth)
    exit "${GH_AUTH_EXIT:-0}"
    ;;
  repo)
    echo "https://github.com/fake-org/fake-repo"
    exit "${GH_REPO_CREATE_EXIT:-0}"
    ;;
esac
exit 0
"#;

struct ShimEnv {
    /// Keeps the temp dir alive for the duration of the test.
    _tmp: tempfile::TempDir,
    project: PathBuf,
    log: PathBuf,
    gitconfig: PathBuf,
    /// The shim executable itself, wired in via `HPDS_GH`.
    gh: PathBuf,
    path: std::ffi::OsString,
}

fn setup(project_name: &str) -> ShimEnv {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let shim_dir = tmp.path().join("bin");
    fs::create_dir(&shim_dir).expect("create shim dir");
    let gh = shim_dir.join("gh");
    fs::write(&gh, GH_SHIM).expect("write gh shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod gh shim");
    }

    let project = tmp.path().join(project_name);
    fs::create_dir(&project).expect("create project dir");

    let gitconfig = tmp.path().join("gitconfig");
    fs::write(
        &gitconfig,
        "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n",
    )
    .expect("write test gitconfig");

    let orig_path = std::env::var_os("PATH").unwrap_or_default();
    let path =
        std::env::join_paths(std::iter::once(shim_dir).chain(std::env::split_paths(&orig_path)))
            .expect("join PATH");

    let log = tmp.path().join("gh.log");
    ShimEnv {
        _tmp: tmp,
        project,
        log,
        gitconfig,
        gh,
        path,
    }
}

fn hpds(env: &ShimEnv) -> Command {
    let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
    cmd.current_dir(&env.project)
        .env("PATH", &env.path)
        .env("HPDS_GH", &env.gh)
        .env("GH_SHIM_LOG", &env.log)
        .env("GIT_CONFIG_GLOBAL", &env.gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

fn gh_log(env: &ShimEnv) -> Vec<String> {
    fs::read_to_string(&env.log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Run real `git` inside the test project (never the shim: the shim dir only
/// contains `gh`).
fn git_in(env: &ShimEnv, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(&env.project)
        .env("GIT_CONFIG_GLOBAL", &env.gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git")
}

#[test]
fn unauthenticated_gh_errors_with_login_hint_and_creates_nothing() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");

    hpds(&env)
        .args(["repo", "create", "--yes"])
        .env("GH_AUTH_EXIT", "1")
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("error:")
                .and(predicate::str::contains("gh auth login"))
                .and(predicate::str::contains("hint:")),
        );

    // Auth was checked, and nothing else touched gh.
    assert_eq!(gh_log(&env), vec!["gh auth status".to_string()]);
    // No git repo was initialized either: we fail before mutating anything.
    assert!(!env.project.join(".git").exists());
}

#[test]
fn happy_path_inits_commits_creates_and_pushes() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");

    hpds(&env)
        .args(["repo", "create", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "https://github.com/fake-org/fake-repo",
        ));

    // git init + initial commit really happened (real git, temp dir).
    assert!(env.project.join(".git").is_dir());
    let head = git_in(&env, &["rev-parse", "--verify", "HEAD"]);
    assert!(head.status.success(), "expected an initial commit");
    let files = git_in(&env, &["ls-files"]);
    assert!(
        String::from_utf8_lossy(&files.stdout).contains("README.md"),
        "initial commit should include project files"
    );

    // gh argv sequence: auth check, then create with defaults
    // (name = dir basename, org = StanfordHPDS, visibility = private) + push.
    assert_eq!(
        gh_log(&env),
        vec![
            "gh auth status".to_string(),
            "gh repo create StanfordHPDS/myproj --private --source=. --push".to_string(),
        ]
    );
}

#[test]
fn name_org_and_visibility_flags_are_honored() {
    let env = setup("somedir");
    fs::write(env.project.join("analysis.R"), "1 + 1\n").expect("write file");

    hpds(&env)
        .args([
            "repo",
            "create",
            "--yes",
            "--name",
            "cool-study",
            "--org",
            "malco",
            "--visibility",
            "public",
        ])
        .assert()
        .success();

    assert_eq!(
        gh_log(&env),
        vec![
            "gh auth status".to_string(),
            "gh repo create malco/cool-study --public --source=. --push".to_string(),
        ]
    );
}

#[test]
fn flags_alone_suffice_without_yes_when_no_prompt_is_needed() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");
    assert!(git_in(&env, &["init"]).status.success());
    assert!(git_in(&env, &["add", "README.md"]).status.success());
    assert!(git_in(&env, &["commit", "-m", "first"]).status.success());

    // No --yes, but every value is provided by a flag and a commit already
    // exists, so nothing needs to prompt even without a TTY.
    hpds(&env)
        .args([
            "repo",
            "create",
            "--name",
            "x",
            "--org",
            "y",
            "--visibility",
            "private",
        ])
        .assert()
        .success();

    // The pre-existing commit was kept; no extra commit was created.
    let count = git_in(&env, &["rev-list", "--count", "HEAD"]);
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    assert_eq!(
        gh_log(&env),
        vec![
            "gh auth status".to_string(),
            "gh repo create y/x --private --source=. --push".to_string(),
        ]
    );
}

#[test]
fn non_interactive_without_yes_or_flags_fails_with_actionable_error() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");

    // stdin is not a TTY under assert_cmd, so the name prompt must refuse
    // with a hint instead of hanging or panicking.
    hpds(&env).args(["repo", "create"]).assert().code(1).stderr(
        predicate::str::contains("non-interactively").and(predicate::str::contains("hint:")),
    );

    // Auth was checked, but no repo was created.
    assert_eq!(gh_log(&env), vec!["gh auth status".to_string()]);
}

#[test]
fn initial_commit_never_includes_beads_database() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");
    fs::create_dir(env.project.join(".beads")).expect("create .beads");
    fs::write(
        env.project.join(".beads").join("hpds.db"),
        "not a repo file",
    )
    .expect("write beads db");

    hpds(&env)
        .args(["repo", "create", "--yes"])
        .assert()
        .success();

    let files = git_in(&env, &["ls-files"]);
    let tracked = String::from_utf8_lossy(&files.stdout).to_string();
    assert!(tracked.contains("README.md"), "tracked: {tracked}");
    assert!(
        !tracked.contains(".beads"),
        "nothing under .beads/ may be committed; tracked: {tracked}"
    );
}

#[test]
fn quiet_suppresses_informational_stdout_but_still_does_the_work() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");

    hpds(&env)
        .args(["repo", "create", "--yes", "--quiet"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // Quiet changes output only: the repo was still initialized, committed,
    // created, and pushed.
    assert!(env.project.join(".git").is_dir());
    assert!(
        git_in(&env, &["rev-parse", "--verify", "HEAD"])
            .status
            .success()
    );
    assert_eq!(
        gh_log(&env),
        vec![
            "gh auth status".to_string(),
            "gh repo create StanfordHPDS/myproj --private --source=. --push".to_string(),
        ]
    );
}

#[test]
fn quiet_still_shows_errors() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");

    hpds(&env)
        .args(["repo", "create", "--yes", "--quiet"])
        .env("GH_AUTH_EXIT", "1")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("hint:")));
}

#[test]
fn nested_directory_inside_a_parent_repo_gets_its_own_repo() {
    // Regression: `git rev-parse --git-dir` succeeds when the cwd is merely
    // nested inside a PARENT repo, which used to skip `git init` and would
    // have pushed the parent's entire history to the new GitHub repo.
    let mut env = setup("outer");
    fs::write(env.project.join("outer-secret.txt"), "parent history\n").expect("write file");
    assert!(git_in(&env, &["init"]).status.success());
    assert!(git_in(&env, &["add", "--all"]).status.success());
    assert!(
        git_in(&env, &["commit", "-m", "parent commit"])
            .status
            .success()
    );

    let inner = env.project.join("inner");
    fs::create_dir(&inner).expect("create inner dir");
    fs::write(inner.join("README.md"), "# inner\n").expect("write file");
    env.project = inner;

    // A user who merely cd'd one level too deep should be told they are
    // about to publish a nested repo, not silently get one.
    hpds(&env)
        .args(["repo", "create", "--yes"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("warning:").and(predicate::str::contains(
                "inside an existing git repository",
            )),
        );

    // inner must have been initialized as its own repo...
    assert!(env.project.join(".git").is_dir());
    // ...whose single commit contains inner's files and none of the parent's.
    let count = git_in(&env, &["rev-list", "--count", "HEAD"]);
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    let files = git_in(&env, &["ls-files"]);
    let tracked = String::from_utf8_lossy(&files.stdout).to_string();
    assert!(tracked.contains("README.md"), "tracked: {tracked}");
    assert!(
        !tracked.contains("outer-secret.txt"),
        "parent repo files must not leak into the new repo; tracked: {tracked}"
    );

    // The repo name defaults to the inner directory's basename.
    assert_eq!(
        gh_log(&env),
        vec![
            "gh auth status".to_string(),
            "gh repo create StanfordHPDS/inner --private --source=. --push".to_string(),
        ]
    );
}

#[test]
fn failed_gh_repo_create_surfaces_the_error() {
    let env = setup("myproj");
    fs::write(env.project.join("README.md"), "# hi\n").expect("write file");

    hpds(&env)
        .args(["repo", "create", "--yes"])
        .env("GH_REPO_CREATE_EXIT", "1")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("error:").and(predicate::str::contains("hint:")));
}
