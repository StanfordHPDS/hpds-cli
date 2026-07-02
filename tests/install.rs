//! Integration tests for `hpds install` argument handling and the
//! installer registry errors.
//!
//! No test here runs a real installer: the registry has no concrete
//! installers yet, so every known tool stops at the "lands soon" error.

use assert_cmd::Command;
use predicates::prelude::*;

fn hpds() -> Command {
    Command::cargo_bin("hpds").expect("hpds binary should build")
}

#[test]
fn install_requires_a_tool_argument() {
    hpds()
        .arg("install")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage:").and(predicate::str::contains("<TOOL>")));
}

#[test]
fn install_unknown_tool_exits_2_and_lists_known_tools() {
    hpds()
        .args(["install", "frobnicate"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("error:")
                .and(predicate::str::contains("frobnicate"))
                .and(predicate::str::contains("quarto"))
                .and(predicate::str::contains("duckdb")),
        );
}

#[test]
fn install_known_but_unimplemented_tool_exits_2_with_lands_soon() {
    for tool in ["r", "quarto", "uv", "gh", "rig", "tinytex", "duckdb"] {
        hpds()
            .args(["install", tool])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(
                predicate::str::contains("lands soon")
                    .and(predicate::str::contains(tool))
                    .and(predicate::str::contains("hint:")),
            );
    }
}

#[test]
fn install_accepts_version_and_yes_flags() {
    // The flags must parse; the tool itself is still unimplemented, so the
    // run stops at the registry with the "lands soon" error.
    hpds()
        .args(["install", "quarto", "--version", "1.8.27", "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("lands soon"));
}

#[test]
fn install_accepts_short_yes_flag() {
    hpds()
        .args(["install", "-y", "quarto"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("lands soon"));
}
