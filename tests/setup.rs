//! Integration tests for `hpds setup`: `--plan` snapshots for both
//! profiles, non-interactive refusal without `--yes`, and the Linux-only
//! guard on the server profile.
//!
//! No test here may execute a real install step: every path exercised
//! either prints a plan, refuses before running anything, or errors on
//! the profile/OS check.

use assert_cmd::Command;
use predicates::prelude::*;

fn hpds() -> Command {
    Command::cargo_bin("hpds").expect("hpds binary should build")
}

fn plan_output(args: &[&str]) -> String {
    let assert = hpds().args(args).assert().success();
    String::from_utf8(assert.get_output().stdout.clone()).expect("plan output should be UTF-8")
}

#[test]
fn plan_dev_prints_the_numbered_plan_and_exits_zero() {
    insta::assert_snapshot!(plan_output(&["setup", "--plan"]));
}

#[test]
fn plan_server_prints_the_numbered_plan_on_every_os() {
    // `--plan --profile server` must work even off Linux so users can
    // inspect what server provisioning would do.
    insta::assert_snapshot!(plan_output(&["setup", "--plan", "--profile", "server"]));
}

#[test]
fn plan_never_prompts_even_without_a_terminal() {
    hpds()
        .args(["setup", "--plan", "--profile", "server"])
        .write_stdin("")
        .assert()
        .success();
}

#[test]
fn plan_stays_offline_and_describes_runtime_release_selection() {
    let output = plan_output(&["setup", "--plan"]);
    for tool in ["quarto", "uv", "gh", "togi"] {
        assert!(output.contains(tool), "missing {tool}: {output}");
    }
    assert!(output.contains("latest stable"), "{output}");
}

#[test]
fn server_plan_keeps_rstudio_discovery_offline() {
    let output = plan_output(&["setup", "--plan", "--profile", "server"]);
    assert!(output.contains("hpds install rstudio-server"), "{output}");
    assert!(output.contains("latest stable"), "{output}");
    assert!(!output.contains("rstudio.org/download"), "{output}");
    assert!(
        !output.contains("s3.amazonaws.com/rstudio-server"),
        "{output}"
    );
    assert!(!output.contains("2025.05.1-513"), "{output}");
    assert!(
        !output.contains("/tmp/rstudio-server-amd64.deb"),
        "{output}"
    );
}

#[test]
fn dev_setup_without_a_terminal_and_without_yes_refuses_with_guidance() {
    hpds()
        .args(["setup"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--yes"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn server_profile_off_linux_errors_clearly() {
    hpds()
        .args(["setup", "--profile", "server"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Linux").and(predicate::str::contains("--profile dev")));
}

#[cfg(target_os = "linux")]
#[test]
fn server_setup_without_a_terminal_and_without_yes_refuses_with_guidance() {
    // The confirmation gate refuses before any step runs.
    hpds()
        .args(["setup", "--profile", "server"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--yes"));
}

#[test]
fn unknown_profile_is_a_usage_error() {
    hpds()
        .args(["setup", "--profile", "laptop"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--profile"));
}
