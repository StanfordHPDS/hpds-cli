//! Putting release binaries on the user's PATH.
//!
//! Install strategies that say "download the release binary" reuse the
//! shared tool downloader (checksum verification, atomic cache installs)
//! and then copy the cached binary into the per-user bin directory. The
//! [`ReleaseFetcher`] seam keeps that network step fakeable, so strategy
//! selection is unit-testable offline.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::Context;
use serde_json::Value;

use crate::tools::{Downloader, InstallContext, Platform, ReleaseSource, ToolCache, ToolSpec};
use crate::ui::{self, HintExt};

/// How installers obtain a release binary. Production code uses
/// [`CacheFetcher`]; tests substitute a recording fake.
pub trait ReleaseFetcher {
    /// Resolve the latest stable release version published for `spec`.
    fn latest_version(&self, spec: &ToolSpec) -> anyhow::Result<String>;

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
    injected: Option<(ToolCache, Platform, String)>,
    resolved: Mutex<HashMap<String, ResolvedRelease>>,
}

#[derive(Clone)]
struct ResolvedRelease {
    version: String,
    tag: String,
    archive_url: String,
    checksum_url: Option<String>,
    digest: Option<String>,
}

impl CacheFetcher {
    pub fn new(verbose: bool) -> CacheFetcher {
        CacheFetcher {
            verbose,
            injected: None,
            resolved: Mutex::new(HashMap::new()),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn fetch_latest_binary_at(
        verbose: bool,
        cache: ToolCache,
        platform: Platform,
        api_base: &str,
        spec: &ToolSpec,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let fetcher = CacheFetcher {
            verbose,
            injected: Some((cache, platform, api_base.to_string())),
            resolved: Mutex::new(HashMap::new()),
        };
        let version = fetcher.latest_version(spec)?;
        fetcher.fetch_binary(spec, &version, bin_dir)
    }
}

impl ReleaseFetcher for CacheFetcher {
    fn latest_version(&self, spec: &ToolSpec) -> anyhow::Result<String> {
        let platform = self
            .injected
            .as_ref()
            .map(|value| value.1)
            .unwrap_or(Platform::current()?);
        let base = self
            .injected
            .as_ref()
            .map(|value| value.2.as_str())
            .unwrap_or("https://api.github.com");
        let resolved = latest_github_release(&crate::tools::github_agent(), base, spec, platform)?;
        let version = resolved.version.clone();
        self.resolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(spec.name.to_string(), resolved);
        Ok(version)
    }

    fn fetch_binary(
        &self,
        spec: &ToolSpec,
        version: &str,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let (cache, platform) = match &self.injected {
            Some((cache, platform, _)) => (cache.clone(), *platform),
            None => (ToolCache::from_env()?, Platform::current()?),
        };
        let ctx = InstallContext {
            label: spec.name,
            command: "hpds install",
            verbose: self.verbose,
        };
        let downloader = Downloader::new(cache, platform);
        let mut resolved = self
            .resolved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(spec.name)
            .cloned();
        let canonical_url = canonical_archive_url(spec, platform, version);
        if resolved.is_none()
            && let Some(cached) = downloader.verified_cached(spec, version, &canonical_url)
        {
            return place(&cached, bin_dir);
        }
        if resolved.is_none() {
            let api_base = self
                .injected
                .as_ref()
                .map(|value| value.2.as_str())
                .unwrap_or("https://api.github.com");
            resolved = Some(exact_github_release(
                &crate::tools::github_agent(),
                api_base,
                spec,
                platform,
                version,
            )?);
        }
        let cached = match resolved.as_ref() {
            Some(release) => {
                downloader.ensure_release_installed(spec, version, &release.source(), &ctx)?
            }
            None => downloader.ensure_installed(spec, version, &ctx)?,
        };
        place(&cached, bin_dir)
    }

    fn fetch_tree(
        &self,
        spec: &ToolSpec,
        version: &str,
        opt_dir: &Path,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let (cache, platform) = match &self.injected {
            Some((cache, platform, _)) => (cache.clone(), *platform),
            None => (ToolCache::from_env()?, Platform::current()?),
        };
        let ctx = InstallContext {
            label: spec.name,
            command: "hpds install",
            verbose: self.verbose,
        };
        let staging = tempfile::tempdir()
            .context("could not create a temporary download directory")
            .hint("check that your temp directory is writable")?;
        let mut release = self
            .resolved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(spec.name)
            .cloned();
        if release.is_none() {
            let api_base = self
                .injected
                .as_ref()
                .map(|value| value.2.as_str())
                .unwrap_or("https://api.github.com");
            release = Some(exact_github_release(
                &crate::tools::github_agent(),
                api_base,
                spec,
                platform,
                version,
            )?);
        }
        let downloader = Downloader::new(cache, platform);
        let archive = match release.as_ref() {
            Some(release) => downloader.fetch_release_archive(
                spec,
                version,
                &release.source(),
                &ctx,
                staging.path(),
            )?,
            None => downloader.fetch_archive(spec, version, &ctx, staging.path())?,
        };
        let binary_name = platform.binary_name(spec.name);
        let root = install_tree(&archive, spec.name, version, &binary_name, opt_dir)?;
        let launcher = place_launcher(&root, spec.name, &binary_name, bin_dir)?;
        warn_if_off_path(bin_dir);
        Ok(launcher)
    }
}

impl ResolvedRelease {
    fn source(&self) -> ReleaseSource<'_> {
        ReleaseSource {
            tag: &self.tag,
            archive_url: &self.archive_url,
            checksum_url: self.checksum_url.as_deref(),
            digest: self.digest.as_deref(),
        }
    }
}

#[cfg(test)]
fn latest_github_version(
    agent: &ureq::Agent,
    base: &str,
    spec: &ToolSpec,
) -> anyhow::Result<String> {
    Ok(latest_github_release(agent, base, spec, Platform::current()?)?.version)
}

fn latest_github_release(
    agent: &ureq::Agent,
    base: &str,
    spec: &ToolSpec,
    platform: Platform,
) -> anyhow::Result<ResolvedRelease> {
    let url = format!("{base}/repos/{}/releases/latest", spec.repo);
    github_release_at(agent, &url, spec, platform, "latest")
}

fn exact_github_release(
    agent: &ureq::Agent,
    base: &str,
    spec: &ToolSpec,
    platform: Platform,
    version: &str,
) -> anyhow::Result<ResolvedRelease> {
    let tag = canonical_tag(spec, version);
    let url = format!("{base}/repos/{}/releases/tags/{tag}", spec.repo);
    let release = github_release_at(agent, &url, spec, platform, version)?;
    if release.version != version {
        return Err(anyhow::anyhow!(
            "GitHub returned {} metadata while installing {} {version}",
            release.version,
            spec.name
        ))
        .hint("retry the exact install after GitHub release metadata is corrected");
    }
    Ok(release)
}

fn canonical_tag(spec: &ToolSpec, version: &str) -> String {
    if spec.name == "uv" {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

fn canonical_archive_url(spec: &ToolSpec, platform: Platform, version: &str) -> String {
    let tag = canonical_tag(spec, version);
    let asset = spec.asset_name(platform, version);
    format!(
        "https://github.com/{}/releases/download/{tag}/{asset}",
        spec.repo
    )
}

fn github_release_at(
    agent: &ureq::Agent,
    url: &str,
    spec: &ToolSpec,
    platform: Platform,
    requested: &str,
) -> anyhow::Result<ResolvedRelease> {
    let response = agent
        .get(url)
        .header("User-Agent", concat!("hpds/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(code)) => {
            return Err(anyhow::anyhow!(
                "GitHub returned HTTP {code} while checking the {requested} {} release",
                spec.name,
            ))
            .hint(format!(
                "retry `hpds install {}` after GitHub is available, or use --version to request an exact release",
                spec.name
            ));
        }
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("could not reach GitHub at `{url}`"))
                .hint(format!(
                    "check your connection (or HTTPS_PROXY), then retry `hpds install {}` or use --version",
                    spec.name
                ));
        }
    };
    let body = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("could not read GitHub's response from `{url}`"))
        .hint(format!("retry `hpds install {}`", spec.name))?;
    let allow_local = url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:");
    parse_latest_release(&body, spec, platform, allow_local)
}

