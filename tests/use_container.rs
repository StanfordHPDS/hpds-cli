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

#[test]
fn docker_r_dockerfile_uses_lab_r_image_and_restores_renv() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "r"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created Dockerfile"));

    let text = sandbox.read("Dockerfile");
    assert!(text.contains("FROM stanfordhpds/r-renv"), "R image: {text}");
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    assert!(
        text.contains("packagemanager.posit.co"),
        "PPM configured: {text}"
    );
    assert!(
        text.contains(&sandbox.project_name()),
        "project name substituted: {text}"
    );
    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
}

#[test]
fn docker_python_dockerfile_uses_lab_python_image_and_syncs_uv() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "python"])
        .assert()
        .success();

    let text = sandbox.read("Dockerfile");
    assert!(
        text.contains("FROM stanfordhpds/python-uv"),
        "python image: {text}"
    );
    assert!(text.contains("uv sync"), "uv sync step: {text}");
    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
}

#[test]
fn docker_both_languages_dockerfile_uses_base_image_with_both_restores() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "docker", "--language", "both"])
        .assert()
        .success();

    let text = sandbox.read("Dockerfile");
    assert!(
        text.contains("FROM stanfordhpds/base"),
        "base image: {text}"
    );
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    assert!(text.contains("uv sync"), "uv sync step: {text}");
    assert_dockerfile_well_formed(&sandbox.path("Dockerfile"));
}

#[test]
fn apptainer_r_def_bootstraps_lab_r_image_and_restores_renv() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "apptainer", "--language", "r"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created container.def"));

    let text = sandbox.read("container.def");
    assert_def_well_formed(&text);
    assert!(
        text.contains("From: stanfordhpds/r-renv"),
        "R image: {text}"
    );
    assert!(text.contains("renv::restore()"), "renv restore: {text}");
    assert!(
        text.contains("packagemanager.posit.co"),
        "PPM configured: {text}"
    );
}

#[test]
fn apptainer_python_def_bootstraps_lab_python_image_and_syncs_uv() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "apptainer", "--language", "python"])
        .assert()
        .success();

    let text = sandbox.read("container.def");
    assert_def_well_formed(&text);
    assert!(
        text.contains("From: stanfordhpds/python-uv"),
        "python image: {text}"
    );
    assert!(text.contains("uv sync"), "uv sync step: {text}");
}

#[test]
fn apptainer_both_languages_def_bootstraps_base_image_with_both_restores() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "apptainer", "--language", "both"])
        .assert()
        .success();

    let text = sandbox.read("container.def");
    assert_def_well_formed(&text);
    assert!(
        text.contains("From: stanfordhpds/base"),
        "base image: {text}"
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
            .contains("FROM stanfordhpds/r-renv"),
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
            .contains("FROM stanfordhpds/r-renv"),
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
fn invalid_kind_errors_listing_the_valid_values() {
    let sandbox = Sandbox::new();
    sandbox
        .use_container(&["--kind", "podman", "--language", "r"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("podman")
                .and(predicate::str::contains("docker"))
                .and(predicate::str::contains("apptainer"))
                .and(predicate::str::contains("both")),
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
