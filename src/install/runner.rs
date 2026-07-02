//! Process-execution seam for installers.
//!
//! Installers never call `std::process::Command` or probe `PATH` directly;
//! they go through [`CommandRunner`] so unit tests can fake both process
//! output and which binaries appear to be installed.

use std::ffi::OsStr;
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
        let output = std::process::Command::new(program)
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
/// executable suffix. Factored out of env access so it is unit-testable.
fn which_in(path: &OsStr, program: &str) -> Option<PathBuf> {
    let file_name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(&file_name))
        .find(|candidate| candidate.is_file())
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
