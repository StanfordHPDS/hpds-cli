use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::install::CommandRunner;
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
            "install Docker Engine from download.docker.com/linux/ubuntu (docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin) and add <user> to the docker group"
        }
        ServerAction::Rust => "install or update stable Rust with verified rustup",
        ServerAction::RProfile => {
            "ensure the Posit Package Manager CRAN repository line is present once"
        }
        ServerAction::RstudioPreferences => {
            "merge RStudio preferences: insert_native_pipe_operator=true save_workspace=never load_workspace=never rainbow_parentheses=true rainbow_fenced_divs=true"
        }
        ServerAction::InstallCodeServer => {
            "download the code-server installer to a secure temporary file and run it"
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
        ServerAction::InstallCodeServer => install_code_server(runner),
        ServerAction::CodeServer => reconcile_code_server(host, runner),
    }
}

fn install_code_server(runner: &dyn CommandRunner) -> Result<()> {
    if runner.which("code-server").is_some() {
        return Ok(());
    }
    let script = tempfile::Builder::new()
        .prefix("hpds-code-server-install-")
        .suffix(".sh")
        .tempfile()
        .context("could not create a temporary code-server installer")?;
    let path = script
        .path()
        .to_str()
        .context("temporary path is not UTF-8")?;
    run_checked(
        runner,
        "curl",
        &["-fsSL", "https://code-server.dev/install.sh", "-o", path],
    )?;
    run_sudo(runner, "sh", &[path])?;
    Ok(())
}

const DOCKER_PACKAGES: &[&str] = &[
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
}

