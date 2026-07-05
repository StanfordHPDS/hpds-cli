//! Installer for `togi`, the lab's formatter/linter.
//!
//! Every OS: the release binary from StanfordHPDS/togi into the per-user
//! bin directory. togi is lab-published (no package-manager presence), so
//! the release binary is the only strategy, pinned or not.

use crate::install::{InstallCtx, Installer};
use crate::tools::{ToolSpec, versions};

use super::{fetch_plan, fetch_to_user_bin};

pub struct Togi;

/// The togi release archive: Rust-triple asset names with a sha256
/// sidecar per asset. Mirrors togi's cargo-dist config — gzip tarballs on
/// unix, zip on Windows — the same shape this repo publishes.
pub(super) fn release_spec() -> ToolSpec {
    ToolSpec {
        name: "togi",
        default_version: versions::TOGI,
        repo: "StanfordHPDS/togi",
        asset_pattern: "togi-{arch}-{os}.{ext}",
        checksum_pattern: Some("togi-{arch}-{os}.{ext}.sha256"),
    }
}

impl Installer for Togi {
    fn name(&self) -> &'static str {
        "togi"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("togi")
    }

    fn supports_pin(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        let version = ctx
            .pin
            .clone()
            .unwrap_or_else(|| versions::TOGI.to_string());
        vec![fetch_plan("togi", &version)]
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx
            .pin
            .clone()
            .unwrap_or_else(|| versions::TOGI.to_string());
        fetch_to_user_bin(ctx, &release_spec(), &version)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::tools::{Arch, Os, Platform};

    #[test]
    fn togi_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", &probe_fixture("togi.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(Togi.detect(&ctx).as_deref(), Some("0.1.0"));
    }

    #[test]
    fn togi_is_undetected_when_not_on_path() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Linux, &runner, &fetcher);
        assert_eq!(Togi.detect(&ctx), None);
    }

    #[test]
    fn togi_fetches_the_release_binary_on_every_os() {
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            // Even with package managers around: togi has no brew formula
            // in this framework's strategy set, no apt repo, and no winget
            // package — the release binary is the only path.
            let runner = FakeRunner::default()
                .on_path("brew")
                .on_path("apt-get")
                .on_path("winget");
            let fetcher = FakeFetcher::default();
            Togi.install(&ctx_on(os, &runner, &fetcher))
                .expect("fetch must succeed");
            assert!(
                runner.calls.borrow().is_empty(),
                "{os:?}: no package manager runs"
            );
            let calls = fetcher.calls.borrow();
            assert_eq!(calls.len(), 1, "{os:?}");
            assert_eq!(calls[0].spec.name, "togi", "{os:?}");
            assert_eq!(calls[0].version, versions::TOGI, "{os:?}");
        }
    }

    #[test]
    fn togi_pin_fetches_the_pinned_version() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("0.2.0".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };
        Togi.install(&ctx).expect("pinned fetch must succeed");
        assert_eq!(fetcher.calls.borrow()[0].version, "0.2.0");
    }

    #[test]
    fn togi_plan_is_the_release_download_on_every_os() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            let plan = Togi.plan(&ctx_on(os, &runner, &fetcher));
            assert_eq!(plan.len(), 1, "{os:?}");
            assert!(plan[0].contains("download"), "{os:?}: {plan:?}");
            assert!(plan[0].contains(versions::TOGI), "{os:?}: {plan:?}");
        }
        assert!(runner.calls.borrow().is_empty(), "planning must not run");
        assert!(fetcher.calls.borrow().is_empty(), "planning must not fetch");
    }

    #[test]
    fn togi_plan_shows_a_pinned_version() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("0.2.0".to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };
        let plan = Togi.plan(&ctx);
        assert!(plan[0].contains("0.2.0"), "{plan:?}");
    }

    #[test]
    fn togi_release_assets_resolve_to_the_dist_published_names() {
        let cases = [
            (Os::Mac, Arch::Aarch64, "togi-aarch64-apple-darwin.tar.gz"),
            (
                Os::Linux,
                Arch::X86_64,
                "togi-x86_64-unknown-linux-gnu.tar.gz",
            ),
            (Os::Windows, Arch::X86_64, "togi-x86_64-pc-windows-msvc.zip"),
        ];
        for (os, arch, want) in cases {
            let platform = Platform { os, arch };
            let spec = release_spec();
            assert_eq!(spec.asset_name(platform, "0.1.0"), want, "{os:?}/{arch:?}");
            assert_eq!(
                spec.checksum_asset_name(platform, "0.1.0").as_deref(),
                Some(format!("{want}.sha256").as_str()),
                "{os:?}/{arch:?}"
            );
        }
    }
}
