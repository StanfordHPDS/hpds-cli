//! Process-execution seam for installers.
//!
//! Installers never call `std::process::Command` or probe `PATH` directly;
//! they go through [`CommandRunner`] so unit tests can fake both process
//! output and which binaries appear to be installed.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use anyhow::Context;

use crate::ui;

/// Captured result of one finished process.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Whether the process exited successfully.
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// How installers execute processes and probe `PATH`. Production code uses
/// [`SystemRunner`]; tests substitute a fake.
pub trait CommandRunner {
    /// Locate `program` on `PATH`; `None` when it is not installed (or not
    /// on `PATH`).
    fn which(&self, program: &str) -> Option<PathBuf>;

    /// Run `program` with `args`, capturing its output. `Err` means the
    /// process could not be spawned; a process that ran and failed comes
    /// back as `Ok` with `success: false`.
    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput>;
}

/// The real thing: spawns processes and searches the actual `PATH`.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn which(&self, program: &str) -> Option<PathBuf> {
        which_in(std::env::var_os("PATH")?.as_os_str(), program)
    }

    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        // A bare name here would make Windows resolve only `program.exe`
        // (CreateProcess appends `.exe`, not PATHEXT), missing the `.cmd`
        // launchers hpds itself installs. Spawn whatever `which` resolves
        // so every program detection can see, `run` can also run; absent
        // programs keep the bare name so the spawn error still names them.
        let resolved = self
            .which(program)
            .map(PathBuf::into_os_string)
            .unwrap_or_else(|| OsString::from(program));
        let output = std::process::Command::new(&resolved)
            .args(args)
            .output()
            .with_context(|| format!("could not run `{program}`"))?;
        Ok(CommandOutput::from(&output))
    }
}

/// A [`CommandRunner`] that writes every command it runs to the log (see
/// [`crate::ui::log_detail`]): the command line, whether it succeeded, and
/// its stdout and stderr. Nothing reaches the terminal, and the output (or
/// spawn error) the inner runner returns is passed back unchanged.
pub struct LoggingRunner<'a> {
    inner: &'a (dyn CommandRunner + Sync),
}

impl<'a> LoggingRunner<'a> {
    /// Wrap `inner`, logging each command it runs.
    pub fn new(inner: &'a (dyn CommandRunner + Sync)) -> Self {
        Self { inner }
    }
}

impl CommandRunner for LoggingRunner<'_> {
    fn which(&self, program: &str) -> Option<PathBuf> {
        self.inner.which(program)
    }

    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        log_command_start(program, args);
        let result = self.inner.run(program, args);
        match &result {
            Ok(output) => log_command_outcome(Ok(output)),
            Err(err) => log_command_outcome(Err(err)),
        }
        result
    }
}

