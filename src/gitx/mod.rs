//! Git and GitHub helpers: ignore vaccination, git defaults, repo
//! creation. Named `gitx` to avoid clashing with the `git` binary/concept.
//!
//! This module returns data only. It never prints to the terminal — all
//! output goes through `ui/`.

mod vaccinate;

pub use vaccinate::{vaccinate_global, vaccinate_project};

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};

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
/// A non-zero exit is NOT an error here — callers decide what it means.
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

/// The user's home directory, from `HOME` (or `USERPROFILE` on Windows).
fn home_dir() -> Result<PathBuf, GitxError> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(GitxError::NoHomeDir)
}
