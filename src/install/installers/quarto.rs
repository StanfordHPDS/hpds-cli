//! Installer for the quarto CLI.
//!
//! macOS and Linux: the GitHub release tarball, extracted whole under the
//! per-user `~/.local/opt` with a launcher in `~/.local/bin` — no sudo,
//! unlike the system-wide pkg//opt paths. Windows: winget when present
//! (which runs the official MSI), else the release zip into the same
//! per-user layout. quarto is a directory tree (`bin/` + `share/`), so the
//! release path goes through the fetcher's whole-tree install.

use crate::install::fetch::{user_bin_dir, user_opt_dir};
use crate::install::{InstallCtx, Installer};
use crate::tools::{Os, ToolKind, ToolSpec, versions};
use crate::ui;

use super::on_path;

pub struct Quarto;

/// The quarto release archive for one OS: tarballs with a top-level
/// `quarto-{version}/` directory on macOS (universal) and Linux
/// (Go-style arches), a flat zip on Windows.
pub(super) fn release_spec(os: Os) -> ToolSpec {
    ToolSpec {
        name: "quarto",
        default_version: versions::QUARTO,
        kind: ToolKind::GithubBinary {
            repo: "quarto-dev/quarto-cli",
            asset_pattern: match os {
                Os::Mac => "quarto-{version}-macos.tar.gz",
                Os::Linux => "quarto-{version}-linux-{alt-arch}.tar.gz",
                Os::Windows => "quarto-{version}-win.zip",
            },
            checksum_pattern: Some("quarto-{version}-checksums.txt"),
        },
    }
}

impl Installer for Quarto {
    fn name(&self) -> &'static str {
        "quarto"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("quarto")
    }

    fn supports_pin(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        let version = ctx
            .pin
            .clone()
            .unwrap_or_else(|| versions::QUARTO.to_string());
        match ctx.os {
            Os::Mac | Os::Linux => vec![tree_plan(&version)],
            Os::Windows => {
                if on_path(ctx, "winget") {
                    let mut line = "winget install --id Posit.Quarto --exact".to_string();
                    if let Some(pin) = ctx.pin.as_deref() {
                        line.push_str(&format!(" --version {pin}"));
                    }
                    vec![line]
                } else {
                    vec![tree_plan(&version)]
                }
            }
        }
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx
            .pin
            .clone()
            .unwrap_or_else(|| versions::QUARTO.to_string());
        match ctx.os {
            Os::Mac | Os::Linux => fetch_tree_to_user_dirs(ctx, &version)?,
            Os::Windows => {
                if on_path(ctx, "winget") {
                    let mut args = vec!["install", "--id", "Posit.Quarto", "--exact"];
                    if let Some(pin) = ctx.pin.as_deref() {
                        args.extend(["--version", pin]);
                    }
                    ctx.run_step("installing quarto with winget", "winget", &args)?;
                } else {
                    fetch_tree_to_user_dirs(ctx, &version)?;
                }
            }
        }
        Ok(())
    }
}

/// One plan line for [`fetch_tree_to_user_dirs`]: the release download
/// and where the tree and launcher will land.
fn tree_plan(version: &str) -> String {
    match (user_opt_dir(), user_bin_dir()) {
        (Ok(opt), Ok(bin)) => format!(
            "download quarto {version} into `{}` with a launcher in `{}`",
            opt.display(),
            bin.display()
        ),
        _ => format!("download the quarto {version} release"),
    }
}

