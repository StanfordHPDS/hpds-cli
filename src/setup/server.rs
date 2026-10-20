use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::install::installers::rstudio_server::parse_debian_identity;
use crate::install::{self, CommandRunner, DownloadedPackage, ReleaseAsset};
use crate::tools::ToolSpec;
use crate::ui::{self, HintExt};

#[derive(Debug)]
pub(crate) enum ServerAction {
    Docker,
    Rust,
    RProfile,
    RstudioPreferences,
    InstallCodeServer,
    CodeServer,
}

pub(crate) fn describe(action: &ServerAction) -> &'static str {
    match action {
        ServerAction::Docker => {
            "if Docker Engine is still missing, install it from download.docker.com/linux/ubuntu (docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin); add <user> to the docker group"
        }
        ServerAction::Rust => "install or update stable Rust with verified rustup",
        ServerAction::RProfile => {
            "ensure the Posit Package Manager CRAN repository line is present once"
        }
        ServerAction::RstudioPreferences => {
            "merge RStudio preferences: insert_native_pipe_operator=true save_workspace=never load_workspace=never rainbow_parentheses=true rainbow_fenced_divs=true"
        }
        ServerAction::InstallCodeServer => {
            "if code-server is still missing, download and verify its package as in step 1 and install it with sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 apt-get -o DPkg::Lock::Timeout=600 install -y <verified code-server package>"
        }
        ServerAction::CodeServer => {
            "set quarto.path to <home>/.local/bin/quarto and systemctl enable --now code-server@<user>"
        }
    }
}

pub(crate) fn run(
    action: &ServerAction,
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
) -> Result<()> {
    match action {
        ServerAction::Docker => reconcile_docker(host, runner, DockerState::Probe),
        ServerAction::Rust => reconcile_rust(host, runner, RustState::Probe),
        ServerAction::RProfile => reconcile_r_profile(host),
        ServerAction::RstudioPreferences => merge_rstudio_preferences(host),
        ServerAction::InstallCodeServer => install_code_server(host, runner),
        ServerAction::CodeServer => reconcile_code_server(host, runner),
    }
}

/// The plan line for registering the Docker apt repository.
pub(crate) const DOCKER_REPO_PLAN: &str = "register the Docker apt repository download.docker.com/linux/ubuntu with its signing key in /etc/apt/keyrings/docker.asc";

/// The plan line for [`prepare_code_server`].
pub(crate) const CODE_SERVER_PACKAGE_PLAN: &str = "download the latest code-server amd64 package from github.com/coder/code-server releases, check it against GitHub's sha256 digest, and verify its Debian package identity with dpkg-deb";

/// code-server's GitHub releases, which publish one amd64 Debian package
/// per release. The package carries the `code-server@` systemd unit.
const CODE_SERVER: ToolSpec = ToolSpec {
    name: "code-server",
    default_version: "latest",
    repo: "coder/code-server",
    asset_pattern: "code-server_{version}_amd64.deb",
    checksum_pattern: None,
};

/// A downloaded code-server package and the release version it claims.
pub(crate) struct CodeServerPackage {
    pub(crate) package: DownloadedPackage,
    pub(crate) version: String,
}

/// The verified code-server package to install, or `None` when
/// code-server is already on `PATH`.
///
/// The package comes from the latest GitHub release (through the
/// [`ServerHost`], which checks GitHub's sha256 digest of the asset), and
/// `dpkg-deb` must report it as exactly that `code-server` amd64 version
/// before anything installs it.
pub(crate) fn prepare_code_server(
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
) -> Result<Option<DownloadedPackage>> {
    if runner.which("code-server").is_some() {
        return Ok(None);
    }
    let CodeServerPackage { package, version } = host.acquire_code_server_package()?;
    let path = package
        .path()
        .to_str()
        .context("the temporary code-server package path is not UTF-8")?;
    let fields = run_checked(
        runner,
        "dpkg-deb",
        &["--field", path, "Package", "Architecture", "Version"],
    )
    .context("could not inspect the downloaded code-server package")?;
    let identity = parse_debian_identity(&fields).hint(
        "the package was not installed; rerun server setup once the code-server release is corrected",
    )?;
    if identity.package != "code-server"
        || identity.architecture != "amd64"
        || identity.version != version
    {
        return Err(anyhow!(
            "the downloaded code-server package identity does not match code-server {version} amd64"
        ))
        .hint("the package was not installed; rerun server setup once the code-server release is corrected");
    }
    Ok(Some(package))
}

fn install_code_server(host: &dyn ServerHost, runner: &dyn CommandRunner) -> Result<()> {
    let Some(package) = prepare_code_server(host, runner)? else {
        return Ok(());
    };
    let path = absolute_package_path(&package)?;
    run_apt(runner, &["install", "-y", &path])?;
    Ok(())
}

/// The path apt-get needs to treat `package` as a local package file
/// rather than a package name.
pub(crate) fn absolute_package_path(package: &DownloadedPackage) -> Result<String> {
    install::apt_package_path(package.path())
}

/// Resolve the latest code-server release from the GitHub API at
/// `api_base` and download its amd64 package to a private temporary
/// `.deb` file, checking GitHub's sha256 digest of the asset.
fn acquire_code_server_from(agent: &ureq::Agent, api_base: &str) -> Result<CodeServerPackage> {
    let asset = install::latest_release_asset(agent, api_base, &CODE_SERVER)
        .map_err(|err| anyhow!("could not resolve the latest code-server release: {err:#}"))
        .hint("check the network connection (or HTTPS_PROXY) and rerun server setup")?;
    let file = download_verified_package(agent, &asset)?;
    Ok(CodeServerPackage {
        package: DownloadedPackage::temporary(file),
        version: asset.version,
    })
}

/// The hex digest in GitHub's `sha256:<64 hex digits>` asset digest.
fn sha256_digest(digest: Option<&str>) -> Result<&str> {
    digest
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow!("GitHub reports no sha256 digest for the code-server package"))
        .hint("rerun server setup later; the package was not downloaded")
}

fn download_verified_package(
    agent: &ureq::Agent,
    asset: &ReleaseAsset,
) -> Result<tempfile::NamedTempFile> {
    let expected = sha256_digest(asset.digest.as_deref())?;
    let mut response = agent
        .get(&asset.url)
        .call()
        .with_context(|| format!("could not download code-server {}", asset.version))
        .hint("check the network connection and rerun server setup")?;
    let mut file = tempfile::Builder::new()
        .prefix("hpds-code-server-")
        .suffix(".deb")
        .tempfile()
        .context("could not create a temporary code-server package file")
        .hint("check that the temporary directory is writable")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut reader = response.body_mut().as_reader();
    loop {
        let count = reader
            .read(&mut buffer)
            .context("could not download the code-server package")
            .hint("check the network connection and rerun server setup")?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count])
            .context("could not write the code-server package")?;
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(anyhow!(
            "the downloaded code-server package does not match GitHub's sha256 digest"
        ))
        .hint("rerun server setup; the package was not installed");
    }
    Ok(file)
}

