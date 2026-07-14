//! Integration tests for `hpds use container`.
//!
//! Every test drives the real binary with assert_cmd inside a sandboxed
//! HOME + temp project directory, so the user's real config is never read
//! and nothing is written outside the sandbox.
//!
//! Dockerfiles are linted with hadolint when it is installed on this
//! machine; the structural assertions (correct `FROM` line per language,
//! restore steps present, `WORKDIR` set) always run so the tests are
//! meaningful without it. Apptainer `.def` files are validated
//! structurally (`Bootstrap:`/`From:`/`%post`).

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Isolated HOME + project directory for one test.
struct Sandbox {
    home: TempDir,
    project: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("temp HOME"),
            project: TempDir::new().expect("temp project dir"),
        }
    }

    fn project(&self) -> &Path {
        self.project.path()
    }

    /// The project directory's basename: `hpds use` substitutes it for
    /// `{{project}}` in the container files.
    fn project_name(&self) -> String {
        self.project()
            .file_name()
            .expect("temp dir has a name")
            .to_string_lossy()
            .into_owned()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.project().join(rel)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel))
            .unwrap_or_else(|e| panic!("read {rel} in the sandbox: {e}"))
    }

    fn write(&self, rel: &str, content: &str) {
        fs::write(self.path(rel), content).expect("write sandbox file");
    }

    /// `hpds use <args...>` run from the sandboxed project directory.
    fn hpds_use(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(self.project())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("HPDS_CONFIG_DIR", self.home.path().join("hpds-config"))
            // The production path resolves this from the R-hub API. Child
            // process injection keeps integration tests fully offline.
            .env("HPDS_R_VERSION", "4.6.1")
            .arg("use")
            .args(args);
        cmd
    }

    /// `hpds use container <args...>`.
    fn use_container(&self, args: &[&str]) -> Command {
        let mut cmd = self.hpds_use(&["container"]);
        cmd.args(args);
        cmd
    }
}

