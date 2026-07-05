//! The installer framework behind `hpds install <tool>`.
//!
//! Each tool implements [`Installer`]; [`run_installer`] drives the shared
//! flow: detect an existing install (idempotent no-op), announce what will
//! happen, run the per-OS strategy, and confirm the result. Installers
//! execute every process through the [`CommandRunner`] seam on
//! [`InstallCtx`], declare sudo steps via [`InstallCtx::run_sudo_step`]
//! (prompted unless `--yes`), and inherit `-v` command logging for free.

mod fetch;
mod installers;
pub mod registry;
mod runner;
#[cfg(test)]
pub(crate) mod test_support;

use std::cell::Cell;
use std::io::IsTerminal;

use anyhow::anyhow;

pub use fetch::{CacheFetcher, ReleaseFetcher};
pub use runner::{CommandOutput, CommandRunner, SystemRunner};

use crate::tools::Os;
use crate::ui::{self, HintExt};

/// Everything an installer needs to act on this machine. `os` is injected
/// (not read from `cfg!`) so strategy selection is testable on any host.
pub struct InstallCtx<'a> {
    /// The OS whose install strategy applies.
    pub os: Os,
    /// Skip confirmation prompts (`--yes`); the plan of what will run is
    /// still printed.
    pub yes: bool,
    /// Log underlying commands (`-v`).
    pub verbose: bool,
    /// Exact version requested via `--version`, when the tool supports it.
    pub pin: Option<String>,
    /// A surrounding flow (the `hpds setup` checklist) already had the
    /// user approve the plan this install belongs to, so the per-install
    /// confirmation is skipped. Sudo steps still ask individually.
    pub plan_approved: bool,
    /// Set once the user confirms this install's own printed plan. The
    /// plan lists any sudo commands, so that single answer covers them
    /// and sudo steps must not ask a second time.
    pub sudo_approved: Cell<bool>,
    /// Seam for process execution and `PATH` probing.
    pub runner: &'a dyn CommandRunner,
    /// Seam for downloading release binaries onto the user's PATH.
    pub fetcher: &'a dyn ReleaseFetcher,
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

    /// The concrete commands or downloads [`install`](Installer::install)
    /// would perform, one line each, mirroring the strategy it picks from
    /// `ctx`. The shared flow prints these before anything mutates so the
    /// user can approve exactly what will happen.
    fn plan(&self, ctx: &InstallCtx) -> Vec<String>;

    /// Whether this installer honors `ctx.pin` (`--version`).
    fn supports_pin(&self) -> bool {
        false
    }
}

/// The shared install flow: idempotence check, plan, confirm, install,
/// verify. Nothing mutates the machine until the plan is approved.
pub fn run_installer(installer: &dyn Installer, ctx: &InstallCtx) -> anyhow::Result<()> {
    run_installer_with(installer, ctx, std::io::stdin().is_terminal(), &|prompt| {
        ui::confirm(prompt, true)
    })
}