#[cfg(test)]
fn parse_latest_version(body: &str, name: &str) -> anyhow::Result<String> {
    let value: Value = serde_json::from_str(body)
        .context("GitHub's release response was not valid JSON")
        .hint(format!("retry `hpds install {name}`"))?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("GitHub's release response had no `tag_name`"))
        .hint(format!("retry `hpds install {name}`"))?;
    let version = strict_version(tag, name)?;
    if value.get("draft").and_then(Value::as_bool) != Some(false) {
        return Err(anyhow::anyhow!("GitHub's latest {name} release is a draft"))
            .hint(format!("retry `hpds install {name}` or use --version"));
    }
    if value.get("prerelease").and_then(Value::as_bool) != Some(false) {
        return Err(anyhow::anyhow!(
            "GitHub's latest {name} release is a prerelease"
        ))
        .hint(format!("retry `hpds install {name}` or use --version"));
    }
    if value
        .get("published_at")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(anyhow::anyhow!(
            "GitHub's latest {name} release is not published"
        ))
        .hint(format!("retry `hpds install {name}` or use --version"));
    }
    Ok(version)
}

fn strict_version(tag: &str, name: &str) -> anyhow::Result<String> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let valid = version.split('.').count() == 3
        && version.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        });
    if valid {
        Ok(version.to_string())
    } else {
        Err(anyhow::anyhow!(
            "GitHub's latest {name} release has invalid stable tag `{tag}`"
        ))
        .hint(format!(
            "retry `hpds install {name}` or use --version with an exact stable version"
        ))
    }
}