impl From<&std::process::Output> for CommandOutput {
    /// A finished process's output, with stdout and stderr decoded as
    /// lossy UTF-8.
    fn from(output: &std::process::Output) -> Self {
        Self {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Write the opening line of a command's log block, `$ program args`, with
/// each word shell-quoted as in [`shell_line`]. Shared by [`LoggingRunner`]
/// and every other place that spawns a process directly, so all command
/// blocks in the log read the same.
pub(crate) fn log_command_start<S: AsRef<OsStr>>(program: &str, args: &[S]) {
    let words: Vec<String> = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    ui::log_detail(&format!("$ {}", shell_line(program, &words)));
}

/// Write the rest of a command's log block: on `Ok`, `status: success` or
/// `status: failed` followed by its `stdout:` and `stderr:` blocks (each
/// omitted when whitespace-only); on `Err`, the spawn error as
/// `failed to start: {err:#}`.
pub(crate) fn log_command_outcome(outcome: Result<&CommandOutput, &dyn std::fmt::Display>) {
    match outcome {
        Ok(output) => {
            let status = if output.success { "success" } else { "failed" };
            ui::log_detail(&format!("status: {status}"));
            log_block("stdout", &output.stdout);
            log_block("stderr", &output.stderr);
        }
        Err(err) => ui::log_detail(&format!("failed to start: {err:#}")),
    }
}

/// Spawn `command`, wait for it, and log it as a command block via
/// [`log_command_start`] and [`log_command_outcome`]: `program` and `args`
/// name it in the `$` line and must match what `command` was built with.
/// The output, or the raw spawn error, is returned unchanged.
pub(crate) fn logged_output<S: AsRef<OsStr>>(
    program: &str,
    args: &[S],
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Output> {
    log_command_start(program, args);
    let result = command.output();
    match &result {
        Ok(output) => log_command_outcome(Ok(&CommandOutput::from(output))),
        Err(err) => log_command_outcome(Err(err)),
    }
    result
}

/// Log `text` under a `label:` line with each line indented two spaces;
/// nothing at all when `text` holds only whitespace. A carriage return
/// counts as a line break (see [`output_lines`]), so each update of a
/// progress line keeps a line of its own.
fn log_block(label: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    ui::log_detail(&format!("{label}:"));
    for line in output_lines(text) {
        ui::log_detail(&format!("  {line}"));
    }
}

/// The lines of a command's output, splitting on `\n`, `\r\n`, and a lone
/// `\r`. Progress output rewrites its line with `\r`; split only on `\n`,
/// each update would land on one line and a terminal would show only the
/// last. A `\r\n` pair yields one break, not an empty line between.
fn output_lines(text: &str) -> impl Iterator<Item = &str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut rest = Some(text).filter(|text| !text.is_empty());
    std::iter::from_fn(move || {
        let current = rest?;
        match current.find(['\r', '\n']) {
            Some(at) => {
                let (line, tail) = current.split_at(at);
                let tail = tail
                    .strip_prefix("\r\n")
                    .or_else(|| tail.get(1..))
                    .unwrap_or("");
                rest = Some(tail);
                Some(line)
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}

/// `program args` as a POSIX shell would read it back: each word bare when
/// it holds only characters a shell takes literally, otherwise single-quoted.
fn shell_line(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(shell_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One shell word: bare for `[A-Za-z0-9-_./=:,+@%]`, else single-quoted with
/// each `'` written as `'\''`. The empty word is `''`.
pub(crate) fn shell_word(word: &str) -> String {
    let bare = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if bare {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// How many non-empty lines of a failed command's output an error carries.
const FAILURE_TAIL_LINES: usize = 10;

/// The last [`FAILURE_TAIL_LINES`] non-empty lines of `text`, each with
/// trailing whitespace trimmed, and how many non-empty lines it holds in
/// all. Only the tail is kept while counting, however long the output.
fn output_tail(text: &str) -> (VecDeque<&str>, usize) {
    let mut tail = VecDeque::with_capacity(FAILURE_TAIL_LINES);
    let mut total = 0;
    for line in output_lines(text).filter(|line| !line.trim().is_empty()) {
        total += 1;
        if tail.len() == FAILURE_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line.trim_end());
    }
    (tail, total)
}

/// The error for a command that ran and failed: `headline`, then the last
/// ten non-empty lines of its stderr (or of its stdout when stderr has no
/// non-empty line), then the path of the log, which holds the full output
/// (see [`ui::log_path`]). Lines split as in [`output_lines`], so a lone
/// `\r` ends a line. A line is non-empty when it holds anything but
/// whitespace (`!line.trim().is_empty()`); only those lines count toward
/// the ten and toward the total that a truncated tail reports as
/// `(last 10 of N lines)`.
pub(crate) fn command_failure(headline: &str, output: &CommandOutput) -> anyhow::Error {
    let (mut tail, mut total) = output_tail(&output.stderr);
    if total == 0 {
        (tail, total) = output_tail(&output.stdout);
    }
    let mut message = headline.to_string();
    if total > tail.len() {
        message.push_str(&format!(" (last {} of {total} lines)", tail.len()));
    }
    if !tail.is_empty() {
        message.push_str(": ");
        message.push_str(&tail.make_contiguous().join("\n"));
    }
    if let Some(path) = ui::log_path() {
        message.push_str(&format!("\nfull output: {}", path.display()));
    }
    anyhow::anyhow!(message)
}

/// Search a `PATH`-style value for `program`, honoring the platform's
/// executable suffixes: none on Unix, the `PATHEXT` list on Windows (so
/// the `.cmd` launchers hpds installs are found, not just `.exe`).
/// Factored out of env access so it is unit-testable.
pub(crate) fn which_in(path: &OsStr, program: &str) -> Option<PathBuf> {
    which_in_with_suffixes(path, program, &executable_suffixes())
}

/// The core `PATH` search: each directory in order, trying `program`
/// with each suffix. Directory order outranks suffix order, matching how
/// Windows resolves commands.
fn which_in_with_suffixes(path: &OsStr, program: &str, suffixes: &[String]) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            suffixes
                .iter()
                .map(|suffix| dir.join(format!("{program}{suffix}")))
                .find(|candidate| candidate.is_file())
        })
}

/// The executable suffixes this platform's `PATH` search must try.
#[cfg(not(windows))]
fn executable_suffixes() -> Vec<String> {
    vec![String::new()]
}

/// The executable suffixes this platform's `PATH` search must try.
#[cfg(windows)]
fn executable_suffixes() -> Vec<String> {
    suffixes_from_pathext(std::env::var_os("PATHEXT"))
}

/// Parse a `PATHEXT`-style value (`.COM;.EXE;.BAT;.CMD`) into lowercase
/// suffixes, falling back to the Windows defaults when it is unset or
/// holds nothing usable. Production code calls this only on Windows;
/// compiled under `test` everywhere so the parsing is covered on every
/// development platform.
#[cfg(any(windows, test))]
fn suffixes_from_pathext(pathext: Option<OsString>) -> Vec<String> {
    const DEFAULTS: &[&str] = &[".com", ".exe", ".bat", ".cmd"];
    let parsed: Vec<String> = pathext
        .as_deref()
        .and_then(OsStr::to_str)
        .map(|value| {
            value
                .split(';')
                .map(str::trim)
                .filter(|ext| ext.len() > 1 && ext.starts_with('.'))
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        DEFAULTS.iter().map(|ext| ext.to_string()).collect()
    } else {
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_in_finds_a_binary_on_the_given_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let name = format!("hpds-probe{}", std::env::consts::EXE_SUFFIX);
        let binary = dir.path().join(&name);
        std::fs::write(&binary, "").expect("write fake binary");

        let path = std::env::join_paths([dir.path()]).expect("join PATH");
        assert_eq!(which_in(&path, "hpds-probe"), Some(binary));
    }

    #[test]
    fn which_in_misses_a_program_that_is_not_there() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = std::env::join_paths([dir.path()]).expect("join PATH");
        assert!(which_in(&path, "hpds-probe").is_none());
    }

    #[test]
    fn which_in_skips_directories_over_a_matching_plain_file() {
        // A directory named like the program must not count as a hit.
        let dir = tempfile::tempdir().expect("create temp dir");
        let name = format!("hpds-probe{}", std::env::consts::EXE_SUFFIX);
        std::fs::create_dir(dir.path().join(&name)).expect("create decoy dir");

        let path = std::env::join_paths([dir.path()]).expect("join PATH");
        assert!(which_in(&path, "hpds-probe").is_none());
    }

    #[test]
    fn which_in_finds_a_cmd_shim_under_windows_suffixes() {
        // Regression: the Windows no-winget quarto fallback installs a
        // `.cmd` launcher; probing only `quarto.exe` made hpds blind to
        // its own install (verification failed, reruns re-downloaded,
        // and tinytex refused to see quarto).
        let dir = tempfile::tempdir().expect("create temp dir");
        let shim = dir.path().join("quarto.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let path = std::env::join_paths([dir.path()]).expect("join PATH");
        let suffixes = suffixes_from_pathext(None);
        assert_eq!(
            which_in_with_suffixes(&path, "quarto", &suffixes),
            Some(shim)
        );
    }

    #[test]
    fn which_in_resolution_is_directory_major_like_windows() {
        // An earlier PATH directory wins even when only a later-priority
        // suffix matches there, which is how Windows itself resolves commands.
        let first = tempfile::tempdir().expect("create temp dir");
        let second = tempfile::tempdir().expect("create temp dir");
        let shim = first.path().join("quarto.cmd");
        std::fs::write(&shim, "").expect("write shim");
        std::fs::write(second.path().join("quarto.exe"), "").expect("write exe");

        let path = std::env::join_paths([first.path(), second.path()]).expect("join PATH");
        let suffixes = vec![".exe".to_string(), ".cmd".to_string()];
        assert_eq!(
            which_in_with_suffixes(&path, "quarto", &suffixes),
            Some(shim)
        );
    }

    #[test]
    fn suffixes_from_pathext_parses_and_lowercases_the_list() {
        let got = suffixes_from_pathext(Some(".COM;.EXE;.BAT;.CMD".into()));
        assert_eq!(got, vec![".com", ".exe", ".bat", ".cmd"]);
    }

    #[test]
    fn suffixes_from_pathext_skips_blank_and_malformed_entries() {
        let got = suffixes_from_pathext(Some(";.EXE;; ;garbage;.CMD;".into()));
        assert_eq!(got, vec![".exe", ".cmd"]);
    }

    #[test]
    fn suffixes_from_pathext_defaults_when_unset_or_useless() {
        for pathext in [None, Some("".into()), Some(";;".into())] {
            let got = suffixes_from_pathext(pathext);
            assert!(got.contains(&".exe".to_string()), "{got:?}");
            assert!(got.contains(&".cmd".to_string()), "{got:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn which_in_finds_a_cmd_shim_on_a_real_windows_host() {
        // End-to-end on Windows: the launcher name hpds installs must be
        // visible through the production probe (real PATHEXT handling).
        let dir = tempfile::tempdir().expect("create temp dir");
        let shim = dir.path().join("quarto.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let path = std::env::join_paths([dir.path()]).expect("join PATH");
        assert_eq!(which_in(&path, "quarto"), Some(shim));
    }

    #[test]
    fn system_which_misses_a_program_that_cannot_exist() {
        assert!(
            SystemRunner
                .which("hpds-definitely-not-a-real-program")
                .is_none()
        );
    }

    #[test]
    fn run_captures_stdout_of_a_real_process() {
        // `git --version` is a safe universal probe: the repo's own tooling
        // already requires git on every dev machine and in CI.
        let out = SystemRunner
            .run("git", &["--version"])
            .expect("git must spawn");
        assert!(out.success);
        assert!(out.stdout.contains("git version"), "{}", out.stdout);
    }

    mod logging_runner {
        use super::super::*;
        use crate::install::test_support::FakeRunner;
        use crate::ui;
        use crate::ui::{read_log, temp_log};

        #[test]
        fn is_send_and_sync_so_it_can_serve_the_background_lane() {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<LoggingRunner<'static>>();
        }

        #[test]
        fn fits_both_runner_seams() {
            let fake = FakeRunner::default().with_output("id -nG analyst", "analyst sudo\n");
            let logging = LoggingRunner::new(&fake);
            let shared: &(dyn CommandRunner + Sync) = &logging;
            let plain: &dyn CommandRunner = &logging;
            assert!(shared.which("rustup").is_none());
            assert!(plain.run("id", &["-nG", "analyst"]).unwrap().success);
        }

        #[test]
        fn delegates_which_to_the_inner_runner() {
            let fake = FakeRunner::default().on_path("quarto");
            let logging = LoggingRunner::new(&fake);
            assert_eq!(
                logging.which("quarto"),
                Some(PathBuf::from("/fake/bin/quarto"))
            );
            assert_eq!(logging.which("rustup"), None);
        }

        #[test]
        fn logs_a_successful_command_with_its_output_and_nothing_on_the_terminal() {
            let (_dir, path) = temp_log();
            let fake = FakeRunner::default().with_output("dpkg --print-architecture", "amd64\n");
            let logging = LoggingRunner::new(&fake);
            let (out, written) =
                ui::tap_terminal(|| logging.run("dpkg", &["--print-architecture"]));
            let out = out.unwrap();
            assert!(written.is_empty(), "written was: {written:?}");
            assert!(out.success);
            assert_eq!(out.stdout, "amd64\n");
            assert_eq!(out.stderr, "");
            assert_eq!(fake.calls(), ["dpkg --print-architecture"]);
            assert_eq!(
                read_log(&path),
                "$ dpkg --print-architecture\n\
                 status: success\n\
                 stdout:\n  amd64\n"
            );
        }

        #[test]
        fn logs_a_failed_command_with_stdout_and_stderr_labeled() {
            let (_dir, path) = temp_log();
            let fake = FakeRunner::default().with_failure_output(
                "rustup update stable",
                "info: syncing channel updates\ninfo: downloading component\n",
                "error: could not download\nerror: caused by: timeout\n",
            );
            let logging = LoggingRunner::new(&fake);
            let (out, written) = ui::tap_terminal(|| logging.run("rustup", &["update", "stable"]));
            let out = out.unwrap();
            assert!(written.is_empty(), "written was: {written:?}");
            assert!(!out.success);
            assert_eq!(
                out.stdout,
                "info: syncing channel updates\ninfo: downloading component\n"
            );
            assert_eq!(
                out.stderr,
                "error: could not download\nerror: caused by: timeout\n"
            );
            assert_eq!(
                read_log(&path),
                "$ rustup update stable\n\
                 status: failed\n\
                 stdout:\n\
                 \x20 info: syncing channel updates\n\
                 \x20 info: downloading component\n\
                 stderr:\n\
                 \x20 error: could not download\n\
                 \x20 error: caused by: timeout\n"
            );
        }

        #[test]
        fn omits_empty_stdout_and_stderr() {
            let (_dir, path) = temp_log();
            let fake = FakeRunner::default().with_output("sudo systemctl restart docker", "");
            let logging = LoggingRunner::new(&fake);
            logging
                .run("sudo", &["systemctl", "restart", "docker"])
                .unwrap();
            assert_eq!(
                read_log(&path),
                "$ sudo systemctl restart docker\nstatus: success\n"
            );
        }

        #[test]
        fn quotes_arguments_a_shell_would_split_or_expand() {
            let (_dir, path) = temp_log();
            let key = "sudo env DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confold install r base  it's /usr/bin/x a,b+c@d%e";
            let fake = FakeRunner::default().with_output(key, "");
            let logging = LoggingRunner::new(&fake);
            logging
                .run(
                    "sudo",
                    &[
                        "env",
                        "DEBIAN_FRONTEND=noninteractive",
                        "apt-get",
                        "-o",
                        "Dpkg::Options::=--force-confold",
                        "install",
                        "r base",
                        "",
                        "it's",
                        "/usr/bin/x",
                        "a,b+c@d%e",
                    ],
                )
                .unwrap();
            assert_eq!(
                read_log(&path),
                "$ sudo env DEBIAN_FRONTEND=noninteractive apt-get -o \
                 Dpkg::Options::=--force-confold install 'r base' '' 'it'\\''s' \
                 /usr/bin/x a,b+c@d%e\n\
                 status: success\n"
            );
        }

        #[test]
        fn logs_a_spawn_error_and_returns_it_unchanged() {
            let (_dir, path) = temp_log();
            let fake = FakeRunner::default();
            let logging = LoggingRunner::new(&fake);
            let (result, written) =
                ui::tap_terminal(|| logging.run("hpds-missing", &["--version"]));
            let err = result.expect_err("an unrecorded command must fail to spawn");
            assert!(written.is_empty(), "written was: {written:?}");
            assert_eq!(
                err.to_string(),
                "no fake output recorded for `hpds-missing --version`"
            );
            assert_eq!(
                read_log(&path),
                "$ hpds-missing --version\n\
                 failed to start: no fake output recorded for `hpds-missing --version`\n"
            );
        }

        /// A runner whose every spawn fails with an OS cause under context.
        struct UnstartableRunner;

        impl CommandRunner for UnstartableRunner {
            fn which(&self, _program: &str) -> Option<PathBuf> {
                None
            }

            fn run(&self, program: &str, _args: &[&str]) -> anyhow::Result<CommandOutput> {
                Err(anyhow::Error::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ))
                .context(format!("could not run `{program}`")))
            }
        }

        #[test]
        fn logs_the_cause_of_a_spawn_error() {
            let (_dir, path) = temp_log();
            let logging = LoggingRunner::new(&UnstartableRunner);
            let err = logging
                .run("rustup", &["--version"])
                .expect_err("the spawn must fail");
            assert_eq!(err.to_string(), "could not run `rustup`");
            assert_eq!(
                read_log(&path),
                "$ rustup --version\n\
                 failed to start: could not run `rustup`: permission denied\n"
            );
        }

        #[test]
        fn whitespace_only_output_logs_no_block() {
            let (_dir, path) = temp_log();
            let fake =
                FakeRunner::default().with_failure_output("apt-get update", "\n\n", " \n\t\n");
            let logging = LoggingRunner::new(&fake);
            logging.run("apt-get", &["update"]).unwrap();
            assert_eq!(read_log(&path), "$ apt-get update\nstatus: failed\n");
        }

        #[test]
        fn carriage_returns_start_new_lines_in_the_log() {
            let (_dir, path) = temp_log();
            let fake = FakeRunner::default()
                .with_output("curl -o code-server.deb", " 10%\r 50%\r100%\r\ndone\r\n");
            let logging = LoggingRunner::new(&fake);
            logging.run("curl", &["-o", "code-server.deb"]).unwrap();
            assert_eq!(
                read_log(&path),
                "$ curl -o code-server.deb\n\
                 status: success\n\
                 stdout:\n\
                 \x20  10%\n\
                 \x20  50%\n\
                 \x20 100%\n\
                 \x20 done\n"
            );
        }

        #[test]
        fn a_failure_on_a_worker_thread_names_the_spawning_threads_log() {
            let (_dir, path) = temp_log();
            let inherited = ui::log_path();
            let fake = FakeRunner::default().with_failure("rustup check", "error: offline\n");
            let logging = LoggingRunner::new(&fake);
            let (message, transcript) = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        ui::with_log_path(inherited, || {
                            ui::capture(|| {
                                let out = logging.run("rustup", &["check"]).unwrap();
                                command_failure("`rustup` failed", &out).to_string()
                            })
                        })
                    })
                    .join()
                    .unwrap()
            });
            ui::replay(&transcript);
            assert_eq!(
                message,
                format!(
                    "`rustup` failed: error: offline\nfull output: {}",
                    path.display()
                )
            );
            assert_eq!(
                read_log(&path),
                "$ rustup check\n\
                 status: failed\n\
                 stderr:\n\
                 \x20 error: offline\n"
            );
        }

        /// The log-only lines `f` writes, as recorded inside a capture.
        fn captured_lines(f: impl FnOnce()) -> Vec<String> {
            let ((), transcript) = ui::capture(f);
            transcript
                .entries()
                .iter()
                .flat_map(|line| line.message.lines().map(str::to_string).collect::<Vec<_>>())
                .collect()
        }

        #[test]
        fn shared_helpers_write_the_same_block_as_the_logging_runner() {
            let stdout = "info: syncing\r\ninfo: done\n";
            let stderr = "warning: it's slow\n";
            let fake = FakeRunner::default().with_failure_output(
                "rustup update stable channel",
                stdout,
                stderr,
            );
            let logging = LoggingRunner::new(&fake);
            let from_runner = captured_lines(|| {
                logging
                    .run("rustup", &["update", "stable channel"])
                    .unwrap();
            });
            let output = super::failed(stdout, stderr);
            let from_helpers = captured_lines(|| {
                log_command_start("rustup", &["update", "stable channel"]);
                log_command_outcome(Ok(&output));
            });
            assert_eq!(
                from_runner,
                [
                    "$ rustup update 'stable channel'",
                    "status: failed",
                    "stdout:",
                    "  info: syncing",
                    "  info: done",
                    "stderr:",
                    "  warning: it's slow",
                ]
            );
            assert_eq!(from_helpers, from_runner);
        }

        #[test]
        fn shared_helpers_log_a_successful_command_with_os_string_args() {
            let output = CommandOutput {
                success: true,
                stdout: "\n".to_string(),
                stderr: String::new(),
            };
            let path = std::ffi::OsString::from("/home/analyst/.gitignore");
            let lines = captured_lines(|| {
                log_command_start(
                    "git",
                    &[
                        OsStr::new("config"),
                        OsStr::new("--global"),
                        OsStr::new("core.excludesFile"),
                        path.as_os_str(),
                    ],
                );
                log_command_outcome(Ok(&output));
            });
            assert_eq!(
                lines,
                [
                    "$ git config --global core.excludesFile /home/analyst/.gitignore",
                    "status: success",
                ]
            );
        }

        #[test]
        fn shared_helpers_log_a_spawn_error_with_its_causes() {
            let err = anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied",
            ))
            .context("could not run `git`");
            let (_dir, path) = temp_log();
            let ((), written) = ui::tap_terminal(|| {
                log_command_start("git", &["init"]);
                log_command_outcome(Err(&err));
            });
            assert!(written.is_empty(), "written was: {written:?}");
            assert_eq!(
                read_log(&path),
                "$ git init\n\
                 failed to start: could not run `git`: permission denied\n"
            );
        }

        #[test]
        fn lines_are_recorded_in_a_capture_so_they_replay_with_their_step() {
            let fake = FakeRunner::default().with_failure("rustup check", "error: offline\n");
            let logging = LoggingRunner::new(&fake);
            let (result, transcript) = ui::capture(|| logging.run("rustup", &["check"]));
            assert!(!result.unwrap().success);
            let messages: Vec<String> = transcript
                .entries()
                .iter()
                .flat_map(|line| line.message.lines().map(str::to_string).collect::<Vec<_>>())
                .collect();
            assert_eq!(
                messages,
                [
                    "$ rustup check",
                    "status: failed",
                    "stderr:",
                    "  error: offline",
                ]
            );
        }
    }

    fn failed(stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn command_failure_treats_carriage_returns_as_line_breaks() {
        let err = command_failure("`curl` failed", &failed("", "a\rb\rc\r\nd\n"));
        assert_eq!(err.to_string(), "`curl` failed: a\nb\nc\nd");
    }

    #[test]
    fn command_failure_counts_carriage_return_lines_toward_the_tail() {
        let progress: String = (1..=12).map(|n| format!("{n:02}%\r")).collect();
        let err = command_failure("`curl` failed", &failed("", &progress));
        let message = err.to_string();
        assert!(
            message.starts_with("`curl` failed (last 10 of 12 lines): 03%\n"),
            "{message}"
        );
        assert!(message.ends_with("\n12%"), "{message}");
    }

    #[test]
    fn output_lines_matches_str_lines_without_carriage_returns() {
        for text in ["", "a", "a\n", "a\n\n", "\na", "a\r\nb\r\n", "a\n\nb"] {
            let ours: Vec<&str> = output_lines(text).collect();
            let std: Vec<&str> = text.lines().collect();
            assert_eq!(ours, std, "text was {text:?}");
        }
    }

    #[test]
    fn run_reports_spawn_failure_as_err() {
        let err = SystemRunner
            .run("hpds-definitely-not-a-real-program", &[])
            .expect_err("spawn must fail");
        assert!(err.to_string().contains("could not run"), "{err}");
    }
}
