//! Integration tests for `hpds use devcontainer`.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn hpds() -> Command {
    Command::cargo_bin("hpds").expect("hpds binary should build")
}

fn compatible_dockerfile() -> &'static str {
    "FROM debian:trixie-slim AS hpds-project\nFROM hpds-project AS hpds-dev\nFROM hpds-project AS hpds-analysis\nCMD [\"bash\"]\n"
}

struct Project {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Project {
    fn new(name: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(name);
        fs::create_dir(&root).expect("create project");
        fs::write(root.join("Dockerfile"), compatible_dockerfile()).expect("write Dockerfile");
        Self { _tmp: tmp, root }
    }

    fn run(&self, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut args = vec!["use", "devcontainer"];
        args.extend_from_slice(extra);
        hpds().args(args).current_dir(&self.root).assert()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{rel} should exist: {e}"))
    }
}

#[test]
fn devcontainer_is_registered_and_listed() {
    hpds()
        .args(["use", "--no-color"])
        .assert()
        .success()
        .stdout(predicate::str::contains("devcontainer"));
}

#[test]
fn generates_editor_neutral_configuration_for_the_actual_folder() {
    let project = Project::new("study-folder");
    project.run(&[]).success();

    let config: Value = serde_json::from_str(&project.read(".devcontainer/devcontainer.json"))
        .expect("generated configuration is JSON");
    assert_eq!(config["name"], "study-folder");
    assert_eq!(config["build"]["dockerfile"], "../Dockerfile");
    assert_eq!(config["build"]["context"], "..");
    assert_eq!(config["build"]["target"], "hpds-dev");
    assert_eq!(
        config["build"]["args"]["HPDS_PROJECT_DIR"],
        "/workspaces/study-folder"
    );
    assert_eq!(config["build"]["args"]["HPDS_UID"], "1000");
    assert_eq!(config["build"]["args"]["HPDS_GID"], "1000");
    assert_eq!(config["workspaceFolder"], "/workspaces/study-folder");
    assert_eq!(
        config["workspaceMount"],
        "source=${localWorkspaceFolder},target=/workspaces/study-folder,type=bind"
    );
    assert_eq!(config["remoteUser"], "hpds");
    assert_eq!(config["updateRemoteUserUID"], false);

    let readme = project.read(".devcontainer/README.md");
    for expected in ["Visual Studio Code", "Positron", "HPDS_UID", "HPDS_GID"] {
        assert!(
            readme.contains(expected),
            "README omits {expected}: {readme}"
        );
    }
}

#[test]
fn serializes_a_human_name_with_json_metacharacters() {
    let project = Project::new("portable-folder");
    hpds()
        .args([
            "init",
            "--yes",
            "--force",
            "--name",
            "quoted \"study\"",
            "--author",
            "malcolm",
            "--use",
            "devcontainer",
        ])
        .current_dir(&project.root)
        .assert()
        .success();
    let text = project.read(".devcontainer/devcontainer.json");
    let config: Value = serde_json::from_str(&text).expect("project name is safely JSON encoded");
    assert_eq!(config["name"], "quoted \"study\"");
    assert_eq!(config["workspaceFolder"], "/workspaces/portable-folder");
}

#[test]
fn requires_a_real_hpds_dev_stage_before_writing() {
    let invalid = [
        ("missing Dockerfile", None),
        ("wrong stage", Some("FROM debian AS development\n")),
        (
            "comment",
            Some("FROM debian\n# FROM hpds-project AS hpds-dev\n"),
        ),
        (
            "heredoc",
            Some("FROM debian\nRUN cat <<'EOF'\nFROM hpds-project AS hpds-dev\nEOF\n"),
        ),
        (
            "continued heredoc",
            Some("FROM debian\nRUN \\\ncat <<'EOF'\nFROM hpds-project AS hpds-dev\nEOF\n"),
        ),
        (
            "multiple heredocs",
            Some(
                "FROM debian\nRUN cat <<'FIRST' <<-SECOND\nFROM base AS hpds-dev\nFIRST\nFROM other AS hpds-dev\n\tSECOND\n",
            ),
        ),
    ];

    for (case, dockerfile) in invalid {
        let project = Project::new(case);
        match dockerfile {
            Some(contents) => fs::write(project.path("Dockerfile"), contents).unwrap(),
            None => fs::remove_file(project.path("Dockerfile")).unwrap(),
        }
        project.run(&[]).failure().stderr(
            predicate::str::contains("Dockerfile").and(predicate::str::contains("hpds-dev")),
        );
        assert!(
            !project.path(".devcontainer").exists(),
            "case {case} wrote files"
        );
    }
}

#[test]
fn rejects_checkout_names_that_cannot_form_a_mount_before_writing() {
    let project = Project::new("comma,folder");
    project
        .run(&[])
        .failure()
        .stderr(predicate::str::contains("folder name").and(predicate::str::contains("comma")));
    assert!(!project.path(".devcontainer").exists());
}

#[test]
fn refuses_competing_devcontainer_configurations_before_writing() {
    for competing in [
        ".devcontainer.json",
        ".devcontainer/alternate/devcontainer.json",
    ] {
        let project = Project::new("competing-config");
        let path = project.path(competing);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{}\n").unwrap();
        project
            .run(&[])
            .failure()
            .stderr(predicate::str::contains(competing));
        assert!(!project.path(".devcontainer/devcontainer.json").exists());
        assert!(!project.path(".devcontainer/README.md").exists());
        assert_eq!(fs::read_to_string(path).unwrap(), "{}\n");
    }
}

#[test]
fn reruns_report_unchanged_and_conflicts_require_force() {
    let project = Project::new("rerun");
    project.run(&[]).success();
    project
        .run(&[])
        .success()
        .stdout(predicate::str::contains("already up to date"));

    fs::write(project.path(".devcontainer/README.md"), "my instructions\n").unwrap();
    project
        .run(&[])
        .success()
        .stdout(predicate::str::contains("skipped").and(predicate::str::contains("--force")));
    assert_eq!(project.read(".devcontainer/README.md"), "my instructions\n");

    project.run(&["--force"]).success();
    assert_ne!(project.read(".devcontainer/README.md"), "my instructions\n");
}

#[test]
fn devcontainer_json_conflicts_require_force() {
    let project = Project::new("json-conflict");
    project.run(&[]).success();
    fs::write(
        project.path(".devcontainer/devcontainer.json"),
        "{\"name\":\"mine\"}\n",
    )
    .unwrap();

    project
        .run(&[])
        .success()
        .stdout(predicate::str::contains("skipped").and(predicate::str::contains("--force")));
    assert_eq!(
        project.read(".devcontainer/devcontainer.json"),
        "{\"name\":\"mine\"}\n"
    );

    project.run(&["--force"]).success();
    let config: Value = serde_json::from_str(&project.read(".devcontainer/devcontainer.json"))
        .expect("force restores generated JSON");
    assert_eq!(config["name"], "json-conflict");
}

#[test]
fn preserves_a_compatible_customized_dockerfile() {
    let project = Project::new("custom-dockerfile");
    let customized = format!(
        "{}RUN printf 'custom editor setup\\n' > /customized\n",
        compatible_dockerfile()
    );
    fs::write(project.path("Dockerfile"), &customized).unwrap();

    project.run(&[]).success();

    assert_eq!(project.read("Dockerfile"), customized);
}

#[test]
fn rejects_flags_that_do_not_apply_to_devcontainers() {
    for flags in [
        vec!["--kind", "docker"],
        vec!["--workflows", "lint"],
        vec!["--r-version", "4.4.3"],
    ] {
        let project = Project::new("unsupported-flags");
        project
            .run(&flags)
            .code(2)
            .stderr(predicate::str::contains(flags[0]));
        assert!(!project.path(".devcontainer").exists());
    }
}

#[test]
fn shipped_dockerfiles_keep_the_stage_contract_and_analysis_last() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for language in ["r", "python", "both"] {
        let path = manifest.join(format!("templates/container/docker/{language}/Dockerfile"));
        let text = fs::read_to_string(&path).unwrap();
        let project = text.find(" AS hpds-project").expect("shared project stage");
        let dev = text.find(" AS hpds-dev").expect("development stage");
        let analysis = text.find(" AS hpds-analysis").expect("analysis stage");
        let command = text.rfind("CMD ").expect("analysis command");
        assert!(
            project < dev && dev < analysis && analysis < command,
            "{}",
            path.display()
        );
    }
}