pub(crate) const DOCKER_PACKAGES: &[&str] = &[
    "docker-ce",
    "docker-ce-cli",
    "containerd.io",
    "docker-buildx-plugin",
    "docker-compose-plugin",
];
const RSTUDIO_PREFS: &str = "/etc/rstudio/rstudio-prefs.json";
const R_PROFILE: &str = "/etc/R/Rprofile.site";
const R_REPOSITORY_LINE: &str = "options(repos = c(P3M = \"https://packagemanager.posit.co/cran/__linux__/noble/latest\", CRAN = \"https://cloud.r-project.org\"))";

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn server_path(parent: &Path, child: &str) -> Result<PathBuf> {
    let parent = parent
        .to_str()
        .context("managed server path is not UTF-8")?
        .trim_end_matches('/');
    Ok(PathBuf::from(format!("{parent}/{child}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockerState {
    Probe,
    Absent,
    Current,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RustState {
    Probe,
    Absent,
    UpdateAvailable,
    NonStable,
    CurrentStable,
}

pub(crate) trait ServerHost {
    fn current_user(&self) -> Result<String>;
    fn home_dir(&self, user: &str) -> Result<PathBuf>;
    fn read(&self, path: &Path) -> Result<Option<Vec<u8>>>;
    fn exists(&self, path: &Path) -> bool;
    fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()>;
    fn acquire_verified_rustup_init(&self) -> Result<PathBuf>;
    fn acquire_docker_key(&self, runner: &dyn CommandRunner) -> Result<Vec<u8>>;
    fn acquire_code_server_package(&self) -> Result<CodeServerPackage>;
}

pub(crate) struct SystemServerHost<'a> {
    runner: &'a (dyn CommandRunner + Sync),
}

impl<'a> SystemServerHost<'a> {
    pub(crate) fn new(runner: &'a (dyn CommandRunner + Sync)) -> Self {
        Self { runner }
    }
}

impl ServerHost for SystemServerHost<'_> {
    fn current_user(&self) -> Result<String> {
        let user = std::env::var("USER").context("USER is not set")?;
        validate_user(&user)?;
        Ok(user)
    }
    fn home_dir(&self, user: &str) -> Result<PathBuf> {
        validate_user(user)?;
        let current = self.current_user()?;
        if user != current {
            return Err(anyhow!("cannot resolve home for a different user `{user}`"));
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")
    }
    fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("could not read `{}`", path.display()))
            }
        }
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let parent = path
            .parent()
            .context("managed file has no parent directory")?;
        if path.starts_with("/etc") {
            let mut staged =
                tempfile::NamedTempFile::new().context("could not stage managed server file")?;
            staged
                .write_all(bytes)
                .context("could not write staged server file")?;
            staged
                .as_file_mut()
                .sync_all()
                .context("could not sync staged server file")?;
            let source = staged
                .path()
                .to_str()
                .context("temporary path is not UTF-8")?;
            let destination = path.to_str().context("destination path is not UTF-8")?;
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .context("destination filename is not UTF-8")?;
            let unique = format!(".{name}.hpds-{}-{}", std::process::id(), unique_suffix());
            let publish = server_path(parent, &unique)?;
            let publish_text = publish.to_str().context("publish path is not UTF-8")?;
            let mode = format!("{mode:04o}");
            let install = run_checked(
                self.runner,
                "sudo",
                &["install", "-D", "-m", &mode, source, publish_text],
            );
            if let Err(error) = install {
                let _ = run_checked(self.runner, "sudo", &["rm", "-f", publish_text]);
                return Err(error);
            }
            let rename = run_checked(
                self.runner,
                "sudo",
                &["mv", "-f", publish_text, destination],
            );
            if rename.is_err() {
                let _ = run_checked(self.runner, "sudo", &["rm", "-f", publish_text]);
            }
            rename?;
            return Ok(());
        }
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create `{}`", parent.display()))?;
        let mut staged =
            tempfile::NamedTempFile::new_in(parent).context("could not stage managed settings")?;
        staged
            .write_all(bytes)
            .context("could not write managed settings")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            staged
                .as_file()
                .set_permissions(fs::Permissions::from_mode(mode))?;
        }
        staged
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("could not publish `{}`", path.display()))?;
        Ok(())
    }
    fn acquire_verified_rustup_init(&self) -> Result<PathBuf> {
        acquire_rustup_init()
    }
    fn acquire_docker_key(&self, runner: &dyn CommandRunner) -> Result<Vec<u8>> {
        let file = tempfile::Builder::new()
            .prefix("hpds-docker-key-")
            .tempfile()
            .context("could not stage Docker signing key")?;
        let path = file
            .path()
            .to_str()
            .context("temporary Docker key path is not UTF-8")?;
        run_checked(
            runner,
            "curl",
            &[
                "-fsSL",
                "https://download.docker.com/linux/ubuntu/gpg",
                "-o",
                path,
            ],
        )?;
        let bytes =
            fs::read(file.path()).context("could not read downloaded Docker signing key")?;
        let text =
            std::str::from_utf8(&bytes).context("Docker signing key is not ASCII-armored text")?;
        if !text.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----\n")
            || !text.contains("\n-----END PGP PUBLIC KEY BLOCK-----")
        {
            return Err(anyhow!(
                "downloaded Docker signing key is not a public PGP key"
            ));
        }
        Ok(bytes)
    }
    fn acquire_code_server_package(&self) -> Result<CodeServerPackage> {
        if std::env::consts::ARCH != "x86_64" {
            return Err(anyhow!(
                "code-server server setup supports Linux amd64 only"
            ))
            .hint("run hpds setup on an amd64 Linux server");
        }
        acquire_code_server_from(&crate::tools::github_agent(), "https://api.github.com")
    }
}

fn run_checked(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<String> {
    let output = runner
        .run(program, args)
        .with_context(|| format!("could not run `{program}`"))?;
    if !output.success {
        let detail = if output.stderr.trim().is_empty() {
            output.stdout.trim()
        } else {
            output.stderr.trim()
        };
        return Err(anyhow!("`{program}` failed: {detail}"))
            .hint("fix the reported command failure and rerun server setup");
    }
    Ok(output.stdout)
}

fn run_sudo(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<String> {
    let mut sudo = vec![program];
    sudo.extend_from_slice(args);
    run_checked(runner, "sudo", &sudo)
}

/// Run `apt-get <args>` under sudo with the shared apt policy.
fn run_apt(runner: &dyn CommandRunner, args: &[&str]) -> Result<String> {
    run_checked(runner, "sudo", &install::apt_get_argv(args))
}

fn validate_user(user: &str) -> Result<()> {
    let valid = !user.is_empty()
        && user.len() <= 32
        && user.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'_'
                || byte == b'-'
                || (index == 0 && byte == b'_')
        })
        && !user.starts_with('-');
    if valid {
        Ok(())
    } else {
        Err(anyhow!("unsafe server username `{user}`")).hint(
            "use a normal local account name containing lowercase letters, digits, `_`, or `-`",
        )
    }
}

fn parse_ubuntu_codename(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("/etc/os-release is not UTF-8")?;
    let mut id = None;
    let mut codename = None;
    for line in text.lines() {
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let value = raw
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(raw);
        if value.contains(['$', '`', '\\', '\n', '\r']) {
            return Err(anyhow!("unsafe value in /etc/os-release"));
        }
        match key {
            "ID" => id = Some(value),
            "VERSION_CODENAME" => codename = Some(value),
            _ => {}
        }
    }
    if id != Some("ubuntu") {
        return Err(anyhow!("Docker setup requires Ubuntu"));
    }
    let codename = codename.context("Ubuntu VERSION_CODENAME is missing")?;
    if codename.is_empty()
        || !codename
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(anyhow!("invalid Ubuntu VERSION_CODENAME `{codename}`"));
    }
    Ok(codename.to_string())
}