/// Lint a generated Dockerfile: hadolint when available, and structural
/// assertions always.
fn assert_dockerfile_well_formed(path: &Path) {
    if let Ok(out) = std::process::Command::new("hadolint").arg(path).output() {
        assert!(
            out.status.success(),
            "hadolint rejected {}:\n{}{}",
            path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    let text = fs::read_to_string(path).expect("Dockerfile exists");
    assert!(
        text.lines().any(|l| l.starts_with("FROM ")),
        "has a FROM instruction:\n{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("WORKDIR ")),
        "sets a WORKDIR:\n{text}"
    );
    assert!(
        !text.contains("sudo "),
        "no sudo inside containers:\n{text}"
    );
    assert!(!text.contains("{{"), "no unrendered variables:\n{text}");
}

/// Structural validation for an Apptainer definition file.
fn assert_def_well_formed(text: &str) {
    assert!(
        text.lines().any(|l| l.trim() == "Bootstrap: docker"),
        "bootstraps from docker:\n{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("From: ")),
        "has a From: line:\n{text}"
    );
    assert!(text.contains("%post"), "has a %post section:\n{text}");
    assert!(!text.contains("{{"), "no unrendered variables:\n{text}");
}

fn assert_pinned_hpds_docker_stage(text: &str) {
    assert!(
        text.contains(&format!(
            "FROM ghcr.io/stanfordhpds/hpds-cli:{} AS hpds",
            env!("CARGO_PKG_VERSION")
        )),
        "hpds stage is pinned to the generating release: {text}"
    );
    assert!(
        text.contains("COPY --from=hpds /hpds /usr/local/bin/hpds"),
        "hpds is copied from its registry image: {text}"
    );
    assert!(
        !text.contains("ARG HPDS"),
        "hpds pin is rendered, not a build arg: {text}"
    );
    assert!(
        !text.contains("hpds-cli:latest"),
        "hpds never uses a rolling tag: {text}"
    );
}

fn assert_pinned_hpds_apptainer_stage(text: &str) {
    assert!(
        text.contains(&format!(
            "From: ghcr.io/stanfordhpds/hpds-cli:{}",
            env!("CARGO_PKG_VERSION")
        )) && text.contains("Stage: hpds"),
        "pinned hpds source stage: {text}"
    );
    assert!(
        text.contains("%files from hpds") && text.contains("/hpds /usr/local/bin/hpds"),
        "hpds is copied between Apptainer stages: {text}"
    );
    assert!(
        !text.contains("hpds-cli:latest"),
        "hpds never uses a rolling tag: {text}"
    );
}

fn assert_pinned_uv_docker_source(text: &str) {
    assert!(
        text.contains("COPY --from=ghcr.io/astral-sh/uv:0.9.5 /uv /uvx /usr/local/bin/"),
        "uv and uvx come from the release-pinned official image: {text}"
    );
    assert!(
        !text.contains("hpds install uv"),
        "hpds is not an intermediary for installing uv: {text}"
    );
}

fn assert_pinned_uv_apptainer_stage(text: &str) {
    assert!(
        text.contains("From: ghcr.io/astral-sh/uv:0.9.5") && text.contains("Stage: uv"),
        "uv comes from the release-pinned official image: {text}"
    );
    assert!(
        text.contains("%files from uv")
            && text.contains("/uv /usr/local/bin/uv")
            && text.contains("/uvx /usr/local/bin/uvx"),
        "uv and uvx are copied from the official image: {text}"
    );
    assert!(
        !text.contains("hpds install uv"),
        "hpds is not an intermediary for installing uv: {text}"
    );
}

fn assert_generic_system_dependencies(text: &str) {
    assert!(
        text.contains("ca-certificates") && text.contains("git"),
        "generic certificate and Git dependencies are installed: {text}"
    );
    assert!(
        text.split_whitespace()
            .any(|token| token.trim_end_matches('\\') == "make"),
        "GNU Make is installed for the generated pipeline entry point: {text}"
    );
    for project_specific_library in ["libglpk40", "libwebpmux3"] {
        assert!(
            !text.contains(project_specific_library),
            "project-specific R library {project_specific_library} is not installed unconditionally: {text}"
        );
    }
}

fn assert_r_system_dependency_guidance(text: &str) {
    let sysreqs = text
        .find("renv::sysreqs")
        .expect("R container explains how to compute project system dependencies");
    let restore = text
        .find("renv::restore()")
        .expect("R container restores the locked R environment");
    assert!(
        sysreqs < restore,
        "system dependencies must be installed before renv restore: {text}"
    );
    assert!(
        text.contains("https://rstudio.github.io/renv/articles/docker.html#system-dependencies"),
        "R container links to the renv system-dependency guidance: {text}"
    );
}

#[test]
fn docker_r_dockerfile_pins_hpds_and_current_r_and_restores_renv() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "r"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created Dockerfile"));

    let text = sandbox.read("Dockerfile");
    assert_pinned_hpds_docker_stage(&text);
    assert_generic_system_dependencies(&text);
    assert_r_system_dependency_guidance(&text);
    assert!(
        text.contains("FROM rocker/r-ver:4.6.1"),
        "R image uses the resolved current release: {text}"
    );
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    for bootstrap in [
        "renv.lock",
        ".Rprofile",
        "renv/activate.R",
        "renv/settings.json",
    ] {
        assert!(
            text.contains(bootstrap),
            "copies the complete renv bootstrap ({bootstrap}): {text}"
        );
    }
    assert!(
        text.contains("RENV_CONFIG_CACHE_SYMLINKS=FALSE") && text.contains("/project/.cache/renv"),
        "renv cache is safe and project-rooted: {text}"
    );
    assert!(
        text.contains(&sandbox.project_name()),
        "project name substituted: {text}"
    );
    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
}

#[test]
fn docker_python_dockerfile_pins_hpds_and_lets_uv_provision_python() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "python"])
        .assert()
        .success();

    let text = sandbox.read("Dockerfile");
    assert_pinned_hpds_docker_stage(&text);
    assert_pinned_uv_docker_source(&text);
    assert_generic_system_dependencies(&text);
    assert!(
        text.contains("FROM debian:trixie-slim"),
        "Python is provisioned by uv rather than a versioned Python base: {text}"
    );
    assert!(
        text.contains("UV_PROJECT_ENVIRONMENT=/project/.venv"),
        "uv environment lives under /project: {text}"
    );
    assert!(
        text.contains("uv sync --locked --no-install-project"),
        "lockfile-first uv sync step: {text}"
    );
    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
}

#[test]
fn docker_both_languages_dockerfile_uses_current_r_and_uv_managed_python() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "both"])
        .assert()
        .success();

    let text = sandbox.read("Dockerfile");
    assert_pinned_hpds_docker_stage(&text);
    assert_pinned_uv_docker_source(&text);
    assert_generic_system_dependencies(&text);
    assert_r_system_dependency_guidance(&text);
    assert!(
        text.contains("FROM rocker/r-ver:4.6.1"),
        "mixed image starts from the resolved R runtime: {text}"
    );
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    assert!(text.contains("uv sync"), "uv sync step: {text}");
    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
}

#[test]
fn apptainer_r_def_uses_registry_hpds_stage_and_current_r() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "apptainer", "--language", "r"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created container.def"));

    let text = sandbox.read("container.def");
    assert_def_well_formed(&text);
    assert_pinned_hpds_apptainer_stage(&text);
    assert_generic_system_dependencies(&text);
    assert_r_system_dependency_guidance(&text);
    assert!(
        text.contains("From: rocker/r-ver:4.6.1") && text.contains("Stage: final"),
        "resolved R final stage: {text}"
    );
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    for bootstrap in [
        "renv.lock",
        ".Rprofile",
        "renv/activate.R",
        "renv/settings.json",
    ] {
        assert!(
            text.contains(bootstrap),
            "copies the complete renv bootstrap ({bootstrap}): {text}"
        );
    }
    assert!(
        text.contains("RENV_PATHS_LIBRARY=/project/R-renv-library")
            && text.contains("R_LIBS_SITE=/project/R-library")
            && text.contains("RENV_CONFIG_AUTOLOADER_ENABLED=FALSE"),
        "baked renv library is available outside the runtime project: {text}"
    );
}

