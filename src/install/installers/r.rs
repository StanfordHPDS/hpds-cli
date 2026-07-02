//! Installer for R itself, driven by rig on every platform.
//!
//! R always installs through rig: when rig is missing it is installed
//! first via its registered installer (looked up in the registry, so the
//! rig strategy lives in one place), then `rig add` puts the requested R
//! on the machine — the current release, or the exact `--version` pin.
//! `rig add` writes into system locations on macOS and Linux, so it runs
//! as a declared sudo step there; on Windows it runs directly.

use crate::install::{InstallCtx, Installer, registry};
use crate::tools::Os;
use crate::ui;

pub struct R;

impl Installer for R {
    fn name(&self) -> &'static str {
        "r"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("R")
    }

    fn supports_pin(&self) -> bool {
        true
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let rig = registry::find("rig")?;
        if rig.detect(ctx).is_none() {
            ui::println("R installs through rig, which is not installed yet; installing rig");
            // rig gets its own ctx: an R `--version` pin must not leak
            // into the rig install.
            let rig_ctx = InstallCtx {
                os: ctx.os,
                yes: ctx.yes,
                verbose: ctx.verbose,
                pin: None,
                runner: ctx.runner,
                fetcher: ctx.fetcher,
            };
            rig.install(&rig_ctx)?;
        }

        let (what, wanted) = match ctx.pin.as_deref() {
            Some(pin) => (format!("installing R {pin} with rig"), pin),
            None => (
                "installing the current R release with rig".to_string(),
                "release",
            ),
        };
        match ctx.os {
            // `rig add` installs into /opt/R (Linux) or runs the CRAN pkg
            // installer (macOS), both of which need root.
            Os::Mac | Os::Linux => ctx.run_sudo_step(&what, "rig", &["add", wanted])?,
            Os::Windows => ctx.run_step(&what, "rig", &["add", wanted])?,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::ui::render_error;

    #[test]
    fn r_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("R")
            .with_output("R --version", &probe_fixture("r.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(R.detect(&ctx).as_deref(), Some("4.6.0"));
    }

    #[test]
    fn r_mac_and_linux_add_the_release_with_rig_under_sudo() {
        for os in [Os::Mac, Os::Linux] {
            let runner = FakeRunner::default()
                .on_path("rig")
                .with_output("rig --version", &probe_fixture("rig.txt"))
                .with_output("sudo rig add release", "");
            let fetcher = FakeFetcher::default();
            R.install(&ctx_on(os, &runner, &fetcher))
                .expect("rig add must succeed");
            assert_eq!(
                *runner.calls.borrow(),
                vec!["rig --version", "sudo rig add release"],
                "{os:?}"
            );
            assert!(fetcher.calls.borrow().is_empty(), "{os:?}");
        }
    }

    #[test]
    fn r_windows_adds_the_release_without_sudo() {
        let runner = FakeRunner::default()
            .on_path("rig")
            .with_output("rig --version", &probe_fixture("rig.txt"))
            .with_output("rig add release", "");
        let fetcher = FakeFetcher::default();
        R.install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("rig add must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["rig --version", "rig add release"]
        );
    }

    #[test]
    fn r_mac_installs_rig_first_through_the_registry_when_missing() {
        // No rig on PATH: the registry's rig installer must run first
        // (here via its Homebrew strategy), then `rig add release`.
        let runner = FakeRunner::default()
            .on_path("brew")
            .with_output("brew tap r-lib/rig", "")
            .with_output("brew install --cask rig", "")
            .with_output("sudo rig add release", "");
        let fetcher = FakeFetcher::default();
        R.install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("rig-then-R install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                "brew tap r-lib/rig",
                "brew install --cask rig",
                "sudo rig add release"
            ]
        );
    }

    #[test]
    fn r_linux_installs_rig_first_via_apt_when_missing() {
        // No rig on PATH: the registry's rig installer must run its apt
        // repository steps first, then `rig add release` under sudo.
        let runner = FakeRunner::default()
            .on_path("apt-get")
            .with_output(
                "sudo curl -L https://rig.r-pkg.org/deb/rig.gpg -o /etc/apt/trusted.gpg.d/rig.gpg",
                "",
            )
            .with_output(
                "sudo sh -c echo 'deb http://rig.r-pkg.org/deb rig main' \
                 > /etc/apt/sources.list.d/rig.list",
                "",
            )
            .with_output("sudo apt-get update", "")
            .with_output("sudo apt-get install -y r-rig", "")
            .with_output("sudo rig add release", "");
        let fetcher = FakeFetcher::default();
        R.install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect("rig-then-R install must succeed");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 5, "{calls:#?}");
        assert_eq!(calls[3], "sudo apt-get install -y r-rig");
        assert_eq!(calls[4], "sudo rig add release");
    }

    #[test]
    fn r_windows_installs_rig_first_via_winget_when_missing() {
        let runner = FakeRunner::default()
            .on_path("winget")
            .with_output("winget install --id posit.rig --exact", "")
            .with_output("rig add release", "");
        let fetcher = FakeFetcher::default();
        R.install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("rig-then-R install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id posit.rig --exact", "rig add release"]
        );
    }

    #[test]
    fn r_pin_installs_that_exact_version_with_rig() {
        let runner = FakeRunner::default()
            .on_path("rig")
            .with_output("rig --version", &probe_fixture("rig.txt"))
            .with_output("sudo rig add 4.5.1", "");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("4.5.1".to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };
        R.install(&ctx).expect("pinned rig add must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["rig --version", "sudo rig add 4.5.1"]
        );
    }

    #[test]
    fn r_pin_does_not_leak_into_the_rig_install() {
        // rig missing + R pinned: rig still installs its own way (no
        // version flags), and only `rig add` sees the pin.
        let runner = FakeRunner::default()
            .on_path("brew")
            .with_output("brew tap r-lib/rig", "")
            .with_output("brew install --cask rig", "")
            .with_output("sudo rig add 4.5.1", "");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("4.5.1".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };
        R.install(&ctx).expect("pinned rig-then-R must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                "brew tap r-lib/rig",
                "brew install --cask rig",
                "sudo rig add 4.5.1"
            ]
        );
    }

    #[test]
    fn r_surfaces_the_rig_installers_error_when_rig_cannot_install() {
        // macOS with neither rig nor brew: the composed rig install fails
        // with rig's own guidance.
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let err = R
            .install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect_err("missing brew must fail through the rig installer");
        let out = render_error(&err, false);
        assert!(out.contains("Homebrew"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn r_supports_version_pins() {
        assert!(R.supports_pin());
    }
}
