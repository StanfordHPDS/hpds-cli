//! Installer for the `duckdb` CLI.
//!
//! Every OS downloads the latest stable upstream release binary into the shared
//! cache and places it in the user bin directory. GitHub's asset digest is
//! mandatory. Exact pins use their release metadata without a latest lookup.

use crate::install::{InstallCtx, Installer};
use crate::tools::{LegacyAsset, Os, ToolSpec};

use super::{fetch_plan, fetch_to_user_bin};

pub struct DuckDb;

/// The duckdb CLI release archive for one OS: tarballs from 2.0 on and zips
/// before, `osx-universal` on macOS, Go-style arches elsewhere. duckdb
/// publishes no per-asset checksums; GitHub's asset digest verifies the
/// archive instead.
pub(super) fn release_spec(os: Os) -> ToolSpec {
    let (asset_pattern, legacy_pattern) = match os {
        Os::Mac => (
            "duckdb-cli-osx-universal.tar.gz",
            "duckdb_cli-osx-universal.zip",
        ),
        Os::Linux => (
            "duckdb-cli-linux-{alt-arch}.tar.gz",
            "duckdb_cli-linux-{alt-arch}.zip",
        ),
        Os::Windows => (
            "duckdb-cli-windows-{alt-arch}.tar.gz",
            "duckdb_cli-windows-{alt-arch}.zip",
        ),
    };
    ToolSpec {
        name: "duckdb",
        default_version: "latest",
        repo: "duckdb/duckdb",
        asset_pattern,
        legacy_asset: Some(LegacyAsset {
            before_major: 2,
            pattern: legacy_pattern,
        }),
        checksum_pattern: None,
    }
}

impl Installer for DuckDb {
    fn name(&self) -> &'static str {
        "duckdb"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("duckdb")
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
            anyhow::anyhow!("the DuckDB release version was not resolved before installation")
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
    fn duckdb_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("duckdb")
            .with_output("duckdb --version", &probe_fixture("duckdb.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(DuckDb.detect(&ctx).as_deref(), Some("1.5.4"));
    }

    #[test]
    fn duckdb_mac_without_brew_fetches_the_universal_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&InstallCtx {
                pin: Some("1.5.4".to_string()),
                ..ctx_on(Os::Mac, &runner, &fetcher)
            })
            .expect("fetch must succeed");
        let calls = fetcher.calls();
        assert_eq!(calls[0].spec.name, "duckdb");
        assert_eq!(calls[0].version, "1.5.4");
    }

    #[test]
    fn duckdb_linux_fetches_the_release_binary_like_the_official_installer() {
        // Even with apt around: duckdb has no apt repo, and the official
        // installer script just downloads the binary.
        let runner = FakeRunner::default().on_path("apt-get");
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&InstallCtx {
                pin: Some("1.5.4".to_string()),
                ..ctx_on(Os::Linux, &runner, &fetcher)
            })
            .expect("fetch must succeed");
        assert!(runner.calls().is_empty());
        assert_eq!(fetcher.calls()[0].spec.name, "duckdb");
    }

    #[test]
    fn duckdb_windows_without_winget_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&InstallCtx {
                pin: Some("1.5.4".to_string()),
                ..ctx_on(Os::Windows, &runner, &fetcher)
            })
            .expect("fetch must succeed");
        assert_eq!(fetcher.calls()[0].spec.name, "duckdb");
    }

    #[test]
    fn duckdb_pin_forces_the_release_binary_even_with_brew() {
        let runner = FakeRunner::default().on_path("brew");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("1.5.0".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };
        DuckDb.install(&ctx).expect("pinned fetch must succeed");
        assert!(runner.calls().is_empty());
        assert_eq!(fetcher.calls()[0].version, "1.5.0");
    }

    #[test]
    fn duckdb_release_assets_resolve_to_the_published_names() {
        let cases = [
            (
                Os::Mac,
                Arch::Aarch64,
                "2.0.0",
                "duckdb-cli-osx-universal.tar.gz",
            ),
            (
                Os::Linux,
                Arch::X86_64,
                "2.0.0",
                "duckdb-cli-linux-amd64.tar.gz",
            ),
            (
                Os::Linux,
                Arch::Aarch64,
                "2.1.3",
                "duckdb-cli-linux-arm64.tar.gz",
            ),
            (
                Os::Windows,
                Arch::X86_64,
                "2.0.0",
                "duckdb-cli-windows-amd64.tar.gz",
            ),
            (
                Os::Mac,
                Arch::Aarch64,
                "1.5.4",
                "duckdb_cli-osx-universal.zip",
            ),
            (
                Os::Linux,
                Arch::X86_64,
                "1.5.4",
                "duckdb_cli-linux-amd64.zip",
            ),
            (
                Os::Linux,
                Arch::Aarch64,
                "1.4.5",
                "duckdb_cli-linux-arm64.zip",
            ),
            (
                Os::Windows,
                Arch::X86_64,
                "1.5.4",
                "duckdb_cli-windows-amd64.zip",
            ),
        ];
        for (os, arch, version, want) in cases {
            let platform = Platform { os, arch };
            assert_eq!(
                release_spec(os).asset_name(platform, version),
                want,
                "{os:?}/{arch:?} {version}"
            );
        }
    }
}