#[test]
fn apptainer_python_def_uses_registry_hpds_stage_and_uv_managed_python() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "apptainer", "--language", "python"])
        .assert()
        .success();

    let text = sandbox.read("container.def");
    assert_def_well_formed(&text);
    assert_pinned_hpds_apptainer_stage(&text);
    assert_pinned_uv_apptainer_stage(&text);
    assert_generic_system_dependencies(&text);
    assert!(
        text.contains("From: debian:trixie-slim") && text.contains("Stage: final"),
        "uv provisions Python in the final stage: {text}"
    );
    assert!(
        text.contains("UV_PROJECT_ENVIRONMENT=/project/.venv"),
        "uv environment lives under /project: {text}"
    );
    assert!(
        text.contains("exec uv run \"$@\""),
        "runs commands through the locked uv environment: {text}"
    );
}

#[test]
fn apptainer_both_languages_def_uses_current_r_and_uv_managed_python() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "apptainer", "--language", "both"])
        .assert()
        .success();

    let text = sandbox.read("container.def");
    assert_def_well_formed(&text);
    assert_pinned_hpds_apptainer_stage(&text);
    assert_pinned_uv_apptainer_stage(&text);
    assert_generic_system_dependencies(&text);
    assert_r_system_dependency_guidance(&text);
    assert!(
        text.contains("From: rocker/r-ver:4.6.1"),
        "mixed image starts from the resolved R runtime: {text}"
    );
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    assert!(text.contains("uv sync"), "uv sync step: {text}");
}

#[test]
fn kind_both_writes_dockerfile_and_def() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "both", "--language", "r"])
        .assert()
        .success();

    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
    assert_def_well_formed(&sandbox.read("container.def"));
}

#[test]
fn language_is_detected_from_project_files_when_the_flag_is_omitted() {
    let sandbox = Sandbox::new();
    sandbox.write("renv.lock", "{}\n");
    sandbox
        .use_container(&["--kind", "docker"])
        .assert()
        .success();
    assert!(
        sandbox
            .read("Dockerfile")
            .contains("FROM rocker/r-ver:4.6.1"),
        "detected R from renv.lock"
    );
}

#[test]
fn second_run_is_idempotent() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "r"])
        .assert()
        .success();
    let first = sandbox.read("Dockerfile");

    sandbox
        .use_container(&["--kind", "docker", "--language", "r"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Dockerfile is already up to date"));
    assert_eq!(sandbox.read("Dockerfile"), first, "file untouched");
}

#[test]
fn existing_dockerfile_is_skipped_with_a_diff_and_a_force_hint() {
    let sandbox = Sandbox::new();
    sandbox.write("Dockerfile", "FROM my-own-image\n");

    sandbox
        .use_container(&["--kind", "docker", "--language", "r"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("-FROM my-own-image").and(predicate::str::contains("--force")),
        )
        .stderr(predicate::str::contains("skipped Dockerfile"));

    assert_eq!(
        sandbox.read("Dockerfile"),
        "FROM my-own-image\n",
        "user file untouched"
    );
}

#[test]
fn force_overwrites_an_existing_dockerfile() {
    let sandbox = Sandbox::new();
    sandbox.write("Dockerfile", "FROM my-own-image\n");

    sandbox
        .use_container(&["--kind", "docker", "--language", "r", "--force"])
        .assert()
        .success();
    assert!(
        sandbox
            .read("Dockerfile")
            .contains("FROM rocker/r-ver:4.6.1"),
        "template replaced the file"
    );
}

#[test]
fn missing_kind_fails_non_interactively_with_a_hint() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--language", "r"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("non-interactively").and(predicate::str::contains("hint:")),
        );
    assert!(!sandbox.path("Dockerfile").exists());
}

#[test]
fn undetectable_language_fails_and_says_to_pass_the_flag() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("could not detect")
                .and(predicate::str::contains("--language")),
        );
    assert!(!sandbox.path("Dockerfile").exists());
}

#[test]
fn invalid_kind_exits_2_listing_the_valid_values() {
    // An unknown --kind value is a usage error, exactly like an unknown
    // component name: both exit 2.
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "podman", "--language", "r"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("podman")
                .and(predicate::str::contains("docker"))
                .and(predicate::str::contains("apptainer"))
                .and(predicate::str::contains("both"))
                .and(predicate::str::contains("hint:")),
        );
    assert!(!sandbox.path("Dockerfile").exists());
}

#[test]
fn invalid_language_errors_listing_the_valid_values() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "rust"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rust").and(predicate::str::contains("python")));
    assert!(!sandbox.path("Dockerfile").exists());
}

#[test]
fn use_without_a_component_lists_container() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&[])
        .assert()
        .success()
        .stdout(predicate::str::contains("container"));
}

#[test]
fn unknown_component_error_names_container_among_the_available_ones() {
    let sandbox = Sandbox::new();
    sandbox.hpds_use(&["flying-car"]).assert().code(2).stderr(
        predicate::str::contains("`flying-car` is not a template component")
            .and(predicate::str::contains("container")),
    );
}
