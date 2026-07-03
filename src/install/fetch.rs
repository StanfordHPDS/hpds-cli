//! Putting release binaries on the user's PATH.
//!
//! Install strategies that say "download the release binary" reuse the
//! shared tool downloader (checksum verification, atomic cache installs)
//! and then copy the cached binary into the per-user bin directory. The
//! [`ReleaseFetcher`] seam keeps that network step fakeable, so strategy
//! selection is unit-testable offline.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

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

    /// Download `spec`'s release archive at `version`, extract its whole
    /// tree under `opt_dir`, and place a launcher for the tree's
    /// `bin/<tool>` into `bin_dir`, returning the launcher path. For
    /// tools (like quarto) whose release is a directory tree rather than
    /// a single binary.
    fn fetch_tree(
        &self,
        spec: &ToolSpec,
        version: &str,
        opt_dir: &Path,
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

    fn fetch_tree(
        &self,
        spec: &ToolSpec,
        version: &str,
        opt_dir: &Path,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let cache = ToolCache::from_env()?;
        let platform = Platform::current()?;
        let ctx = InstallContext {
            label: spec.name,
            command: "hpds install",
            verbose: self.verbose,
        };
        let staging = tempfile::tempdir()
            .context("could not create a temporary download directory")
            .hint("check that your temp directory is writable")?;
        let archive =
            Downloader::new(cache, platform).fetch_archive(spec, version, &ctx, staging.path())?;
        let binary_name = platform.binary_name(spec.name);
        let root = install_tree(&archive, spec.name, version, &binary_name, opt_dir)?;
        let launcher = place_launcher(&root, spec.name, &binary_name, bin_dir)?;
        warn_if_off_path(bin_dir);
        Ok(launcher)
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

/// Extract the whole release archive under `opt_dir` as `<tool>-<version>`
/// and return that root — the directory holding `bin/<binary_name>`.
/// Handles both archive layouts the tools we manage publish: a single
/// top-level directory (tarballs) and `bin/` at the archive root (zips).
/// Replaces any existing install of the same version.
pub(crate) fn install_tree(
    archive: &Path,
    tool: &str,
    version: &str,
    binary_name: &str,
    opt_dir: &Path,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(opt_dir)
        .with_context(|| format!("could not create `{}`", opt_dir.display()))
        .hint("check that your home directory is writable")?;
    // Staging inside `opt_dir` keeps the final rename on one filesystem.
    let staging = tempfile::Builder::new()
        .prefix(".hpds-staging-")
        .tempdir_in(opt_dir)
        .with_context(|| {
            format!(
                "could not create a staging directory in `{}`",
                opt_dir.display()
            )
        })
        .hint("check that the directory is writable")?;
    let tree = staging.path().join("tree");
    fs::create_dir(&tree).context("could not create the extraction directory")?;
    extract_all(archive, &tree)?;
    let root = find_tree_root(&tree, binary_name)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            root.join("bin").join(binary_name),
            fs::Permissions::from_mode(0o755),
        )
        .with_context(|| format!("could not mark `{binary_name}` executable"))?;
    }

    let dest = opt_dir.join(format!("{tool}-{version}"));
    if dest.exists() {
        fs::remove_dir_all(&dest)
            .with_context(|| format!("could not remove the old install at `{}`", dest.display()))
            .hint("remove the directory by hand, then re-run")?;
    }
    fs::rename(&root, &dest)
        .with_context(|| format!("could not move the install into `{}`", dest.display()))
        .hint("check that the directory is writable, then re-run")?;
    Ok(dest)
}

/// Extract every entry of a `.tar.gz` or `.zip` archive into `dest`.
fn extract_all(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    let file = fs::File::open(archive)
        .with_context(|| format!("could not open `{}`", archive.display()))?;
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(dest)
            .context("could not extract the release archive")
            .hint("the download may be corrupt; re-run to download it again")?;
    } else if name.ends_with(".zip") {
        zip::ZipArchive::new(file)
            .context("could not read the release archive")
            .hint("the download may be corrupt; re-run to download it again")?
            .extract(dest)
            .context("could not extract the release archive")
            .hint("the download may be corrupt; re-run to download it again")?;
    } else {
        return Err(anyhow::anyhow!(
            "cannot extract `{name}`: unsupported archive type"
        ))
        .hint("this is an hpds bug (unexpected release asset pattern); please report it");
    }
    Ok(())
}

