//! `hpds upgrade`: self-update from GitHub releases of StanfordHPDS/hpds-cli.
//!
//! Reuses the tool-download machinery (release-asset resolution, checksum
//! verification, atomic staging) to fetch the release archive for the
//! current platform, then swaps the running executable in place with
//! `self_replace`. The replace is atomic: a failed download or verify
//! leaves the installed binary untouched.
//!
//! Before touching anything, hpds infers how it was installed from the path
//! of the running executable. Homebrew and `cargo install` manage their own
//! copies, so hpds refuses to overwrite them and instead points at the right
//! upgrade command.

use std::path::Path;

use anyhow::Context;
use serde_json::Value;

use crate::tools::{
    Downloader, InstallContext, Platform, ToolCache, ToolSpec, extract_binary, github_agent,
};
use crate::ui::{self, HintExt};

/// The GitHub repository hpds releases from.
const REPO: &str = "StanfordHPDS/hpds-cli";

/// Base URL of the GitHub REST API.
const GITHUB_API_BASE: &str = "https://api.github.com";

/// The hpds binary's base name (no extension).
const BINARY_NAME: &str = "hpds";

/// User-Agent for GitHub API requests (GitHub rejects requests without one).
const USER_AGENT: &str = concat!("hpds/", env!("CARGO_PKG_VERSION"));

// cargo-dist artifact naming: `hpds-<target>.<ext>` archives, each with a
// `.sha256` sidecar. The target is the arch + vendor-os pair, which is
// exactly the asset pattern already used for managed tool downloads.
const ASSET_PATTERN: &str = "hpds-{arch}-{os}.{ext}";
const CHECKSUM_PATTERN: &str = "hpds-{arch}-{os}.{ext}.sha256";

/// Detect the install method, check the latest release, and self-update when
/// there is a newer one. Homebrew/cargo installs are left untouched with
/// advice on how to upgrade them.
pub fn run(global: &super::GlobalArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .context("could not determine the path to the running hpds executable")
        .hint("upgrade hpds manually from https://github.com/StanfordHPDS/hpds-cli/releases")?;
    let source = GithubReleaseSource;
    let replacer = ReleaseReplacer {
        verbose: global.verbose,
    };
    let outcome = plan(&exe, env!("CARGO_PKG_VERSION"), &source, &replacer)?;
    render(&outcome);
    Ok(())
}

/// How hpds was installed, inferred from the running binary's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    /// Installed under a Homebrew prefix; upgrade via `brew`.
    Homebrew,
    /// Installed by `cargo install` into a Cargo bin directory.
    Cargo,
    /// A standalone binary hpds may replace in place.
    Standalone,
}

/// What an upgrade run decided to do. Returned by [`plan`] so the decision
/// is testable without running the download or touching the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Managed by Homebrew; advise `brew upgrade hpds`.
    ManagedByHomebrew,
    /// Managed by cargo; advise `cargo install hpds`.
    ManagedByCargo,
    /// The repository has published no releases yet.
    NoRelease,
    /// Already on the newest release (carries the current version).
    UpToDate(String),
    /// Replaced the running binary; carries the old and new versions.
    Upgraded { from: String, to: String },
}

/// Looks up the latest published release tag.
trait ReleaseSource {
    /// The latest release tag (e.g. `v0.2.0`), or `None` when the project
    /// has published no full releases yet.
    fn latest_tag(&self) -> anyhow::Result<Option<String>>;
}

/// Downloads a release and atomically replaces the running executable.
/// A trait so tests never touch the real binary; production wires in
/// [`ReleaseReplacer`]; tests substitute a recording fake.
trait SelfReplacer {
    /// Fetch the release for the bare `version` and replace the current exe.
    fn replace_with(&self, version: &str) -> anyhow::Result<()>;
}

