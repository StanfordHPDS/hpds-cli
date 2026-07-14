//! Integration tests for `hpds use readme` and `hpds use slurm`.
//!
//! Every test drives the real binary with assert_cmd inside a sandboxed
//! HOME + temp project directory, so the user's real config is never read
//! and nothing is written outside the sandbox.

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

    /// The project directory's basename: `hpds use` derives the project
    /// name (e.g. the Slurm job name) from it.
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
            .env("HPDS_GH", self.home.path().join("no-such-gh"))
            .arg("use")
            .args(args);
        cmd
    }
}

#[test]
fn use_without_a_component_lists_components_with_descriptions() {
    let sandbox = Sandbox::new();
    sandbox.hpds_use(&[]).assert().success().stdout(
        predicate::str::contains("readme")
            .and(predicate::str::contains("slurm"))
            .and(predicate::str::contains("lab-manual"))
            .and(predicate::str::contains("sbatch")),
    );
}

#[test]
fn unknown_component_fails_and_names_the_available_ones() {
    let sandbox = Sandbox::new();
    sandbox.hpds_use(&["frobnicate"]).assert().code(2).stderr(
        predicate::str::contains("`frobnicate` is not a template component")
            .and(predicate::str::contains("readme"))
            .and(predicate::str::contains("slurm")),
    );
}

#[test]
fn hpds_toml_component_creates_valid_metadata_without_a_language() {
    let sandbox = Sandbox::new();
    let config_dir = sandbox.home.path().join("hpds-config");
    fs::create_dir_all(&config_dir).expect("create user config directory");
    fs::write(
        config_dir.join("config.toml"),
        "[project]\nprimary-author = \"malcolm\"\n",
    )
    .expect("write user config");

    sandbox
        .hpds_use(&["hpds.toml"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created hpds.toml"));

    let text = sandbox.read("hpds.toml");
    let parsed: toml::Value = toml::from_str(&text).expect("generated hpds.toml is valid TOML");
    assert_eq!(parsed["project"]["status"].as_str(), Some("active"));
    assert_eq!(
        parsed["project"]["primary-author"].as_str(),
        Some("malcolm")
    );
    assert!(text.contains(&sandbox.project_name()), "{text}");
}

#[test]
fn hpds_toml_component_preserves_an_existing_file_without_force() {
    let sandbox = Sandbox::new();
    sandbox.write("hpds.toml", "# mine\n");

    sandbox
        .hpds_use(&["hpds.toml"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--force"))
        .stderr(predicate::str::contains("skipped hpds.toml"));
    assert_eq!(sandbox.read("hpds.toml"), "# mine\n");
}

#[test]
fn readme_in_a_detected_python_project_writes_readme_md() {
    let sandbox = Sandbox::new();
    sandbox.write("pyproject.toml", "[project]\nname = \"x\"\n");
    sandbox
        .hpds_use(&["readme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created README.md"));

    let text = sandbox.read("README.md");
    assert!(!sandbox.path("README.qmd").exists());
    for section in [
        "## Description",
        "## File structure",
        "## How to run",
        "## Dependencies",
    ] {
        assert!(text.contains(section), "README.md has `{section}`");
    }
    assert!(!text.contains("{{"), "no unrendered variables: {text}");
}

#[test]
fn readme_in_a_detected_r_project_writes_plain_readme_md() {
    let sandbox = Sandbox::new();
    sandbox.write("renv.lock", "{}\n");
    sandbox
        .hpds_use(&["readme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created README.md"));

    let text = sandbox.read("README.md");
    assert!(!sandbox.path("README.qmd").exists());
    assert!(
        !text.contains("quarto render") && !text.starts_with("---"),
        "README is plain Markdown with no render step: {text}"
    );
}

#[test]
fn language_flag_overrides_detection() {
    let sandbox = Sandbox::new();
    sandbox.write("renv.lock", "{}\n");
    sandbox
        .hpds_use(&["readme", "--language", "python"])
        .assert()
        .success();
    assert!(sandbox.path("README.md").exists());
    assert!(!sandbox.path("README.qmd").exists());
}

#[test]
fn undetectable_language_fails_and_says_to_pass_the_flag() {
    let sandbox = Sandbox::new();
    sandbox.hpds_use(&["readme"]).assert().code(1).stderr(
        predicate::str::contains("could not detect").and(predicate::str::contains("--language")),
    );
    assert!(!sandbox.path("README.md").exists());
    assert!(!sandbox.path("README.qmd").exists());
}

#[test]
fn existing_readme_is_skipped_with_a_diff_and_a_force_hint() {
    let sandbox = Sandbox::new();
    sandbox.write("README.md", "my notes\n");
    sandbox
        .hpds_use(&["readme", "--language", "python"])
        .assert()
        .success()
        .stdout(predicate::str::contains("-my notes").and(predicate::str::contains("--force")))
        .stderr(predicate::str::contains("skipped README.md"));
    assert_eq!(sandbox.read("README.md"), "my notes\n");
}

#[test]
fn force_overwrites_a_conflicting_readme() {
    let sandbox = Sandbox::new();
    sandbox.write("README.md", "my notes\n");
    sandbox
        .hpds_use(&["readme", "--language", "python", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("README.md"));
    let text = sandbox.read("README.md");
    assert!(text.contains("## Description"), "template applied: {text}");
}

#[test]
fn readme_rejects_the_kind_flag() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["readme", "--language", "python", "--kind", "make"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--kind"));
    assert!(!sandbox.path("README.md").exists());
}

#[test]
fn slurm_writes_the_script_and_logs_dir_with_a_sherlock_docs_link() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["slurm", "--language", "r"])
        .assert()
        .success();

    let script = sandbox.read("scripts/slurm_job.sh");
    assert!(
        script.contains(&format!("#SBATCH --job-name={}", sandbox.project_name())),
        "job name comes from the project directory: {script}"
    );
    assert!(
        script.contains("targets::tar_make()"),
        "r pipeline: {script}"
    );
    assert!(
        script.contains("https://www.sherlock.stanford.edu/docs/"),
        "script points to Sherlock documentation: {script}"
    );
    assert!(!sandbox.path("docs/slurm.md").exists());
    assert!(sandbox.path("logs/.gitkeep").exists());
}

#[test]
fn slurm_is_idempotent_on_a_second_run() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["slurm", "--language", "r"])
        .assert()
        .success();
    sandbox
        .hpds_use(&["slurm", "--language", "r"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "scripts/slurm_job.sh is already up to date",
        ));
}

#[cfg(unix)]
#[test]
fn rendered_slurm_script_is_executable_and_passes_bash_n() {
    use std::os::unix::fs::PermissionsExt;

    for language in ["r", "python"] {
        let sandbox = Sandbox::new();
        sandbox
            .hpds_use(&["slurm", "--language", language])
            .assert()
            .success();
        let script = sandbox.path("scripts/slurm_job.sh");
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits set: {mode:o}");
        let status = std::process::Command::new("bash")
            .arg("-n")
            .arg(&script)
            .status()
            .expect("bash is available on unix");
        assert!(status.success(), "bash -n fails for {language}");
    }
}

#[test]
fn quiet_suppresses_the_informational_output() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["readme", "--language", "python", "--quiet"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    assert!(sandbox.path("README.md").exists());
}
