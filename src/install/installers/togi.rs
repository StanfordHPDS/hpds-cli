//! Installer for `togi`, the lab's formatter/linter.
//!
//! Every OS: the release binary from StanfordHPDS/togi into the per-user
//! bin directory. togi is lab-published (no package-manager presence), so
//! the release binary is the only strategy, pinned or not.

use crate::install::{InstallCtx, Installer};
use crate::tools::ToolSpec;
use crate::ui::HintExt;

use super::{fetch_plan, fetch_to_user_bin};

pub struct Togi;

/// The togi release archive: Rust-triple asset names with a sha256
/// sidecar per asset. Mirrors togi's cargo-dist config (gzip tarballs on
/// unix, zip on Windows), the same shape this repo publishes.
pub(crate) fn release_spec() -> ToolSpec {
    ToolSpec {
        name: "togi",
        // Dynamic installs resolve a concrete version before downloading.
        default_version: "latest",
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
        let version = ctx.pin.as_deref().unwrap_or("latest stable");
        vec![fetch_plan(&release_spec(), version)]
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx.pin.as_deref().ok_or_else(|| {
            anyhow::anyhow!("the togi release version was not resolved before installation")
        })?;
        fetch_to_user_bin(ctx, &release_spec(), version)?;
        Ok(())
    }

    fn resolve_target(&self, ctx: &InstallCtx) -> anyhow::Result<Option<String>> {
        let requested = match ctx.pin.as_deref() {
            Some(pin) => pin.to_string(),
            None => ctx.fetcher.latest_version(&release_spec())?,
        };
        let version = requested
            .strip_prefix('v')
            .unwrap_or(&requested)
            .to_string();
        validate_version(&version)?;
        Ok(Some(version))
    }

    fn verifies_target(&self) -> bool {
        true
    }
}

