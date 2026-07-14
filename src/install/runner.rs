//! Process-execution seam for installers.
//!
//! Installers never call `std::process::Command` or probe `PATH` directly;
//! they go through [`CommandRunner`] so unit tests can fake both process
//! output and which binaries appear to be installed.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use anyhow::Context;

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
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
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

    #[test]
    fn run_reports_spawn_failure_as_err() {
        let err = SystemRunner
            .run("hpds-definitely-not-a-real-program", &[])
            .expect_err("spawn must fail");
        assert!(err.to_string().contains("could not run"), "{err}");
    }
}