/// Whether any Docker Engine package is not installed.
pub(crate) fn docker_missing(runner: &dyn CommandRunner) -> bool {
    docker_state(runner) == DockerState::Absent
}

fn docker_state(runner: &dyn CommandRunner) -> DockerState {
    let args = [
        "-W",
        "-f",
        "${Status}",
        "docker-ce",
        "docker-ce-cli",
        "containerd.io",
        "docker-buildx-plugin",
        "docker-compose-plugin",
    ];
    match runner.run("dpkg-query", &args) {
        Ok(output)
            if output.success
                && output.stdout.matches("install ok installed").count()
                    == DOCKER_PACKAGES.len() =>
        {
            DockerState::Current
        }
        _ => DockerState::Absent,
    }
}

pub(crate) fn reconcile_docker(
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
    state: DockerState,
) -> Result<()> {
    let state = if state == DockerState::Probe {
        docker_state(runner)
    } else {
        state
    };
    let user = host.current_user()?;
    validate_user(&user)?;
    if state == DockerState::Absent {
        let created = register_docker_repo(host, runner)?;
        if let Err(error) = run_apt(runner, &["update"]) {
            // An unreachable Docker repository must not break the package
            // list refreshes of the steps after this one.
            if created && let Err(cleanup) = run_sudo(runner, "rm", &["-f", DOCKER_SOURCES]) {
                ui::warn(&format!("could not remove `{DOCKER_SOURCES}`: {cleanup:#}"));
            }
            return Err(error);
        }
        let mut args = vec!["install", "-y"];
        args.extend_from_slice(DOCKER_PACKAGES);
        run_apt(runner, &args)?;
    }
    let groups = run_checked(runner, "id", &["-nG", &user])?;
    if !groups.split_whitespace().any(|group| group == "docker") {
        run_sudo(runner, "usermod", &["-aG", "docker", &user])?;
        ui::warn("Docker group membership takes effect after the next login");
    }
    Ok(())
}

/// Where the Docker apt repository is registered.
pub(crate) const DOCKER_SOURCES: &str = "/etc/apt/sources.list.d/docker.sources";

/// Install Docker's signing key and register its apt repository for this
/// Ubuntu release, after checking that the host is amd64 Ubuntu. The
/// caller refreshes the package lists. Returns whether this call created
/// [`DOCKER_SOURCES`], which did not exist before.
pub(crate) fn register_docker_repo(
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
) -> Result<bool> {
    let os_release = host
        .read(Path::new("/etc/os-release"))?
        .context("/etc/os-release is missing")?;
    let codename = parse_ubuntu_codename(&os_release)?;
    let arch = run_checked(runner, "dpkg", &["--print-architecture"])?;
    if arch.trim() != "amd64" {
        return Err(anyhow!(
            "Docker server setup requires dpkg architecture amd64"
        ));
    }
    run_sudo(
        runner,
        "install",
        &["-m", "0755", "-d", "/etc/apt/keyrings"],
    )?;
    let key_path = Path::new("/etc/apt/keyrings/docker.asc");
    let key = host.acquire_docker_key(runner)?;
    host.write_atomic(key_path, &key, 0o644)?;
    let source = format!(
        "Types: deb\nURIs: https://download.docker.com/linux/ubuntu\nSuites: {codename}\nComponents: stable\nArchitectures: amd64\nSigned-By: /etc/apt/keyrings/docker.asc\n"
    );
    let source_path = Path::new(DOCKER_SOURCES);
    let existing = host.read(source_path)?;
    if existing.as_deref() != Some(source.as_bytes()) {
        host.write_atomic(source_path, source.as_bytes(), 0o644)?;
    }
    Ok(existing.is_none())
}

fn rust_state(runner: &dyn CommandRunner) -> Result<RustState> {
    if runner.which("rustup").is_none() {
        return Ok(RustState::Absent);
    }
    let check = runner
        .run("rustup", &["check"])
        .context("installed rustup could not run `rustup check`")?;
    if !check.success {
        return Err(anyhow!(
            "installed rustup failed `rustup check`: {}",
            check.stderr.trim()
        ))
        .hint("repair rustup, then rerun server setup");
    }
    let text = check.stdout.to_ascii_lowercase();
    let recognized = text.lines().any(|line| {
        line.contains("update available")
            || line.contains("up to date")
            || line.contains("unchanged")
    });
    if !recognized {
        return Err(anyhow!("could not interpret `rustup check` output"))
            .hint("run `rustup check`, resolve its error, and retry");
    }
    let update = text
        .lines()
        .any(|line| line.contains("stable") && line.contains("update available"));
    let default = run_checked(runner, "rustup", &["default"])?;
    let stable = default
        .split_whitespace()
        .next()
        .is_some_and(|value| value.starts_with("stable"));
    Ok(match (update, stable) {
        (true, true) => RustState::UpdateAvailable,
        (true, false) | (false, false) => RustState::NonStable,
        (false, true) => RustState::CurrentStable,
    })
}

pub(crate) fn reconcile_rust(
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
    state: RustState,
) -> Result<()> {
    let state = if state == RustState::Probe {
        rust_state(runner)?
    } else {
        state
    };
    match state {
        RustState::Absent => {
            let init = host.acquire_verified_rustup_init()?;
            let path = init.to_str().context("rustup-init path is not UTF-8")?;
            let execution = run_checked(
                runner,
                path,
                &[
                    "-y",
                    "--profile",
                    "default",
                    "--default-toolchain",
                    "stable",
                ],
            );
            let cleanup = fs::remove_file(&init);
            if let Err(error) = cleanup
                && error.kind() != std::io::ErrorKind::NotFound
            {
                ui::warn(&format!(
                    "could not remove temporary rustup-init `{}`: {error}",
                    init.display()
                ));
            }
            execution?;
        }
        RustState::UpdateAvailable => {
            run_checked(runner, "rustup", &["update", "stable"])?;
        }
        RustState::NonStable => {
            run_checked(runner, "rustup", &["update", "stable"])?;
            run_checked(runner, "rustup", &["default", "stable"])?;
        }
        RustState::CurrentStable => {}
        RustState::Probe => unreachable!(),
    }
    Ok(())
}

fn acquire_rustup_init() -> Result<PathBuf> {
    const TARGET: &str = "x86_64-unknown-linux-gnu";
    let base = format!("https://static.rust-lang.org/rustup/dist/{TARGET}/rustup-init");
    let agent = crate::tools::github_agent();
    let checksum = agent
        .get(&format!("{base}.sha256"))
        .call()
        .context("could not download the rustup-init checksum")?
        .into_body()
        .read_to_string()?;
    let expected = parse_rustup_checksum(&checksum)?;
    let mut response = agent
        .get(&base)
        .call()
        .context("could not download rustup-init")?;
    let mut file = stage_rustup_init()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = response.body_mut().as_reader().read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count])?;
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(anyhow!("rustup-init sha256 mismatch"))
            .hint("retry; the bootstrap download was not executed");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    let (_, path) = file
        .keep()
        .context("could not retain verified rustup-init")?;
    Ok(path)
}

