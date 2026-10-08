//! Concrete tool installers behind `hpds install <tool>`.
//!
//! Each installer picks a strategy from the injected OS, probes package
//! managers through the runner seam, and downloads release binaries
//! through the fetcher seam, so every strategy is assertable offline.

pub mod duckdb;
pub mod gh;
pub mod quarto;
pub mod r;
pub mod rig;
pub mod rstudio_server;
pub mod tinytex;
pub mod togi;
pub mod uv;

use std::path::PathBuf;

use crate::tools::ToolSpec;
use crate::ui::{self, HintExt};

use super::InstallCtx;
use super::fetch::{user_bin_dir, warn_if_off_path};

fn resolve_release_target(ctx: &InstallCtx, spec: &ToolSpec) -> anyhow::Result<Option<String>> {
    let requested = match ctx.pin.as_deref() {
        Some(version) => version.to_string(),
        None => ctx.fetcher.latest_version(spec)?,
    };
    let version = requested.strip_prefix('v').unwrap_or(&requested);
    validate_release_version(version, spec.name)?;
    Ok(Some(version.to_string()))
}

fn validate_release_version(version: &str, tool: &str) -> anyhow::Result<()> {
    let valid = version.split('.').count() == 3
        && version.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        });
    if valid {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "invalid {tool} release version `{version}`"
        ))
        .hint("use an exact stable version such as `1.2.3`")
    }
}

/// Whether `program` is on `PATH`, probed through the runner seam.
fn on_path(ctx: &InstallCtx, program: &str) -> bool {
    ctx.runner.which(program).is_some()
}

/// One plan line for the release-binary strategy: what
/// [`fetch_to_user_bin`] will download, where it comes from (the tool's
/// GitHub releases), and where it will land.
fn fetch_plan(spec: &ToolSpec, version: &str) -> String {
    let ToolSpec { name, repo, .. } = spec;
    match user_bin_dir() {
        Ok(dir) => format!(
            "download the {name} {version} release binary from github.com/{repo} \
             releases into `{}`",
            dir.display()
        ),
        Err(_) => {
            format!("download the {name} {version} release binary from github.com/{repo} releases")
        }
    }
}

/// Download `spec`'s release binary at `version` and place it in the
/// per-user bin directory, warning when that directory is off `PATH`.
fn fetch_to_user_bin(ctx: &InstallCtx, spec: &ToolSpec, version: &str) -> anyhow::Result<PathBuf> {
    let bin_dir = user_bin_dir()?;
    ui::println(&format!(
        "downloading {} {version} into `{}`",
        spec.name,
        bin_dir.display()
    ));
    let installed = ctx.fetcher.fetch_binary(spec, version, &bin_dir)?;
    warn_if_off_path(&bin_dir);
    Ok(installed)
}