/// Decide (and, when warranted, carry out) an upgrade. Pure orchestration
/// over the two seams so every branch is unit-testable: install-method
/// detection short-circuits before any network call, and the replace step
/// runs only for a strictly newer release.
fn plan(
    exe: &Path,
    current: &str,
    source: &dyn ReleaseSource,
    replacer: &dyn SelfReplacer,
) -> anyhow::Result<Outcome> {
    match detect_install_method(exe) {
        InstallMethod::Homebrew => return Ok(Outcome::ManagedByHomebrew),
        InstallMethod::Cargo => return Ok(Outcome::ManagedByCargo),
        InstallMethod::Standalone => {}
    }

    let Some(tag) = source.latest_tag()? else {
        return Ok(Outcome::NoRelease);
    };
    let latest = parse_version(&tag)
        .ok_or_else(|| anyhow::anyhow!("could not read the latest release tag `{tag}`"))
        .hint(
            "the release tag is not valid semver; upgrade hpds manually from \
             https://github.com/StanfordHPDS/hpds-cli/releases",
        )?;
    let installed = parse_version(current).expect("hpds's own version is valid semver");

    if latest <= installed {
        return Ok(Outcome::UpToDate(current.to_string()));
    }

    let bare = normalize_tag(&tag);
    replacer.replace_with(&bare)?;
    Ok(Outcome::Upgraded {
        from: current.to_string(),
        to: bare,
    })
}

/// Print the result of an upgrade run. All output goes through `ui`.
fn render(outcome: &Outcome) {
    match outcome {
        Outcome::ManagedByHomebrew => {
            ui::println("hpds was installed with Homebrew; upgrade it with Homebrew:");
            ui::println("  brew upgrade hpds");
        }
        Outcome::ManagedByCargo => {
            ui::println("hpds was installed with cargo; upgrade it with cargo:");
            ui::println("  cargo install hpds");
        }
        Outcome::NoRelease => {
            ui::println("hpds has published no releases yet; there is nothing to upgrade to.");
        }
        Outcome::UpToDate(version) => {
            ui::success(&format!("hpds {version} is up to date"));
        }
        Outcome::Upgraded { from, to } => {
            ui::success(&format!("upgraded hpds {from} → {to}"));
        }
    }
}

/// Infer the install method from the running binary's path. Homebrew keeps
/// its binaries under a `Cellar` (and links them under a `homebrew` prefix);
/// `cargo install` puts them in `<cargo-home>/bin` (usually `~/.cargo/bin`).
fn detect_install_method(exe: &Path) -> InstallMethod {
    let has_dir = |name: &str| {
        exe.components()
            .any(|component| component.as_os_str() == name)
    };
    if has_dir("Cellar") || has_dir("homebrew") {
        return InstallMethod::Homebrew;
    }
    if is_under_cargo_bin(exe) {
        return InstallMethod::Cargo;
    }
    InstallMethod::Standalone
}

/// Whether `exe` sits in a Cargo `bin` directory: a `.cargo` path component
/// immediately followed by `bin` (e.g. `~/.cargo/bin/hpds`).
fn is_under_cargo_bin(exe: &Path) -> bool {
    let names: Vec<_> = exe
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    names
        .windows(2)
        .any(|pair| pair[0] == ".cargo" && pair[1] == "bin")
}

/// Parse a `X.Y.Z` version, tolerating a leading `v` and ignoring any
/// pre-release/build suffix (`0.2.0-rc.1` → `0.2.0`). `None` when the core
/// is not three dotted integers.
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let core = normalize_tag(tag);
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Strip a leading `v` and surrounding whitespace from a release tag.
fn normalize_tag(tag: &str) -> String {
    let trimmed = tag.trim();
    trimmed.strip_prefix('v').unwrap_or(trimmed).to_string()
}

/// The [`ToolSpec`] describing hpds's own release artifacts, so the shared
/// downloader resolves and verifies them exactly like a managed tool.
fn self_spec() -> ToolSpec {
    ToolSpec {
        name: BINARY_NAME,
        default_version: env!("CARGO_PKG_VERSION"),
        repo: REPO,
        asset_pattern: ASSET_PATTERN,
        checksum_pattern: Some(CHECKSUM_PATTERN),
    }
}

/// The production release lookup: `gh` when it is authenticated, else the
/// public GitHub REST API over `ureq` (honoring the standard proxy env).
struct GithubReleaseSource;

impl ReleaseSource for GithubReleaseSource {
    fn latest_tag(&self) -> anyhow::Result<Option<String>> {
        if let Some(tag) = gh_latest_tag() {
            return Ok(Some(tag));
        }
        api_latest_tag(&github_agent(), GITHUB_API_BASE, REPO)
    }
}

