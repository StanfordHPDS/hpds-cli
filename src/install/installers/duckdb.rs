//! Installer for the `duckdb` CLI.
//!
//! macOS: Homebrew when present, else the release binary. Linux: the
//! release binary into `~/.local/bin` — the same thing duckdb's official
//! installer script does. Windows: winget when present, else the release
//! binary. A `--version` pin always takes the release-binary path.

use crate::install::{InstallCtx, Installer};
use crate::tools::{Os, ToolSpec, versions};

use super::{fetch_plan, fetch_to_user_bin, on_path};

pub struct DuckDb;

/// The duckdb CLI release archive for one OS: zips on every platform,
/// `osx-universal` on macOS, Go-style arches elsewhere. duckdb publishes
/// no per-asset checksums, so the downloader warns and skips verification.
pub(super) fn release_spec(os: Os) -> ToolSpec {
    ToolSpec {
        name: "duckdb",
        default_version: versions::DUCKDB,
        repo: "duckdb/duckdb",
        asset_pattern: match os {
            Os::Mac => "duckdb_cli-osx-universal.zip",
            Os::Linux => "duckdb_cli-linux-{alt-arch}.zip",
            Os::Windows => "duckdb_cli-windows-{alt-arch}.zip",
        },
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

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        let version = ctx
            .pin
            .clone()
            .unwrap_or_else(|| versions::DUCKDB.to_string());
        match ctx.os {
            Os::Mac if ctx.pin.is_none() && on_path(ctx, "brew") => {
                vec!["brew install duckdb".to_string()]
            }
            Os::Windows if ctx.pin.is_none() && on_path(ctx, "winget") => {
                vec!["winget install --id DuckDB.cli --exact".to_string()]
            }
            _ => vec![fetch_plan(&release_spec(ctx.os), &version)],
        }
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx
            .pin
            .clone()
            .unwrap_or_else(|| versions::DUCKDB.to_string());
        match ctx.os {
            Os::Mac => {
                if ctx.pin.is_none() && on_path(ctx, "brew") {
                    ctx.run_step(
                        "installing duckdb with Homebrew",
                        "brew",
                        &["install", "duckdb"],
                    )?;
                } else {
                    fetch_to_user_bin(ctx, &release_spec(ctx.os), &version)?;
                }
            }
            Os::Linux => {
                fetch_to_user_bin(ctx, &release_spec(ctx.os), &version)?;
            }
            Os::Windows => {
                if ctx.pin.is_none() && on_path(ctx, "winget") {
                    ctx.run_step(
                        "installing duckdb with winget",
                        "winget",
                        &["install", "--id", "DuckDB.cli", "--exact"],
                    )?;
                } else {
                    fetch_to_user_bin(ctx, &release_spec(ctx.os), &version)?;
                }
            }
        }
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
    fn duckdb_mac_prefers_brew_when_present() {
        let runner = FakeRunner::default()
            .on_path("brew")
            .with_output("brew install duckdb", "");
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("brew install must succeed");
        assert_eq!(*runner.calls.borrow(), vec!["brew install duckdb"]);
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn duckdb_mac_without_brew_fetches_the_universal_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("fetch must succeed");
        let calls = fetcher.calls.borrow();
        assert_eq!(calls[0].spec.name, "duckdb");
        assert_eq!(calls[0].version, versions::DUCKDB);
    }

    #[test]
    fn duckdb_linux_fetches_the_release_binary_like_the_official_installer() {
        // Even with apt around: duckdb has no apt repo, and the official
        // installer script just downloads the binary.
        let runner = FakeRunner::default().on_path("apt-get");
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect("fetch must succeed");
        assert!(runner.calls.borrow().is_empty());
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "duckdb");
    }

    #[test]
    fn duckdb_windows_uses_winget_when_present() {
        let runner = FakeRunner::default()
            .on_path("winget")
            .with_output("winget install --id DuckDB.cli --exact", "");
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("winget install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id DuckDB.cli --exact"]
        );
    }

    #[test]
    fn duckdb_windows_without_winget_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        DuckDb
            .install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("fetch must succeed");
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "duckdb");
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
        assert!(runner.calls.borrow().is_empty());
        assert_eq!(fetcher.calls.borrow()[0].version, "1.5.0");
    }

    #[test]
    fn duckdb_plan_mirrors_the_strategy_selection() {
        let fetcher = FakeFetcher::default();

        let with_brew = FakeRunner::default().on_path("brew");
        assert_eq!(
            DuckDb.plan(&ctx_on(Os::Mac, &with_brew, &fetcher)),
            vec!["brew install duckdb".to_string()]
        );

        let bare = FakeRunner::default();
        let plan = DuckDb.plan(&ctx_on(Os::Mac, &bare, &fetcher));
        assert_eq!(plan.len(), 1);
        assert!(plan[0].contains("download"), "{plan:?}");
        assert!(plan[0].contains(versions::DUCKDB), "{plan:?}");
        assert!(
            plan[0].contains("github.com/duckdb/duckdb"),
            "the plan must name where the binary comes from: {plan:?}"
        );

        let pinned = InstallCtx {
            pin: Some("1.5.0".to_string()),
            ..ctx_on(Os::Mac, &with_brew, &fetcher)
        };
        let plan = DuckDb.plan(&pinned);
        assert!(
            plan[0].contains("1.5.0"),
            "a pin must take the release path: {plan:?}"
        );
    }

    #[test]
    fn duckdb_release_assets_resolve_to_the_published_names() {
        let cases = [
            (Os::Mac, Arch::Aarch64, "duckdb_cli-osx-universal.zip"),
            (Os::Linux, Arch::X86_64, "duckdb_cli-linux-amd64.zip"),
            (Os::Linux, Arch::Aarch64, "duckdb_cli-linux-arm64.zip"),
            (Os::Windows, Arch::X86_64, "duckdb_cli-windows-amd64.zip"),
        ];
        for (os, arch, want) in cases {
            let platform = Platform { os, arch };
            assert_eq!(
                release_spec(os).asset_name(platform, "1.5.4"),
                want,
                "{os:?}/{arch:?}"
            );
        }
    }
}
