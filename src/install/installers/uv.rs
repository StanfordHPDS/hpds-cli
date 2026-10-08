//! Installer for `uv`.
//!
//! Every OS downloads the latest stable upstream release binary into the shared
//! verified cache and places it in the user bin directory. Exact pins use the
//! same direct release path without looking up the latest release.

use crate::install::{InstallCtx, Installer};
use crate::tools::ToolSpec;

use super::{fetch_plan, fetch_to_user_bin};

pub struct Uv;

/// The uv release archive: Rust-triple asset names with a sha256 sidecar
/// per asset, the same on every OS.
pub(super) fn release_spec() -> ToolSpec {
    ToolSpec {
        name: "uv",
        default_version: "latest",
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

    fn resolve_target(&self, ctx: &InstallCtx) -> anyhow::Result<Option<String>> {
        super::resolve_release_target(ctx, &release_spec())
    }

    fn verifies_target(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        vec![fetch_plan(
            &release_spec(),
            ctx.pin.as_deref().unwrap_or("latest stable"),
        )]
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx.pin.as_deref().ok_or_else(|| {
            anyhow::anyhow!("the uv release version was not resolved before installation")
        })?;
        fetch_to_user_bin(ctx, &release_spec(), version)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::tools::{Os, versions};

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
    fn uv_mac_without_brew_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Uv.install(&InstallCtx {
            pin: Some(versions::UV.to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        })
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
        Uv.install(&InstallCtx {
            pin: Some(versions::UV.to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        })
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
}
