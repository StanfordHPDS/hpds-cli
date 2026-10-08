use std::io::copy;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};

use crate::install::{InstallCtx, Installer};
use crate::tools::Os;
use crate::ui::HintExt;

pub const STABLE_ENDPOINT: &str =
    "https://rstudio.org/download/latest/stable/server/jammy/rstudio-server-latest-amd64.deb";
const RELEASE_PREFIX: &str =
    "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-";
const RELEASE_SUFFIX: &str = "-amd64.deb";

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

#[derive(Debug)]
pub struct DownloadedPackage {
    path: PathBuf,
    _temporary: Option<tempfile::NamedTempFile>,
}

impl DownloadedPackage {
    fn temporary(file: tempfile::NamedTempFile) -> Self {
        Self {
            path: file.path().to_path_buf(),
            _temporary: Some(file),
        }
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self {
            path,
            _temporary: None,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
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

pub fn parse_debian_metadata(output: &str, selected: &str) -> Result<DebianMetadata> {
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
    let metadata = DebianMetadata {
        package: one(output, "Package")?,
        architecture: one(output, "Architecture")?,
        version: one(output, "Version")?,
    };
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
        let path = package
            .to_str()
            .context("the temporary package path is not valid UTF-8")?;
        ctx.run_sudo_step("installing RStudio Server", "gdebi", &["-n", path])
            .context("could not install the verified RStudio Server package")?;
        Ok(())
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
        let package = self.backend.download(&release)?;
        let metadata = self
            .backend
            .inspect(ctx, package.path())
            .context("could not inspect the downloaded RStudio Server package")?;
        parse_debian_metadata(&metadata, &release.version)?;
        self.backend.install(ctx, package.path())
    }
    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        let selected = ctx.pin.as_deref().unwrap_or("latest stable");
        vec![
            format!("download the RStudio Server {selected} amd64 package from Posit"),
            "verify its Debian package identity with dpkg-deb".to_string(),
            "sudo gdebi -n the verified temporary package".to_string(),
        ]
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
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};

    use super::{
        DebianMetadata, DownloadedPackage, RstudioBackend, RstudioRelease, RstudioServer,
        STABLE_ENDPOINT, discover_stable, download_package_from, parse_debian_metadata,
        parse_stable_redirect,
    };
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on};
    use crate::install::{InstallCtx, Installer, run_installer};
    use crate::tools::Os;

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
            self.phases.borrow_mut().push("sudo gdebi".to_string());
            *self.installed.borrow_mut() = self.downloaded.borrow().clone();
            Ok(())
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
            assert_eq!(phases.iter().any(|phase| phase == "sudo gdebi"), installs);
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
        assert!(phases.iter().any(|phase| phase == "sudo gdebi"));
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
            assert!(runner.calls.borrow().is_empty());
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
                "sudo gdebi"
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
}