fn parse_latest_release(
    body: &str,
    spec: &ToolSpec,
    platform: Platform,
    allow_local_urls: bool,
) -> anyhow::Result<ResolvedRelease> {
    let value: Value = serde_json::from_str(body)
        .context("GitHub's release response was not valid JSON")
        .hint(format!(
            "retry `hpds install {}` or use --version",
            spec.name
        ))?;
    if value.get("draft").and_then(Value::as_bool) != Some(false) {
        return Err(anyhow::anyhow!(
            "GitHub's latest {} release is a draft",
            spec.name
        ))
        .hint("use --version with a published stable release");
    }
    if value.get("prerelease").and_then(Value::as_bool) != Some(false) {
        return Err(anyhow::anyhow!(
            "GitHub's latest {} release is a prerelease",
            spec.name
        ))
        .hint("use --version with a published stable release");
    }
    if value
        .get("published_at")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(anyhow::anyhow!(
            "GitHub's latest {} release is not published",
            spec.name
        ))
        .hint("retry later or use --version");
    }
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("GitHub's release response had no `tag_name`"))?;
    let version = strict_version(tag, spec.name)?;
    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("GitHub's latest {} release has no assets", spec.name))?;
    let archive_name = spec.asset_name(platform, &version);
    let matching: Vec<_> = assets
        .iter()
        .filter(|a| a.get("name").and_then(Value::as_str) == Some(&archive_name))
        .collect();
    if matching.len() != 1 {
        return Err(anyhow::anyhow!(
            "GitHub's latest {} release must contain exactly one archive asset `{archive_name}`",
            spec.name
        ))
        .hint(format!(
            "use --version to request another {} release",
            spec.name
        ));
    }
    let checksum_name = spec.checksum_asset_name(platform, &version);
    let checksum_asset = checksum_name
        .as_ref()
        .map(|checksum| {
            let matching: Vec<_> = assets
                .iter()
                .filter(|asset| asset.get("name").and_then(Value::as_str) == Some(checksum))
                .collect();
            if matching.len() != 1 {
                return Err(anyhow::anyhow!(
                    "GitHub's latest {} release must contain exactly one checksum asset `{checksum}`",
                    spec.name
                ))
                .hint("use --version to request another release");
            }
            Ok(matching[0])
        })
        .transpose()?;
    let archive = matching[0];
    let archive_url = archive
        .get("browser_download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("GitHub's archive asset has no download URL"))?;
    validate_asset_url(archive_url, spec.repo, tag, &archive_name, allow_local_urls)?;
    let checksum_url = checksum_asset
        .map(|asset| {
            let name = checksum_name.as_deref().expect("checksum asset has a name");
            let url = asset
                .get("browser_download_url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("GitHub's checksum asset has no download URL"))?;
            validate_asset_url(url, spec.repo, tag, name, allow_local_urls)?;
            Ok::<_, anyhow::Error>(url.to_string())
        })
        .transpose()?;
    let digest = archive
        .get("digest")
        .and_then(Value::as_str)
        .map(str::to_string);
    if spec.name == "duckdb"
        && digest.as_deref().is_none_or(|d| {
            d.strip_prefix("sha256:")
                .is_none_or(|hex| hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()))
        })
    {
        return Err(anyhow::anyhow!(
            "GitHub's DuckDB archive asset has a missing or malformed sha256 digest"
        ))
        .hint("retry later or use --version");
    }

    Ok(ResolvedRelease {
        version,
        tag: tag.to_string(),
        archive_url: archive_url.to_string(),
        checksum_url,
        digest,
    })
}