/// The directory inside a freshly extracted `tree` that holds
/// `bin/<binary_name>`: the extraction root itself, or a single top-level
/// directory (how tarball releases are laid out).
fn find_tree_root(tree: &Path, binary_name: &str) -> anyhow::Result<PathBuf> {
    if tree.join("bin").join(binary_name).is_file() {
        return Ok(tree.to_path_buf());
    }
    let entries = fs::read_dir(tree).context("could not read the extracted archive")?;
    for entry in entries.flatten() {
        let candidate = entry.path();
        if candidate.join("bin").join(binary_name).is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow::anyhow!(
        "the release archive holds no `bin/{binary_name}`"
    ))
    .hint(
        "the tool's release layout may have changed; pin a different version \
         with --version or report an hpds bug",
    )
}

/// Put a launcher for `root/bin/<binary_name>` into `bin_dir` under the
/// tool's plain name, replacing any previous launcher: a symlink on Unix.
#[cfg(unix)]
pub(crate) fn place_launcher(
    root: &Path,
    tool: &str,
    binary_name: &str,
    bin_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let target = root.join("bin").join(binary_name);
    let dest = bin_dir.join(tool);
    prepare_launcher_dest(bin_dir, &dest)?;
    std::os::unix::fs::symlink(&target, &dest)
        .with_context(|| format!("could not link `{}` into `{}`", tool, bin_dir.display()))
        .hint("check that the directory is writable, then re-run")?;
    Ok(dest)
}

/// Put a launcher for `root/bin/<binary_name>` into `bin_dir` under the
/// tool's plain name, replacing any previous launcher: a `.cmd` shim on
/// Windows (symlinks there need special privileges).
#[cfg(windows)]
pub(crate) fn place_launcher(
    root: &Path,
    tool: &str,
    binary_name: &str,
    bin_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let target = root.join("bin").join(binary_name);
    let dest = bin_dir.join(format!("{tool}.cmd"));
    prepare_launcher_dest(bin_dir, &dest)?;
    fs::write(
        &dest,
        format!("@echo off\r\n\"{}\" %*\r\n", target.display()),
    )
    .with_context(|| format!("could not write `{}`", dest.display()))
    .hint("check that the directory is writable, then re-run")?;
    Ok(dest)
}

/// Create `bin_dir` and clear any previous launcher at `dest`
/// (`symlink_metadata` so a dangling symlink still counts).
fn prepare_launcher_dest(bin_dir: &Path, dest: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(bin_dir)
        .with_context(|| format!("could not create `{}`", bin_dir.display()))
        .hint("check that your home directory is writable")?;
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_file(dest)
            .with_context(|| format!("could not replace `{}`", dest.display()))
            .hint("check that the directory is writable, then re-run")?;
    }
    Ok(())
}

/// The per-user directory whole-tree installs live under: `~/.local/opt`.
pub(crate) fn user_opt_dir() -> anyhow::Result<PathBuf> {
    let dirs = directories::BaseDirs::new()
        .context("could not determine your home directory")
        .hint("make sure HOME (or USERPROFILE on Windows) is set")?;
    Ok(dirs.home_dir().join(".local").join("opt"))
}

/// The per-user bin directory release binaries are placed into:
/// `~/.local/bin`.
pub(crate) fn user_bin_dir() -> anyhow::Result<PathBuf> {
    let dirs = directories::BaseDirs::new()
        .context("could not determine your home directory")
        .hint("make sure HOME (or USERPROFILE on Windows) is set")?;
    Ok(dirs.home_dir().join(".local").join("bin"))
}