#[cfg(test)]
mod runtime_release_tests {
    use super::{
        duckdb::{self, DuckDb},
        gh::{self, Gh},
        quarto::{self, Quarto},
        togi::{self, Togi},
        uv::{self, Uv},
    };
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on};
    use crate::install::{InstallCtx, Installer, run_installer};
    use crate::tools::{Arch, Os, Platform};

    fn fetched_versions(fetcher: &FakeFetcher) -> Vec<String> {
        fetcher
            .calls
            .borrow()
            .iter()
            .map(|call| call.version.clone())
            .chain(
                fetcher
                    .tree_calls
                    .borrow()
                    .iter()
                    .map(|call| call.version.clone()),
            )
            .collect()
    }

    #[test]
    fn recorded_releases_contain_the_exact_linux_amd64_assets() {
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        for (body, spec, version) in [
            (
                include_str!("../../../tests/fixtures/releases/uv-latest.json"),
                uv::release_spec(),
                "0.10.1",
            ),
            (
                include_str!("../../../tests/fixtures/releases/gh-latest.json"),
                gh::release_spec(Os::Linux),
                "2.97.1",
            ),
            (
                include_str!("../../../tests/fixtures/releases/duckdb-latest.json"),
                duckdb::release_spec(Os::Linux),
                "1.5.5",
            ),
            (
                include_str!("../../../tests/fixtures/releases/quarto-latest.json"),
                quarto::release_spec(Os::Linux),
                "1.10.19",
            ),
            (
                include_str!("../../../tests/fixtures/releases/togi-latest.json"),
                togi::release_spec(),
                "0.2.0",
            ),
        ] {
            let value: serde_json::Value = serde_json::from_str(body).expect("fixture JSON");
            let names: Vec<&str> = value["assets"]
                .as_array()
                .expect("assets")
                .iter()
                .map(|asset| asset["name"].as_str().expect("asset name"))
                .collect();
            assert!(names.contains(&spec.asset_name(platform, version).as_str()));
            if let Some(checksum) = spec.checksum_asset_name(platform, version) {
                assert!(names.contains(&checksum.as_str()));
            }
        }
    }

    #[test]
    fn unpinned_installers_resolve_latest_and_ignore_package_managers() {
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            for (installer, latest) in [
                (&Uv as &dyn Installer, "0.10.1"),
                (&Gh, "2.97.1"),
                (&DuckDb, "1.5.5"),
                (&Quarto, "1.10.19"),
                (&Togi, "0.2.0"),
            ] {
                let runner = FakeRunner::default()
                    .on_path("brew")
                    .on_path("apt-get")
                    .on_path("winget");
                let fetcher = FakeFetcher::default().with_latest(latest);
                let _ = run_installer(installer, &ctx_on(os, &runner, &fetcher));
                assert_eq!(*fetcher.latest_calls.borrow(), vec![installer.name()]);
                assert!(
                    runner.calls.borrow().is_empty(),
                    "{os:?}/{}",
                    installer.name()
                );
                assert_eq!(
                    fetched_versions(&fetcher),
                    vec![latest],
                    "{os:?}/{}",
                    installer.name()
                );
            }
        }
    }

    #[test]
    fn exact_pins_bypass_latest_lookup_and_use_release_assets() {
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            for (installer, pin) in [
                (&Uv as &dyn Installer, "0.9.5"),
                (&Gh, "2.96.0"),
                (&DuckDb, "1.5.4"),
                (&Quarto, "1.9.36"),
                (&Togi, "0.1.1"),
            ] {
                let runner = FakeRunner::default()
                    .on_path("brew")
                    .on_path("apt-get")
                    .on_path("winget");
                let fetcher = FakeFetcher::default();
                let ctx = InstallCtx {
                    pin: Some(pin.to_string()),
                    ..ctx_on(os, &runner, &fetcher)
                };
                let _ = run_installer(installer, &ctx);
                assert!(
                    fetcher.latest_calls.borrow().is_empty(),
                    "{}",
                    installer.name()
                );
                assert!(
                    runner.calls.borrow().is_empty(),
                    "{os:?}/{}",
                    installer.name()
                );
                assert_eq!(
                    fetched_versions(&fetcher),
                    vec![pin],
                    "{os:?}/{}",
                    installer.name()
                );
            }
        }
    }

    #[test]
    fn lookup_failures_stop_every_dynamic_installer_before_download() {
        for installer in [&Uv as &dyn Installer, &Gh, &DuckDb, &Quarto, &Togi] {
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default().with_latest_error("release metadata unavailable");
            let err = run_installer(installer, &ctx_on(Os::Linux, &runner, &fetcher))
                .expect_err("latest lookup must fail");
            assert!(err.to_string().contains("metadata unavailable"), "{err:#}");
            assert!(fetched_versions(&fetcher).is_empty());
        }
    }

    #[test]
    fn installed_older_equal_and_newer_versions_share_update_semantics() {
        let tools: [(&dyn Installer, &str, &str); 5] = [
            (&Uv, "uv", "uv {version}"),
            (&Gh, "gh", "gh version {version}"),
            (&DuckDb, "duckdb", "v{version}"),
            (&Quarto, "quarto", "{version}"),
            (&Togi, "togi", "togi {version}"),
        ];
        for (installer, command, template) in tools {
            for (installed, expected_fetches) in [("1.9.0", 1), ("2.0.0", 0), ("2.1.0", 0)] {
                let output = template.replace("{version}", installed);
                let runner = FakeRunner::default()
                    .on_path(command)
                    .with_output(&format!("{command} --version"), &output);
                let fetcher = FakeFetcher::default().with_latest("2.0.0");
                let _ = run_installer(installer, &ctx_on(Os::Linux, &runner, &fetcher));
                assert_eq!(*fetcher.latest_calls.borrow(), vec![installer.name()]);
                assert_eq!(
                    fetched_versions(&fetcher).len(),
                    expected_fetches,
                    "{command} {installed}"
                );
            }
        }
    }

    #[test]
    fn installed_version_newer_than_upstream_is_not_downgraded() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", "togi 0.3.0");
        let fetcher = FakeFetcher::default().with_latest("0.2.0");
        run_installer(&Togi, &ctx_on(Os::Linux, &runner, &fetcher))
            .expect("a newer installed version must be retained");
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn an_explicit_pin_may_deliberately_downgrade() {
        let runner = FakeRunner::default()
            .on_path("togi")
            .with_output("togi --version", "togi 0.3.0");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("0.2.0".to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };
        let _ = run_installer(&Togi, &ctx);
        assert!(fetcher.latest_calls.borrow().is_empty());
        assert_eq!(fetcher.calls.borrow()[0].version, "0.2.0");
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    //! Real-download checks for the release-binary strategies, on the OS
    //! this test run happens on.
    //!
    //! Run with: `cargo test --features online-tests -- --ignored`
    //!
    //! Tests for fixed-version tools skip (with a note) when the tool is
    //! already installed on this machine. The latest-togi test always runs.
    //! Every download uses a throwaway directory: the release is downloaded
    //! into a temp cache, placed into a temp bin dir, and probed with
    //! `--version` right there.

    use crate::install::fetch::place;
    use crate::install::{CacheFetcher, CommandRunner, ReleaseFetcher, SystemRunner};
    use crate::tools::{Downloader, InstallContext, Os, Platform, ToolCache, ToolSpec, versions};

    /// Skip guard: `true` (after a note) when `tool` is already on PATH.
    fn already_installed(tool: &str) -> bool {
        let installed = SystemRunner.which(tool).is_some();
        if installed {
            eprintln!(
                "note: {tool} is already installed on this machine; skipping its online \
                 install test rather than reinstalling"
            );
        }
        installed
    }

    /// Download `spec` at `version` from the real release host into a
    /// temp cache, place it in a temp bin dir, and check `--version`.
    fn fetch_and_probe(spec: &ToolSpec, version: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(&dir.path().join("cache"));
        let platform = Platform::current().expect("supported platform");
        let ctx = InstallContext {
            label: spec.name,
            command: "hpds install",
            verbose: true,
        };
        let cached = Downloader::new(cache, platform)
            .ensure_installed(spec, version, &ctx)
            .expect("download the release binary");
        let installed = place(&cached, &dir.path().join("bin")).expect("place the binary");

        let out = std::process::Command::new(&installed)
            .arg("--version")
            .output()
            .expect("run --version on the installed binary");
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(version),
            "must report version {version}: {stdout}"
        );
    }

    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn uv_release_binary_downloads_and_runs_when_uv_is_absent() {
        if already_installed("uv") {
            return;
        }
        fetch_and_probe(&super::uv::release_spec(), versions::UV);
    }

    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn gh_release_binary_downloads_and_runs_when_gh_is_absent() {
        if already_installed("gh") {
            return;
        }
        let os = Platform::current().expect("supported platform").os;
        fetch_and_probe(&super::gh::release_spec(os), "2.96.0");
    }

    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn duckdb_release_binary_downloads_and_runs_when_duckdb_is_absent() {
        if already_installed("duckdb") {
            return;
        }
        let os = Platform::current().expect("supported platform").os;
        fetch_and_probe(&super::duckdb::release_spec(os), "1.5.4");
    }

    #[test]
    #[ignore = "looks up and downloads the latest real togi release from GitHub"]
    fn latest_togi_release_downloads_and_runs_in_a_temporary_directory() {
        let spec = super::togi::release_spec();
        let version = CacheFetcher::new(true)
            .latest_version(&spec)
            .expect("resolve latest togi release");
        fetch_and_probe(&spec, &version);
    }

    use crate::install::test_support::PanicFetcher;
    use crate::install::{InstallCtx, Installer};

    /// An `InstallCtx` against the real machine that can only observe:
    /// probes run through the system runner, and any fetch panics.
    fn probe_ctx(runner: &SystemRunner) -> InstallCtx<'_> {
        InstallCtx {
            os: Platform::current().expect("supported platform").os,
            yes: false,
            verbose: false,
            pin: None,
            plan_approved: false,
            sudo_approved: std::cell::Cell::new(false),
            runner,
            fetcher: &PanicFetcher,
        }
    }

    /// Assert that `installer`'s detection agrees with `probe` being on
    /// PATH: installing r/quarto/tinytex would mutate this machine's
    /// real toolchain, so their online tests only exercise detection
    /// (and skip, with a note, when the tool is absent).
    fn assert_detection_matches_path(installer: &dyn Installer, probe: &str) {
        let runner = SystemRunner;
        let ctx = probe_ctx(&runner);
        let on_path = runner.which(probe).is_some();
        match installer.detect(&ctx) {
            Some(version) => {
                assert!(
                    on_path,
                    "{} detected {version} but `{probe}` is not on PATH",
                    installer.name()
                );
                assert!(!version.is_empty(), "{}", installer.name());
                eprintln!(
                    "note: {} {version} is already installed; detection verified, \
                     skipping install",
                    installer.name()
                );
            }
            None => {
                assert!(
                    !on_path,
                    "`{probe}` is on PATH but {} detection missed it",
                    installer.name()
                );
                eprintln!(
                    "note: {} is absent; installing it would mutate this machine, \
                     so only the detection miss is verified",
                    installer.name()
                );
            }
        }
    }

    #[test]
    #[ignore = "probes the real R install on this machine"]
    fn r_detection_matches_the_real_machine() {
        assert_detection_matches_path(&super::r::R, "R");
    }

    #[test]
    #[ignore = "probes the real quarto install on this machine"]
    fn quarto_detection_matches_the_real_machine() {
        assert_detection_matches_path(&super::quarto::Quarto, "quarto");
    }

    #[test]
    #[ignore = "probes the real quarto/tlmgr installs on this machine"]
    fn tinytex_detection_reads_the_real_machine() {
        // tinytex has no single probe binary: detection goes through
        // `quarto list tools` and falls back to tlmgr. When either is
        // around and reports a TeX, detection must see it.
        let runner = SystemRunner;
        let ctx = probe_ctx(&runner);
        let detected = super::tinytex::TinyTex.detect(&ctx);
        if runner.which("tlmgr").is_some() {
            assert!(
                detected.is_some(),
                "tlmgr is on PATH but tinytex detection found nothing"
            );
            eprintln!(
                "note: tinytex ({}) is already installed; detection verified, \
                 skipping install",
                detected.expect("just checked")
            );
        } else {
            eprintln!("note: no tlmgr on this machine; tinytex detection returned {detected:?}");
        }
    }

    #[test]
    #[ignore = "may drive a real package manager install"]
    fn rig_online_test_skips_rather_than_mutating_this_machine() {
        if already_installed("rig") {
            return;
        }
        // rig has no release-binary strategy: every path goes through a
        // package manager (brew/apt/winget) and would mutate this
        // machine, so there is nothing safe to execute from a test.
        // Strategy selection and exact argv are covered offline in
        // `rig::tests`; run `hpds install rig` by hand to verify live.
        eprintln!(
            "note: rig is absent, but installing it would mutate this machine through a \
             package manager; verify manually with `hpds install rig`"
        );
        let os = Platform::current().expect("supported platform").os;
        // Sanity-check that this OS has a declared strategy at all.
        assert!(matches!(os, Os::Mac | Os::Linux | Os::Windows));
    }
}