fn validate_asset_url(
    url: &str,
    repo: &str,
    tag: &str,
    asset: &str,
    allow_local: bool,
) -> anyhow::Result<()> {
    let expected_path = format!("/{repo}/releases/download/{tag}/{asset}");
    let valid_github = url == format!("https://github.com{expected_path}");
    let valid_local = (url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://localhost:"))
        && url.ends_with(&expected_path);
    #[cfg(test)]
    let valid_fixture = url.starts_with("https://example.test/");
    #[cfg(not(test))]
    let valid_fixture = false;
    if valid_github || (allow_local && valid_local) || valid_fixture {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "GitHub release asset `{asset}` has unexpected download URL `{url}`"
        ))
        .hint("retry after the upstream release metadata is corrected")
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
/// and return that root, the directory holding `bin/<binary_name>`.
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

    #[test]
    fn parses_latest_release_fixture_as_a_bare_version() {
        let body = include_str!("../../tests/fixtures/tool-output/gh/togi-release-latest.json");
        assert_eq!(
            parse_latest_version(body, "togi").expect("parse latest release"),
            "0.1.1"
        );
    }

    #[test]
    fn latest_release_lookup_uses_the_github_endpoint_and_fixture_response() {
        use std::collections::HashMap;

        use crate::tools::test_support::FixtureServer;

        let path = "/repos/StanfordHPDS/togi/releases/latest";
        let server = FixtureServer::serve(HashMap::from([(
            path.to_string(),
            include_bytes!("../../tests/fixtures/tool-output/gh/togi-release-latest.json").to_vec(),
        )]));

        let version = latest_github_version(
            &crate::tools::github_agent(),
            &server.base_url,
            &crate::install::installers::togi::release_spec(),
        )
        .expect("resolve fixture release");

        assert_eq!(version, "0.1.1");
        assert_eq!(server.hits(), vec![path]);
    }

    #[test]
    fn exact_uv_lookup_uses_its_single_bare_canonical_tag() {
        use crate::tools::test_support::FixtureServer;

        let spec = ToolSpec {
            name: "uv",
            default_version: "latest",
            repo: "astral-sh/uv",
            asset_pattern: "uv-{arch}-{os}.{ext}",
            checksum_pattern: Some("uv-{arch}-{os}.{ext}.sha256"),
        };
        let path = "/repos/astral-sh/uv/releases/tags/0.10.1";
        let server = FixtureServer::serve(HashMap::from([(
            path.to_string(),
            include_bytes!("../../tests/fixtures/releases/uv-latest.json").to_vec(),
        )]));

        let release = exact_github_release(
            &crate::tools::github_agent(),
            &server.base_url,
            &spec,
            Platform {
                os: crate::tools::Os::Linux,
                arch: crate::tools::Arch::X86_64,
            },
            "0.10.1",
        )
        .expect("resolve the exact uv release");

        assert_eq!(release.version, "0.10.1");
        assert_eq!(server.hits(), vec![path]);
    }

    #[test]
    fn latest_release_http_failure_is_actionable() {
        use std::collections::HashMap;

        use crate::tools::test_support::FixtureServer;

        let path = "/repos/StanfordHPDS/togi/releases/latest";
        let server = FixtureServer::serve_responses(HashMap::from([(
            path.to_string(),
            (503, b"unavailable".to_vec()),
        )]));

        let err = latest_github_version(
            &crate::tools::github_agent(),
            &server.base_url,
            &crate::install::installers::togi::release_spec(),
        )
        .expect_err("an unavailable release endpoint must fail");
        let rendered = crate::ui::render_error(&err, false);

        assert!(rendered.contains("HTTP 503"), "{rendered}");
        assert!(rendered.contains("--version"), "{rendered}");
        assert_eq!(server.hits(), vec![path]);
    }

    #[test]
    fn malformed_latest_release_metadata_is_actionable() {
        for body in ["not json", r#"{"name":"togi"}"#] {
            let err = parse_latest_version(body, "togi").expect_err("metadata must fail");
            let rendered = crate::ui::render_error(&err, false);
            assert!(rendered.contains("hint:"), "{rendered}");
            assert!(rendered.contains("hpds install togi"), "{rendered}");
        }
    }

    #[test]
    fn recorded_latest_release_fixtures_use_strict_stable_tags() {
        for (name, body, expected) in [
            (
                "uv",
                include_str!("../../tests/fixtures/releases/uv-latest.json"),
                "0.10.1",
            ),
            (
                "gh",
                include_str!("../../tests/fixtures/releases/gh-latest.json"),
                "2.97.1",
            ),
            (
                "duckdb",
                include_str!("../../tests/fixtures/releases/duckdb-latest.json"),
                "1.5.5",
            ),
            (
                "quarto",
                include_str!("../../tests/fixtures/releases/quarto-latest.json"),
                "1.10.19",
            ),
            (
                "togi",
                include_str!("../../tests/fixtures/releases/togi-latest.json"),
                "0.2.0",
            ),
        ] {
            assert_eq!(parse_latest_version(body, name).expect(name), expected);
        }
    }

    #[test]
    fn latest_release_rejects_non_strict_tags_and_unpublished_metadata() {
        for body in [
            r#"{"tag_name":"v1.2.3+build.1","draft":false,"prerelease":false,"assets":[]}"#,
            r#"{"tag_name":"latest","draft":false,"prerelease":false,"assets":[]}"#,
            r#"{"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[]}"#,
            r#"{"tag_name":"v01.2.3","draft":false,"prerelease":false,"published_at":"2026-01-01T00:00:00Z","assets":[]}"#,
        ] {
            let err = parse_latest_version(body, "uv").expect_err("unstable metadata must fail");
            let rendered = crate::ui::render_error(&err, false);
            assert!(rendered.contains("hint:"), "{rendered}");
            assert!(rendered.contains("--version"), "{rendered}");
        }
    }

    fn resolve_fixture(body: String, spec: &ToolSpec) -> anyhow::Result<String> {
        use std::collections::HashMap;

        use crate::tools::test_support::FixtureServer;

        let path = format!("/repos/{}/releases/latest", spec.repo);
        let server = FixtureServer::serve(HashMap::from([(path, body.into_bytes())]));
        latest_github_version(&crate::tools::github_agent(), &server.base_url, spec)
    }

    fn published_body(tag: &str, assets: Vec<Value>) -> String {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "published_at": "2026-09-30T12:00:00Z",
            "assets": assets,
        })
        .to_string()
    }

    fn asset(name: &str, digest: Option<&str>) -> Value {
        serde_json::json!({
            "name": name,
            "browser_download_url": format!("https://example.test/{name}"),
            "digest": digest,
        })
    }

    fn complete_togi_body(draft: bool, prerelease: bool) -> String {
        let spec = crate::install::installers::togi::release_spec();
        let platform = Platform::current().expect("supported platform");
        let archive = spec.asset_name(platform, "1.2.3");
        let checksum = spec
            .checksum_asset_name(platform, "1.2.3")
            .expect("checksum");
        let mut value: Value = serde_json::from_str(&published_body(
            "v1.2.3",
            vec![asset(&archive, None), asset(&checksum, None)],
        ))
        .expect("valid metadata");
        value["draft"] = Value::Bool(draft);
        value["prerelease"] = Value::Bool(prerelease);
        value.to_string()
    }

    #[test]
    fn latest_release_rejects_an_otherwise_valid_draft() {
        let err = parse_latest_version(&complete_togi_body(true, false), "togi")
            .expect_err("draft must fail");
        assert!(crate::ui::render_error(&err, false).contains("draft"));
    }

    #[test]
    fn latest_release_rejects_an_otherwise_valid_prerelease() {
        let err = parse_latest_version(&complete_togi_body(false, true), "togi")
            .expect_err("prerelease must fail");
        assert!(crate::ui::render_error(&err, false).contains("prerelease"));
    }

    #[test]
    fn latest_lookup_rejects_a_missing_platform_archive() {
        let spec = crate::install::installers::togi::release_spec();
        let platform = Platform::current().expect("supported platform");
        let checksum = spec
            .checksum_asset_name(platform, "9.9.9")
            .expect("checksum");
        let err = resolve_fixture(
            published_body("v9.9.9", vec![asset(&checksum, None)]),
            &spec,
        )
        .expect_err("the exact archive is mandatory");
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("archive"), "{rendered}");
        assert!(rendered.contains("--version"), "{rendered}");
    }

    #[test]
    fn latest_lookup_rejects_a_missing_declared_checksum_asset() {
        let spec = crate::install::installers::togi::release_spec();
        let platform = Platform::current().expect("supported platform");
        let archive = spec.asset_name(platform, "9.9.9");
        let err = resolve_fixture(published_body("v9.9.9", vec![asset(&archive, None)]), &spec)
            .expect_err("the declared checksum asset is mandatory");
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("checksum"), "{rendered}");
        assert!(rendered.contains("--version"), "{rendered}");
    }

    #[test]
    fn duckdb_latest_requires_a_well_formed_github_sha256_digest() {
        let spec = ToolSpec {
            name: "duckdb",
            default_version: "1.5.4",
            repo: "duckdb/duckdb",
            asset_pattern: match Platform::current().expect("supported platform").os {
                crate::tools::Os::Mac => "duckdb_cli-osx-universal.zip",
                crate::tools::Os::Linux => "duckdb_cli-linux-{alt-arch}.zip",
                crate::tools::Os::Windows => "duckdb_cli-windows-{alt-arch}.zip",
            },
            checksum_pattern: None,
        };
        let archive = spec.asset_name(Platform::current().expect("supported platform"), "1.5.5");
        for digest in [None, Some("sha256:not-hex")] {
            let err = resolve_fixture(
                published_body("v1.5.5", vec![asset(&archive, digest)]),
                &spec,
            )
            .expect_err("DuckDB requires GitHub's archive digest");
            let rendered = crate::ui::render_error(&err, false);
            assert!(rendered.contains("digest"), "{rendered}");
            assert!(rendered.contains("--version"), "{rendered}");
        }
    }

    #[test]
    fn duckdb_fixture_records_githubs_archive_digest() {
        let value: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/releases/duckdb-latest.json"
        ))
        .expect("fixture JSON");
        let digest = value["assets"][0]["digest"].as_str().expect("asset digest");
        assert!(digest.starts_with("sha256:"), "{digest}");
        assert_eq!(digest.len(), "sha256:".len() + 64);
    }

    fn fetch_duckdb_with_digest(
        digest_for: impl FnOnce(&[u8]) -> String,
    ) -> (anyhow::Result<PathBuf>, tempfile::TempDir, ToolCache) {
        use std::collections::HashMap;

        use crate::tools::test_support::{FixtureServer, zip_with};
        use crate::tools::{Arch, Os};

        let spec = ToolSpec {
            name: "duckdb",
            default_version: "1.5.4",
            repo: "duckdb/duckdb",
            asset_pattern: "duckdb_cli-linux-{alt-arch}.zip",
            checksum_pattern: None,
        };
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let archive_name = spec.asset_name(platform, "1.5.5");
        let archive = zip_with("duckdb", b"fake duckdb");
        let digest = digest_for(&archive);
        let archive_path = format!("/duckdb/duckdb/releases/download/v1.5.5/{archive_name}");
        let archive_server = FixtureServer::serve(HashMap::from([(archive_path.clone(), archive)]));
        let metadata = serde_json::json!({
            "tag_name": "v1.5.5",
            "draft": false,
            "prerelease": false,
            "published_at": "2026-09-30T12:00:00Z",
            "assets": [{
                "name": archive_name,
                "browser_download_url": format!("{}{archive_path}", archive_server.base_url),
                "digest": digest,
            }],
        })
        .to_string();
        let metadata_path = "/repos/duckdb/duckdb/releases/latest";
        let metadata_server = FixtureServer::serve(HashMap::from([(
            metadata_path.to_string(),
            metadata.into_bytes(),
        )]));
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let result = CacheFetcher::fetch_latest_binary_at(
            false,
            cache.clone(),
            platform,
            &metadata_server.base_url,
            &spec,
            &dir.path().join("bin"),
        );
        (result, dir, cache)
    }

    #[test]
    fn incorrect_duckdb_github_digest_prevents_cache_publication() {
        let wrong_digest = format!("sha256:{}", "0".repeat(64));
        let (result, _dir, cache) = fetch_duckdb_with_digest(|_| wrong_digest);
        let err = result.expect_err("an incorrect GitHub digest must fail");
        let rendered = crate::ui::render_error(&err, false);

        assert!(rendered.contains("digest"), "{rendered}");
        assert!(rendered.contains("does not match"), "{rendered}");
        assert!(!cache.tool_dir("duckdb", "1.5.5").exists());
        assert!(!cache.manifest_path("duckdb", "1.5.5").exists());
    }

    #[test]
    fn matching_duckdb_github_digest_publishes_binary_and_manifest() {
        use crate::tools::test_support::sha256_hex_of;

        let (result, _dir, cache) =
            fetch_duckdb_with_digest(|archive| format!("sha256:{}", sha256_hex_of(archive)));
        let binary = result.expect("matching GitHub digest must install");

        assert!(binary.is_file(), "{}", binary.display());
        let manifest_path = cache.manifest_path("duckdb", "1.5.5");
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path).expect("published manifest"),
        )
        .expect("manifest JSON");
        assert!(
            manifest["checksum"]
                .as_str()
                .is_some_and(|value| value.len() == 64)
        );
    }

    #[test]
    fn exact_duckdb_reuses_a_verified_cache_without_metadata_access() {
        use crate::tools::Manifest;
        use crate::tools::test_support::FixtureServer;

        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let platform = Platform {
            os: crate::tools::Os::Linux,
            arch: crate::tools::Arch::X86_64,
        };
        let spec = ToolSpec {
            name: "duckdb",
            default_version: "latest",
            repo: "duckdb/duckdb",
            asset_pattern: "duckdb_cli-linux-{alt-arch}.zip",
            checksum_pattern: None,
        };
        let binary = cache.binary_path("duckdb", "1.5.5", platform);
        fs::create_dir_all(binary.parent().expect("parent")).expect("cache dir");
        fs::write(&binary, b"verified duckdb").expect("binary");
        Manifest::new(
            "1.5.5".to_string(),
            canonical_archive_url(&spec, platform, "1.5.5"),
            Some("a".repeat(64)),
        )
        .save(&cache.manifest_path("duckdb", "1.5.5"))
        .expect("manifest");
        let server = FixtureServer::serve(HashMap::new());
        let fetcher = CacheFetcher {
            verbose: false,
            injected: Some((cache, platform, server.base_url.clone())),
            resolved: Mutex::new(HashMap::new()),
        };

        fetcher
            .fetch_binary(&spec, "1.5.5", &dir.path().join("bin"))
            .expect("verified exact cache works offline");

        assert!(server.hits().is_empty(), "{:?}", server.hits());
    }

    #[test]
    fn exact_cache_from_a_competing_bare_tag_does_not_bypass_metadata() {
        use crate::tools::Manifest;
        use crate::tools::test_support::FixtureServer;

        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ToolCache::at(dir.path());
        let platform = Platform {
            os: crate::tools::Os::Linux,
            arch: crate::tools::Arch::X86_64,
        };
        let spec = ToolSpec {
            name: "duckdb",
            default_version: "latest",
            repo: "duckdb/duckdb",
            asset_pattern: "duckdb_cli-linux-{alt-arch}.zip",
            checksum_pattern: None,
        };
        let binary = cache.binary_path("duckdb", "1.5.5", platform);
        fs::create_dir_all(binary.parent().expect("parent")).expect("cache dir");
        fs::write(&binary, b"competing duckdb").expect("binary");
        let asset = spec.asset_name(platform, "1.5.5");
        Manifest::new(
            "1.5.5".to_string(),
            format!("https://github.com/duckdb/duckdb/releases/download/1.5.5/{asset}"),
            Some("a".repeat(64)),
        )
        .save(&cache.manifest_path("duckdb", "1.5.5"))
        .expect("manifest");
        let server = FixtureServer::serve(HashMap::new());
        let fetcher = CacheFetcher {
            verbose: false,
            injected: Some((cache, platform, server.base_url.clone())),
            resolved: Mutex::new(HashMap::new()),
        };

        fetcher
            .fetch_binary(&spec, "1.5.5", &dir.path().join("bin"))
            .expect_err("a competing bare-tag cache must require canonical metadata");

        assert_eq!(
            server.hits(),
            vec!["/repos/duckdb/duckdb/releases/tags/v1.5.5"]
        );
    }

    #[test]
    fn production_metadata_cannot_redirect_an_asset_to_loopback() {
        let err = validate_asset_url(
            "http://127.0.0.1:1234/duckdb/duckdb/releases/download/v1.5.5/duckdb.zip",
            "duckdb/duckdb",
            "v1.5.5",
            "duckdb.zip",
            false,
        )
        .expect_err("GitHub metadata cannot redirect downloads to loopback");
        assert!(err.to_string().contains("unexpected download URL"), "{err}");
    }
}
