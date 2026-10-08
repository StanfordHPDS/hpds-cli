//! Installer for `gh` (the GitHub CLI).
//!
//! Every OS downloads the latest stable upstream release binary into the shared
//! verified cache and places it in the user bin directory. Exact pins use the
//! same direct release path without looking up the latest release.

use crate::install::{InstallCtx, Installer};
use crate::tools::{Os, ToolSpec};

use super::{fetch_plan, fetch_to_user_bin};

pub struct Gh;

/// The gh release archive for one OS. gh names assets `macOS`/`linux`/
/// `windows` with Go-style arches, and macOS archives are zips.
pub(super) fn release_spec(os: Os) -> ToolSpec {
    ToolSpec {
        name: "gh",
        default_version: "latest",
        repo: "cli/cli",
        asset_pattern: match os {
            Os::Mac => "gh_{version}_macOS_{alt-arch}.zip",
            Os::Linux => "gh_{version}_linux_{alt-arch}.tar.gz",
            Os::Windows => "gh_{version}_windows_{alt-arch}.zip",
        },
        checksum_pattern: Some("gh_{version}_checksums.txt"),
    }
}

impl Installer for Gh {
    fn name(&self) -> &'static str {
        "gh"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("gh")
    }

    fn supports_pin(&self) -> bool {
        true
    }

    fn resolve_target(&self, ctx: &InstallCtx) -> anyhow::Result<Option<String>> {
        super::resolve_release_target(ctx, &release_spec(ctx.os))
    }

    fn verifies_target(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        vec![fetch_plan(
            &release_spec(ctx.os),
            ctx.pin.as_deref().unwrap_or("latest stable"),
        )]
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx.pin.as_deref().ok_or_else(|| {
            anyhow::anyhow!("the gh release version was not resolved before installation")
        })?;
        fetch_to_user_bin(ctx, &release_spec(ctx.os), version)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::tools::{Arch, Platform};

    #[test]
    fn gh_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("gh")
            .with_output("gh --version", &probe_fixture("gh.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(Gh.detect(&ctx).as_deref(), Some("2.95.0"));
    }

    #[test]
    fn gh_mac_without_brew_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Gh.install(&InstallCtx {
            pin: Some("2.96.0".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        })
        .expect("fetch must succeed");
        let calls = fetcher.calls.borrow();
        assert_eq!(calls[0].spec.name, "gh");
        assert_eq!(calls[0].version, "2.96.0");
    }

    #[test]
    fn gh_linux_without_apt_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Gh.install(&InstallCtx {
            pin: Some("2.96.0".to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        })
        .expect("fetch must succeed");
        assert!(runner.calls.borrow().is_empty());
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "gh");
    }

    #[test]
    fn gh_windows_without_winget_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Gh.install(&InstallCtx {
            pin: Some("2.96.0".to_string()),
            ..ctx_on(Os::Windows, &runner, &fetcher)
        })
        .expect("fetch must succeed");
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "gh");
    }

    #[test]
    fn gh_pin_forces_the_release_binary_over_package_managers() {
        for os in [Os::Mac, Os::Linux] {
            let runner = FakeRunner::default().on_path("brew").on_path("apt-get");
            let fetcher = FakeFetcher::default();
            let ctx = InstallCtx {
                pin: Some("2.90.0".to_string()),
                ..ctx_on(os, &runner, &fetcher)
            };
            Gh.install(&ctx).expect("pinned fetch must succeed");
            assert!(runner.calls.borrow().is_empty(), "{os:?}");
            assert_eq!(fetcher.calls.borrow()[0].version, "2.90.0", "{os:?}");
        }
    }

    #[test]
    fn gh_release_assets_resolve_to_the_published_names() {
        let arm = |os| Platform {
            os,
            arch: Arch::Aarch64,
        };
        let cases = [
            (Os::Mac, "gh_2.96.0_macOS_arm64.zip"),
            (Os::Linux, "gh_2.96.0_linux_arm64.tar.gz"),
            (Os::Windows, "gh_2.96.0_windows_arm64.zip"),
        ];
        for (os, want) in cases {
            let spec = release_spec(os);
            assert_eq!(spec.asset_name(arm(os), "2.96.0"), want, "{os:?}");
            assert_eq!(
                spec.checksum_asset_name(arm(os), "2.96.0").as_deref(),
                Some("gh_2.96.0_checksums.txt"),
                "{os:?}"
            );
        }
    }
}
