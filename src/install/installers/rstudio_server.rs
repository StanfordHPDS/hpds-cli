use std::io::copy;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};

pub use crate::install::DownloadedPackage;
use crate::install::{
    InstallCtx, Installer, apt_get_command_line, apt_package_path, ensure_step_success, is_current,
};
use crate::tools::Os;
use crate::ui::{self, HintExt};

pub const STABLE_ENDPOINT: &str =
    "https://rstudio.org/download/latest/stable/server/jammy/rstudio-server-latest-amd64.deb";
const RELEASE_PREFIX: &str =
    "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-";
const RELEASE_SUFFIX: &str = "-amd64.deb";

/// The directory systemd creates at boot; it exists only on hosts that
/// systemd runs (the check behind `sd_booted`).
const SYSTEMD_RUNTIME: &str = "/run/systemd/system";

/// The plan line for the service check that follows every install,
/// including one that finds the current version already installed.
pub(crate) const SERVICE_PLAN: &str = "if systemd runs this host and rstudio-server is not active: sudo systemctl reset-failed rstudio-server; sudo systemctl restart rstudio-server";

const SERVICE_HINT: &str = "RStudio Server needs R: install it with `hpds install r` (or rerun the R step of server setup), then run `sudo systemctl restart rstudio-server`; `journalctl -u rstudio-server` shows why it stopped";

