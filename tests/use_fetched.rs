//! Integration tests for the fetched components: `hpds use slides`,
//! `hpds use poster`, and `hpds use thesis`.
//!
//! Fetching needs the network, so these tests only drive the offline
//! surface: the component listing, flag rejection, and the pre-fetch
//! guard against an existing destination directory. The fetch itself is
//! covered by unit tests against shim tools and by the `online-tests`
//! feature.

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

    fn path(&self, rel: &str) -> PathBuf {
        self.project().join(rel)
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
}

#[test]
fn listing_includes_the_fetched_components_with_their_repos() {
    let sandbox = Sandbox::new();
    sandbox.hpds_use(&[]).assert().success().stdout(
        predicate::str::contains("slides")
            .and(predicate::str::contains("poster"))
            .and(predicate::str::contains("thesis"))
            .and(predicate::str::contains("StanfordHPDS/hpds-slides-theme"))
            .and(predicate::str::contains("StanfordHPDS/hpds-poster"))
            .and(predicate::str::contains(
                "StanfordHPDS/typst-stanford-thesis",
            )),
    );
}

#[test]
fn slides_reject_the_kind_flag_without_fetching() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["slides", "--kind", "fancy"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--kind"));
    assert!(
        !sandbox.path("hpds-slides-theme").exists(),
        "nothing was fetched"
    );
}

#[test]
fn poster_rejects_the_workflows_flag_without_fetching() {
    let sandbox = Sandbox::new();
    sandbox
        .hpds_use(&["poster", "--workflows", "lint"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--workflows"));
    assert!(!sandbox.path("hpds-poster").exists(), "nothing was fetched");
}

#[test]
fn existing_destination_errors_before_any_fetch_and_says_how_to_proceed() {
    let sandbox = Sandbox::new();
    fs::create_dir(sandbox.path("typst-stanford-thesis")).expect("pre-existing dest");
    sandbox.hpds_use(&["thesis"]).assert().code(1).stderr(
        predicate::str::contains("typst-stanford-thesis")
            .and(predicate::str::contains("hpds use thesis")),
    );
}
