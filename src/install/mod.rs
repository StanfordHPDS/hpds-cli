//! The installer framework behind `hpds install <tool>`.
//!
//! Each tool implements [`Installer`]; [`run_installer`] drives the shared
//! flow: detect an existing install (idempotent no-op), announce what will
//! happen, run the per-OS strategy, and confirm the result. Installers
//! execute every process through the [`CommandRunner`] seam on
//! [`InstallCtx`], declare sudo steps via [`InstallCtx::run_sudo_step`]
//! (prompted unless `--yes`), and inherit `-v` command logging for free.

pub mod registry;
mod runner;

use std::io::IsTerminal;

use anyhow::anyhow;

pub use runner::{CommandOutput, CommandRunner, SystemRunner};

use crate::tools::Os;
use crate::ui::{self, HintExt};

/// Everything an installer needs to act on this machine. `os` is injected
/// (not read from `cfg!`) so strategy selection is testable on any host.
pub struct InstallCtx<'a> {
    /// The OS whose install strategy applies. Read by installers'
    /// `install` implementations; until one registers, only unit tests
    /// exercise it, hence the dead-code allowance.
    #[allow(dead_code)]
    pub os: Os,
    /// Skip confirmation prompts (`--yes`); sudo steps still announce
    /// themselves.
    pub yes: bool,
    /// Log underlying commands (`-v`).
    pub verbose: bool,
    /// Exact version requested via `--version`, when the tool supports it.
    pub pin: Option<String>,
    /// Seam for process execution and `PATH` probing.
    pub runner: &'a dyn CommandRunner,
}

/// One installable tool: how to spot an existing install and how to put it
/// on this machine.
pub trait Installer {
    /// Tool name as typed on the command line (`quarto`, `uv`, ...).
    fn name(&self) -> &'static str;

    /// Version of the existing install, or `None` when absent. Usually
    /// implemented with [`InstallCtx::probe_version`].
    fn detect(&self, ctx: &InstallCtx) -> Option<String>;

    /// Install the tool, choosing a strategy from `ctx.os`. Steps run
    /// through `ctx` so they are announced, logged at `-v`, and gated
    /// before sudo.
    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()>;

    /// Whether this installer honors `ctx.pin` (`--version`).
    fn supports_pin(&self) -> bool {
        false
    }
}

/// The shared install flow: idempotence check, announce, install, verify.
pub fn run_installer(installer: &dyn Installer, ctx: &InstallCtx) -> anyhow::Result<()> {
    let name = installer.name();
    if let Some(found) = installer.detect(ctx) {
        match ctx.pin.as_deref() {
            Some(pin) if pin != found => {
                ui::println(&format!(
                    "{name} {found} is installed; replacing it with {pin}"
                ));
            }
            _ => {
                ui::success(&format!("{name} {found} already installed"));
                return Ok(());
            }
        }
    }

    match ctx.pin.as_deref() {
        Some(pin) => ui::println(&format!("installing {name} {pin}")),
        None => ui::println(&format!("installing {name}")),
    }
    installer.install(ctx)?;

    match installer.detect(ctx) {
        Some(version) => {
            ui::success(&format!("{name} {version} installed"));
            Ok(())
        }
        None => Err(anyhow!(
            "{name} was installed but `{name}` is still not found on PATH"
        ))
        .hint(
            "open a new shell so PATH changes take effect, then check `hpds install` output above",
        ),
    }
}

// NOTE: the ctx helpers below are the surface concrete installers build on;
// until installers register in `registry::INSTALLERS` they are exercised by
// unit tests only.
#[allow(dead_code)]
impl InstallCtx<'_> {
    /// Detect an installed tool by probing `program --version` on `PATH`.
    /// `None` when the program is absent, fails, or prints no version.
    pub fn probe_version(&self, program: &str) -> Option<String> {
        self.runner.which(program)?;
        let out = self.runner.run(program, &["--version"]).ok()?;
        if !out.success {
            return None;
        }
        extract_version(&out.stdout)
    }

    /// Run one install step: announce `what`, log the command at `-v`, and
    /// fail with the process's stderr when it exits nonzero.
    pub fn run_step(
        &self,
        what: &str,
        program: &str,
        args: &[&str],
    ) -> anyhow::Result<CommandOutput> {
        ui::println(what);
        self.log_command(program, args);
        let out = self.runner.run(program, args)?;
        ensure_step_success(program, out)
    }

    /// Run one install step under sudo. The step and its exact command are
    /// always announced; the user is asked before anything runs unless
    /// `--yes` was given. Non-interactive runs without `--yes` refuse with
    /// guidance instead of hanging.
    pub fn run_sudo_step(
        &self,
        what: &str,
        program: &str,
        args: &[&str],
    ) -> anyhow::Result<CommandOutput> {
        ui::println(&format!("{what} (needs sudo)"));
        ui::println(&format!("  will run: sudo {program} {}", args.join(" ")));
        approve_sudo(what, self.yes, std::io::stdin().is_terminal(), |prompt| {
            ui::confirm(prompt, true)
        })?;
        let mut sudo_args = vec![program];
        sudo_args.extend_from_slice(args);
        self.log_command("sudo", &sudo_args);
        let out = self.runner.run("sudo", &sudo_args)?;
        ensure_step_success(program, out)
    }

    /// Echo the underlying command when `-v` is on.
    fn log_command(&self, program: &str, args: &[&str]) {
        if self.verbose {
            ui::println(&format!("$ {program} {}", args.join(" ")));
        }
    }
}

