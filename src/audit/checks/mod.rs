//! The local audit checks. Each one inspects the repo (shelling out to
//! `git` where needed) and returns [`Finding`]s — no printing, no mutation.
//!
//! Shared plumbing lives here: careful `git` invocation (a broken repo
//! state is data or a finding, never a panic) and the tracked-file listing
//! most checks start from.

mod artifacts;
mod branches;
mod gitignore;
mod junk;
mod lifecycle;
mod lockfiles;
mod readme;
mod workspace;

use std::path::Path;
use std::process::Command;

use super::{Check, Finding, Severity};

/// Every local check, in the order they run and report.
pub(super) fn all() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(workspace::DirtyFiles),
        Box::new(workspace::UntrackedFiles),
        Box::new(branches::StaleBranches),
        Box::new(artifacts::StaleArtifacts),
        Box::new(junk::JunkFiles),
        Box::new(gitignore::GitignoreHygiene),
        Box::new(readme::Readme),
        Box::new(lifecycle::LifecycleMetadata),
        Box::new(lockfiles::Lockfiles),
    ]
}

/// A `git` invocation that could not answer: the command and why.
#[derive(Debug)]
struct GitError {
    args: String,
    detail: String,
}

/// Run `git -C <repo> <args>`, requiring exit 0. Non-zero exit and spawn
/// failures both come back as [`GitError`] — callers turn that into a
/// finding (or treat it as a legitimate state), never a panic.
fn git(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let rendered = || args.join(" ");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|err| GitError {
            args: rendered(),
            detail: match err.kind() {
                std::io::ErrorKind::NotFound => {
                    "git was not found on PATH; install it from https://git-scm.com".to_string()
                }
                _ => err.to_string(),
            },
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(GitError {
            args: rendered(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Like [`git`], but a failure is a legitimate answer (e.g. asking about
/// `HEAD` in a repo with no commits yet): `None` instead of an error.
/// Shared with the GitHub checks, which ask git about remotes and branch
/// tips the same way.
pub(super) fn git_maybe(repo: &Path, args: &[&str]) -> Option<String> {
    git(repo, args).ok()
}

/// The finding every check reports when `git` itself cannot answer.
fn git_failed_finding(check_id: &str, err: &GitError) -> Finding {
    let detail = if err.detail.is_empty() {
        String::new()
    } else {
        format!(": {}", err.detail)
    };
    Finding {
        check_id: check_id.to_string(),
        severity: Severity::Error,
        message: format!(
            "could not inspect the repo (`git {}` failed{detail})",
            err.args
        ),
        remediation: "check that git is installed and that this directory is a git \
                      repository (run `git init` if it should be one)"
            .to_string(),
    }
}

/// The root of the repository containing `dir`, when git can find one.
pub(super) fn repo_root(dir: &Path) -> Option<std::path::PathBuf> {
    let out = git_maybe(dir, &["rev-parse", "--show-toplevel"])?;
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| std::path::PathBuf::from(trimmed))
}

/// Committed (tracked) files as slash-separated paths relative to the repo
/// root, exactly as `git ls-files` reports them.
fn tracked_files(repo: &Path) -> Result<Vec<String>, GitError> {
    let out = git(repo, &["ls-files", "-z"])?;
    Ok(out
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

/// The epoch seconds of the last commit touching `path`, or `None` when no
/// commit does (untracked, staged-only, or a repo with no commits).
fn last_commit_epoch(repo: &Path, path: &str) -> Option<u64> {
    let out = git_maybe(repo, &["log", "-1", "--format=%ct", "--", path])?;
    out.trim().parse().ok()
}

/// First few paths for a one-line message: `a, b, c, … and 2 more`.
fn preview(paths: &[String]) -> String {
    const MAX: usize = 3;
    if paths.len() <= MAX {
        paths.join(", ")
    } else {
        format!(
            "{}, … and {} more",
            paths[..MAX].join(", "),
            paths.len() - MAX
        )
    }
}

/// Test support: build real throwaway git repos and audit contexts.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use crate::audit::AuditCtx;
    use crate::config::Config;

    /// Run `git -C <repo> <args>` with an isolated identity/config,
    /// panicking on failure (tests construct repos; failures are bugs in
    /// the test, not states under test).
    pub(crate) fn git(repo: &Path, args: &[&str]) {
        git_at(repo, args, None);
    }

    /// Like [`git`], with author/committer dates pinned to `epoch` seconds
    /// so tests can order commits in time deterministically.
    pub(crate) fn git_at(repo: &Path, args: &[&str], epoch: Option<u64>) {
        let excludes = format!(
            "core.excludesFile={}",
            repo.join("no-such-excludes").display()
        );
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            // The default excludes file (~/.config/git/ignore) applies even
            // with GIT_CONFIG_GLOBAL unset, so pin it somewhere empty too.
            .args(["-c", &excludes])
            .args(args)
            // Point global/system config at nonexistent files so the
            // developer's real git config can never leak into tests.
            .env("GIT_CONFIG_GLOBAL", repo.join("no-such-global-config"))
            .env("GIT_CONFIG_SYSTEM", repo.join("no-such-system-config"));
        if let Some(epoch) = epoch {
            let date = format!("{epoch} +0000");
            cmd.env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date);
        }
        let output = cmd.output().expect("run git in test repo");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A fresh repo on branch `main` with no commits yet.
    pub(crate) fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo = tmp.path().to_path_buf();
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        (tmp, repo)
    }

    /// Write `content` to `rel` (slash-separated) inside the repo,
    /// creating parent directories.
    pub(crate) fn write(repo: &Path, rel: &str, content: &str) {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write test file");
    }

    /// Stage everything and commit at the given epoch (or "now").
    pub(crate) fn commit_all(repo: &Path, message: &str, epoch: Option<u64>) {
        git(repo, &["add", "-A"]);
        git_at(repo, &["commit", "--quiet", "-m", message], epoch);
    }

    /// An audit context over `repo` with default config.
    pub(crate) fn ctx(repo: &Path) -> AuditCtx {
        AuditCtx {
            repo: repo.to_path_buf(),
            config: Config::default(),
            github: None,
        }
    }

    /// A repo that passes every local check: committed README with the
    /// lab-manual sections and a complete `hpds.toml`.
    pub(crate) fn compliant_repo() -> (tempfile::TempDir, PathBuf) {
        let (tmp, repo) = init_repo();
        write(
            &repo,
            "README.md",
            "# demo\n\n## Description\n\n## File structure\n\n## How to run\n\n## Dependencies\n",
        );
        write(
            &repo,
            "hpds.toml",
            "[project]\nstatus = \"active\"\nprimary-author = \"malcolm\"\n",
        );
        commit_all(&repo, "initial", None);
        (tmp, repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_lists_up_to_three_paths_then_counts_the_rest() {
        let paths: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(preview(&paths[..2]), "a, b");
        assert_eq!(preview(&paths[..3]), "a, b, c");
        assert_eq!(preview(&paths), "a, b, c, … and 2 more");
    }

    #[test]
    fn git_in_a_plain_directory_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = git(tmp.path(), &["status", "--porcelain"]).expect_err("not a repo");
        assert!(err.args.contains("status"));
        let finding = git_failed_finding("dirty-files", &err);
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.remediation.contains("git init"), "{finding:?}");
    }

    #[test]
    fn git_in_a_nonexistent_directory_is_an_error_not_a_panic() {
        let missing = std::env::temp_dir().join("hpds-no-such-dir-for-audit-tests");
        assert!(git(&missing, &["status"]).is_err());
    }

    #[test]
    fn tracked_files_lists_committed_paths_with_slashes() {
        let (_tmp, repo) = testutil::init_repo();
        testutil::write(&repo, "a.txt", "a\n");
        testutil::write(&repo, "sub/dir/b.txt", "b\n");
        testutil::commit_all(&repo, "add files", None);
        let mut tracked = tracked_files(&repo).expect("tracked files");
        tracked.sort();
        assert_eq!(tracked, ["a.txt", "sub/dir/b.txt"]);
    }

    #[test]
    fn tracked_files_is_empty_in_a_repo_with_no_commits() {
        let (_tmp, repo) = testutil::init_repo();
        assert_eq!(
            tracked_files(&repo).expect("empty repo"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn last_commit_epoch_reads_the_pinned_commit_date() {
        let (_tmp, repo) = testutil::init_repo();
        testutil::write(&repo, "a.txt", "a\n");
        testutil::commit_all(&repo, "add a", Some(1_000_000_000));
        assert_eq!(last_commit_epoch(&repo, "a.txt"), Some(1_000_000_000));
        assert_eq!(last_commit_epoch(&repo, "missing.txt"), None);
    }

    #[test]
    fn last_commit_epoch_is_none_with_no_commits() {
        let (_tmp, repo) = testutil::init_repo();
        assert_eq!(last_commit_epoch(&repo, "a.txt"), None);
    }
}