fn validate_version(version: &str) -> anyhow::Result<()> {
    let valid = !version.is_empty()
        && version.split('.').count() == 3
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if valid {
        Ok(())
    } else {
        Err(anyhow::anyhow!("invalid togi release version `{version}`"))
            .hint("use a bare stable version such as `0.1.1`")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::install::{CommandOutput, CommandRunner, run_installer};
    use crate::tools::{Arch, Os, Platform};

    #[test]
    fn togi_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", &probe_fixture("togi.txt"));
        let fetcher = FakeFetcher::default().with_latest("0.1.1");
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
            // package: the release binary is the only path.
            let runner = FakeRunner::default()
                .on_path("brew")
                .on_path("apt-get")
                .on_path("winget");
            let fetcher = FakeFetcher::default().with_latest("0.1.1");
            let _ = run_installer(&Togi, &ctx_on(os, &runner, &fetcher));
            assert!(
                runner.calls.borrow().is_empty(),
                "{os:?}: no package manager runs"
            );
            let calls = fetcher.calls.borrow();
            assert_eq!(calls.len(), 1, "{os:?}");
            assert_eq!(calls[0].spec.name, "togi", "{os:?}");
            assert_eq!(calls[0].version, "0.1.1", "{os:?}");
            assert_eq!(*fetcher.latest_calls.borrow(), vec!["togi"], "{os:?}");
        }
    }

    #[test]
    fn unpinned_togi_replaces_an_installed_older_entry_point() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", "togi 0.1.0");
        let fetcher = FakeFetcher::default().with_latest("0.1.1");
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);

        // The recording fetcher does not replace the fake PATH entry, so
        // post-install verification may fail after the download attempt.
        let _ = run_installer(&Togi, &ctx);

        assert_eq!(
            fetcher.calls.borrow().len(),
            1,
            "an existing togi entry point must not bypass latest-release installation"
        );
        assert_eq!(
            fetcher.latest_calls.borrow().len(),
            1,
            "the latest release must be resolved exactly once"
        );
    }

    #[test]
    fn unpinned_togi_upgrade_completes_after_the_new_entry_point_is_visible() {
        struct UpgradingRunner {
            probes: Cell<usize>,
        }

        impl CommandRunner for UpgradingRunner {
            fn which(&self, program: &str) -> Option<PathBuf> {
                (program == "togi").then(|| PathBuf::from("/fake/bin/togi"))
            }

            fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
                assert_eq!((program, args), ("togi", ["--version"].as_slice()));
                let probe = self.probes.get();
                self.probes.set(probe + 1);
                Ok(CommandOutput {
                    success: true,
                    stdout: format!("togi {}", if probe == 0 { "0.1.0" } else { "0.1.1" }),
                    stderr: String::new(),
                })
            }
        }

        let runner = UpgradingRunner {
            probes: Cell::new(0),
        };
        let fetcher = FakeFetcher::default().with_latest("0.1.1");
        let ctx = InstallCtx {
            os: Os::Mac,
            yes: true,
            verbose: false,
            pin: None,
            plan_approved: false,
            sudo_approved: Cell::new(false),
            runner: &runner,
            fetcher: &fetcher,
        };

        run_installer(&Togi, &ctx).expect("the latest entry point must verify successfully");

        assert_eq!(runner.probes.get(), 2);
        assert_eq!(*fetcher.latest_calls.borrow(), vec!["togi"]);
        assert_eq!(fetcher.calls.borrow().len(), 1);
        assert_eq!(fetcher.calls.borrow()[0].version, "0.1.1");
    }

    #[test]
    fn stale_path_entry_after_install_reports_the_requested_and_found_versions() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", "togi 0.1.0");
        let fetcher = FakeFetcher::default().with_latest("0.1.1");

        let err = run_installer(&Togi, &ctx_on(Os::Mac, &runner, &fetcher))
            .expect_err("a stale PATH entry must not be reported as updated");
        let rendered = crate::ui::render_error(&err, false);

        assert!(rendered.contains("0.1.1"), "{rendered}");
        assert!(rendered.contains("0.1.0"), "{rendered}");
        assert!(rendered.contains("PATH"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    #[test]
    fn unpinned_togi_at_the_latest_version_is_a_no_op() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", "togi 0.1.1");
        let fetcher = FakeFetcher::default().with_latest("0.1.1");

        run_installer(&Togi, &ctx_on(Os::Mac, &runner, &fetcher))
            .expect("the current release must be a no-op");

        assert!(fetcher.calls.borrow().is_empty());
        assert_eq!(*fetcher.latest_calls.borrow(), vec!["togi"]);
    }

    #[test]
    fn latest_lookup_failure_does_not_fall_back_to_the_baked_version() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default().with_latest_error("release metadata unavailable");

        let err = run_installer(&Togi, &ctx_on(Os::Linux, &runner, &fetcher))
            .expect_err("an unavailable latest release must fail");

        assert!(err.to_string().contains("metadata unavailable"), "{err:#}");
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn malformed_remote_or_explicit_versions_never_reach_the_downloader() {
        for (pin, latest) in [(None, Some("../0.1.1")), (Some("../0.1.1"), None)] {
            let runner = FakeRunner::default();
            let fetcher = match latest {
                Some(version) => FakeFetcher::default().with_latest(version),
                None => FakeFetcher::default(),
            };
            let ctx = InstallCtx {
                pin: pin.map(str::to_string),
                ..ctx_on(Os::Mac, &runner, &fetcher)
            };

            let err = run_installer(&Togi, &ctx).expect_err("unsafe version must fail");

            assert!(err.to_string().contains("invalid togi release version"));
            assert!(fetcher.calls.borrow().is_empty());
        }
    }

    #[test]
    fn v_prefixed_pin_bypasses_lookup_and_fetches_the_bare_version() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("v0.1.1".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };

        let _ = run_installer(&Togi, &ctx);

        assert!(fetcher.latest_calls.borrow().is_empty());
        assert_eq!(fetcher.calls.borrow().len(), 1);
        assert_eq!(fetcher.calls.borrow()[0].version, "0.1.1");
    }

    #[test]
    fn v_prefixed_pin_matches_an_installed_bare_version() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", "togi 0.1.1");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("v0.1.1".to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };

        run_installer(&Togi, &ctx).expect("equivalent installed version must be a no-op");

        assert!(fetcher.latest_calls.borrow().is_empty());
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn togi_pin_fetches_the_pinned_version() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("0.2.0".to_string()),
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };
        let _ = run_installer(&Togi, &ctx);
        assert_eq!(fetcher.calls.borrow()[0].version, "0.2.0");
        assert!(
            fetcher.latest_calls.borrow().is_empty(),
            "an explicit pin must bypass latest-release lookup"
        );
    }

    #[test]
    fn togi_plan_is_the_release_download_on_every_os() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            let plan = Togi.plan(&ctx_on(os, &runner, &fetcher));
            assert_eq!(plan.len(), 1, "{os:?}");
            assert!(plan[0].contains("download"), "{os:?}: {plan:?}");
            assert!(
                plan[0].contains("latest stable"),
                "an offline plan must describe the unpinned target symbolically; {os:?}: {plan:?}"
            );
            assert!(
                !plan[0].contains("0.1.0"),
                "an unpinned plan must not promise the baked fallback; {os:?}: {plan:?}"
            );
            assert!(
                plan[0].contains("github.com/StanfordHPDS/togi"),
                "the plan must name where the binary comes from; {os:?}: {plan:?}"
            );
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