/// The sudo gate, pure in everything but the injected `confirm` so every
/// branch is unit-testable: `--yes` pre-approves, an interactive session is
/// asked, and a non-interactive session without `--yes` refuses.
fn approve_sudo(
    what: &str,
    yes: bool,
    interactive: bool,
    confirm: impl FnOnce(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    if !interactive {
        return Err(anyhow!(
            "`{what}` needs sudo, and hpds cannot ask for permission in a non-interactive session"
        ))
        .hint("re-run with --yes to pre-approve sudo steps, or run from an interactive terminal");
    }
    if confirm("run this step with sudo?")? {
        Ok(())
    } else {
        Err(anyhow!("sudo step declined: {what}"))
            .hint("re-run when you are ready to grant sudo, or perform this step manually")
    }
}

/// Turn a finished process into an error when it exited nonzero, carrying
/// whatever the process said on stderr (or stdout as a fallback).
fn ensure_step_success(program: &str, out: CommandOutput) -> anyhow::Result<CommandOutput> {
    if out.success {
        return Ok(out);
    }
    let detail = match out.stderr.trim() {
        "" => out.stdout.trim().to_string(),
        stderr => stderr.to_string(),
    };
    let message = if detail.is_empty() {
        format!("`{program}` failed")
    } else {
        format!("`{program}` failed: {detail}")
    };
    Err(anyhow!(message)).hint("re-run with -v to see the exact commands hpds ran")
}

/// Pull the first version-shaped token (`1.8.27`, `v2.95.0`, ...) out of a
/// `--version` output, tolerating banner text around it.
fn extract_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.'))
        .find(|token| looks_like_version(token))
        .map(str::to_string)
}

