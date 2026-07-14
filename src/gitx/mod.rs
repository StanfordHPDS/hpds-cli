//! Git and GitHub helpers: ignore vaccination, git defaults, repo
//! creation. Named `gitx` to avoid clashing with the `git` binary/concept.
//!
//! This module returns data only. It never prints to the terminal; all
//! output goes through `ui/`.

pub mod repo;
mod vaccinate;

pub(crate) use vaccinate::{PYTHON_PATTERNS, R_PATTERNS};
pub use vaccinate::{VaccinateReport, vaccinate_global, vaccinate_project};

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::Context;

/// Errors from git helpers. Every message says what to do next.
#[derive(Debug, thiserror::Error)]
pub enum GitxError {
    #[error("`git` was not found on PATH; install it from https://git-scm.com and re-run")]
    GitNotFound(#[source] std::io::Error),

    #[error("could not run `git {args}`: {source}; check that git is installed correctly")]
    GitSpawn {
        args: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "`git {args}` failed{}; check that your git installation and config are intact",
        render_stderr(stderr)
    )]
    GitFailed { args: String, stderr: String },

    #[error(
        "not inside a git repository; run `git init` first, or drop `--project` to vaccinate the global ignore instead"
    )]
    NotARepo,

    #[error(
        "could not determine your home directory; set the HOME environment variable (USERPROFILE on Windows) and re-run"
    )]
    NoHomeDir,

    #[error("could not read {}: {source}; check the file's permissions", path.display())]
    ReadIgnore {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "could not write {}: {source}; check that the directory exists and is writable",
        path.display()
    )]
    WriteIgnore {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn render_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

/// Run `git` with `args` in the current directory and capture its output.
/// A non-zero exit is NOT an error here: callers decide what it means.
fn git_output<S: AsRef<OsStr>>(args: &[S]) -> Result<Output, GitxError> {
    let rendered = || {
        args.iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    Command::new("git")
        .args(args)
        .output()
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => GitxError::GitNotFound(source),
            _ => GitxError::GitSpawn {
                args: rendered(),
                source,
            },
        })
}

/// Run `git` with `args` and fail with a rendered error on non-zero exit.
fn git_expect_success<S: AsRef<OsStr>>(args: &[S]) -> Result<Output, GitxError> {
    let output = git_output(args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitxError::GitFailed {
            args: args
                .iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Run `git init` in `dir`. Fails with a rendered error on non-zero exit.
pub fn git_init(dir: &std::path::Path) -> Result<(), GitxError> {
    let output = Command::new("git")
        .arg("init")
        .current_dir(dir)
        .output()
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => GitxError::GitNotFound(source),
            _ => GitxError::GitSpawn {
                args: "init".to_string(),
                source,
            },
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GitxError::GitFailed {
            args: "init".to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Read one key from the global git config; `None` when unset or empty.
pub fn global_config_get(key: &str) -> Result<Option<String>, GitxError> {
    let output = git_output(&["config", "--global", "--get", key])?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

/// Set one key in the global git config.
pub fn global_config_set(key: &str, value: &str) -> Result<(), GitxError> {
    git_expect_success(&["config", "--global", key, value]).map(|_| ())
}

/// The `gh` executable every GitHub interaction spawns: the `HPDS_GH`
/// override when set (an internal escape hatch mirroring `HPDS_DATA_DIR`;
/// the test suite points it at fake gh scripts so no test can ever reach
/// a real `gh` through PATH), else `gh` resolved from PATH as usual.
pub(crate) fn gh_program() -> std::ffi::OsString {
    gh_program_from(std::env::var_os("HPDS_GH"))
}

/// Pure core of [`gh_program`], factored out so tests never mutate
/// process-global environment variables.
fn gh_program_from(override_path: Option<std::ffi::OsString>) -> std::ffi::OsString {
    override_path
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("gh"))
}

/// Auth state of the GitHub CLI, as probed by [`gh_auth`].
pub enum GhAuth {
    /// `gh auth status` succeeded: a user is logged in.
    Authenticated,
    /// `gh` ran but reports no login; the raw output carries gh's own
    /// message for callers that want the detail.
    Unauthenticated(Output),
    /// No `gh` executable was found on PATH.
    NotInstalled,
}

/// Probe the GitHub CLI's auth state via `gh auth status`. All three
/// expected states are data, not errors; only an unexpected spawn failure
/// (not "gh missing") is an `Err`.
pub fn gh_auth() -> anyhow::Result<GhAuth> {
    match Command::new(gh_program()).args(["auth", "status"]).output() {
        Ok(out) if out.status.success() => Ok(GhAuth::Authenticated),
        Ok(out) => Ok(GhAuth::Unauthenticated(out)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(GhAuth::NotInstalled),
        Err(err) => {
            Err(err).context("could not run `gh auth status`; check that gh is installed correctly")
        }
    }
}

/// The GitHub login of the user `gh` is authenticated as, via
/// `gh api user -q .login`. `None` when gh is missing, unauthenticated,
/// offline, or otherwise cannot answer; callers treat the login as a
/// best-effort default, never a hard requirement.
pub fn gh_login() -> Option<String> {
    let output = Command::new(gh_program())
        .args(["api", "user", "-q", ".login"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!login.is_empty()).then_some(login)
}

/// The user's home directory, from `HOME` (or `USERPROFILE` on Windows).
fn home_dir() -> Result<PathBuf, GitxError> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(GitxError::NoHomeDir)
}

#[cfg(test)]
mod gh_program_tests {
    use super::*;

    #[test]
    fn gh_program_defaults_to_path_lookup() {
        assert_eq!(gh_program_from(None), std::ffi::OsString::from("gh"));
    }

    #[test]
    fn gh_program_override_replaces_the_path_lookup() {
        assert_eq!(
            gh_program_from(Some("/fixtures/fake-gh".into())),
            std::ffi::OsString::from("/fixtures/fake-gh")
        );
    }

    #[test]
    fn empty_gh_program_override_is_ignored() {
        assert_eq!(
            gh_program_from(Some(std::ffi::OsString::new())),
            std::ffi::OsString::from("gh")
        );
    }
}
