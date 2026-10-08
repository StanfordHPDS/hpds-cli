//! Installer for the quarto CLI.
//!
//! Every OS downloads the latest stable upstream archive and extracts it under
//! the per-user `~/.local/opt`, with a launcher in `~/.local/bin`. Quarto is a
//! directory tree (`bin/` plus `share/`), so it uses the whole-tree release path.

use crate::install::fetch::{user_bin_dir, user_opt_dir};
use crate::install::{InstallCtx, Installer};
use crate::tools::{Os, ToolSpec};
use crate::ui;

pub struct Quarto;

/// The quarto release archive for one OS: tarballs with a top-level
/// `quarto-{version}/` directory on macOS (universal) and Linux
/// (Go-style arches), a flat zip on Windows.
pub(super) fn release_spec(os: Os) -> ToolSpec {
    ToolSpec {
        name: "quarto",
        default_version: "latest",
        repo: "quarto-dev/quarto-cli",
        asset_pattern: match os {
            Os::Mac => "quarto-{version}-macos.tar.gz",
            Os::Linux => "quarto-{version}-linux-{alt-arch}.tar.gz",
            Os::Windows => "quarto-{version}-win.zip",
        },
        checksum_pattern: Some("quarto-{version}-checksums.txt"),
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

    fn resolve_target(&self, ctx: &InstallCtx) -> anyhow::Result<Option<String>> {
        super::resolve_release_target(ctx, &release_spec(ctx.os))
    }

    fn verifies_target(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        vec![tree_plan(ctx.pin.as_deref().unwrap_or("latest stable"))]
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx.pin.as_deref().ok_or_else(|| {
            anyhow::anyhow!("the Quarto release version was not resolved before installation")
        })?;
        fetch_tree_to_user_dirs(ctx, version)
    }
}

/// One plan line for [`fetch_tree_to_user_dirs`]: the release download,
/// where it comes from, and where the tree and launcher will land.
fn tree_plan(version: &str) -> String {
    let source = "github.com/quarto-dev/quarto-cli releases";
    match (user_opt_dir(), user_bin_dir()) {
        (Ok(opt), Ok(bin)) => format!(
            "download quarto {version} from {source} into `{}` with a launcher in `{}`",
            opt.display(),
            bin.display()
        ),
        _ => format!("download the quarto {version} release from {source}"),
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
                .install(&InstallCtx {
                    pin: Some("1.9.36".to_string()),
                    ..ctx_on(os, &runner, &fetcher)
                })
                .expect("tree fetch must succeed");
            assert!(runner.calls.borrow().is_empty(), "{os:?}");
            let calls = fetcher.tree_calls.borrow();
            assert_eq!(calls.len(), 1, "{os:?}");
            assert_eq!(calls[0].spec.name, "quarto", "{os:?}");
            assert_eq!(calls[0].version, "1.9.36", "{os:?}");
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
    fn quarto_windows_without_winget_fetches_the_release_tree() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Quarto
            .install(&InstallCtx {
                pin: Some("1.9.36".to_string()),
                ..ctx_on(Os::Windows, &runner, &fetcher)
            })
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
            assert_eq!(spec.asset_name(platform, "1.9.36"), want, "{os:?}/{arch:?}");
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
}