pub(crate) struct SystemServerHost<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> SystemServerHost<'a> {
    pub(crate) fn new(runner: &'a dyn CommandRunner) -> Self {
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
            let publish = parent.join(unique);
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
        let source_path = Path::new("/etc/apt/sources.list.d/docker.sources");
        if host.read(source_path)?.as_deref() != Some(source.as_bytes()) {
            host.write_atomic(source_path, source.as_bytes(), 0o644)?;
        }
        run_sudo(runner, "apt-get", &["update"])?;
        let mut args = vec!["install", "-y"];
        args.extend_from_slice(DOCKER_PACKAGES);
        run_sudo(runner, "apt-get", &args)?;
    }
    let groups = run_checked(runner, "id", &["-nG", &user])?;
    if !groups.split_whitespace().any(|group| group == "docker") {
        run_sudo(runner, "usermod", &["-aG", "docker", &user])?;
        ui::warn("Docker group membership takes effect after the next login");
    }
    Ok(())
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
    let lines: Vec<_> = checksum
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let [line] = lines.as_slice() else {
        return Err(anyhow!(
            "rustup-init checksum file must contain exactly one entry"
        ));
    };
    let fields: Vec<_> = line.split_whitespace().collect();
    let expected = match fields.as_slice() {
        [digest] => *digest,
        [digest, filename] if filename.trim_start_matches('*') == "rustup-init" => *digest,
        _ => {
            return Err(anyhow!(
                "rustup-init checksum entry is malformed or names another file"
            ));
        }
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("rustup-init checksum is malformed"));
    }
    let mut response = agent
        .get(&base)
        .call()
        .context("could not download rustup-init")?;
    let mut file = tempfile::Builder::new()
        .prefix("hpds-rustup-init-")
        .tempfile()
        .context("could not create a temporary rustup-init file")?;
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
    let quarto = home.join(".local/bin/quarto");
    if !host.exists(&quarto) {
        return Err(anyhow!(
            "managed Quarto launcher `{}` is missing",
            quarto.display()
        ))
        .hint("install Quarto for this user before enabling code-server");
    }
    merge_json(
        host,
        &home.join(".local/share/code-server/User/settings.json"),
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
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use serde_json::{Value, json};

    use super::{
        DockerState, RustState, ServerHost, SystemServerHost, merge_code_server_settings,
        merge_rstudio_preferences, reconcile_code_server, reconcile_docker, reconcile_r_profile,
        reconcile_rust, reconcile_server,
    };
    use crate::install::InstallCtx;
    use crate::install::test_support::{FakeRunner, PanicFetcher};
    use crate::setup::{Profile, SetupDeps, execute, finish, steps, summary};
    use crate::tools::Os;

    const USER: &str = "analyst";
    const HOME: &str = "/home/analyst";
    const QUARTO: &str = "/home/analyst/.local/bin/quarto";

    #[derive(Default)]
    struct FakeHost {
        files: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
        writes: RefCell<Vec<(PathBuf, Vec<u8>, u32)>>,
        existing: RefCell<Vec<PathBuf>>,
        rustup_acquisitions: RefCell<usize>,
        rustup_acquisition_fails: bool,
        docker_key_acquisitions: RefCell<usize>,
        docker_key_acquisition_fails: bool,
    }

    impl FakeHost {
        fn with_file(self, path: &str, text: &str) -> Self {
            self.files
                .borrow_mut()
                .insert(PathBuf::from(path), text.as_bytes().to_vec());
            self
        }

        fn with_existing(self, path: &str) -> Self {
            self.existing.borrow_mut().push(PathBuf::from(path));
            self
        }

        fn json(&self, path: &str) -> Value {
            serde_json::from_slice(
                self.files
                    .borrow()
                    .get(Path::new(path))
                    .expect("file exists"),
            )
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
            Ok(self.files.borrow().get(path).cloned())
        }

        fn exists(&self, path: &Path) -> bool {
            self.existing.borrow().iter().any(|item| item == path)
                || self.files.borrow().contains_key(path)
        }

        fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), bytes.to_vec());
            self.writes
                .borrow_mut()
                .push((path.to_path_buf(), bytes.to_vec(), mode));
            Ok(())
        }

        fn acquire_verified_rustup_init(&self) -> Result<PathBuf> {
            *self.rustup_acquisitions.borrow_mut() += 1;
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
            *self.docker_key_acquisitions.borrow_mut() += 1;
            if self.docker_key_acquisition_fails {
                Err(anyhow::anyhow!("Docker key download interrupted"))
            } else {
                Ok(b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nfixture\n-----END PGP PUBLIC KEY BLOCK-----\n".to_vec())
            }
        }
    }

    fn mutation_calls(runner: &FakeRunner) -> Vec<String> {
        runner
            .calls
            .borrow()
            .iter()
            .filter(|call| {
                [
                    "apt-get install",
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
            .with_output("sudo apt-get update", "")
            .with_output(
                "sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
                "",
            )
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "");

        reconcile_docker(&host, &runner, DockerState::Absent).expect("install Docker");

        let calls = runner.calls.borrow();
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
            .files
            .borrow()
            .get(Path::new("/etc/apt/sources.list.d/docker.sources"))
            .cloned()
            .expect("Docker apt source");
        let sources = String::from_utf8(sources).expect("UTF-8 source");
        assert!(sources.contains("https://download.docker.com/linux/ubuntu"));
        assert!(sources.contains("Signed-By: /etc/apt/keyrings/docker.asc"));
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
        assert!(host.writes.borrow().is_empty());
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
            assert!(host.writes.borrow().is_empty());
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
        assert_eq!(*host.rustup_acquisitions.borrow(), 1);
        assert_eq!(
            *runner.calls.borrow(),
            ["/verified-download/rustup-init -y --profile default --default-toolchain stable"]
        );
        assert!(
            runner
                .calls
                .borrow()
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
        assert_eq!(*host.rustup_acquisitions.borrow(), 1);
        assert!(runner.calls.borrow().is_empty());
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
            *runner.calls.borrow(),
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
        assert!(failed.writes.borrow().is_empty());
        assert_eq!(*failed.docker_key_acquisitions.borrow(), 1);

        let retry =
            FakeHost::default().with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let runner = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output("sudo apt-get update", "")
            .with_output("sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin", "")
            .with_output("id -nG analyst", "analyst docker");
        reconcile_docker(&retry, &runner, DockerState::Absent).expect("retry Docker setup");
        assert_eq!(*retry.docker_key_acquisitions.borrow(), 1);
        assert!(
            retry
                .files
                .borrow()
                .contains_key(Path::new("/etc/apt/keyrings/docker.asc"))
        );
    }

    #[test]
    fn r_profile_managed_line_is_added_once() {
        let host = FakeHost::default().with_file("/etc/R/Rprofile.site", "# existing\n");
        reconcile_r_profile(&host).expect("first merge");
        reconcile_r_profile(&host).expect("second merge");
        let text =
            String::from_utf8(host.files.borrow()[Path::new("/etc/R/Rprofile.site")].clone())
                .unwrap();
        assert_eq!(text.matches("packagemanager.posit.co").count(), 1);
        assert_eq!(host.writes.borrow().len(), 1);
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
        assert_eq!(host.writes.borrow().len(), 1);
        assert_eq!(host.writes.borrow()[0].2, 0o644);
        merge_rstudio_preferences(&host).expect("second merge");
        assert_eq!(
            host.writes.borrow().len(),
            1,
            "unchanged file is not rewritten"
        );
    }

    #[test]
    fn malformed_or_non_object_preferences_fail_without_write() {
        for text in ["not json", "[]", "null"] {
            let host = FakeHost::default().with_file("/etc/rstudio/rstudio-prefs.json", text);
            merge_rstudio_preferences(&host).expect_err("invalid preferences");
            assert!(host.writes.borrow().is_empty());
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
        assert_eq!(host.writes.borrow().len(), 1);
        assert_eq!(
            *runner.calls.borrow(),
            ["sudo systemctl enable --now code-server@analyst"]
        );

        merge_code_server_settings(&host, USER).expect("second settings merge");
        assert_eq!(
            host.writes.borrow().len(),
            1,
            "unchanged settings are not rewritten"
        );
    }

    #[test]
    fn missing_managed_quarto_fails_before_settings_or_service() {
        let host = FakeHost::default();
        let runner = FakeRunner::default();
        reconcile_code_server(&host, &runner).expect_err("managed Quarto launcher is required");
        assert!(host.writes.borrow().is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn complete_server_reconciliation_is_stable_on_second_run() {
        let host = FakeHost::default()
            .with_existing(QUARTO)
            .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let first = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output("sudo apt-get update", "")
            .with_output(
                "sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
                "",
            )
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "")
            .with_output(
                "/verified-download/rustup-init -y --profile default --default-toolchain stable",
                "",
            )
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_server(&host, &first, DockerState::Absent, RustState::Absent)
            .expect("first reconciliation");

        let files_after_first = host.files.borrow().clone();
        let writes_after_first = host.writes.borrow().len();
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

        assert_eq!(*host.files.borrow(), files_after_first);
        assert_eq!(host.writes.borrow().len(), writes_after_first);
        assert!(mutation_calls(&second).is_empty());
    }

    #[derive(Default)]
    struct PatternRunner {
        calls: RefCell<Vec<String>>,
        fail_install: bool,
    }
    impl crate::install::CommandRunner for PatternRunner {
        fn which(&self, _program: &str) -> Option<PathBuf> {
            None
        }
        fn run(&self, program: &str, args: &[&str]) -> Result<crate::install::CommandOutput> {
            let command = format!("{program} {}", args.join(" "));
            self.calls.borrow_mut().push(command);
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
        let calls = runner.calls.borrow();
        assert!(calls[0].starts_with("sudo install -D -m 0644 "));
        assert!(calls[0].contains("/etc/.hpds-test.json.hpds-"));
        assert!(calls[1].starts_with("sudo mv -f /etc/.hpds-test.json.hpds-"));
        drop(calls);

        let failed = PatternRunner {
            fail_install: true,
            ..PatternRunner::default()
        };
        let host = SystemServerHost::new(&failed);
        host.write_atomic(Path::new("/etc/hpds-test.json"), b"replacement", 0o644)
            .expect_err("install failure");
        let calls = failed.calls.borrow();
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

        let results = execute(&selected, &deps);
        assert!(
            host.files
                .borrow()
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
}