/// Download the release archive and install its whole tree under the
/// per-user opt directory, with a launcher on the user's bin dir.
fn fetch_tree_to_user_dirs(ctx: &InstallCtx, version: &str) -> anyhow::Result<()> {
    let opt_dir = user_opt_dir()?;
    let bin_dir = user_bin_dir()?;
    ui::println(&format!(
        "downloading quarto {version} into `{}`",
        opt_dir.display()
    ));
    ctx.fetcher
        .fetch_tree(&release_spec(ctx.os), version, &opt_dir, &bin_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::tools::{Arch, Platform};
    use std::path::Path;

    #[test]
    fn quarto_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("quarto")
            .with_output("quarto --version", &probe_fixture("quarto.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(Quarto.detect(&ctx).as_deref(), Some("1.9.36"));
    }

    #[test]
    fn quarto_mac_and_linux_fetch_the_release_tree_into_user_dirs() {
        // The user-dir tarball path needs no sudo, so it is the strategy
        // even when brew or apt are around.
        for os in [Os::Mac, Os::Linux] {
            let runner = FakeRunner::default().on_path("brew").on_path("apt-get");
            let fetcher = FakeFetcher::default();
            Quarto
                .install(&ctx_on(os, &runner, &fetcher))
                .expect("tree fetch must succeed");
            assert!(runner.calls.borrow().is_empty(), "{os:?}");
            let calls = fetcher.tree_calls.borrow();
            assert_eq!(calls.len(), 1, "{os:?}");
            assert_eq!(calls[0].spec.name, "quarto", "{os:?}");
            assert_eq!(calls[0].version, versions::QUARTO, "{os:?}");
            assert!(
                calls[0].opt_dir.ends_with(Path::new(".local").join("opt")),
                "{os:?}: {:?}",
                calls[0].opt_dir
            );
            assert!(
                calls[0].bin_dir.ends_with(Path::new(".local").join("bin")),
                "{os:?}: {:?}",
                calls[0].bin_dir
            );
        }
    }

    #[test]
    fn quarto_windows_uses_winget_when_present() {
        let runner = FakeRunner::default()
            .on_path("winget")
            .with_output("winget install --id Posit.Quarto --exact", "");
        let fetcher = FakeFetcher::default();
        Quarto
            .install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("winget install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id Posit.Quarto --exact"]
        );
        assert!(fetcher.tree_calls.borrow().is_empty());
    }

    #[test]
    fn quarto_windows_winget_pins_with_its_version_flag() {
        let runner = FakeRunner::default().on_path("winget").with_output(
            "winget install --id Posit.Quarto --exact --version 1.8.27",
            "",
        );
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("1.8.27".to_string()),
            ..ctx_on(Os::Windows, &runner, &fetcher)
        };
        Quarto
            .install(&ctx)
            .expect("pinned winget install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id Posit.Quarto --exact --version 1.8.27"]
        );
    }

    #[test]
    fn quarto_windows_without_winget_fetches_the_release_tree() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Quarto
            .install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("tree fetch must succeed");
        assert_eq!(fetcher.tree_calls.borrow()[0].spec.name, "quarto");
    }

    #[test]
    fn quarto_pin_fetches_that_version_on_mac_and_linux() {
        for os in [Os::Mac, Os::Linux] {
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();
            let ctx = InstallCtx {
                pin: Some("1.8.27".to_string()),
                ..ctx_on(os, &runner, &fetcher)
            };
            Quarto.install(&ctx).expect("pinned fetch must succeed");
            assert_eq!(fetcher.tree_calls.borrow()[0].version, "1.8.27", "{os:?}");
        }
    }

    #[test]
    fn quarto_release_assets_resolve_to_the_published_names() {
        let cases = [
            (Os::Mac, Arch::Aarch64, "quarto-1.9.36-macos.tar.gz"),
            (Os::Linux, Arch::X86_64, "quarto-1.9.36-linux-amd64.tar.gz"),
            (Os::Linux, Arch::Aarch64, "quarto-1.9.36-linux-arm64.tar.gz"),
            (Os::Windows, Arch::X86_64, "quarto-1.9.36-win.zip"),
        ];
        for (os, arch, want) in cases {
            let platform = Platform { os, arch };
            let spec = release_spec(os);
            assert_eq!(
                spec.asset_name(platform, "1.9.36").as_deref(),
                Some(want),
                "{os:?}/{arch:?}"
            );
            assert_eq!(
                spec.checksum_asset_name(platform, "1.9.36").as_deref(),
                Some("quarto-1.9.36-checksums.txt"),
                "{os:?}/{arch:?}"
            );
        }
    }

    #[test]
    fn quarto_supports_version_pins() {
        assert!(Quarto.supports_pin());
    }

    #[test]
    fn quarto_plan_mirrors_the_strategy_selection() {
        let fetcher = FakeFetcher::default();
        let bare = FakeRunner::default();

        let plan = Quarto.plan(&ctx_on(Os::Linux, &bare, &fetcher));
        assert_eq!(plan.len(), 1);
        assert!(plan[0].contains("download quarto"), "{plan:?}");
        assert!(plan[0].contains(versions::QUARTO), "{plan:?}");

        let with_winget = FakeRunner::default().on_path("winget");
        assert_eq!(
            Quarto.plan(&ctx_on(Os::Windows, &with_winget, &fetcher)),
            vec!["winget install --id Posit.Quarto --exact".to_string()]
        );
    }
}