/// `X.Y` / `X.Y.Z`-shaped: at least two nonempty all-digit parts.
fn looks_like_version(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::ui::render_error;

    /// Fake `CommandRunner`: `paths` answers `which`, `outputs` answers
    /// `run` (keyed by the full command line), and every run is recorded.
    #[derive(Default)]
    struct FakeRunner {
        paths: HashMap<String, PathBuf>,
        outputs: HashMap<String, CommandOutput>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn on_path(mut self, program: &str) -> Self {
            self.paths.insert(
                program.to_string(),
                PathBuf::from("/fake/bin").join(program),
            );
            self
        }

        fn with_output(mut self, command_line: &str, stdout: &str) -> Self {
            self.outputs.insert(
                command_line.to_string(),
                CommandOutput {
                    success: true,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            );
            self
        }

        fn with_failure(mut self, command_line: &str, stderr: &str) -> Self {
            self.outputs.insert(
                command_line.to_string(),
                CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                },
            );
            self
        }
    }

    impl CommandRunner for FakeRunner {
        fn which(&self, program: &str) -> Option<PathBuf> {
            self.paths.get(program).cloned()
        }

        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
            let key = if args.is_empty() {
                program.to_string()
            } else {
                format!("{program} {}", args.join(" "))
            };
            self.calls.borrow_mut().push(key.clone());
            self.outputs
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow!("no fake output recorded for `{key}`"))
        }
    }

    fn ctx<'a>(runner: &'a FakeRunner) -> InstallCtx<'a> {
        InstallCtx {
            os: Os::Mac,
            yes: false,
            verbose: false,
            pin: None,
            runner,
        }
    }

    /// Fake installer whose detection result flips after `install` runs,
    /// recording the OS each install was asked to target.
    struct FakeInstaller {
        detected: RefCell<Option<String>>,
        after_install: Option<String>,
        installed_on: RefCell<Vec<Os>>,
    }

    impl FakeInstaller {
        fn new(detected: Option<&str>, after_install: Option<&str>) -> Self {
            FakeInstaller {
                detected: RefCell::new(detected.map(str::to_string)),
                after_install: after_install.map(str::to_string),
                installed_on: RefCell::new(Vec::new()),
            }
        }

        fn install_count(&self) -> usize {
            self.installed_on.borrow().len()
        }
    }

    impl Installer for FakeInstaller {
        fn name(&self) -> &'static str {
            "faketool"
        }

        fn detect(&self, _ctx: &InstallCtx) -> Option<String> {
            self.detected.borrow().clone()
        }

        fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
            self.installed_on.borrow_mut().push(ctx.os);
            *self.detected.borrow_mut() = self.after_install.clone();
            Ok(())
        }

        fn supports_pin(&self) -> bool {
            true
        }
    }

    // --- detection via --version probing --------------------------------

    /// A recorded `--version` output from `tests/fixtures/tool-output/`.
    fn probe_fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/version-probes")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    #[test]
    fn probe_version_hits_when_on_path_and_version_prints() {
        let runner = FakeRunner::default()
            .on_path("quarto")
            .with_output("quarto --version", &probe_fixture("quarto.txt"));
        assert_eq!(
            ctx(&runner).probe_version("quarto"),
            Some("1.9.36".to_string())
        );
    }

    #[test]
    fn probe_version_misses_when_not_on_path_without_running_anything() {
        let runner = FakeRunner::default();
        assert_eq!(ctx(&runner).probe_version("quarto"), None);
        assert!(
            runner.calls.borrow().is_empty(),
            "must not run a program that is not on PATH"
        );
    }

    #[test]
    fn probe_version_misses_when_the_probe_exits_nonzero() {
        let runner = FakeRunner::default()
            .on_path("quarto")
            .with_failure("quarto --version", "boom");
        assert_eq!(ctx(&runner).probe_version("quarto"), None);
    }

    #[test]
    fn extract_version_parses_recorded_tool_outputs() {
        for (fixture, want) in [
            ("gh.txt", "2.95.0"),
            ("uv.txt", "0.9.0"),
            ("quarto.txt", "1.9.36"),
            ("rig.txt", "0.8.1"),
            ("r.txt", "4.6.0"),
        ] {
            assert_eq!(
                extract_version(&probe_fixture(fixture)).as_deref(),
                Some(want),
                "{fixture}"
            );
        }
    }

    #[test]
    fn extract_version_strips_a_v_prefix() {
        assert_eq!(
            extract_version("v1.1.3 19864453f7").as_deref(),
            Some("1.1.3")
        );
    }

    #[test]
    fn extract_version_ignores_dates_hashes_and_prose() {
        for output in [
            "released 2026-06-17",
            "build 39b688653",
            "no version here",
            "",
        ] {
            assert_eq!(extract_version(output), None, "{output:?}");
        }
    }

    // --- idempotency ------------------------------------------------------

    #[test]
    fn already_installed_is_a_no_op() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(Some("1.8.27"), None);
        run_installer(&installer, &ctx(&runner)).expect("no-op must succeed");
        assert_eq!(installer.install_count(), 0);
    }

    #[test]
    fn already_at_the_pinned_version_is_a_no_op() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(Some("1.8.27"), None);
        let ctx = InstallCtx {
            pin: Some("1.8.27".to_string()),
            ..ctx(&runner)
        };
        run_installer(&installer, &ctx).expect("no-op must succeed");
        assert_eq!(installer.install_count(), 0);
    }

    #[test]
    fn a_different_pinned_version_reinstalls() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(Some("1.8.27"), Some("1.9.36"));
        let ctx = InstallCtx {
            pin: Some("1.9.36".to_string()),
            ..ctx(&runner)
        };
        run_installer(&installer, &ctx).expect("reinstall must succeed");
        assert_eq!(installer.install_count(), 1);
    }

    #[test]
    fn absent_tool_is_installed_and_verified() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, Some("1.9.36"));
        run_installer(&installer, &ctx(&runner)).expect("install must succeed");
        assert_eq!(installer.install_count(), 1);
    }

    #[test]
    fn install_that_leaves_the_tool_off_path_errors_with_guidance() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, None);
        let err = run_installer(&installer, &ctx(&runner)).expect_err("verify must fail");
        let out = render_error(&err, false);
        assert!(out.contains("PATH"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn installer_failure_propagates() {
        struct FailingInstaller;
        impl Installer for FailingInstaller {
            fn name(&self) -> &'static str {
                "faketool"
            }
            fn detect(&self, _ctx: &InstallCtx) -> Option<String> {
                None
            }
            fn install(&self, _ctx: &InstallCtx) -> anyhow::Result<()> {
                Err(anyhow!("download failed"))
            }
        }
        let runner = FakeRunner::default();
        let err = run_installer(&FailingInstaller, &ctx(&runner)).expect_err("must fail");
        assert!(err.to_string().contains("download failed"), "{err}");
    }

    // --- per-OS strategy selection ---------------------------------------

    #[test]
    fn strategy_follows_the_injected_os_not_the_host() {
        /// Installer that picks its strategy from `ctx.os`.
        struct StrategyInstaller {
            chosen: RefCell<Vec<&'static str>>,
        }
        impl Installer for StrategyInstaller {
            fn name(&self) -> &'static str {
                "faketool"
            }
            fn detect(&self, _ctx: &InstallCtx) -> Option<String> {
                // Report installed once a strategy ran, so verification passes.
                if self.chosen.borrow().is_empty() {
                    None
                } else {
                    Some("1.0.0".to_string())
                }
            }
            fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
                self.chosen.borrow_mut().push(match ctx.os {
                    Os::Mac => "brew",
                    Os::Linux => "apt",
                    Os::Windows => "winget",
                });
                Ok(())
            }
        }

        for (os, want) in [
            (Os::Mac, "brew"),
            (Os::Linux, "apt"),
            (Os::Windows, "winget"),
        ] {
            let runner = FakeRunner::default();
            let installer = StrategyInstaller {
                chosen: RefCell::new(Vec::new()),
            };
            let ctx = InstallCtx { os, ..ctx(&runner) };
            run_installer(&installer, &ctx).expect("install must succeed");
            assert_eq!(*installer.chosen.borrow(), vec![want], "{os:?}");
        }
    }

    // --- steps and sudo gating --------------------------------------------

    #[test]
    fn run_step_executes_through_the_runner() {
        let runner = FakeRunner::default().with_output("apt-get update", "");
        ctx(&runner)
            .run_step("refreshing package lists", "apt-get", &["update"])
            .expect("step must succeed");
        assert_eq!(*runner.calls.borrow(), vec!["apt-get update"]);
    }

    #[test]
    fn run_step_failure_carries_the_process_stderr_and_a_hint() {
        let runner =
            FakeRunner::default().with_failure("apt-get update", "E: Unable to locate package");
        let err = ctx(&runner)
            .run_step("refreshing package lists", "apt-get", &["update"])
            .expect_err("step must fail");
        let out = render_error(&err, false);
        assert!(out.contains("Unable to locate package"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn sudo_step_with_yes_runs_under_sudo_without_prompting() {
        let runner = FakeRunner::default().with_output("sudo apt-get install -y quarto", "");
        let ctx = InstallCtx {
            yes: true,
            ..ctx(&runner)
        };
        ctx.run_sudo_step("installing quarto", "apt-get", &["install", "-y", "quarto"])
            .expect("pre-approved sudo step must run");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["sudo apt-get install -y quarto"]
        );
    }

    #[test]
    fn approve_sudo_with_yes_never_prompts() {
        approve_sudo("install quarto", true, true, |_| {
            panic!("--yes must skip the prompt")
        })
        .expect("pre-approved");
        approve_sudo("install quarto", true, false, |_| {
            panic!("--yes must skip the prompt even when non-interactive")
        })
        .expect("pre-approved");
    }

    #[test]
    fn approve_sudo_interactive_asks_and_honors_a_yes() {
        let asked = RefCell::new(false);
        approve_sudo("install quarto", false, true, |_| {
            *asked.borrow_mut() = true;
            Ok(true)
        })
        .expect("confirmed sudo must proceed");
        assert!(*asked.borrow(), "an interactive session must be asked");
    }

    #[test]
    fn approve_sudo_interactive_honors_a_decline() {
        let err = approve_sudo("install quarto", false, true, |_| Ok(false))
            .expect_err("declining must stop the step");
        let out = render_error(&err, false);
        assert!(out.contains("declined"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn approve_sudo_non_interactive_without_yes_refuses_with_guidance() {
        let err = approve_sudo("install quarto", false, false, |_| {
            panic!("must not prompt in a non-interactive session")
        })
        .expect_err("must refuse");
        let out = render_error(&err, false);
        assert!(out.contains("install quarto"), "{out}");
        assert!(out.contains("--yes"), "{out}");
    }

    #[test]
    fn verbose_ctx_logs_the_underlying_command() {
        // `log_command` prints via ui (captured by the test harness); this
        // asserts the gating logic: no panic, and the runner still runs.
        let runner = FakeRunner::default().with_output("apt-get update", "");
        let ctx = InstallCtx {
            verbose: true,
            ..ctx(&runner)
        };
        ctx.run_step("refreshing package lists", "apt-get", &["update"])
            .expect("step must succeed");
        assert_eq!(*runner.calls.borrow(), vec!["apt-get update"]);
    }
}