/// Bin directories already warned about this process. Several installs
/// in one run (e.g. `hpds setup`) place tools into the same off-PATH
/// directory; the advice is identical every time, so it prints once.
static OFF_PATH_WARNED: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Warn when `bin_dir` is not on `PATH`, so a fresh install that "cannot
/// be found" afterwards is no mystery. Warns at most once per directory
/// per process.
pub(crate) fn warn_if_off_path(bin_dir: &Path) {
    if dir_on_path(bin_dir, std::env::var_os("PATH")) {
        return;
    }
    if first_report(&OFF_PATH_WARNED, bin_dir) {
        ui::warn(&format!(
            "`{}` is not on your PATH; add it in your shell profile, then open a new shell",
            bin_dir.display()
        ));
    }
}

/// Record `dir` in `seen`, returning `true` only the first time it shows
/// up. A poisoned lock is reclaimed rather than panicking: the set holds
/// nothing a panic could leave half-updated.
fn first_report(seen: &Mutex<HashSet<PathBuf>>, dir: &Path) -> bool {
    seen.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(dir.to_path_buf())
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

    // --- whole-tree installs (quarto-style release archives) --------------

    use crate::tools::test_support::{targz_of, zip_of};
    use crate::ui::render_error;

    /// Write `bytes` as `name` inside `dir` and return the path.
    fn archive_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write archive");
        path
    }

    #[test]
    fn install_tree_extracts_a_tar_gz_with_a_top_level_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(
            dir.path(),
            "quarto-9.9.9-linux-amd64.tar.gz",
            &targz_of(&[
                ("quarto-9.9.9/bin/quarto", b"#!/bin/sh\necho 9.9.9\n"),
                ("quarto-9.9.9/share/data.txt", b"payload"),
            ]),
        );
        let opt_dir = dir.path().join("opt");

        let root = install_tree(&archive, "quarto", "9.9.9", "quarto", &opt_dir).expect("install");

        assert_eq!(root, opt_dir.join("quarto-9.9.9"));
        assert!(root.join("bin").join("quarto").is_file());
        assert_eq!(
            std::fs::read(root.join("share").join("data.txt")).expect("read payload"),
            b"payload"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_tree_marks_the_binary_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(
            dir.path(),
            "quarto-9.9.9-macos.tar.gz",
            &targz_of(&[("quarto-9.9.9/bin/quarto", b"#!/bin/sh\n")]),
        );
        let root = install_tree(
            &archive,
            "quarto",
            "9.9.9",
            "quarto",
            &dir.path().join("opt"),
        )
        .expect("install");
        let mode = std::fs::metadata(root.join("bin").join("quarto"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "must be executable, mode {mode:o}");
    }

    #[test]
    fn install_tree_extracts_a_zip_with_a_flat_layout() {
        // The Windows release zip has bin/ and share/ at the archive root.
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(
            dir.path(),
            "quarto-9.9.9-win.zip",
            &zip_of(&[
                ("bin/quarto.exe", b"fake exe".as_slice()),
                ("share/data.txt", b"payload".as_slice()),
            ]),
        );
        let opt_dir = dir.path().join("opt");

        let root =
            install_tree(&archive, "quarto", "9.9.9", "quarto.exe", &opt_dir).expect("install");

        assert_eq!(root, opt_dir.join("quarto-9.9.9"));
        assert!(root.join("bin").join("quarto.exe").is_file());
        assert!(root.join("share").join("data.txt").is_file());
    }

    #[test]
    fn install_tree_replaces_an_existing_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opt_dir = dir.path().join("opt");
        let stale = opt_dir.join("quarto-9.9.9");
        std::fs::create_dir_all(stale.join("bin")).expect("create stale install");
        std::fs::write(stale.join("bin").join("quarto"), b"stale").expect("write stale binary");

        let archive = archive_file(
            dir.path(),
            "quarto-9.9.9-linux-amd64.tar.gz",
            &targz_of(&[("quarto-9.9.9/bin/quarto", b"fresh")]),
        );
        let root = install_tree(&archive, "quarto", "9.9.9", "quarto", &opt_dir).expect("install");
        assert_eq!(
            std::fs::read(root.join("bin").join("quarto")).expect("read binary"),
            b"fresh"
        );
    }

    #[test]
    fn install_tree_without_the_expected_binary_errors_with_guidance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(
            dir.path(),
            "quarto-9.9.9-linux-amd64.tar.gz",
            &targz_of(&[("quarto-9.9.9/share/data.txt", b"payload")]),
        );
        let err = install_tree(
            &archive,
            "quarto",
            "9.9.9",
            "quarto",
            &dir.path().join("opt"),
        )
        .expect_err("missing bin/quarto must fail");
        let out = render_error(&err, false);
        assert!(out.contains("bin"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn install_tree_rejects_an_unsupported_archive_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "quarto-9.9.9.pkg", b"not an archive");
        let err = install_tree(
            &archive,
            "quarto",
            "9.9.9",
            "quarto",
            &dir.path().join("opt"),
        )
        .expect_err("unknown archive type must fail");
        assert!(err.to_string().contains("archive"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn place_launcher_symlinks_the_tool_into_bin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("opt").join("quarto-9.9.9");
        std::fs::create_dir_all(root.join("bin")).expect("create tree");
        std::fs::write(root.join("bin").join("quarto"), b"real").expect("write binary");
        let bin_dir = dir.path().join("bin");

        let launcher = place_launcher(&root, "quarto", "quarto", &bin_dir).expect("place");

        assert_eq!(launcher, bin_dir.join("quarto"));
        assert_eq!(
            std::fs::read_link(&launcher).expect("read link"),
            root.join("bin").join("quarto")
        );
        assert_eq!(
            std::fs::read(&launcher).expect("read through link"),
            b"real"
        );
    }

    #[cfg(unix)]
    #[test]
    fn place_launcher_replaces_an_existing_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("opt").join("quarto-9.9.9");
        std::fs::create_dir_all(root.join("bin")).expect("create tree");
        std::fs::write(root.join("bin").join("quarto"), b"new").expect("write binary");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        std::fs::write(bin_dir.join("quarto"), b"old launcher").expect("write old launcher");

        let launcher = place_launcher(&root, "quarto", "quarto", &bin_dir).expect("place");
        assert_eq!(std::fs::read(&launcher).expect("read"), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn place_launcher_writes_a_cmd_shim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("opt").join("quarto-9.9.9");
        std::fs::create_dir_all(root.join("bin")).expect("create tree");
        std::fs::write(root.join("bin").join("quarto.exe"), b"exe").expect("write binary");
        let bin_dir = dir.path().join("bin");

        let launcher = place_launcher(&root, "quarto", "quarto.exe", &bin_dir).expect("place");

        assert_eq!(launcher, bin_dir.join("quarto.cmd"));
        let shim = std::fs::read_to_string(&launcher).expect("read shim");
        assert!(shim.contains("quarto.exe"), "{shim}");
        assert!(shim.contains("%*"), "{shim}");
    }

    #[cfg(windows)]
    #[test]
    fn a_placed_cmd_launcher_is_visible_to_the_path_probe() {
        // Regression: after the no-winget fallback install, detect probes
        // PATH for `quarto`; the `.cmd` shim must be found or the install
        // verification fails and reruns are never idempotent.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("opt").join("quarto-9.9.9");
        std::fs::create_dir_all(root.join("bin")).expect("create tree");
        std::fs::write(root.join("bin").join("quarto.exe"), b"exe").expect("write binary");
        let bin_dir = dir.path().join("bin");

        let launcher = place_launcher(&root, "quarto", "quarto.exe", &bin_dir).expect("place");

        let path = std::env::join_paths([&bin_dir]).expect("join PATH");
        assert_eq!(
            super::super::runner::which_in(&path, "quarto"),
            Some(launcher)
        );
    }

    #[test]
    fn user_opt_dir_is_local_opt_under_home() {
        let dir = user_opt_dir().expect("home dir exists on dev machines");
        assert!(dir.ends_with(Path::new(".local").join("opt")), "{dir:?}");
    }

    #[test]
    fn off_path_advice_is_reported_once_per_directory() {
        let seen = Mutex::new(HashSet::new());
        let bin = Path::new("/home/user/.local/bin");
        assert!(first_report(&seen, bin), "the first sighting warns");
        assert!(!first_report(&seen, bin), "repeat advice is suppressed");
        assert!(
            first_report(&seen, Path::new("/home/user/other-bin")),
            "a different directory gets its own warning"
        );
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
