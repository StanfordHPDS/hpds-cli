//! Integration tests for `hpds use` and the pipeline component.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;

fn hpds() -> Command {
    Command::cargo_bin("hpds").expect("hpds binary should build")
}

/// Run `hpds use pipeline` in a fresh temp project dir and return the dir.
fn use_pipeline(kind: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["use", "pipeline", "--kind", kind])
        .current_dir(tmp.path())
        .assert()
        .success();
    tmp
}

/// `true` when a usable `make` is on PATH; tests that drive make skip
/// (with a note) when it is absent.
fn make_available() -> bool {
    std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_make(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("make")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("make should run")
}

// --- `hpds use` command surface -----------------------------------------

#[test]
fn use_without_component_lists_components_and_exits_0() {
    hpds()
        .arg("use")
        .assert()
        .success()
        .stdout(predicate::str::contains("pipeline"));
}

#[test]
fn use_listing_includes_a_description_for_each_component() {
    let assert = hpds().arg("use").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let pipeline_line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("pipeline"))
        .unwrap_or_else(|| panic!("listing has a pipeline line: {stdout}"));
    assert!(
        pipeline_line.trim_start().len() > "pipeline".len() + 2,
        "pipeline line carries a description: {pipeline_line:?}"
    );
}

#[test]
fn unknown_component_exits_2_with_hint_listing_valid_names() {
    hpds()
        .args(["use", "frobnicate"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("error:")
                .and(predicate::str::contains("frobnicate"))
                .and(predicate::str::contains("hint:"))
                .and(predicate::str::contains("pipeline")),
        );
}

// --- pipeline: kind resolution -------------------------------------------

#[test]
fn pipeline_invalid_kind_fails_and_lists_valid_kinds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["use", "pipeline", "--kind", "cmake"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cmake")
                .and(predicate::str::contains("make"))
                .and(predicate::str::contains("targets"))
                .and(predicate::str::contains("both")),
        );
}

#[test]
fn pipeline_without_kind_non_interactive_fails_with_actionable_error() {
    // stdin is not a TTY under the test harness, so the prompt must refuse
    // with a pointer at the flag-driven path instead of hanging.
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["use", "pipeline"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("non-interactively").and(predicate::str::contains("hint:")),
        );
}

// --- pipeline: make -------------------------------------------------------

#[test]
fn pipeline_kind_make_creates_makefile_with_starter_targets() {
    let tmp = use_pipeline("make");
    let makefile = fs::read_to_string(tmp.path().join("Makefile")).expect("Makefile created");
    assert!(makefile.contains("clean:"), "{makefile}");
    assert!(makefile.contains("deep-clean:"), "{makefile}");
    assert!(makefile.contains("sync-mtimes:"), "{makefile}");
    assert!(
        !tmp.path().join("_targets.R").exists(),
        "make kind must not create _targets.R"
    );
}

#[test]
fn pipeline_make_kind_makefile_parses_with_make() {
    if !make_available() {
        eprintln!("skipping: `make` not found on PATH");
        return;
    }
    let tmp = use_pipeline("make");
    let out = run_make(tmp.path(), &["-n", "clean"]);
    assert!(
        out.status.success(),
        "`make -n clean` should parse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// --- pipeline: targets -----------------------------------------------------

#[test]
fn pipeline_kind_targets_creates_targets_file_with_starter_pipeline() {
    let tmp = use_pipeline("targets");
    let targets = fs::read_to_string(tmp.path().join("_targets.R")).expect("_targets.R created");
    assert!(targets.contains("library(targets)"), "{targets}");
    assert!(targets.contains("library(tarchetypes)"), "{targets}");
    assert!(targets.contains("tar_target("), "{targets}");
    assert!(targets.contains("renv"), "renv note present: {targets}");
    assert!(
        !tmp.path().join("Makefile").exists(),
        "targets kind must not create a Makefile"
    );
}

// --- pipeline: both ---------------------------------------------------------

#[test]
fn pipeline_kind_both_creates_targets_setup_and_tar_make_makefile() {
    let tmp = use_pipeline("both");
    assert!(tmp.path().join("_targets.R").exists());
    let makefile = fs::read_to_string(tmp.path().join("Makefile")).expect("Makefile created");
    assert!(
        makefile.contains("Rscript -e 'targets::tar_make()'"),
        "{makefile}"
    );
}

#[test]
fn pipeline_both_kind_default_target_runs_tar_make() {
    if !make_available() {
        eprintln!("skipping: `make` not found on PATH");
        return;
    }
    let tmp = use_pipeline("both");
    // `make -n` prints the default target's recipe without running it.
    let out = run_make(tmp.path(), &["-n"]);
    assert!(
        out.status.success(),
        "`make -n` should parse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("targets::tar_make()"),
        "default target runs the pipeline: {stdout}"
    );
    let clean = run_make(tmp.path(), &["-n", "clean"]);
    assert!(
        clean.status.success(),
        "`make -n clean` should parse: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
}

// --- conflict / --force semantics -------------------------------------------

#[test]
fn pipeline_skips_a_conflicting_file_and_shows_a_diff_preview() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("Makefile"), "user: edits\n").expect("seed Makefile");
    let assert = hpds()
        .args(["use", "pipeline", "--kind", "make"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("--force"), "points at --force: {stdout}");
    assert!(
        stdout.contains("-user: edits"),
        "diff preview shows the existing line: {stdout}"
    );
    // The user's file is untouched.
    assert_eq!(
        fs::read_to_string(tmp.path().join("Makefile")).unwrap(),
        "user: edits\n"
    );
}

#[test]
fn pipeline_force_overwrites_a_conflicting_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("Makefile"), "user: edits\n").expect("seed Makefile");
    hpds()
        .args(["use", "pipeline", "--kind", "make", "--force"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let makefile = fs::read_to_string(tmp.path().join("Makefile")).unwrap();
    assert!(makefile.contains("clean:"), "{makefile}");
}

#[test]
fn pipeline_is_idempotent_on_a_second_run() {
    let tmp = use_pipeline("make");
    hpds()
        .args(["use", "pipeline", "--kind", "make"])
        .current_dir(tmp.path())
        .assert()
        .success();
}
