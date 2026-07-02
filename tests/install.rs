//! Integration tests for `hpds install` argument handling, the installer
//! registry errors, and installed-tool detection.
//!
//! No test here mutates the machine: implemented installers are exercised
//! only up to their idempotent already-installed check, against fake
//! tools on a controlled `PATH`.

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
    for tool in ["r", "quarto", "tinytex"] {
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
fn install_rejects_a_version_pin_for_tools_that_cannot_pin() {
    // rig installs through package managers only, so `--version` is a
    // usage error (caught before anything touches the system).
    hpds()
        .args(["install", "rig", "--version", "0.8.1"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("does not support").and(predicate::str::contains("--version")),
        );
}

/// Detection through the real `SystemRunner` end to end: fake tools on a
/// controlled `PATH` make every implemented installer an idempotent
/// no-op, so nothing on the machine is touched.
#[cfg(unix)]
#[test]
fn install_is_a_no_op_when_the_tool_is_already_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let bin = tempfile::tempdir().expect("tempdir");
    let fake_tools = [
        ("uv", "uv 0.9.0 (39b688653 2025-10-07)", "0.9.0"),
        ("gh", "gh version 2.95.0 (2026-06-17)", "2.95.0"),
        ("rig", "RIG -- The R Installation Manager 0.8.1", "0.8.1"),
        ("duckdb", "v1.5.4 (Variegata) 08e34c447b", "1.5.4"),
    ];
    for (tool, version_output, _) in fake_tools {
        let path = bin.path().join(tool);
        std::fs::write(&path, format!("#!/bin/sh\necho '{version_output}'\n"))
            .expect("write fake tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("mark fake tool executable");
    }

    for (tool, _, version) in fake_tools {
        hpds()
            .args(["install", tool])
            .env("PATH", bin.path())
            .assert()
            .success()
            .stdout(
                predicate::str::contains("already installed")
                    .and(predicate::str::contains(tool))
                    .and(predicate::str::contains(version)),
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
