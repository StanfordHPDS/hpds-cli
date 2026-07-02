//! Putting release binaries on the user's PATH.
//!
//! Install strategies that say "download the release binary" reuse the
//! shared tool downloader (checksum verification, atomic cache installs)
//! and then copy the cached binary into the per-user bin directory. The
//! [`ReleaseFetcher`] seam keeps that network step fakeable, so strategy
//! selection is unit-testable offline.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::tools::{Downloader, InstallContext, Platform, ToolCache, ToolSpec};
use crate::ui::{self, HintExt};

/// How installers obtain a release binary. Production code uses
/// [`CacheFetcher`]; tests substitute a recording fake.
pub trait ReleaseFetcher {
    /// Download `spec` at `version` and place its binary into `bin_dir`,
    /// returning the installed path.
    fn fetch_binary(
        &self,
        spec: &ToolSpec,
        version: &str,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf>;
}

/// The real fetcher: downloads into the hpds tool cache (verified,
/// atomic), then copies the cached binary into `bin_dir`.
pub struct CacheFetcher {
    verbose: bool,
}

impl CacheFetcher {
    pub fn new(verbose: bool) -> CacheFetcher {
        CacheFetcher { verbose }
    }
}

impl ReleaseFetcher for CacheFetcher {
    fn fetch_binary(
        &self,
        spec: &ToolSpec,
        version: &str,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let cache = ToolCache::from_env()?;
        let platform = Platform::current()?;
        let ctx = InstallContext {
            label: spec.name,
            command: "hpds install",
            verbose: self.verbose,
        };
        let cached = Downloader::new(cache, platform).ensure_installed(spec, version, &ctx)?;
        place(&cached, bin_dir)
    }
}

/// Copy a cached tool binary into `bin_dir` (created as needed), returning
/// the destination path. `fs::copy` carries the executable bit along on
/// Unix.
pub(crate) fn place(binary: &Path, bin_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(bin_dir)
        .with_context(|| format!("could not create `{}`", bin_dir.display()))
        .hint("check that your home directory is writable")?;
    let name = binary
        .file_name()
        .context("the cached binary path has no file name")
        .hint("this is an hpds bug; please report it")?;
    let dest = bin_dir.join(name);
    std::fs::copy(binary, &dest)
        .with_context(|| {
            format!(
                "could not copy `{}` into `{}`",
                name.to_string_lossy(),
                bin_dir.display()
            )
        })
        .hint("check that the directory is writable, then re-run")?;
    Ok(dest)
}

/// The per-user bin directory release binaries are placed into:
/// `~/.local/bin`.
pub(crate) fn user_bin_dir() -> anyhow::Result<PathBuf> {
    let dirs = directories::BaseDirs::new()
        .context("could not determine your home directory")
        .hint("make sure HOME (or USERPROFILE on Windows) is set")?;
    Ok(dirs.home_dir().join(".local").join("bin"))
}

/// Warn when `bin_dir` is not on `PATH`, so a fresh install that "cannot
/// be found" afterwards is no mystery.
pub(crate) fn warn_if_off_path(bin_dir: &Path) {
    if !dir_on_path(bin_dir, std::env::var_os("PATH")) {
        ui::warn(&format!(
            "`{}` is not on your PATH; add it in your shell profile, then open a new shell",
            bin_dir.display()
        ));
    }
}

/// Whether `dir` appears in a `PATH`-style value. Factored out of env
/// access so it is unit-testable.
fn dir_on_path(dir: &Path, path: Option<OsString>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(&path).any(|entry| entry == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_copies_the_binary_into_a_created_bin_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("duckdb");
        std::fs::write(&binary, b"#!/bin/sh\necho fake\n").expect("write binary");

        let bin_dir = dir.path().join("home").join(".local").join("bin");
        let dest = place(&binary, &bin_dir).expect("place");

        assert_eq!(dest, bin_dir.join("duckdb"));
        assert_eq!(
            std::fs::read(&dest).expect("read placed binary"),
            b"#!/bin/sh\necho fake\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn place_preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("gh");
        std::fs::write(&binary, b"").expect("write binary");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x");

        let bin_dir = dir.path().join("bin");
        let dest = place(&binary, &bin_dir).expect("place");
        let mode = std::fs::metadata(&dest)
            .expect("metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "must stay executable, mode {mode:o}");
    }

    #[test]
    fn place_over_an_existing_binary_replaces_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("uv");
        std::fs::write(&binary, b"new version").expect("write binary");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        std::fs::write(bin_dir.join("uv"), b"old version").expect("write old binary");

        let dest = place(&binary, &bin_dir).expect("place");
        assert_eq!(std::fs::read(&dest).expect("read"), b"new version");
    }

    #[test]
    fn user_bin_dir_is_local_bin_under_home() {
        let dir = user_bin_dir().expect("home dir exists on dev machines");
        assert!(dir.ends_with(Path::new(".local").join("bin")), "{dir:?}");
    }

    #[test]
    fn dir_on_path_matches_exact_entries_only() {
        let bin = Path::new("/home/user/.local/bin");
        let on = std::env::join_paths([Path::new("/usr/bin"), bin]).expect("join");
        let off = std::env::join_paths([Path::new("/usr/bin")]).expect("join");
        assert!(dir_on_path(bin, Some(on)));
        assert!(!dir_on_path(bin, Some(off)));
        assert!(!dir_on_path(bin, None));
    }
}