/// [`run_installer`] with interactivity and the confirmation prompt
/// injected, so every branch of the gate is testable without a terminal.
fn run_installer_with(
    installer: &dyn Installer,
    ctx: &InstallCtx,
    interactive: bool,
    confirm: &dyn Fn(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
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
        Some(pin) => ui::println(&format!("installing {name} {pin} will:")),
        None => ui::println(&format!("installing {name} will:")),
    }
    for line in installer.plan(ctx) {
        ui::println(&format!("  {line}"));
    }
    if approve_install(name, ctx.yes, ctx.plan_approved, interactive, confirm)?
        == InstallApproval::ConfirmedNow
    {
        // The plan the user just approved listed any sudo commands, so
        // that one answer covers them: sudo steps must not ask again.
        ctx.sudo_approved.set(true);
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
    /// `--yes` was given or this install's plan (which listed the sudo
    /// command) was already confirmed. Non-interactive runs without
    /// `--yes` refuse with guidance instead of hanging.
    pub fn run_sudo_step(
        &self,
        what: &str,
        program: &str,
        args: &[&str],
    ) -> anyhow::Result<CommandOutput> {
        ui::println(&format!("{what} (needs sudo)"));
        ui::println(&format!("  will run: sudo {program} {}", args.join(" ")));
        approve_sudo(
            what,
            self.yes || self.sudo_approved.get(),
            std::io::stdin().is_terminal(),
            |prompt| ui::confirm(prompt, true),
        )?;
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

/// How the install gate let a run proceed.
#[derive(Debug, PartialEq, Eq)]
enum InstallApproval {
    /// `--yes`, or a surrounding flow already approved the plan.
    AlreadyApproved,
    /// The user answered yes to this install's own confirmation.
    ConfirmedNow,
}

/// The install gate, pure in everything but the injected `confirm` so
/// every branch is unit-testable: `--yes` and an already-approved plan
/// proceed without asking, an interactive session is asked once, and a
/// non-interactive session without `--yes` refuses before anything
/// mutates.
fn approve_install(
    name: &str,
    yes: bool,
    plan_approved: bool,
    interactive: bool,
    confirm: &dyn Fn(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<InstallApproval> {
    if yes || plan_approved {
        return Ok(InstallApproval::AlreadyApproved);
    }
    if !interactive {
        return Err(anyhow!(
            "installing {name} would change this machine, and hpds cannot ask for \
             permission in a non-interactive session"
        ))
        .hint("re-run with --yes to approve the plan above, or run from an interactive terminal");
    }
    if confirm(&format!("install {name} now?"))? {
        Ok(InstallApproval::ConfirmedNow)
    } else {
        Err(anyhow!("install declined: {name}"))
            .hint(format!("re-run `hpds install {name}` when you are ready"))
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

    use super::test_support::{FakeRunner, PanicFetcher};
    use super::*;
    use crate::ui::render_error;

    /// A ctx for tests of the shared flow: nothing here may fetch, and the
    /// prompt-gating tests need `yes: false`.
    fn ctx(runner: &FakeRunner) -> InstallCtx<'_> {
        InstallCtx {
            os: Os::Mac,
            yes: false,
            verbose: false,
            pin: None,
            plan_approved: false,
            sudo_approved: Cell::new(false),
            runner,
            fetcher: &PanicFetcher,
        }
    }

    /// A ctx whose plan was already approved by a surrounding flow, for
    /// tests of everything downstream of the confirmation gate.
    fn approved_ctx(runner: &FakeRunner) -> InstallCtx<'_> {
        InstallCtx {
            plan_approved: true,
            ..ctx(runner)
        }
    }

    /// A confirm seam for paths that must never ask.
    fn no_prompt(prompt: &str) -> anyhow::Result<bool> {
        panic!("this flow must not prompt (asked: {prompt})");
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

        fn plan(&self, _ctx: &InstallCtx) -> Vec<String> {
            vec!["fake install strategy".to_string()]
        }

        fn supports_pin(&self) -> bool {
            true
        }
    }

    // --- detection via --version probing --------------------------------

    use super::test_support::probe_fixture;

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
            ("duckdb.txt", "1.5.4"),
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
            ..approved_ctx(&runner)
        };
        run_installer(&installer, &ctx).expect("reinstall must succeed");
        assert_eq!(installer.install_count(), 1);
    }

    #[test]
    fn absent_tool_is_installed_and_verified() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, Some("1.9.36"));
        run_installer(&installer, &approved_ctx(&runner)).expect("install must succeed");
        assert_eq!(installer.install_count(), 1);
    }

    #[test]
    fn install_that_leaves_the_tool_off_path_errors_with_guidance() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, None);
        let err = run_installer(&installer, &approved_ctx(&runner)).expect_err("verify must fail");
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
            fn plan(&self, _ctx: &InstallCtx) -> Vec<String> {
                vec!["fake install strategy".to_string()]
            }
        }
        let runner = FakeRunner::default();
        let err = run_installer(&FailingInstaller, &approved_ctx(&runner)).expect_err("must fail");
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
            fn plan(&self, _ctx: &InstallCtx) -> Vec<String> {
                vec!["fake install strategy".to_string()]
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
            let ctx = InstallCtx {
                os,
                ..approved_ctx(&runner)
            };
            run_installer(&installer, &ctx).expect("install must succeed");
            assert_eq!(*installer.chosen.borrow(), vec![want], "{os:?}");
        }
    }

    // --- the confirmation gate before any mutation --------------------------

    /// Fake installer whose install runs one command through the runner,
    /// under sudo when asked, so the gate's ordering against real
    /// mutation is observable in the runner's call log.
    struct OneStepInstaller {
        sudo: bool,
        installed: Cell<bool>,
    }

    impl OneStepInstaller {
        fn new(sudo: bool) -> Self {
            OneStepInstaller {
                sudo,
                installed: Cell::new(false),
            }
        }
    }

    impl Installer for OneStepInstaller {
        fn name(&self) -> &'static str {
            "faketool"
        }

        fn detect(&self, _ctx: &InstallCtx) -> Option<String> {
            // Installed once its command ran, so verification passes.
            if self.installed.get() {
                Some("1.0.0".to_string())
            } else {
                None
            }
        }

        fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
            if self.sudo {
                ctx.run_sudo_step("adding faketool", "faker", &["add", "release"])?;
            } else {
                ctx.run_step("adding faketool", "faker", &["add", "release"])?;
            }
            self.installed.set(true);
            Ok(())
        }

        fn plan(&self, _ctx: &InstallCtx) -> Vec<String> {
            if self.sudo {
                vec!["sudo faker add release".to_string()]
            } else {
                vec!["faker add release".to_string()]
            }
        }
    }

    #[test]
    fn interactive_install_asks_before_any_command_and_declining_runs_nothing() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, Some("1.9.36"));
        let err = run_installer_with(&installer, &ctx(&runner), true, &|_| Ok(false))
            .expect_err("declining must stop the install");
        let out = render_error(&err, false);
        assert!(out.contains("declined"), "{out}");
        assert!(out.contains("hint:"), "{out}");
        assert_eq!(installer.install_count(), 0, "nothing may install");
        assert!(
            runner.calls.borrow().is_empty(),
            "zero commands may run: {:?}",
            runner.calls.borrow()
        );
    }

    #[test]
    fn interactive_install_runs_after_the_user_confirms() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, Some("1.9.36"));
        let asked = RefCell::new(0);
        run_installer_with(&installer, &ctx(&runner), true, &|_| {
            *asked.borrow_mut() += 1;
            Ok(true)
        })
        .expect("a confirmed install must proceed");
        assert_eq!(*asked.borrow(), 1, "exactly one confirmation");
        assert_eq!(installer.install_count(), 1);
    }

    #[test]
    fn yes_installs_without_prompting() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, Some("1.9.36"));
        let ctx = InstallCtx {
            yes: true,
            ..ctx(&runner)
        };
        run_installer_with(&installer, &ctx, true, &no_prompt).expect("--yes must not prompt");
        assert_eq!(installer.install_count(), 1);
    }

    #[test]
    fn already_installed_no_ops_without_prompting() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(Some("1.8.27"), None);
        run_installer_with(&installer, &ctx(&runner), true, &no_prompt)
            .expect("a no-op must not prompt");
        assert_eq!(installer.install_count(), 0);
    }

    #[test]
    fn non_interactive_without_yes_refuses_before_any_command() {
        let runner = FakeRunner::default();
        let installer = FakeInstaller::new(None, Some("1.9.36"));
        let err = run_installer_with(&installer, &ctx(&runner), false, &no_prompt)
            .expect_err("must refuse without --yes");
        let out = render_error(&err, false);
        assert!(out.contains("--yes"), "{out}");
        assert!(out.contains("hint:"), "{out}");
        assert_eq!(installer.install_count(), 0, "nothing may install");
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn a_single_sudo_step_install_prompts_exactly_once() {
        // The whole install is one sudo step: the plan confirmation must
        // cover it, so the user sees exactly one prompt.
        let runner = FakeRunner::default().with_output("sudo faker add release", "");
        let installer = OneStepInstaller::new(true);
        let asked = RefCell::new(0);
        run_installer_with(&installer, &ctx(&runner), true, &|_| {
            *asked.borrow_mut() += 1;
            Ok(true)
        })
        .expect("a confirmed sudo-only install must run");
        assert_eq!(*asked.borrow(), 1, "one confirmation covers the sudo step");
        assert_eq!(*runner.calls.borrow(), vec!["sudo faker add release"]);
    }

    #[test]
    fn plan_approved_ctx_installs_without_its_own_prompt() {
        // `hpds setup` confirms its whole plan up front; each install must
        // then run without re-asking — but its sudo steps still would (the
        // plan approval deliberately does not pre-approve sudo).
        let runner = FakeRunner::default().with_output("faker add release", "");
        let installer = OneStepInstaller::new(false);
        let ctx = approved_ctx(&runner);
        run_installer_with(&installer, &ctx, true, &no_prompt)
            .expect("an approved plan must not re-prompt");
        assert_eq!(*runner.calls.borrow(), vec!["faker add release"]);
        assert!(
            !ctx.sudo_approved.get(),
            "a surrounding flow's approval must not silently cover sudo"
        );
    }

    #[test]
    fn approve_install_with_yes_or_an_approved_plan_never_prompts() {
        for (yes, plan_approved) in [(true, false), (false, true), (true, true)] {
            let approval = approve_install("quarto", yes, plan_approved, false, &no_prompt)
                .expect("pre-approved");
            assert_eq!(approval, InstallApproval::AlreadyApproved);
        }
    }

    #[test]
    fn approve_install_interactive_confirms_and_covers_sudo() {
        let approval = approve_install("quarto", false, false, true, &|_| Ok(true))
            .expect("confirmed install must proceed");
        assert_eq!(approval, InstallApproval::ConfirmedNow);
    }

    #[test]
    fn approve_install_interactive_decline_is_an_actionable_error() {
        let err = approve_install("quarto", false, false, true, &|_| Ok(false))
            .expect_err("declining must stop the install");
        let out = render_error(&err, false);
        assert!(out.contains("declined"), "{out}");
        assert!(out.contains("hpds install quarto"), "{out}");
    }

    #[test]
    fn approve_install_non_interactive_without_yes_refuses_with_guidance() {
        let err = approve_install("quarto", false, false, false, &no_prompt)
            .expect_err("must refuse without --yes");
        let out = render_error(&err, false);
        assert!(out.contains("quarto"), "{out}");
        assert!(out.contains("--yes"), "{out}");
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
    fn sudo_step_skips_the_prompt_after_the_plan_was_confirmed() {
        // Tests run without a terminal, so if the confirmed plan did not
        // cover this step, approve_sudo would refuse instead of running.
        let runner = FakeRunner::default().with_output("sudo apt-get update", "");
        let ctx = ctx(&runner);
        ctx.sudo_approved.set(true);
        ctx.run_sudo_step("refreshing package lists", "apt-get", &["update"])
            .expect("a confirmed plan must cover its sudo steps");
        assert_eq!(*runner.calls.borrow(), vec!["sudo apt-get update"]);
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