/// The latest release tag via an authenticated `gh`, or `None` when gh is
/// missing, unauthenticated, offline, or reports no release, every one of
/// which falls back to the REST API, which answers authoritatively.
fn gh_latest_tag() -> Option<String> {
    if !matches!(
        crate::gitx::gh_auth(),
        Ok(crate::gitx::GhAuth::Authenticated)
    ) {
        return None;
    }
    let output = std::process::Command::new(crate::gitx::gh_program())
        .args([
            "api",
            &format!("repos/{REPO}/releases/latest"),
            "--jq",
            ".tag_name",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!tag.is_empty()).then_some(tag)
}

/// The latest release tag via the GitHub REST API. A 404 means the project
/// has no releases yet; that is `Ok(None)`, not an error.
fn api_latest_tag(agent: &ureq::Agent, base: &str, repo: &str) -> anyhow::Result<Option<String>> {
    let url = format!("{base}/repos/{repo}/releases/latest");
    let response = agent
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let mut response = match response {
        Ok(response) => response,
        // No full release published yet: nothing to upgrade to.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(ureq::Error::StatusCode(code)) => {
            return Err(anyhow::anyhow!("GitHub returned HTTP {code} for `{url}`")).hint(
                "GitHub may be rate-limiting or down; retry in a few minutes, or upgrade \
                 hpds manually from https://github.com/StanfordHPDS/hpds-cli/releases",
            );
        }
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("could not reach GitHub at `{url}`"))
                .hint(
                    "`hpds upgrade` needs network access to check for a newer release; \
                     check your connection (or HTTPS_PROXY) and retry",
                );
        }
    };
    let body = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("could not read GitHub's response from `{url}`"))
        .hint("retry `hpds upgrade`, or download the latest release manually")?;
    parse_tag_name(&body).map(Some)
}