/// Create the temporary file that holds the downloaded rustup-init.
///
/// rustup chooses its mode from the file stem of `argv[0]` and runs the
/// installer only when that stem starts with `rustup-init` or
/// `rustup-setup`; any other name is treated as a toolchain proxy. The
/// random suffix appended to this prefix is alphanumeric, so the stem keeps
/// the prefix.
fn stage_rustup_init() -> Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix("rustup-init-")
        .tempfile()
        .context("could not create a temporary rustup-init file")
}

/// Extract the sha256 digest from a `rustup-init.sha256` file: exactly one
/// `sha256sum`-style entry, either a bare digest or a digest and filename.
/// The filename must be `rustup-init`, optionally marked as binary mode with
/// a leading `*` and then prefixed with a single `./`; upstream serves
/// `<hex> *./rustup-init`.
fn parse_rustup_checksum(checksum: &str) -> Result<&str> {
    let lines: Vec<_> = checksum
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let [line] = lines.as_slice() else {
        return Err(anyhow!(
            "rustup-init checksum file must contain exactly one entry"
        ))
        .hint("retry server setup; rustup-init was not downloaded or run");
    };
    let fields: Vec<_> = line.split_whitespace().collect();
    let expected = match fields.as_slice() {
        [digest] => *digest,
        [digest, filename] if names_rustup_init(filename) => *digest,
        _ => {
            return Err(anyhow!(
                "rustup-init checksum entry is malformed or names another file"
            ))
            .hint("retry server setup; rustup-init was not downloaded or run");
        }
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("rustup-init checksum is malformed"))
            .hint("retry server setup; rustup-init was not downloaded or run");
    }
    Ok(expected)
}

/// Whether a checksum entry's filename is `rustup-init`, allowing one leading
/// binary-mode `*` followed by one leading `./`.
fn names_rustup_init(filename: &str) -> bool {
    let name = filename.strip_prefix('*').unwrap_or(filename);
    let name = name.strip_prefix("./").unwrap_or(name);
    name == "rustup-init"
}

fn merge_json(
    host: &dyn ServerHost,
    path: &Path,
    managed: &[(&str, Value)],
    mode: u32,
) -> Result<()> {
    let existing = host.read(path)?;
    let mut object = match existing.as_deref() {
        None => Map::new(),
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .with_context(|| format!("`{}` is malformed JSON", path.display()))?
            .as_object()
            .cloned()
            .context("managed settings JSON must be an object")?,
    };
    for (key, value) in managed {
        object.insert((*key).to_string(), value.clone());
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(object))?;
    bytes.push(b'\n');
    let semantically_equal = existing
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        == serde_json::from_slice::<Value>(&bytes).ok();
    if !semantically_equal {
        host.write_atomic(path, &bytes, mode)?;
    }
    Ok(())
}

pub(crate) fn reconcile_r_profile(host: &dyn ServerHost) -> Result<()> {
    let path = Path::new(R_PROFILE);
    let existing = host.read(path)?.unwrap_or_default();
    let text = std::str::from_utf8(&existing).context("Rprofile.site is not UTF-8")?;
    if text.lines().any(|line| line == R_REPOSITORY_LINE) {
        return Ok(());
    }
    let mut bytes = existing;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(R_REPOSITORY_LINE.as_bytes());
    bytes.push(b'\n');
    host.write_atomic(path, &bytes, 0o644)
}

pub(crate) fn merge_rstudio_preferences(host: &dyn ServerHost) -> Result<()> {
    merge_json(
        host,
        Path::new(RSTUDIO_PREFS),
        &[
            ("insert_native_pipe_operator", json!(true)),
            ("save_workspace", json!("never")),
            ("load_workspace", json!("never")),
            ("rainbow_parentheses", json!(true)),
            ("rainbow_fenced_divs", json!(true)),
        ],
        0o644,
    )
}

pub(crate) fn merge_code_server_settings(host: &dyn ServerHost, user: &str) -> Result<()> {
    validate_user(user)?;
    let home = host.home_dir(user)?;
    let quarto = server_path(&home, ".local/bin/quarto")?;
    if !host.exists(&quarto) {
        return Err(anyhow!(
            "managed Quarto launcher `{}` is missing",
            quarto.display()
        ))
        .hint("install Quarto for this user before enabling code-server");
    }
    merge_json(
        host,
        &server_path(&home, ".local/share/code-server/User/settings.json")?,
        &[("quarto.path", json!(quarto))],
        0o644,
    )
}

