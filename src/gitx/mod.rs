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

use crate::install::logged_output;

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
        "could not run `git {args}` in {}: the directory does not exist; check the path and re-run",
        dir.display()
    )]
    WorkDirMissing {
        dir: PathBuf,
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

/// Run `git` with `args` in the current directory and capture its output,
/// logging the command block (see [`crate::install::logged_output`]).
/// A non-zero exit is NOT an error here: callers decide what it means.
fn git_output<S: AsRef<OsStr>>(args: &[S]) -> Result<Output, GitxError> {
    logged_output("git", args, Command::new("git").args(args))
        .map_err(|source| spawn_error(&render_args(args), source))
}

/// `args` joined with spaces, for error messages.
fn render_args<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The error for a `git` process that could not be spawned: a missing
/// executable is [`GitxError::GitNotFound`], anything else
/// [`GitxError::GitSpawn`].
fn spawn_error(args: &str, source: std::io::Error) -> GitxError {
    match source.kind() {
        std::io::ErrorKind::NotFound => GitxError::GitNotFound(source),
        _ => GitxError::GitSpawn {
            args: args.to_string(),
            source,
        },
    }
}

/// Run `git` with `args` and fail with a rendered error on non-zero exit.
fn git_expect_success<S: AsRef<OsStr>>(args: &[S]) -> Result<Output, GitxError> {
    let output = git_output(args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitxError::GitFailed {
            args: render_args(args),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Run `git init` in `dir`, logging the command block. Fails with a
/// rendered error on non-zero exit, and with [`GitxError::WorkDirMissing`]
/// when `dir` does not exist.
pub fn git_init(dir: &std::path::Path) -> Result<(), GitxError> {
    let output = logged_output(
        "git",
        &["init"],
        Command::new("git").arg("init").current_dir(dir),
    )
    .map_err(|source| {
        if dir.exists() {
            spawn_error("init", source)
        } else {
            GitxError::WorkDirMissing {
                dir: dir.to_path_buf(),
                args: "init".to_string(),
                source,
            }
        }
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

/// Auth state of the GitHub CLI, as probed by [`gh_auth`]. The state
/// reflects only the exit code of `gh auth status`; the audit's GitHub
/// probe additionally trusts an environment token inside GitHub Actions.
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
    gh_auth_with(&gh_program())
}

/// [`gh_auth`] with the `gh` executable given, so tests choose it without
/// touching the environment. The command block is logged.
fn gh_auth_with(program: &OsStr) -> anyhow::Result<GhAuth> {
    let args = ["auth", "status"];
    match logged_output(
        &program.to_string_lossy(),
        &args,
        Command::new(program).args(args),
    ) {
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
    gh_login_with(&gh_program(), |_| {})
}

/// [`gh_login`] with the `gh` executable given, so tests choose it without
/// touching the environment. The command block is logged.
fn gh_login_with(program: &OsStr, configure: impl FnOnce(&mut Command)) -> Option<String> {
    let args = ["api", "user", "-q", ".login"];
    let mut command = Command::new(program);
    command.args(args);
    configure(&mut command);
    let output = logged_output(&program.to_string_lossy(), &args, &mut command).ok()?;
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

#[cfg(test)]
mod logging_tests {
    use super::*;
    use crate::install::shell_word;
    use crate::ui;
    use crate::ui::{read_log, temp_log};

    /// A git config file holding `user.name = Ada Lovelace`, written in its
    /// own temp dir so no test reads or writes the real global config.
    fn config_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gitconfig");
        std::fs::write(&path, "[user]\n\tname = Ada Lovelace\n").unwrap();
        (dir, path)
    }

    /// The log-only lines `f` writes, as recorded inside a capture.
    fn captured<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
        let (value, transcript) = ui::capture(f);
        let lines = transcript
            .entries()
            .iter()
            .flat_map(|line| line.message.lines().map(str::to_string).collect::<Vec<_>>())
            .collect();
        (value, lines)
    }

    /// `text` as a log block's lines: `label:` then each line indented.
    fn block(label: &str, text: &str) -> String {
        let mut out = format!("{label}:\n");
        for line in text.lines() {
            out.push_str(&format!("  {line}\n"));
        }
        out
    }

    #[test]
    fn a_config_get_is_recorded_in_a_capture_with_its_stdout() {
        let (_dir, config) = config_file();
        let config = config.to_str().unwrap().to_string();
        let args = ["config", "--file", config.as_str(), "--get", "user.name"];
        let quoted = shell_word(&config);
        let (output, lines) = captured(|| git_output(&args));
        let output = output.unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "Ada Lovelace\n");
        assert_eq!(
            lines,
            [
                format!("$ git config --file {quoted} --get user.name"),
                "status: success".to_string(),
                "stdout:".to_string(),
                "  Ada Lovelace".to_string(),
            ]
        );
    }

    #[test]
    fn a_config_get_reaches_the_open_log_and_not_the_terminal() {
        let (_dir, config) = config_file();
        let config = config.to_str().unwrap().to_string();
        let quoted = shell_word(&config);
        let (_log_dir, log) = temp_log();
        let (output, written) = ui::tap_terminal(|| {
            git_output(&["config", "--file", config.as_str(), "--get", "user.name"])
        });
        assert!(output.unwrap().status.success());
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(
            read_log(&log),
            format!(
                "$ git config --file {quoted} --get user.name\n\
                 status: success\n\
                 stdout:\n  Ada Lovelace\n"
            )
        );
    }

    #[test]
    fn a_missing_key_logs_a_failed_status_and_no_output_blocks() {
        let (_dir, config) = config_file();
        let config = config.to_str().unwrap().to_string();
        let quoted = shell_word(&config);
        let (_log_dir, log) = temp_log();
        let (output, written) = ui::tap_terminal(|| {
            git_output(&["config", "--file", config.as_str(), "--get", "hpds.missing"])
        });
        let output = output.unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(
            read_log(&log),
            format!("$ git config --file {quoted} --get hpds.missing\nstatus: failed\n")
        );
    }

    #[test]
    fn a_failing_git_call_logs_its_stderr_and_returns_the_error_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let missing = missing.to_str().unwrap().to_string();
        let (_log_dir, log) = temp_log();
        let (result, written) =
            ui::tap_terminal(|| git_expect_success(&["-C", missing.as_str(), "status"]));
        assert!(written.is_empty(), "written was: {written:?}");
        let err = result.expect_err("git -C <missing dir> must fail");
        let GitxError::GitFailed { args, stderr } = &err else {
            panic!("expected GitFailed, got {err:?}");
        };
        assert_eq!(args, &format!("-C {missing} status"));
        assert!(stderr.starts_with("fatal:"), "stderr was: {stderr:?}");
        assert_eq!(
            err.to_string(),
            format!(
                "`git -C {missing} status` failed: {}; check that your git installation \
                 and config are intact",
                stderr.trim()
            )
        );
        assert_eq!(
            read_log(&log),
            format!(
                "$ git -C {} status\nstatus: failed\n{}",
                shell_word(&missing),
                block("stderr", stderr)
            )
        );
    }

    #[test]
    fn global_config_get_logs_the_command_it_runs() {
        // Read-only: the value (if any) of the real global key is not
        // asserted, only that the command and its status reach the log.
        let (result, lines) = captured(|| global_config_get("user.name"));
        result.unwrap();
        assert_eq!(
            lines.first().map(String::as_str),
            Some("$ git config --global --get user.name")
        );
        assert!(
            lines
                .get(1)
                .is_some_and(|line| line.starts_with("status: ")),
            "lines were: {lines:?}"
        );
    }

    #[test]
    fn git_init_logs_the_command_and_its_status() {
        let dir = tempfile::tempdir().unwrap();
        let (_log_dir, log) = temp_log();
        let (result, written) = ui::tap_terminal(|| git_init(dir.path()));
        result.unwrap();
        assert!(written.is_empty(), "written was: {written:?}");
        let logged = read_log(&log);
        assert!(
            logged.starts_with("$ git init\nstatus: success\nstdout:\n  "),
            "log was: {logged:?}"
        );
    }

    #[test]
    fn git_init_logs_a_spawn_error_and_returns_it_unchanged() {
        // A missing working directory makes the spawn itself fail.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let (_log_dir, log) = temp_log();
        let (result, written) = ui::tap_terminal(|| git_init(&missing));
        assert!(written.is_empty(), "written was: {written:?}");
        let err = result.expect_err("git init in a missing dir must fail to start");
        let GitxError::WorkDirMissing { source, .. } = &err else {
            panic!("expected a missing-directory error, got {err:?}");
        };
        assert_eq!(
            read_log(&log),
            format!("$ git init\nfailed to start: {source}\n")
        );
    }

    #[test]
    fn git_init_in_a_missing_dir_names_the_dir_not_a_missing_git() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let (err, _) = captured(|| git_init(&missing));
        let err = err.expect_err("git init in a missing dir must fail");
        let GitxError::WorkDirMissing {
            dir: named, args, ..
        } = &err
        else {
            panic!("expected WorkDirMissing, got {err:?}");
        };
        assert_eq!(named, &missing);
        assert_eq!(args, "init");
        assert_eq!(
            err.to_string(),
            format!(
                "could not run `git init` in {}: the directory does not exist; \
                 check the path and re-run",
                missing.display()
            )
        );
    }

    #[test]
    fn gh_auth_with_a_missing_gh_logs_the_spawn_error_and_reports_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("no-such-gh");
        let (_log_dir, log) = temp_log();
        let (result, written) = ui::tap_terminal(|| gh_auth_with(gh.as_os_str()));
        assert!(written.is_empty(), "written was: {written:?}");
        assert!(matches!(result, Ok(GhAuth::NotInstalled)));
        let logged = read_log(&log);
        let start = format!(
            "$ {} auth status\nfailed to start: ",
            shell_word(&gh.to_string_lossy())
        );
        assert!(logged.starts_with(&start), "log was: {logged:?}");
        assert_eq!(logged.lines().count(), 2, "log was: {logged:?}");
    }

    #[test]
    fn a_gh_invocation_that_runs_and_fails_logs_its_status_and_stderr() {
        // `git` stands in for gh: it runs everywhere the suite runs and
        // rejects the unknown `api` subcommand on stderr, which is the
        // shape of a gh call that started and failed.
        // The command runs in a temp dir with git's system and global config
        // pointed at nothing, set on this `Command` only, so an `alias.api`
        // or autocorrect setting on the machine cannot change the outcome.
        let tmp = tempfile::tempdir().unwrap();
        let empty_config = tmp.path().join("empty-gitconfig");
        std::fs::write(&empty_config, "").unwrap();
        let (login, lines) = captured(|| {
            gh_login_with(OsStr::new("git"), |command| {
                command
                    .current_dir(tmp.path())
                    .env("GIT_CONFIG_GLOBAL", &empty_config)
                    .env("GIT_CONFIG_NOSYSTEM", "1");
            })
        });
        assert_eq!(login, None);
        assert_eq!(
            lines.get(..3).map(<[String]>::to_vec),
            Some(vec![
                "$ git api user -q .login".to_string(),
                "status: failed".to_string(),
                "stderr:".to_string(),
            ]),
            "lines were: {lines:?}"
        );
        assert!(
            lines.get(3).is_some_and(|line| line.contains("api")),
            "lines were: {lines:?}"
        );
    }
}
