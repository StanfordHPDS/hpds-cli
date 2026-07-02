//! Concrete tool installers behind `hpds install <tool>`.
//!
//! Each installer picks a strategy from the injected OS, probes package
//! managers through the runner seam, and downloads release binaries
//! through the fetcher seam — so every strategy is assertable offline.

pub mod duckdb;
pub mod gh;
pub mod quarto;
pub mod r;
pub mod rig;
pub mod tinytex;
pub mod uv;

use std::path::PathBuf;

use crate::tools::ToolSpec;
use crate::ui;

use super::InstallCtx;
use super::fetch::{user_bin_dir, warn_if_off_path};

/// Whether `program` is on `PATH`, probed through the runner seam.
fn on_path(ctx: &InstallCtx, program: &str) -> bool {
    ctx.runner.which(program).is_some()
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

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    //! Real-download checks for the release-binary strategies, on the OS
    //! this test run happens on.
    //!
    //! Run with: `cargo test --features online-tests -- --ignored`
    //!
    //! Every test skips (with a note) when the tool is already installed
    //! on this machine — these must never reinstall or otherwise touch
    //! real tools — and otherwise touches only a throwaway directory:
    //! the release is downloaded into a temp cache, placed into a temp
    //! bin dir, and probed with `--version` right there.

    use crate::install::fetch::place;
    use crate::install::{CommandRunner, SystemRunner};
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
        let spec = ToolSpec::builtin("uv").expect("uv is built in");
        fetch_and_probe(&spec, versions::UV);
    }

    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn gh_release_binary_downloads_and_runs_when_gh_is_absent() {
        if already_installed("gh") {
            return;
        }
        let os = Platform::current().expect("supported platform").os;
        fetch_and_probe(&super::gh::release_spec(os), versions::GH);
    }

    #[test]
    #[ignore = "downloads a real release from GitHub"]
    fn duckdb_release_binary_downloads_and_runs_when_duckdb_is_absent() {
        if already_installed("duckdb") {
            return;
        }
        let os = Platform::current().expect("supported platform").os;
        fetch_and_probe(&super::duckdb::release_spec(os), versions::DUCKDB);
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
            runner,
            fetcher: &PanicFetcher,
        }
    }

    /// Assert that `installer`'s detection agrees with `probe` being on
    /// PATH — installing r/quarto/tinytex would mutate this machine's
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
