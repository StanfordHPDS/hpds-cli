//! Installer for `uv`.
//!
//! macOS/Linux: Homebrew when present (and no pin); otherwise the release
//! binary is downloaded — checksum-verified, through the shared tool
//! cache — and placed in `~/.local/bin`, the same steps the official
//! installer script performs, run natively. Windows: not implemented;
//! errors cleanly with the official installer command.

use anyhow::anyhow;

use crate::install::{InstallCtx, Installer};
use crate::tools::{Os, ToolSpec, versions};
use crate::ui::HintExt;

use super::{fetch_plan, fetch_to_user_bin, on_path};

pub struct Uv;

/// The uv release archive: Rust-triple asset names with a sha256 sidecar
/// per asset, the same on every OS.
pub(super) fn release_spec() -> ToolSpec {
    ToolSpec {
        name: "uv",
        default_version: versions::UV,
        repo: "astral-sh/uv",
        asset_pattern: "uv-{arch}-{os}.{ext}",
        checksum_pattern: Some("uv-{arch}-{os}.{ext}.sha256"),
    }
}

impl Installer for Uv {
    fn name(&self) -> &'static str {
        "uv"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("uv")
    }

    fn supports_pin(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        match ctx.os {
            Os::Mac | Os::Linux => {
                if ctx.pin.is_none() && on_path(ctx, "brew") {
                    vec!["brew install uv".to_string()]
                } else {
                    let version = ctx.pin.clone().unwrap_or_else(|| versions::UV.to_string());
                    vec![fetch_plan("uv", &version)]
                }
            }
            Os::Windows => vec![
                "nothing: uv on Windows is not supported yet, so only the official \
                 installer command is printed"
                    .to_string(),
            ],
        }
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        match ctx.os {
            Os::Mac | Os::Linux => {
                if ctx.pin.is_none() && on_path(ctx, "brew") {
                    ctx.run_step("installing uv with Homebrew", "brew", &["install", "uv"])?;
                } else {
                    let version = ctx.pin.clone().unwrap_or_else(|| versions::UV.to_string());
                    fetch_to_user_bin(ctx, &release_spec(), &version)?;
                }
                Ok(())
            }
            Os::Windows => Err(anyhow!(
                "installing uv on Windows is not supported on this machine yet"
            ))
            .hint(
                "run the official installer instead: powershell -ExecutionPolicy ByPass -c \
                 \"irm https://astral.sh/uv/install.ps1 | iex\"",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::ui::render_error;

    #[test]
    fn uv_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("uv")
            .with_output("uv --version", &probe_fixture("uv.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(Uv.detect(&ctx).as_deref(), Some("0.9.0"));
    }

    #[test]
    fn uv_is_undetected_when_not_on_path() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Linux, &runner, &fetcher);
        assert_eq!(Uv.detect(&ctx), None);
    }

    #[test]
    fn uv_mac_prefers_brew_when_present() {
        let runner = FakeRunner::default()
            .on_path("brew")
            .with_output("brew install uv", "");
        let fetcher = FakeFetcher::default();
        Uv.install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("brew install must succeed");
        assert_eq!(*runner.calls.borrow(), vec!["brew install uv"]);
        assert!(
            fetcher.calls.borrow().is_empty(),
            "brew path must not fetch"
        );
    }

    #[test]
    fn uv_mac_without_brew_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Uv.install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("fetch must succeed");
        assert!(runner.calls.borrow().is_empty(), "no package manager runs");
        let calls = fetcher.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].spec.name, "uv");
        assert_eq!(calls[0].version, versions::UV);
        assert!(
            calls[0].bin_dir.ends_with(".local/bin"),
            "{:?}",
            calls[0].bin_dir
        );
    }

    #[test]
    fn uv_linux_without_brew_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Uv.install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect("fetch must succeed");
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "uv");
    }

    #[test]
    fn uv_pin_forces_the_release_binary_even_when_brew_is_present() {
        let runner = FakeRunner::default().on_path("brew");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("0.9.9".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };
        Uv.install(&ctx).expect("pinned fetch must succeed");
        assert!(runner.calls.borrow().is_empty(), "brew cannot pin versions");
        assert_eq!(fetcher.calls.borrow()[0].version, "0.9.9");
    }

    #[test]
    fn uv_plan_mirrors_the_strategy_selection() {
        let fetcher = FakeFetcher::default();

        let with_brew = FakeRunner::default().on_path("brew");
        assert_eq!(
            Uv.plan(&ctx_on(Os::Linux, &with_brew, &fetcher)),
            vec!["brew install uv".to_string()]
        );

        let bare = FakeRunner::default();
        let plan = Uv.plan(&ctx_on(Os::Mac, &bare, &fetcher));
        assert_eq!(plan.len(), 1);
        assert!(plan[0].contains("download"), "{plan:?}");
        assert!(plan[0].contains(versions::UV), "{plan:?}");

        let plan = Uv.plan(&ctx_on(Os::Windows, &bare, &fetcher));
        assert!(plan[0].contains("not supported"), "{plan:?}");
    }

    #[test]
    fn uv_windows_errors_cleanly_with_the_official_installer_hint() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let err = Uv
            .install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect_err("windows must be a clean unsupported error");
        let out = render_error(&err, false);
        assert!(out.contains("not supported on this machine"), "{out}");
        assert!(out.contains("install.ps1"), "{out}");
        assert!(fetcher.calls.borrow().is_empty());
        assert!(runner.calls.borrow().is_empty());
    }
}