/// Pull `tag_name` out of a GitHub `releases/latest` JSON response.
fn parse_tag_name(body: &str) -> anyhow::Result<String> {
    let value: Value = serde_json::from_str(body)
        .context("GitHub's release response was not valid JSON")
        .hint("retry `hpds upgrade`, or download the latest release manually")?;
    value
        .get("tag_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("GitHub's release response had no `tag_name`"))
        .hint("retry `hpds upgrade`, or download the latest release manually")
}

/// The production replacer: download the release archive into a scratch dir
/// (verified against its published checksum), extract the hpds binary, and
/// atomically swap it for the running executable.
struct ReleaseReplacer {
    verbose: bool,
}

impl SelfReplacer for ReleaseReplacer {
    fn replace_with(&self, version: &str) -> anyhow::Result<()> {
        let platform = Platform::current()?;
        let cache = ToolCache::from_env()?;
        let downloader = Downloader::new(cache, platform);
        let spec = self_spec();

        let staging = tempfile::Builder::new()
            .prefix(".hpds-upgrade-")
            .tempdir()
            .context("could not create a staging directory for the upgrade")
            .hint("check that your temp directory is writable")?;

        let ctx = InstallContext {
            label: "hpds",
            command: "hpds upgrade",
            verbose: self.verbose,
        };
        let archive = downloader.fetch_archive(&spec, version, &ctx, staging.path())?;

        let asset = spec.asset_name(platform, version);
        let binary_name = platform.binary_name(BINARY_NAME);
        let new_exe = staging.path().join(&binary_name);
        extract_binary(&archive, &asset, &binary_name, &new_exe)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new_exe, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("could not mark `{binary_name}` executable"))?;
        }

        // self_replace does the atomic swap (and the Windows rename dance),
        // so a failure here leaves the running binary in place.
        self_replace::self_replace(&new_exe)
            .context("could not replace the running hpds executable")
            .hint(
                "check that you can write to hpds's install location, or upgrade it \
                 manually from https://github.com/StanfordHPDS/hpds-cli/releases",
            )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::cmp::Ordering;
    use std::path::PathBuf;

    /// A [`ReleaseSource`] that returns a fixed tag and counts its calls.
    struct FakeSource {
        tag: Option<String>,
        calls: Cell<usize>,
    }

    impl FakeSource {
        fn new(tag: Option<&str>) -> FakeSource {
            FakeSource {
                tag: tag.map(str::to_string),
                calls: Cell::new(0),
            }
        }
    }

    impl ReleaseSource for FakeSource {
        fn latest_tag(&self) -> anyhow::Result<Option<String>> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.tag.clone())
        }
    }

    /// A [`SelfReplacer`] that records the versions it was asked to install
    /// and never touches the filesystem: the dev binary is never replaced.
    #[derive(Default)]
    struct FakeReplacer {
        replaced: std::cell::RefCell<Vec<String>>,
    }

    impl SelfReplacer for FakeReplacer {
        fn replace_with(&self, version: &str) -> anyhow::Result<()> {
            self.replaced.borrow_mut().push(version.to_string());
            Ok(())
        }
    }

    fn plan_with(exe: &str, current: &str, tag: Option<&str>) -> (Outcome, usize, Vec<String>) {
        let source = FakeSource::new(tag);
        let replacer = FakeReplacer::default();
        let outcome = plan(Path::new(exe), current, &source, &replacer).expect("plan");
        (outcome, source.calls.get(), replacer.replaced.into_inner())
    }

    // --- install-method detection (injected paths) --------------------------

    #[test]
    fn detects_a_homebrew_cellar_install() {
        let exe = PathBuf::from("/usr/local/Cellar/hpds/0.1.0/bin/hpds");
        assert_eq!(detect_install_method(&exe), InstallMethod::Homebrew);
    }

    #[test]
    fn detects_an_apple_silicon_homebrew_install() {
        let exe = PathBuf::from("/opt/homebrew/bin/hpds");
        assert_eq!(detect_install_method(&exe), InstallMethod::Homebrew);
    }

    #[test]
    fn detects_a_cargo_install() {
        let exe = PathBuf::from("/home/malcolm/.cargo/bin/hpds");
        assert_eq!(detect_install_method(&exe), InstallMethod::Cargo);
    }

    #[test]
    fn a_plain_bin_path_is_standalone() {
        for path in [
            "/usr/local/bin/hpds",
            "/home/malcolm/.local/bin/hpds",
            "/opt/hpds/hpds",
        ] {
            assert_eq!(
                detect_install_method(Path::new(path)),
                InstallMethod::Standalone,
                "{path}"
            );
        }
    }

    #[test]
    fn a_cargo_named_dir_that_is_not_a_bin_is_standalone() {
        // `.cargo` must be immediately followed by `bin` to count.
        let exe = PathBuf::from("/home/malcolm/.cargo/registry/hpds");
        assert_eq!(detect_install_method(&exe), InstallMethod::Standalone);
    }

    // --- version parsing / comparison --------------------------------------

    #[test]
    fn parses_bare_and_v_prefixed_versions() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.4.9"), Some((1, 4, 9)));
        assert_eq!(parse_version("  v10.0.3 "), Some((10, 0, 3)));
    }

    #[test]
    fn ignores_prerelease_and_build_suffixes() {
        assert_eq!(parse_version("v0.2.0-rc.1"), Some((0, 2, 0)));
        assert_eq!(parse_version("0.2.0+build.7"), Some((0, 2, 0)));
    }

    #[test]
    fn rejects_non_semver_tags() {
        for bad in ["latest", "1.2", "1.2.3.4", "1.x.0", ""] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn version_tuples_order_by_component() {
        assert!(parse_version("0.2.0") > parse_version("0.1.9"));
        assert!(parse_version("1.0.0") > parse_version("0.99.99"));
        assert_eq!(parse_version("0.1.0"), parse_version("v0.1.0"));
    }

    #[test]
    fn normalize_tag_strips_the_v_prefix() {
        assert_eq!(normalize_tag("v0.2.0"), "0.2.0");
        assert_eq!(normalize_tag("0.2.0"), "0.2.0");
        assert_eq!(normalize_tag(" v1.0.0 "), "1.0.0");
    }

    // --- orchestration (fakes for both seams) ------------------------------

    #[test]
    fn homebrew_install_advises_brew_and_touches_nothing() {
        let (outcome, source_calls, replaced) =
            plan_with("/opt/homebrew/bin/hpds", "0.1.0", Some("v9.9.9"));
        assert_eq!(outcome, Outcome::ManagedByHomebrew);
        assert_eq!(
            source_calls, 0,
            "must not query releases for a brew install"
        );
        assert!(replaced.is_empty(), "must not self-replace a brew install");
    }

    #[test]
    fn cargo_install_advises_cargo_and_touches_nothing() {
        let (outcome, source_calls, replaced) =
            plan_with("/home/m/.cargo/bin/hpds", "0.1.0", Some("v9.9.9"));
        assert_eq!(outcome, Outcome::ManagedByCargo);
        assert_eq!(source_calls, 0);
        assert!(replaced.is_empty());
    }

    #[test]
    fn no_release_yet_reports_nothing_to_upgrade_to() {
        let (outcome, _, replaced) = plan_with("/usr/local/bin/hpds", "0.1.0", None);
        assert_eq!(outcome, Outcome::NoRelease);
        assert!(replaced.is_empty(), "no release means no replace");
    }

    #[test]
    fn already_newest_is_up_to_date_and_does_not_replace() {
        let (outcome, _, replaced) = plan_with("/usr/local/bin/hpds", "0.1.0", Some("v0.1.0"));
        assert_eq!(outcome, Outcome::UpToDate("0.1.0".to_string()));
        assert!(replaced.is_empty(), "an equal version must not replace");
    }

    #[test]
    fn a_dev_build_ahead_of_the_latest_release_is_up_to_date() {
        let (outcome, _, replaced) = plan_with("/usr/local/bin/hpds", "0.5.0", Some("v0.4.0"));
        assert_eq!(outcome, Outcome::UpToDate("0.5.0".to_string()));
        assert!(replaced.is_empty());
    }

    #[test]
    fn a_newer_release_triggers_a_replace_with_the_bare_version() {
        let (outcome, _, replaced) = plan_with("/usr/local/bin/hpds", "0.1.0", Some("v0.2.0"));
        assert_eq!(
            outcome,
            Outcome::Upgraded {
                from: "0.1.0".to_string(),
                to: "0.2.0".to_string(),
            }
        );
        assert_eq!(
            replaced,
            vec!["0.2.0".to_string()],
            "the replacer gets the bare version, not the v-prefixed tag"
        );
    }

    #[test]
    fn a_malformed_release_tag_fails_with_guidance() {
        let source = FakeSource::new(Some("nightly"));
        let replacer = FakeReplacer::default();
        let err = plan(
            Path::new("/usr/local/bin/hpds"),
            "0.1.0",
            &source,
            &replacer,
        )
        .expect_err("a non-semver tag must fail");
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("nightly"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
        assert!(
            replacer.replaced.into_inner().is_empty(),
            "a malformed tag must not trigger a replace"
        );
    }

    // --- release JSON parsing (recorded fixture) ---------------------------

    #[test]
    fn parses_the_tag_from_a_recorded_release_response() {
        let body = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tool-output/gh/release-latest.json"
        ))
        .expect("read recorded release fixture");
        assert_eq!(parse_tag_name(&body).expect("parse tag"), "v0.2.0");
    }

    #[test]
    fn a_release_response_without_a_tag_fails_clearly() {
        let err = parse_tag_name("{\"name\": \"hpds 0.2.0\"}")
            .expect_err("a response with no tag_name must fail");
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("tag_name"), "{rendered}");
        assert!(rendered.contains("hint:"), "{rendered}");
    }

    #[test]
    fn invalid_json_fails_clearly() {
        let err = parse_tag_name("not json at all").expect_err("invalid JSON must fail");
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("JSON"), "{rendered}");
    }

    #[test]
    fn the_self_spec_resolves_cargo_dist_asset_names() {
        // Sanity: the reused downloader will look for cargo-dist's
        // `hpds-<target>.<ext>` archive plus its `.sha256` sidecar.
        use crate::tools::{Arch, Os};
        let mac = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        assert_eq!(
            self_spec().asset_name(mac, "0.2.0"),
            "hpds-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            self_spec().checksum_asset_name(mac, "0.2.0").as_deref(),
            Some("hpds-aarch64-apple-darwin.tar.gz.sha256")
        );
    }

    #[test]
    fn tuple_ordering_matches_semver_precedence() {
        // `plan` relies on tuple ordering for its up-to-date check.
        assert_eq!((0u64, 2u64, 0u64).cmp(&(0, 1, 9)), Ordering::Greater);
    }
}

#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;

    /// The real repo has cut no release yet, so `releases/latest` 404s.
    /// `hpds upgrade` must read that as "no release yet" (`Ok(None)`), not
    /// an error. Run with:
    /// `cargo test --features online-tests -- --ignored`
    #[test]
    #[ignore = "queries the real GitHub releases API"]
    fn latest_release_is_none_while_the_repo_has_no_releases() {
        let agent = github_agent();
        let tag = api_latest_tag(&agent, GITHUB_API_BASE, REPO)
            .expect("a 404 for a repo with no releases must be Ok(None), not an error");
        assert_eq!(tag, None, "expected no release yet, got {tag:?}");
    }
}