pub(crate) fn reconcile_code_server(
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let user = host.current_user()?;
    validate_user(&user)?;
    merge_code_server_settings(host, &user)?;
    let unit = format!("code-server@{user}");
    run_sudo(runner, "systemctl", &["enable", "--now", &unit])?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn reconcile_server(
    host: &dyn ServerHost,
    runner: &dyn CommandRunner,
    docker: DockerState,
    rust: RustState,
) -> Result<()> {
    reconcile_docker(host, runner, docker)?;
    reconcile_rust(host, runner, rust)?;
    merge_rstudio_preferences(host)?;
    reconcile_code_server(host, runner)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use anyhow::Result;
    use serde_json::{Value, json};

    use super::{
        CodeServerPackage, DOCKER_PACKAGES, DockerState, RustState, ServerAction, ServerHost,
        SystemServerHost, merge_code_server_settings, merge_rstudio_preferences,
        prepare_code_server, reconcile_code_server, reconcile_docker, reconcile_r_profile,
        reconcile_rust, reconcile_server,
    };
    use crate::install::test_support::{FakeRunner, PanicFetcher};
    use crate::install::{DownloadedPackage, InstallCtx, apt_get_command_line};
    use crate::setup::{Profile, SetupDeps, execute, finish, steps, summary};
    use crate::tools::Os;

    const USER: &str = "analyst";
    const HOME: &str = "/home/analyst";
    const QUARTO: &str = "/home/analyst/.local/bin/quarto";

    #[derive(Default)]
    struct FakeHost {
        files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
        writes: Mutex<Vec<(PathBuf, Vec<u8>, u32)>>,
        existing: Vec<PathBuf>,
        rustup_acquisitions: Mutex<usize>,
        rustup_acquisition_fails: bool,
        docker_key_acquisitions: Mutex<usize>,
        docker_key_acquisition_fails: bool,
        code_server_acquisitions: Mutex<usize>,
    }

    impl FakeHost {
        fn with_file(self, path: &str, text: &str) -> Self {
            self.files
                .lock()
                .unwrap()
                .insert(PathBuf::from(path), text.as_bytes().to_vec());
            self
        }

        fn with_existing(mut self, path: &str) -> Self {
            self.existing.push(PathBuf::from(path));
            self
        }

        /// A snapshot of every file the host holds.
        fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            self.files.lock().unwrap().clone()
        }

        /// Every atomic write, in order: path, bytes, and mode.
        fn writes(&self) -> Vec<(PathBuf, Vec<u8>, u32)> {
            self.writes.lock().unwrap().clone()
        }

        fn rustup_acquisitions(&self) -> usize {
            *self.rustup_acquisitions.lock().unwrap()
        }

        fn docker_key_acquisitions(&self) -> usize {
            *self.docker_key_acquisitions.lock().unwrap()
        }

        fn json(&self, path: &str) -> Value {
            serde_json::from_slice(self.files().get(Path::new(path)).expect("file exists"))
                .expect("valid JSON")
        }
    }

    impl ServerHost for FakeHost {
        fn current_user(&self) -> Result<String> {
            Ok(USER.to_string())
        }

        fn home_dir(&self, user: &str) -> Result<PathBuf> {
            assert_eq!(user, USER);
            Ok(PathBuf::from(HOME))
        }

        fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
            Ok(self.files.lock().unwrap().get(path).cloned())
        }

        fn exists(&self, path: &Path) -> bool {
            self.existing.iter().any(|item| item == path)
                || self.files.lock().unwrap().contains_key(path)
        }

        fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            self.writes
                .lock()
                .unwrap()
                .push((path.to_path_buf(), bytes.to_vec(), mode));
            Ok(())
        }

        fn acquire_verified_rustup_init(&self) -> Result<PathBuf> {
            *self.rustup_acquisitions.lock().unwrap() += 1;
            if self.rustup_acquisition_fails {
                Err(anyhow::anyhow!("could not acquire verified rustup-init"))
            } else {
                Ok(PathBuf::from("/verified-download/rustup-init"))
            }
        }

        fn acquire_docker_key(
            &self,
            _runner: &dyn crate::install::CommandRunner,
        ) -> Result<Vec<u8>> {
            *self.docker_key_acquisitions.lock().unwrap() += 1;
            if self.docker_key_acquisition_fails {
                Err(anyhow::anyhow!("Docker key download interrupted"))
            } else {
                Ok(b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nfixture\n-----END PGP PUBLIC KEY BLOCK-----\n".to_vec())
            }
        }

        fn acquire_code_server_package(&self) -> Result<CodeServerPackage> {
            *self.code_server_acquisitions.lock().unwrap() += 1;
            Ok(CodeServerPackage {
                package: DownloadedPackage::at(PathBuf::from(CODE_SERVER_DEB)),
                version: "4.141.0".to_string(),
            })
        }
    }

    const CODE_SERVER_DEB: &str = "/verified-download/hpds-code-server-abc.deb";
    const CODE_SERVER_FIELDS: &str =
        "dpkg-deb --field /verified-download/hpds-code-server-abc.deb Package Architecture Version";

    #[test]
    fn code_server_on_path_needs_no_package() {
        let host = FakeHost::default();
        let runner = FakeRunner::default().on_path("code-server");
        assert!(
            prepare_code_server(&host, &runner)
                .expect("nothing to prepare")
                .is_none()
        );
        assert_eq!(*host.code_server_acquisitions.lock().unwrap(), 0);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn missing_code_server_installs_the_verified_package_by_absolute_path() {
        let host = FakeHost::default();
        let install = apt_get_command_line(&["install", "-y", CODE_SERVER_DEB]);
        let runner = FakeRunner::default()
            .with_output(
                CODE_SERVER_FIELDS,
                "Package: code-server\nArchitecture: amd64\nVersion: 4.141.0\n",
            )
            .with_output(&install, "");
        super::run(&ServerAction::InstallCodeServer, &host, &runner).expect("install code-server");
        assert_eq!(runner.calls(), [CODE_SERVER_FIELDS.to_string(), install]);
        assert_eq!(*host.code_server_acquisitions.lock().unwrap(), 1);
    }

    #[test]
    fn a_code_server_package_with_the_wrong_identity_is_never_installed() {
        for fields in [
            "Package: code-server-insiders\nArchitecture: amd64\nVersion: 4.141.0\n",
            "Package: code-server\nArchitecture: arm64\nVersion: 4.141.0\n",
            "Package: code-server\nArchitecture: amd64\nVersion: 4.140.0\n",
            "Package: code-server\nArchitecture: amd64\n",
        ] {
            let host = FakeHost::default();
            let runner = FakeRunner::default().with_output(CODE_SERVER_FIELDS, fields);
            let err = super::run(&ServerAction::InstallCodeServer, &host, &runner)
                .expect_err("mismatched package");
            assert_eq!(runner.calls(), [CODE_SERVER_FIELDS], "{fields:?}");
            let rendered = crate::ui::render_error(&err, false);
            assert!(rendered.contains("hint:"), "{rendered}");
        }
    }

    #[test]
    fn code_server_never_runs_an_install_script() {
        let text = super::describe(&ServerAction::InstallCodeServer);
        assert!(!text.contains("install.sh"), "{text}");
        assert!(!text.contains("code-server.dev"), "{text}");
    }

    #[test]
    fn registering_the_docker_repo_writes_key_and_source_without_apt() {
        let host =
            FakeHost::default().with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let runner = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "");
        assert!(super::register_docker_repo(&host, &runner).expect("register the repo"));
        assert!(
            !super::register_docker_repo(&host, &runner).expect("register again"),
            "a source file that already exists was not created by this call"
        );
        assert_eq!(
            runner.calls()[..2],
            [
                "dpkg --print-architecture",
                "sudo install -m 0755 -d /etc/apt/keyrings"
            ]
        );
        assert!(runner.calls().iter().all(|call| !call.contains("apt-get")));
        let written: Vec<_> = host.writes().into_iter().map(|(path, _, _)| path).collect();
        assert_eq!(
            written[..2],
            [
                PathBuf::from("/etc/apt/keyrings/docker.asc"),
                PathBuf::from("/etc/apt/sources.list.d/docker.sources")
            ]
        );
    }

    #[test]
    fn code_server_release_resolves_the_amd64_package_from_github() {
        use std::collections::HashMap;

        use crate::tools::test_support::FixtureServer;

        let path = "/repos/coder/code-server/releases/latest";
        let server = FixtureServer::serve(HashMap::from([(
            path.to_string(),
            include_bytes!("../../tests/fixtures/releases/code-server-latest.json").to_vec(),
        )]));
        let asset = crate::install::latest_release_asset(
            &crate::tools::github_agent(),
            &server.base_url,
            &super::CODE_SERVER,
        )
        .expect("resolve the recorded release");
        assert_eq!(asset.version, "4.141.0");
        assert_eq!(
            asset.url,
            "https://github.com/coder/code-server/releases/download/v4.141.0/code-server_4.141.0_amd64.deb"
        );
        assert_eq!(
            asset.digest.as_deref(),
            Some("sha256:f384beb4f3ba207e0bca27d0dbd36dd825d5aa909d78c6d4baa8703eaf6b77ba")
        );
        assert_eq!(server.hits(), vec![path]);
    }

    /// A GitHub release body for code-server 4.141.0 whose amd64 package
    /// is served from `base` with `digest`.
    fn local_code_server_release(base: &str, digest: Option<&str>) -> Vec<u8> {
        let mut asset = json!({
            "name": "code-server_4.141.0_amd64.deb",
            "browser_download_url": format!(
                "{base}/coder/code-server/releases/download/v4.141.0/code-server_4.141.0_amd64.deb"
            ),
        });
        if let Some(digest) = digest {
            asset["digest"] = json!(digest);
        }
        serde_json::to_vec(&json!({
            "tag_name": "v4.141.0",
            "draft": false,
            "prerelease": false,
            "published_at": "2026-10-08T08:02:54Z",
            "assets": [asset],
        }))
        .unwrap()
    }

    /// Serve a release and its package from one local server, then
    /// acquire code-server from it. The release body needs the server's
    /// address, so the routes are filled in once it is bound.
    fn acquire_from_local(
        package: &[u8],
        digest: impl Fn(&[u8]) -> Option<String>,
    ) -> (Result<CodeServerPackage>, Vec<String>) {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        let release_path = "/repos/coder/code-server/releases/latest";
        let deb_path =
            "/coder/code-server/releases/download/v4.141.0/code-server_4.141.0_amd64.deb";
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let routes: HashMap<String, Vec<u8>> = HashMap::from([
            (
                release_path.to_string(),
                local_code_server_release(&base, digest(package).as_deref()),
            ),
            (deb_path.to_string(), package.to_vec()),
        ]);
        let hits = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let server = Arc::clone(&server);
            let hits = Arc::clone(&hits);
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    let url = request.url().to_string();
                    hits.lock().unwrap().push(url.clone());
                    let response = match routes.get(&url) {
                        Some(body) => tiny_http::Response::from_data(body.clone()),
                        None => tiny_http::Response::from_data(Vec::new()).with_status_code(404),
                    };
                    let _ = request.respond(response);
                }
            })
        };
        let result = super::acquire_code_server_from(&crate::tools::github_agent(), &base);
        server.unblock();
        handle.join().unwrap();
        let hits = hits.lock().unwrap().clone();
        (result, hits)
    }

    fn github_digest(bytes: &[u8]) -> Option<String> {
        Some(format!(
            "sha256:{}",
            crate::tools::test_support::sha256_hex_of(bytes)
        ))
    }

    #[test]
    fn code_server_package_downloads_to_a_private_deb_matching_githubs_digest() {
        let (result, hits) = acquire_from_local(b"fixture deb bytes", github_digest);
        let acquired = result.expect("download the package");
        assert_eq!(acquired.version, "4.141.0");
        let path = acquired.package.path().to_path_buf();
        assert_eq!(path.extension().and_then(|v| v.to_str()), Some("deb"));
        assert!(
            path.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|name| name.starts_with("hpds-code-server-"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fixture deb bytes");
        assert_eq!(hits.len(), 2, "{hits:?}");
        drop(acquired);
        assert!(
            !path.exists(),
            "the temporary package is removed when dropped"
        );
    }

    #[test]
    fn code_server_package_with_a_wrong_or_missing_digest_is_rejected() {
        let wrong = |_: &[u8]| github_digest(b"something else");
        let (result, _) = acquire_from_local(b"fixture deb bytes", wrong);
        let err = result.err().expect("digest mismatch");
        assert!(err.to_string().contains("sha256"), "{err:#}");

        let (result, hits) = acquire_from_local(b"fixture deb bytes", |_| None);
        let err = result.err().expect("missing digest");
        assert!(err.to_string().contains("sha256"), "{err:#}");
        assert_eq!(
            hits.len(),
            1,
            "nothing is downloaded without a digest: {hits:?}"
        );
    }

    fn docker_install_line() -> String {
        let mut args = vec!["install", "-y"];
        args.extend_from_slice(DOCKER_PACKAGES);
        apt_get_command_line(&args)
    }

    fn mutation_calls(runner: &FakeRunner) -> Vec<String> {
        runner
            .calls()
            .iter()
            .filter(|call| {
                [
                    "apt-get -o DPkg::Lock::Timeout=600 install",
                    "usermod",
                    "rustup update",
                    "rustup default",
                    "rustup-init",
                ]
                .iter()
                .any(|needle| call.contains(needle))
            })
            .cloned()
            .collect()
    }

    #[test]
    fn docker_absent_installs_official_repo_plugins_then_adds_injected_user() {
        let host =
            FakeHost::default().with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let runner = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output(&apt_get_command_line(&["update"]), "")
            .with_output(&docker_install_line(), "")
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "");

        reconcile_docker(&host, &runner, DockerState::Absent).expect("install Docker");

        let calls = runner.calls();
        assert!(
            calls.contains(&"sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 apt-get -o DPkg::Lock::Timeout=600 install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin".to_string()),
            "{calls:?}"
        );
        let install = calls
            .iter()
            .position(|call| call.contains("docker-ce docker-ce-cli containerd.io"))
            .expect("package install");
        let group = calls
            .iter()
            .position(|call| call == "sudo usermod -aG docker analyst")
            .expect("group mutation");
        assert!(install < group, "{calls:?}");
        assert!(
            calls.iter().all(|call| !call.contains("$USER")),
            "{calls:?}"
        );
        let sources = host
            .files()
            .get(Path::new("/etc/apt/sources.list.d/docker.sources"))
            .cloned()
            .expect("Docker apt source");
        let sources = String::from_utf8(sources).expect("UTF-8 source");
        assert!(sources.contains("https://download.docker.com/linux/ubuntu"));
        assert!(sources.contains("Signed-By: /etc/apt/keyrings/docker.asc"));
    }

    #[test]
    fn a_failed_refresh_removes_only_a_docker_source_this_run_created() {
        for (existing, removed) in [(false, true), (true, false)] {
            let mut host = FakeHost::default()
                .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
            if existing {
                host = host.with_file(super::DOCKER_SOURCES, "# older source\n");
            }
            let runner = FakeRunner::default()
                .with_output("dpkg --print-architecture", "amd64")
                .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
                .with_failure(&apt_get_command_line(&["update"]), "E: not signed")
                .with_output("sudo rm -f /etc/apt/sources.list.d/docker.sources", "");
            let err = reconcile_docker(&host, &runner, DockerState::Absent)
                .expect_err("the refresh fails");
            assert!(err.to_string().contains("E: not signed"), "{err:#}");
            let calls = runner.calls();
            assert_eq!(
                calls.contains(&"sudo rm -f /etc/apt/sources.list.d/docker.sources".to_string()),
                removed,
                "existing {existing}: {calls:#?}"
            );
        }
    }

    #[test]
    fn docker_current_packages_only_add_missing_group_and_then_noop() {
        let host = FakeHost::default();
        let outside = FakeRunner::default()
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "");
        reconcile_docker(&host, &outside, DockerState::Current).expect("add group");
        assert_eq!(
            mutation_calls(&outside),
            ["sudo usermod -aG docker analyst"]
        );

        let current = FakeRunner::default().with_output("id -nG analyst", "analyst sudo docker");
        reconcile_docker(&host, &current, DockerState::Current).expect("already current");
        assert!(mutation_calls(&current).is_empty());
        assert!(host.writes().is_empty());
    }

    #[test]
    fn invalid_docker_host_stops_before_write_or_install() {
        for (arch, distro) in [
            ("s390x", "ID=ubuntu\nVERSION_CODENAME=noble\n"),
            ("amd64", "ID=debian\nVERSION_CODENAME=bookworm\n"),
            ("amd64", "ID=ubuntu\n"),
        ] {
            let host = FakeHost::default().with_file("/etc/os-release", distro);
            let runner = FakeRunner::default().with_output("dpkg --print-architecture", arch);
            reconcile_docker(&host, &runner, DockerState::Absent)
                .expect_err("unsupported Docker host");
            assert!(host.writes().is_empty());
            assert!(mutation_calls(&runner).is_empty());
        }
    }

    #[test]
    fn rust_absent_outdated_nonstable_and_current_are_reconciled() {
        let cases = [
            (
                RustState::Absent,
                vec![
                    "/verified-download/rustup-init -y --profile default --default-toolchain stable",
                ],
            ),
            (RustState::UpdateAvailable, vec!["rustup update stable"]),
            (
                RustState::NonStable,
                vec!["rustup update stable", "rustup default stable"],
            ),
            (RustState::CurrentStable, vec![]),
        ];
        for (state, mutations) in cases {
            let host = FakeHost::default();
            let mut runner = FakeRunner::default();
            for command in &mutations {
                runner = runner.with_output(command, "");
            }
            reconcile_rust(&host, &runner, state).expect("reconcile stable Rust");
            assert_eq!(mutation_calls(&runner), mutations, "{state:?}");
        }

        let host = FakeHost::default();
        let current = FakeRunner::default();
        reconcile_rust(&host, &current, RustState::CurrentStable).expect("second run");
        assert!(mutation_calls(&current).is_empty());
    }

    #[test]
    fn absent_rust_acquires_verified_bootstrap_before_executing_its_path() {
        let host = FakeHost::default();
        let runner = FakeRunner::default().with_output(
            "/verified-download/rustup-init -y --profile default --default-toolchain stable",
            "",
        );
        reconcile_rust(&host, &runner, RustState::Absent).expect("install stable Rust");
        assert_eq!(host.rustup_acquisitions(), 1);
        assert_eq!(
            runner.calls(),
            ["/verified-download/rustup-init -y --profile default --default-toolchain stable"]
        );
        assert!(
            runner
                .calls()
                .iter()
                .all(|call| !call.starts_with("rustup-init "))
        );
    }

    #[test]
    fn rust_bootstrap_acquisition_failure_prevents_execution() {
        let host = FakeHost {
            rustup_acquisition_fails: true,
            ..FakeHost::default()
        };
        let runner = FakeRunner::default();
        reconcile_rust(&host, &runner, RustState::Absent)
            .expect_err("failed acquisition stops bootstrap");
        assert_eq!(host.rustup_acquisitions(), 1);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn nightly_only_probe_installs_and_selects_stable() {
        let host = FakeHost::default();
        let runner = FakeRunner::default()
            .on_path("rustup")
            .with_output(
                "rustup check",
                "nightly-x86_64-unknown-linux-gnu - Up to date",
            )
            .with_output(
                "rustup default",
                "nightly-x86_64-unknown-linux-gnu (default)",
            )
            .with_output("rustup update stable", "")
            .with_output("rustup default stable", "");
        reconcile_rust(&host, &runner, RustState::Probe).expect("select stable");
        assert_eq!(
            runner.calls(),
            [
                "rustup check",
                "rustup default",
                "rustup update stable",
                "rustup default stable"
            ]
        );
    }

    #[test]
    fn interrupted_docker_key_download_preserves_managed_files_and_retry_reacquires() {
        let failed = FakeHost {
            docker_key_acquisition_fails: true,
            ..FakeHost::default()
                .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n")
        };
        let runner = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "");
        reconcile_docker(&failed, &runner, DockerState::Absent).expect_err("interrupted key");
        assert!(failed.writes().is_empty());
        assert_eq!(failed.docker_key_acquisitions(), 1);

        let retry =
            FakeHost::default().with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let runner = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output(&apt_get_command_line(&["update"]), "")
            .with_output(&docker_install_line(), "")
            .with_output("id -nG analyst", "analyst docker");
        reconcile_docker(&retry, &runner, DockerState::Absent).expect("retry Docker setup");
        assert_eq!(retry.docker_key_acquisitions(), 1);
        assert!(
            retry
                .files()
                .contains_key(Path::new("/etc/apt/keyrings/docker.asc"))
        );
    }

    #[test]
    fn r_profile_managed_line_is_added_once() {
        let host = FakeHost::default().with_file("/etc/R/Rprofile.site", "# existing\n");
        reconcile_r_profile(&host).expect("first merge");
        reconcile_r_profile(&host).expect("second merge");
        let text =
            String::from_utf8(host.files()[Path::new("/etc/R/Rprofile.site")].clone()).unwrap();
        assert_eq!(text.matches("packagemanager.posit.co").count(), 1);
        assert_eq!(host.writes().len(), 1);
    }

    #[test]
    fn malformed_rustup_check_is_an_error_without_mutation() {
        let host = FakeHost::default();
        let runner = FakeRunner::default()
            .on_path("rustup")
            .with_output("rustup check", "unexpected output");
        reconcile_rust(&host, &runner, RustState::Probe).expect_err("unknown rustup output");
        assert!(mutation_calls(&runner).is_empty());
    }

    #[test]
    fn rstudio_preferences_merge_exact_policy_atomically_and_idempotently() {
        let path = "/etc/rstudio/rstudio-prefs.json";
        let host = FakeHost::default().with_file(
            path,
            r#"{"unrelated":{"keep":true},"save_workspace":"ask"}"#,
        );
        merge_rstudio_preferences(&host).expect("merge preferences");
        assert_eq!(
            host.json(path),
            json!({
                "unrelated": {"keep": true},
                "insert_native_pipe_operator": true,
                "save_workspace": "never",
                "load_workspace": "never",
                "rainbow_parentheses": true,
                "rainbow_fenced_divs": true
            })
        );
        assert_eq!(host.writes().len(), 1);
        assert_eq!(host.writes()[0].2, 0o644);
        merge_rstudio_preferences(&host).expect("second merge");
        assert_eq!(host.writes().len(), 1, "unchanged file is not rewritten");
    }

    #[test]
    fn malformed_or_non_object_preferences_fail_without_write() {
        for text in ["not json", "[]", "null"] {
            let host = FakeHost::default().with_file("/etc/rstudio/rstudio-prefs.json", text);
            merge_rstudio_preferences(&host).expect_err("invalid preferences");
            assert!(host.writes().is_empty());
        }
    }

    #[test]
    fn code_server_merges_managed_quarto_and_enables_the_legacy_unit() {
        let settings = "/home/analyst/.local/share/code-server/User/settings.json";
        let host = FakeHost::default().with_existing(QUARTO).with_file(
            settings,
            r#"{"editor.fontSize":15,"quarto.path":"/old/quarto"}"#,
        );
        let runner = FakeRunner::default()
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_code_server(&host, &runner).expect("configure code-server");
        assert_eq!(
            host.json(settings),
            json!({"editor.fontSize": 15, "quarto.path": QUARTO})
        );
        assert_eq!(host.writes().len(), 1);
        assert_eq!(
            runner.calls(),
            ["sudo systemctl enable --now code-server@analyst"]
        );

        merge_code_server_settings(&host, USER).expect("second settings merge");
        assert_eq!(
            host.writes().len(),
            1,
            "unchanged settings are not rewritten"
        );
    }

    #[test]
    fn missing_managed_quarto_fails_before_settings_or_service() {
        let host = FakeHost::default();
        let runner = FakeRunner::default();
        reconcile_code_server(&host, &runner).expect_err("managed Quarto launcher is required");
        assert!(host.writes().is_empty());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn complete_server_reconciliation_is_stable_on_second_run() {
        let host = FakeHost::default()
            .with_existing(QUARTO)
            .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let first = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output(&apt_get_command_line(&["update"]), "")
            .with_output(&docker_install_line(), "")
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "")
            .with_output(
                "/verified-download/rustup-init -y --profile default --default-toolchain stable",
                "",
            )
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_server(&host, &first, DockerState::Absent, RustState::Absent)
            .expect("first reconciliation");

        let files_after_first = host.files();
        let writes_after_first = host.writes().len();
        let second = FakeRunner::default()
            .with_output("id -nG analyst", "analyst sudo docker")
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_server(
            &host,
            &second,
            DockerState::Current,
            RustState::CurrentStable,
        )
        .expect("second reconciliation");

        assert_eq!(host.files(), files_after_first);
        assert_eq!(host.writes().len(), writes_after_first);
        assert!(mutation_calls(&second).is_empty());
    }

    #[derive(Default)]
    struct PatternRunner {
        calls: Mutex<Vec<String>>,
        fail_install: bool,
    }
    impl PatternRunner {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl crate::install::CommandRunner for PatternRunner {
        fn which(&self, _program: &str) -> Option<PathBuf> {
            None
        }
        fn run(&self, program: &str, args: &[&str]) -> Result<crate::install::CommandOutput> {
            let command = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push(command);
            let fail = self.fail_install
                && program == "sudo"
                && args.first() == Some(&"install")
                && !args.contains(&"-d");
            Ok(crate::install::CommandOutput {
                success: !fail,
                stdout: String::new(),
                stderr: if fail {
                    "interrupted".into()
                } else {
                    String::new()
                },
            })
        }
    }

    #[test]
    fn privileged_atomic_write_installs_temp_then_renames_and_cleans_failures() {
        let runner = PatternRunner::default();
        let host = SystemServerHost::new(&runner);
        host.write_atomic(Path::new("/etc/hpds-test.json"), b"new", 0o644)
            .expect("publish");
        let calls = runner.calls();
        assert!(calls[0].starts_with("sudo install -D -m 0644 "));
        assert!(calls[0].contains("/etc/.hpds-test.json.hpds-"));
        assert!(calls[1].starts_with("sudo mv -f /etc/.hpds-test.json.hpds-"));

        let failed = PatternRunner {
            fail_install: true,
            ..PatternRunner::default()
        };
        let host = SystemServerHost::new(&failed);
        host.write_atomic(Path::new("/etc/hpds-test.json"), b"replacement", 0o644)
            .expect_err("install failure");
        let calls = failed.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].starts_with("sudo install"));
        assert!(calls[1].starts_with("sudo rm -f"));
        assert!(!calls.iter().any(|call| call.starts_with("sudo mv")));
    }

    #[test]
    fn real_server_steps_continue_after_docker_failure_and_finish_reports_it() {
        let host = FakeHost::default()
            .with_existing(QUARTO)
            .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let docker = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_failure(
                "sudo install -m 0755 -d /etc/apt/keyrings",
                "permission denied",
            );
        let git_setup = || Ok(());
        let deps = SetupDeps {
            install: InstallCtx {
                os: Os::Linux,
                yes: true,
                verbose: false,
                pin: None,
                plan_approved: true,
                sudo_approved: std::cell::Cell::new(false),
                runner: &docker,
                fetcher: &PanicFetcher,
            },
            git_setup: &git_setup,
            installer_lookup: &crate::install::registry::find,
            server_host: &host,
        };
        let selected: Vec<_> = steps(Profile::Server)
            .iter()
            .filter(|step| matches!(step.title, "Docker Engine" | "RStudio preferences"))
            .copied()
            .collect();
        assert_eq!(
            selected.iter().map(|step| step.title).collect::<Vec<_>>(),
            ["Docker Engine", "RStudio preferences"]
        );

        let results = execute(&selected, &deps, true);
        assert!(
            host.files()
                .contains_key(Path::new("/etc/rstudio/rstudio-prefs.json"))
        );
        let failures: Vec<_> = results
            .iter()
            .filter(|result| result.error.is_some())
            .map(|result| result.title)
            .collect();
        assert_eq!(failures, ["Docker Engine"]);
        let text = summary(&results);
        assert!(text.contains("✗ Docker Engine"), "{text}");
        assert!(text.contains("✓ RStudio preferences"), "{text}");
        let error = finish(&results, None).expect_err("one failed step fails setup");
        assert!(error.to_string().contains("1 of 2"), "{error:#}");
    }

    /// rustup picks its mode from the file stem of `argv[0]`: only a stem
    /// starting with `rustup-init` or `rustup-setup` runs the installer, and
    /// any other unrecognized name is treated as a toolchain proxy.
    fn rustup_runs_as_installer(path: &Path) -> bool {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with("rustup-init") || stem.starts_with("rustup-setup"))
    }

    #[test]
    fn staged_rustup_init_has_a_name_rustup_runs_as_the_installer() {
        let staged = super::stage_rustup_init().unwrap();
        let path = staged.path();
        assert!(
            rustup_runs_as_installer(path),
            "rustup would treat `{}` as a toolchain proxy",
            path.display()
        );
    }

    const RUSTUP_DIGEST: &str = "dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71";

    #[test]
    fn rustup_checksum_accepts_the_upstream_form_and_sha256sum_variants() {
        let d = RUSTUP_DIGEST;
        let accepted = [
            // Verbatim body of the published rustup-init.sha256.
            format!("{d} *./rustup-init\n"),
            format!("{d}  ./rustup-init"),
            format!("{d} *rustup-init"),
            format!("{d}  rustup-init"),
            d.to_string(),
        ];
        let failures: Vec<String> = accepted
            .iter()
            .filter_map(|text| match super::parse_rustup_checksum(text) {
                Ok(digest) if digest == d => None,
                Ok(other) => Some(format!("{text:?} parsed to {other:?}")),
                Err(error) => Some(format!("{text:?} was rejected: {error}")),
            })
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn rustup_checksum_rejects_other_files_and_malformed_digests() {
        let d = RUSTUP_DIGEST;
        let rejected = [
            format!("{d} *./rustup-init.exe"),
            format!("{d}  ../rustup-init"),
            format!("{d}  ././rustup-init"),
            format!("{d} *./other/rustup-init"),
            format!("{d}  rustup-init\n{d}  rustup-init\n"),
            "dda7234360b7f578 *./rustup-init".to_string(),
            format!("{} *./rustup-init", "z".repeat(64)),
        ];
        let accepted: Vec<&String> = rejected
            .iter()
            .filter(|text| super::parse_rustup_checksum(text).is_ok())
            .collect();
        assert!(accepted.is_empty(), "wrongly accepted: {accepted:#?}");
    }

    #[test]
    fn rustup_checksum_rejections_say_what_to_do_next() {
        let d = RUSTUP_DIGEST;
        let rejected = [
            format!("{d}  rustup-init\n{d}  rustup-init\n"),
            format!("{d} *./rustup-init.exe"),
            format!("{} *./rustup-init", "z".repeat(64)),
        ];
        for text in &rejected {
            let err = super::parse_rustup_checksum(text).unwrap_err();
            let rendered = crate::ui::render_error(&err, false);
            assert!(rendered.contains("hint:"), "{text:?}: {rendered}");
            assert!(
                rendered.contains("retry server setup"),
                "{text:?}: {rendered}"
            );
        }
    }
}