const RESET_FAILED: &[&str] = &["systemctl", "reset-failed", "rstudio-server"];
const RESTART: &[&str] = &["systemctl", "restart", "rstudio-server"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RstudioRelease {
    pub version: String,
    pub filename: String,
    pub url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DebianMetadata {
    pub package: String,
    pub architecture: String,
    pub version: String,
}

pub trait RstudioBackend {
    fn preflight(&self, ctx: &InstallCtx<'_>) -> Result<()> {
        require_linux(ctx)
    }
    fn detect(&self, ctx: &InstallCtx<'_>) -> Option<String>;
    fn latest(&self) -> Result<RstudioRelease>;
    fn exact(&self, version: &str) -> Result<RstudioRelease>;
    fn download(&self, release: &RstudioRelease) -> Result<DownloadedPackage>;
    fn inspect(&self, ctx: &InstallCtx<'_>, package: &Path) -> Result<String>;
    fn install(&self, ctx: &InstallCtx<'_>, package: &Path) -> Result<()>;
    /// Whether systemd runs this host, so `rstudio-server` is a service
    /// it manages. Hosts without systemd (such as most containers) have
    /// no service to start.
    fn systemd_running(&self) -> bool;
}

pub struct RstudioServer<B = HttpBackend> {
    backend: B,
    resolved: Mutex<Option<RstudioRelease>>,
}

impl RstudioServer<HttpBackend> {
    pub const fn production() -> Self {
        Self {
            backend: HttpBackend,
            resolved: Mutex::new(None),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_backend<B: RstudioBackend>(backend: &B) -> RstudioServer<&B> {
        RstudioServer {
            backend,
            resolved: Mutex::new(None),
        }
    }
}

impl<B: RstudioBackend + ?Sized> RstudioBackend for &B {
    fn preflight(&self, ctx: &InstallCtx<'_>) -> Result<()> {
        (**self).preflight(ctx)
    }
    fn detect(&self, ctx: &InstallCtx<'_>) -> Option<String> {
        (**self).detect(ctx)
    }
    fn latest(&self) -> Result<RstudioRelease> {
        (**self).latest()
    }
    fn exact(&self, version: &str) -> Result<RstudioRelease> {
        (**self).exact(version)
    }
    fn download(&self, release: &RstudioRelease) -> Result<DownloadedPackage> {
        (**self).download(release)
    }
    fn inspect(&self, ctx: &InstallCtx<'_>, package: &Path) -> Result<String> {
        (**self).inspect(ctx, package)
    }
    fn install(&self, ctx: &InstallCtx<'_>, package: &Path) -> Result<()> {
        (**self).install(ctx, package)
    }
    fn systemd_running(&self) -> bool {
        (**self).systemd_running()
    }
}

pub fn parse_stable_redirect(url: &str) -> Result<RstudioRelease> {
    let encoded = url
        .strip_prefix(RELEASE_PREFIX)
        .and_then(|value| value.strip_suffix(RELEASE_SUFFIX))
        .ok_or_else(|| anyhow!("RStudio Server returned an untrusted release URL `{url}`"))?;
    let mut parts = encoded.split('-');
    let base = parts.next().unwrap_or_default();
    let build = parts.next().unwrap_or_default();
    if parts.next().is_some() || !strict_base(base) || !strict_component(build) {
        return Err(anyhow!(
            "RStudio Server returned a malformed release URL `{url}`"
        ))
        .hint("retry later or install a trusted exact release with --version");
    }
    let filename = format!("rstudio-server-{encoded}-amd64.deb");
    Ok(RstudioRelease {
        version: format!("{base}+{build}"),
        filename,
        url: url.to_string(),
    })
}

fn strict_base(version: &str) -> bool {
    let mut parts = version.split('.');
    let valid = (0..3).all(|_| parts.next().is_some_and(strict_component));
    valid && parts.next().is_none()
}

fn strict_component(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn exact_release(version: &str) -> Result<RstudioRelease> {
    let Some((base, build)) = version.split_once('+') else {
        return Err(anyhow!("invalid RStudio Server version `{version}`"))
            .hint("use an exact stable version such as `2026.09.0+174`");
    };
    if !strict_base(base) || !strict_component(build) || build.contains('+') {
        return Err(anyhow!("invalid RStudio Server version `{version}`"))
            .hint("use an exact stable version such as `2026.09.0+174`");
    }
    parse_stable_redirect(&format!("{RELEASE_PREFIX}{base}-{build}{RELEASE_SUFFIX}"))
}

/// Read the `Package`, `Architecture`, and `Version` fields that
/// `dpkg-deb --field <deb> Package Architecture Version` prints. Each must
/// appear exactly once and be nonempty.
pub(crate) fn parse_debian_identity(output: &str) -> Result<DebianMetadata> {
    fn one(output: &str, field: &str) -> Result<String> {
        let prefix = format!("{field}:");
        let values: Vec<_> = output
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::trim))
            .collect();
        match values.as_slice() {
            [value] if !value.is_empty() => Ok((*value).to_string()),
            _ => Err(anyhow!(
                "the downloaded package has invalid `{field}` metadata"
            )),
        }
    }
    Ok(DebianMetadata {
        package: one(output, "Package")?,
        architecture: one(output, "Architecture")?,
        version: one(output, "Version")?,
    })
}

pub fn parse_debian_metadata(output: &str, selected: &str) -> Result<DebianMetadata> {
    let metadata = parse_debian_identity(output)?;
    if metadata.package != "rstudio-server"
        || metadata.architecture != "amd64"
        || metadata.version != selected
    {
        return Err(anyhow!(
            "the downloaded RStudio Server package identity does not match {selected}"
        ))
        .hint(
            "do not install the package; retry after Posit's stable release metadata is corrected",
        );
    }
    Ok(metadata)
}

pub struct HttpBackend;

fn discover_stable(agent: &ureq::Agent, endpoint: &str) -> Result<RstudioRelease> {
    let response = agent
        .head(endpoint)
        .call()
        .context("could not discover the latest stable RStudio Server release")
        .hint("check the network connection and retry")?;
    if !response.status().is_redirection() {
        return Err(anyhow!(
            "RStudio Server stable discovery did not return a redirect"
        ))
        .hint("retry later or install a trusted exact release with --version");
    }
    let location = response
        .headers()
        .get_all("location")
        .iter()
        .map(|value| value.to_str())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    match location.as_slice() {
        [url] => parse_stable_redirect(url),
        _ => Err(anyhow!(
            "RStudio Server stable discovery returned an invalid redirect"
        ))
        .hint("retry later or install a trusted exact release with --version"),
    }
}

fn download_package(agent: &ureq::Agent, release: &RstudioRelease) -> Result<DownloadedPackage> {
    download_package_from(agent, release, &release.url, false)
}

fn download_package_from(
    agent: &ureq::Agent,
    release: &RstudioRelease,
    request_url: &str,
    allow_injected_origin: bool,
) -> Result<DownloadedPackage> {
    if !allow_injected_origin && request_url != release.url {
        return Err(anyhow!(
            "the package request does not match the selected RStudio Server release"
        ))
        .hint("retry the install so hpds can resolve the release again");
    }
    let mut response = agent
        .get(request_url)
        .call()
        .with_context(|| format!("could not download RStudio Server {}", release.version))
        .hint("check the network connection and retry")?;
    if !response.status().is_success() || response.headers().contains_key("location") {
        return Err(anyhow!(
            "the selected RStudio Server package URL returned an unexpected redirect or status {}",
            response.status()
        ))
        .hint("retry later; hpds will only install the selected trusted S3 package");
    }
    let mut file = tempfile::Builder::new()
        .prefix("hpds-rstudio-server-")
        .suffix(".deb")
        .tempfile()
        .context("could not create a secure temporary package file")
        .hint("check that the temporary directory is writable")?;
    copy(&mut response.body_mut().as_reader(), file.as_file_mut())
        .context("could not write the RStudio Server package")?;
    Ok(DownloadedPackage::temporary(file))
}

impl RstudioBackend for HttpBackend {
    fn preflight(&self, ctx: &InstallCtx<'_>) -> Result<()> {
        require_linux_amd64(ctx)
    }
    fn detect(&self, ctx: &InstallCtx<'_>) -> Option<String> {
        ctx.runner.which("rstudio-server")?;
        let output = ctx.runner.run("rstudio-server", &["version"]).ok()?;
        output.success.then_some(())?;
        output.stdout.split_whitespace().find_map(|token| {
            let token =
                token.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.' && ch != '+');
            exact_release(token).ok().map(|release| release.version)
        })
    }

    fn latest(&self) -> Result<RstudioRelease> {
        discover_stable(&crate::tools::no_redirect_agent(), STABLE_ENDPOINT)
    }

    fn exact(&self, version: &str) -> Result<RstudioRelease> {
        exact_release(version)
    }

    fn download(&self, release: &RstudioRelease) -> Result<DownloadedPackage> {
        download_package(&crate::tools::no_redirect_agent(), release)
    }

    fn inspect(&self, ctx: &InstallCtx<'_>, package: &Path) -> Result<String> {
        let path = package
            .to_str()
            .context("the temporary package path is not valid UTF-8")?;
        let output = ctx
            .run_step(
                "verifying the RStudio Server package",
                "dpkg-deb",
                &["--field", path, "Package", "Architecture", "Version"],
            )
            .context("could not inspect the downloaded RStudio Server package")?;
        Ok(output.stdout)
    }

    fn install(&self, ctx: &InstallCtx<'_>, package: &Path) -> Result<()> {
        // apt-get treats an argument as a local package file only when it
        // starts with `/`, `./`, or `../` and names a `.deb` (or `.ddeb`)
        // file, so the path is always passed in absolute form.
        let path = apt_package_path(package)?;
        ctx.run_apt_step("installing RStudio Server", &["install", "-y", &path])
            .context("could not install the verified RStudio Server package")?;
        Ok(())
    }

    fn systemd_running(&self) -> bool {
        Path::new(SYSTEMD_RUNTIME).is_dir()
    }
}

fn require_linux(ctx: &InstallCtx<'_>) -> Result<()> {
    if ctx.os == Os::Linux {
        Ok(())
    } else {
        Err(anyhow!("RStudio Server is only supported on Linux amd64"))
            .hint("run hpds setup on an amd64 Linux server")
    }
}

fn require_linux_amd64(ctx: &InstallCtx<'_>) -> Result<()> {
    require_linux(ctx)?;
    if !cfg!(target_os = "linux") || std::env::consts::ARCH == "x86_64" {
        Ok(())
    } else {
        Err(anyhow!("RStudio Server is only supported on Linux amd64"))
            .hint("run hpds setup on an amd64 Linux server")
    }
}

/// Make sure the `rstudio-server` service is running.
///
/// The package's install script starts the service right away, and that
/// start fails when R is not installed yet; systemd then gives up on it.
/// Once R is in place, a reset and restart brings it up. Hosts without
/// systemd have no service to start, so they are left as they are.
fn ensure_service<B: RstudioBackend>(backend: &B, ctx: &InstallCtx<'_>) -> Result<()> {
    if !backend.systemd_running() || service_active(ctx)? {
        return Ok(());
    }
    ui::println("the rstudio-server service is not running");
    ctx.approve_sudo_commands("restarting rstudio-server", &[RESET_FAILED, RESTART])?;
    // A unit that never hit systemd's start limit has nothing to reset.
    let _ = ctx.run_approved_sudo(RESET_FAILED);
    let restarted = ctx
        .run_approved_sudo(RESTART)
        .and_then(|output| ensure_step_success("systemctl", output));
    if let Err(err) = restarted {
        // The restart's own generic hint would shadow the one that names R.
        return Err(anyhow!("rstudio-server did not start: {err:#}")).hint(SERVICE_HINT);
    }
    if service_active(ctx)? {
        ui::success("rstudio-server is running");
        Ok(())
    } else {
        Err(anyhow!("rstudio-server is not active after a restart")).hint(SERVICE_HINT)
    }
}

/// Whether systemd reports `rstudio-server` active. `systemctl is-active`
/// prints the unit's state and exits nonzero unless it is active.
fn service_active(ctx: &InstallCtx<'_>) -> Result<bool> {
    let output = ctx
        .runner
        .run("systemctl", &["is-active", "rstudio-server"])
        .context("could not run `systemctl`")
        .hint(
            "check that systemd's `systemctl` is on PATH, then rerun `hpds install rstudio-server`",
        )?;
    Ok(output.success && output.stdout.trim() == "active")
}

impl<B: RstudioBackend> RstudioServer<B> {
    /// Download `release` and check that the file is that exact
    /// `rstudio-server` amd64 package before anything installs it.
    fn verified_package(
        &self,
        ctx: &InstallCtx<'_>,
        release: &RstudioRelease,
    ) -> Result<DownloadedPackage> {
        let package = self.backend.download(release)?;
        let metadata = self
            .backend
            .inspect(ctx, package.path())
            .context("could not inspect the downloaded RStudio Server package")?;
        parse_debian_metadata(&metadata, &release.version)?;
        Ok(package)
    }
}

impl<B: RstudioBackend> Installer for RstudioServer<B> {
    fn name(&self) -> &'static str {
        "rstudio-server"
    }
    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        self.backend.detect(ctx)
    }
    fn resolve_target(&self, ctx: &InstallCtx) -> Result<Option<String>> {
        self.backend.preflight(ctx)?;
        let release = match ctx.pin.as_deref() {
            Some(version) => self.backend.exact(version)?,
            None => self.backend.latest()?,
        };
        let version = release.version.clone();
        *self
            .resolved
            .lock()
            .unwrap_or_else(|lock| lock.into_inner()) = Some(release);
        Ok(Some(version))
    }
    fn install(&self, ctx: &InstallCtx) -> Result<()> {
        self.backend.preflight(ctx)?;
        let release = self
            .resolved
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .take()
            .context("missing resolved RStudio Server release")?;
        let package = self.verified_package(ctx, &release)?;
        self.backend.install(ctx, package.path())
    }
    fn prepare_package(&self, ctx: &InstallCtx) -> Result<Option<DownloadedPackage>> {
        self.backend.preflight(ctx)?;
        let release = match ctx.pin.as_deref() {
            Some(version) => self.backend.exact(version)?,
            None => self.backend.latest()?,
        };
        if let Some(found) = self.backend.detect(ctx)
            && is_current(&found, &release.version, ctx.pin.is_some())
        {
            return Ok(None);
        }
        self.verified_package(ctx, &release).map(Some)
    }
    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        let selected = ctx.pin.as_deref().unwrap_or("latest stable");
        vec![
            format!("download the RStudio Server {selected} amd64 package from Posit"),
            "verify its Debian package identity with dpkg-deb".to_string(),
            apt_get_command_line(&["install", "-y", "<absolute path of the verified package>"]),
            SERVICE_PLAN.to_string(),
        ]
    }
    fn ensure(&self, ctx: &InstallCtx) -> Result<()> {
        ensure_service(&self.backend, ctx)
    }
    fn supports_pin(&self) -> bool {
        true
    }
    fn verifies_target(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};

    use super::{
        DebianMetadata, DownloadedPackage, HttpBackend, RstudioBackend, RstudioRelease,
        RstudioServer, STABLE_ENDPOINT, discover_stable, download_package_from,
        parse_debian_metadata, parse_stable_redirect,
    };
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on};
    use crate::install::{CommandOutput, CommandRunner, InstallCtx, Installer, run_installer};
    use crate::tools::Os;
    use crate::ui::render_error;

    const LATEST: &str = "2026.09.0+174";
    const LATEST_URL: &str = "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb";

    struct LocalServer {
        server: Arc<tiny_http::Server>,
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl LocalServer {
        fn redirect(location: String) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind server"));
            let address = server.server_addr().to_ip().expect("IP listener");
            let hits = Arc::new(Mutex::new(Vec::new()));
            let handle = {
                let server = Arc::clone(&server);
                let hits = Arc::clone(&hits);
                std::thread::spawn(move || {
                    for request in server.incoming_requests() {
                        hits.lock().expect("hits lock").push(format!(
                            "{} {}",
                            request.method(),
                            request.url()
                        ));
                        let header = tiny_http::Header::from_bytes("Location", location.as_bytes())
                            .expect("location header");
                        let response = tiny_http::Response::empty(302).with_header(header);
                        let _ = request.respond(response);
                    }
                })
            };
            Self {
                server,
                base: format!("http://{address}"),
                hits,
                handle: Some(handle),
            }
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().expect("hits lock").clone()
        }
    }

    impl Drop for LocalServer {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn discovery_uses_posits_stable_server_endpoint() {
        assert_eq!(
            STABLE_ENDPOINT,
            "https://rstudio.org/download/latest/stable/server/jammy/rstudio-server-latest-amd64.deb"
        );
    }

    #[test]
    fn discovery_rejects_an_untrusted_redirect_without_contacting_it() {
        let target = LocalServer::redirect(LATEST_URL.to_string());
        let source = LocalServer::redirect(format!("{}/untrusted", target.base));

        let error = discover_stable(
            &crate::tools::no_redirect_agent(),
            &format!("{}/stable", source.base),
        )
        .expect_err("local redirect is not a trusted release");

        assert!(error.to_string().contains("untrusted"), "{error:#}");
        assert_eq!(source.hits(), ["HEAD /stable"]);
        assert!(target.hits().is_empty(), "redirect target was contacted");
    }

    #[test]
    fn package_download_rejects_a_redirect_without_contacting_its_target() {
        let target = LocalServer::redirect(LATEST_URL.to_string());
        let source = LocalServer::redirect(format!("{}/target", target.base));
        let release = parse_stable_redirect(LATEST_URL).expect("trusted release");

        let error = download_package_from(
            &crate::tools::no_redirect_agent(),
            &release,
            &format!("{}/package", source.base),
            true,
        )
        .expect_err("package redirects are forbidden");

        assert!(error.to_string().contains("redirect"), "{error:#}");
        assert_eq!(source.hits(), ["GET /package"]);
        assert!(target.hits().is_empty(), "redirect target was contacted");
    }

    #[test]
    fn stable_redirect_parses_the_canonical_release() {
        let release = parse_stable_redirect(LATEST_URL).expect("valid stable redirect");
        assert_eq!(release.version, LATEST);
        assert_eq!(release.filename, "rstudio-server-2026.09.0-174-amd64.deb");
        assert_eq!(release.url, LATEST_URL);
    }

    #[test]
    fn stable_redirect_rejects_untrusted_or_malformed_targets() {
        let invalid = [
            "http://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://example.com/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com@example.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/not-rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/desktop/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-workbench-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/arm64/rstudio-server-2026.09.0-174-arm64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-rc1-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb?download=1",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb#fragment",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/%2e%2e%2frstudio-server-2026.09.0-174-amd64.deb",
        ];

        for url in invalid {
            assert!(parse_stable_redirect(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn debian_metadata_accepts_only_the_discovered_package() {
        let metadata = parse_debian_metadata(
            "Package: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            LATEST,
        )
        .expect("matching package metadata");
        assert_eq!(
            metadata,
            DebianMetadata {
                package: "rstudio-server".to_string(),
                architecture: "amd64".to_string(),
                version: LATEST.to_string(),
            }
        );
    }

    #[test]
    fn debian_metadata_rejects_wrong_missing_duplicate_or_malformed_fields() {
        let invalid = [
            "Package: rstudio-workbench\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: arm64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+173\n",
            "Architecture: amd64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: amd64\n",
            "Package: rstudio-server\nPackage: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            "Package rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
        ];

        for metadata in invalid {
            assert!(
                parse_debian_metadata(metadata, LATEST).is_err(),
                "accepted {metadata:?}"
            );
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        phases: RefCell<Vec<String>>,
        installed: RefCell<Option<String>>,
        downloaded: RefCell<Option<String>>,
        metadata_override: Option<String>,
        fail_inspect: bool,
        systemd: bool,
    }

    impl FakeBackend {
        fn installed(version: &str) -> Self {
            Self {
                installed: RefCell::new(Some(version.to_string())),
                ..Self::default()
            }
        }
    }

    impl RstudioBackend for FakeBackend {
        fn detect(&self, _ctx: &InstallCtx<'_>) -> Option<String> {
            self.installed.borrow().clone()
        }

        fn latest(&self) -> Result<RstudioRelease> {
            self.phases.borrow_mut().push("resolve latest".to_string());
            parse_stable_redirect(LATEST_URL)
        }

        fn exact(&self, version: &str) -> Result<RstudioRelease> {
            self.phases
                .borrow_mut()
                .push(format!("resolve exact {version}"));
            let filename_version = version.replace('+', "-");
            parse_stable_redirect(&format!(
                "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-{filename_version}-amd64.deb"
            ))
        }

        fn download(&self, release: &RstudioRelease) -> Result<DownloadedPackage> {
            self.phases
                .borrow_mut()
                .push(format!("download {}", release.version));
            *self.downloaded.borrow_mut() = Some(release.version.clone());
            Ok(DownloadedPackage::at(PathBuf::from(
                "/tmp/test-rstudio-server.deb",
            )))
        }

        fn inspect(&self, _ctx: &InstallCtx<'_>, _package: &Path) -> Result<String> {
            self.phases.borrow_mut().push("inspect".to_string());
            if self.fail_inspect {
                Err(anyhow!("dpkg-deb failed"))
            } else {
                Ok(self.metadata_override.clone().unwrap_or_else(|| {
                    format!(
                        "Package: rstudio-server\nArchitecture: amd64\nVersion: {}\n",
                        self.downloaded.borrow().as_deref().expect("download first")
                    )
                }))
            }
        }

        fn install(&self, _ctx: &InstallCtx<'_>, _package: &Path) -> Result<()> {
            self.phases.borrow_mut().push("install package".to_string());
            *self.installed.borrow_mut() = self.downloaded.borrow().clone();
            Ok(())
        }

        fn systemd_running(&self) -> bool {
            self.systemd
        }
    }

    const IS_ACTIVE: &str = "systemctl is-active rstudio-server";
    const RESET: &str = "sudo systemctl reset-failed rstudio-server";
    const RESTART: &str = "sudo systemctl restart rstudio-server";
    const R_HINT: &str = "hint: RStudio Server needs R: install it with `hpds install r` (or rerun the R step of server setup), then run `sudo systemctl restart rstudio-server`; `journalctl -u rstudio-server` shows why it stopped";

    /// The `rstudio-server` unit on a systemd host, simulated through the
    /// runner seam: `systemctl is-active` reports the unit's state, and a
    /// restart starts it unless the restart fails or the service does not
    /// stay up. Any other command is unexpected.
    #[derive(Default)]
    struct ServiceRunner {
        active: Mutex<bool>,
        /// `systemctl reset-failed` exits nonzero, as it does for a unit
        /// that never hit systemd's start limit.
        reset_fails: bool,
        /// `systemctl restart` exits nonzero, as it does without R.
        restart_fails: bool,
        /// A restart reports success but the service does not stay up.
        never_starts: bool,
        calls: Mutex<Vec<String>>,
    }

    impl ServiceRunner {
        fn active() -> Self {
            Self {
                active: Mutex::new(true),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn is_active(&self) -> bool {
            *self.active.lock().unwrap()
        }
    }

    impl CommandRunner for ServiceRunner {
        fn which(&self, _program: &str) -> Option<PathBuf> {
            None
        }

        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput> {
            let command = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(command.clone());
            let output = |success: bool, stdout: &str, stderr: &str| CommandOutput {
                success,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            };
            match command.as_str() {
                IS_ACTIVE if self.is_active() => Ok(output(true, "active\n", "")),
                IS_ACTIVE => Ok(output(false, "failed\n", "")),
                RESET if self.reset_fails => Ok(output(
                    false,
                    "",
                    "Failed to reset failed state of unit rstudio-server.service: Unit rstudio-server.service not loaded.",
                )),
                RESET => Ok(output(true, "", "")),
                RESTART if self.restart_fails => Ok(output(
                    false,
                    "",
                    "Job for rstudio-server.service failed because the control process exited with error code.",
                )),
                RESTART => {
                    if !self.never_starts {
                        *self.active.lock().unwrap() = true;
                    }
                    Ok(output(true, "", ""))
                }
                _ => Err(anyhow!("unexpected command `{command}`")),
            }
        }
    }

    /// A ctx on Linux with `--yes` whose commands go to `runner`.
    fn service_ctx<'a>(runner: &'a ServiceRunner, fetcher: &'a FakeFetcher) -> InstallCtx<'a> {
        InstallCtx {
            os: Os::Linux,
            yes: true,
            verbose: false,
            pin: None,
            plan_approved: false,
            sudo_approved: Cell::new(false),
            runner,
            fetcher,
        }
    }

    /// A backend on a systemd host with `installed` already in place.
    fn systemd_backend(installed: Option<&str>) -> FakeBackend {
        FakeBackend {
            installed: RefCell::new(installed.map(str::to_string)),
            systemd: true,
            ..FakeBackend::default()
        }
    }

    #[test]
    fn a_fresh_install_restarts_a_service_that_did_not_start() {
        let backend = systemd_backend(None);
        let installer = RstudioServer::with_backend(&backend);
        let runner = ServiceRunner::default();
        let fetcher = FakeFetcher::default();

        run_installer(&installer, &service_ctx(&runner, &fetcher))
            .expect("the service starts after a restart");

        assert!(
            backend
                .phases
                .borrow()
                .iter()
                .any(|phase| phase == "install package")
        );
        assert_eq!(runner.calls(), [IS_ACTIVE, RESET, RESTART, IS_ACTIVE]);
        assert!(runner.is_active());
    }

    #[test]
    fn a_current_install_restarts_a_service_that_is_down() {
        for reset_fails in [false, true] {
            let backend = systemd_backend(Some(LATEST));
            let installer = RstudioServer::with_backend(&backend);
            let runner = ServiceRunner {
                reset_fails,
                ..ServiceRunner::default()
            };
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &service_ctx(&runner, &fetcher))
                .expect("the service starts after a restart");

            assert!(
                !backend
                    .phases
                    .borrow()
                    .iter()
                    .any(|phase| phase == "install package"),
                "a current install is not reinstalled"
            );
            assert_eq!(
                runner.calls(),
                [IS_ACTIVE, RESET, RESTART, IS_ACTIVE],
                "reset fails: {reset_fails}"
            );
            assert!(runner.is_active());
        }
    }

    #[test]
    fn an_active_service_is_left_alone() {
        for installed in [None, Some(LATEST)] {
            let backend = systemd_backend(installed);
            let installer = RstudioServer::with_backend(&backend);
            let runner = ServiceRunner::active();
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &service_ctx(&runner, &fetcher))
                .expect("an active service needs nothing");

            assert_eq!(runner.calls(), [IS_ACTIVE], "{installed:?}");
        }
    }

    #[test]
    fn a_service_that_will_not_start_fails_with_the_r_hint() {
        for installed in [None, Some(LATEST)] {
            for (restart_fails, never_starts) in [(true, false), (false, true)] {
                let backend = systemd_backend(installed);
                let installer = RstudioServer::with_backend(&backend);
                let runner = ServiceRunner {
                    restart_fails,
                    never_starts,
                    ..ServiceRunner::default()
                };
                let fetcher = FakeFetcher::default();

                let error = run_installer(&installer, &service_ctx(&runner, &fetcher))
                    .expect_err("a service that will not start fails the install");

                let rendered = render_error(&error, false);
                let case = format!("{installed:?}, restart fails: {restart_fails}");
                assert!(rendered.contains("rstudio-server"), "{case}: {rendered}");
                assert!(rendered.contains(R_HINT), "{case}: {rendered}");
                let calls = runner.calls();
                let restart = calls
                    .iter()
                    .position(|call| call == RESTART)
                    .unwrap_or_else(|| panic!("{case}: no restart in {calls:?}"));
                if never_starts {
                    assert_eq!(
                        calls[restart + 1..],
                        [IS_ACTIVE],
                        "{case}: the service is checked again after the restart"
                    );
                }
            }
        }
    }

    #[test]
    fn without_systemd_no_service_is_checked() {
        for installed in [None, Some(LATEST)] {
            let backend = FakeBackend {
                installed: RefCell::new(installed.map(str::to_string)),
                ..FakeBackend::default()
            };
            let installer = RstudioServer::with_backend(&backend);
            let runner = ServiceRunner::default();
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &service_ctx(&runner, &fetcher))
                .expect("a host without systemd installs as before");

            assert!(runner.calls().is_empty(), "{installed:?}");
        }
    }

    #[test]
    fn unpinned_latest_updates_older_and_retains_equal_or_newer() {
        for (installed, installs) in [
            ("2025.05.1+513", true),
            (LATEST, false),
            ("2026.10.0+1", false),
        ] {
            let backend = FakeBackend::installed(installed);
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
                .expect("latest install decision");

            let phases = backend.phases.borrow();
            assert_eq!(phases.first().map(String::as_str), Some("resolve latest"));
            assert_eq!(
                phases.iter().any(|phase| phase == "install package"),
                installs
            );
        }
    }

    #[test]
    fn exact_pin_bypasses_latest_and_may_downgrade() {
        const PIN: &str = "2025.05.1+513";
        let backend = FakeBackend::installed("2026.10.0+1");
        let installer = RstudioServer::with_backend(&backend);
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some(PIN.to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };

        assert!(installer.supports_pin());
        run_installer(&installer, &ctx).expect("an exact pin may downgrade");
        let phases = backend.phases.borrow();
        assert_eq!(
            phases.first().map(String::as_str),
            Some("resolve exact 2025.05.1+513")
        );
        assert!(!phases.iter().any(|phase| phase == "resolve latest"));
        assert!(phases.iter().any(|phase| phase == "download 2025.05.1+513"));
        assert!(phases.iter().any(|phase| phase == "install package"));
        assert_eq!(backend.installed.borrow().as_deref(), Some(PIN));
    }

    #[test]
    fn non_linux_fails_before_network_or_commands() {
        for os in [Os::Mac, Os::Windows] {
            let backend = FakeBackend::default();
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();

            let error = run_installer(&installer, &ctx_on(os, &runner, &fetcher))
                .expect_err("RStudio Server is Linux-only");
            assert!(error.to_string().contains("Linux"), "{error:#}");
            assert!(backend.phases.borrow().is_empty());
            assert!(runner.calls().is_empty());
        }
    }

    #[test]
    fn install_orders_phases_and_inspector_failure_prevents_install() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();

        let backend = FakeBackend::installed("2025.05.1+513");
        let installer = RstudioServer::with_backend(&backend);
        run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
            .expect("valid package installs");
        assert_eq!(
            *backend.phases.borrow(),
            [
                "resolve latest",
                "download 2026.09.0+174",
                "inspect",
                "install package"
            ]
        );

        let failing = FakeBackend {
            installed: RefCell::new(Some("2025.05.1+513".to_string())),
            fail_inspect: true,
            ..FakeBackend::default()
        };
        let installer = RstudioServer::with_backend(&failing);
        run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
            .expect_err("inspector failure must stop installation");
        assert_eq!(
            *failing.phases.borrow(),
            ["resolve latest", "download 2026.09.0+174", "inspect"]
        );
    }

    #[test]
    fn invalid_inspected_identity_prevents_install() {
        for metadata in [
            "Package: rstudio-workbench\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: arm64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+173\n",
        ] {
            let backend = FakeBackend {
                installed: RefCell::new(Some("2025.05.1+513".to_string())),
                metadata_override: Some(metadata.to_string()),
                ..FakeBackend::default()
            };
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
                .expect_err("invalid package identity must stop installation");
            assert_eq!(
                *backend.phases.borrow(),
                ["resolve latest", "download 2026.09.0+174", "inspect"],
                "{metadata:?}"
            );
        }
    }

    #[test]
    fn prepare_package_skips_a_current_install_without_downloading() {
        for installed in [LATEST, "2026.10.0+1"] {
            let backend = FakeBackend::installed(installed);
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();
            let package = installer
                .prepare_package(&ctx_on(Os::Linux, &runner, &fetcher))
                .expect("a current install needs no package");
            assert!(package.is_none(), "{installed}");
            assert_eq!(*backend.phases.borrow(), ["resolve latest"], "{installed}");
        }
    }

    #[test]
    fn prepare_package_downloads_and_verifies_without_installing() {
        for installed in [None, Some("2025.05.1+513")] {
            let backend = FakeBackend {
                installed: RefCell::new(installed.map(str::to_string)),
                ..FakeBackend::default()
            };
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();
            let package = installer
                .prepare_package(&ctx_on(Os::Linux, &runner, &fetcher))
                .expect("an outdated install needs the package")
                .expect("a package");
            assert_eq!(package.path(), Path::new("/tmp/test-rstudio-server.deb"));
            assert_eq!(
                *backend.phases.borrow(),
                ["resolve latest", "download 2026.09.0+174", "inspect"],
                "{installed:?}"
            );
        }
    }

    #[test]
    fn prepare_package_rejects_a_package_with_the_wrong_identity() {
        let backend = FakeBackend {
            metadata_override: Some(
                "Package: rstudio-workbench\nArchitecture: amd64\nVersion: 2026.09.0+174\n"
                    .to_string(),
            ),
            ..FakeBackend::default()
        };
        let installer = RstudioServer::with_backend(&backend);
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        installer
            .prepare_package(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect_err("a mismatched package is never handed out");
        assert!(
            !backend
                .phases
                .borrow()
                .iter()
                .any(|p| p == "install package")
        );
    }

    #[test]
    fn install_hands_apt_get_the_verified_package_by_absolute_path() {
        let line = crate::install::apt_get_command_line(&[
            "install",
            "-y",
            "/tmp/hpds-rstudio-server-abc.deb",
        ]);
        let runner = FakeRunner::default().with_output(&line, "");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            yes: true,
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };
        HttpBackend
            .install(&ctx, Path::new("/tmp/hpds-rstudio-server-abc.deb"))
            .expect("apt-get installs the package");
        assert_eq!(runner.calls(), [line]);
    }

    #[test]
    fn install_makes_a_relative_package_path_absolute_for_apt_get() {
        let absolute = std::path::absolute("hpds-rstudio-server-abc.deb").expect("absolute path");
        let absolute = absolute.to_str().expect("UTF-8 path");
        let line = crate::install::apt_get_command_line(&["install", "-y", absolute]);
        let runner = FakeRunner::default().with_output(&line, "");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            yes: true,
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };
        HttpBackend
            .install(&ctx, Path::new("hpds-rstudio-server-abc.deb"))
            .expect("apt-get installs the package");
        assert_eq!(runner.calls(), [line]);
    }

    #[test]
    fn plan_installs_the_verified_package_with_apt_get_not_gdebi() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let plan = RstudioServer::production().plan(&ctx_on(Os::Linux, &runner, &fetcher));
        let [.., install, service] = plan.as_slice() else {
            panic!("plan has an install line and a service line: {plan:?}");
        };
        assert!(
            install.starts_with(&crate::install::apt_get_command_line(&["install", "-y"])),
            "{install}"
        );
        assert_eq!(service, super::SERVICE_PLAN);
        assert!(plan.iter().all(|line| !line.contains("gdebi")), "{plan:?}");
    }
}
