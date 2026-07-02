//! Integration tests for the CLI skeleton.
//!
//! Covers: `--help` snapshots for every command, the "not yet implemented"
//! stub behavior (clean message, exit 2), global flags, `hpds version`, and
//! `hpds completions`.

use assert_cmd::Command;
use predicates::prelude::*;

fn hpds() -> Command {
    Command::cargo_bin("hpds").expect("hpds binary should build")
}

fn help_output(args: &[&str]) -> String {
    let assert = hpds().args(args).assert().success();
    String::from_utf8(assert.get_output().stdout.clone()).expect("help output should be UTF-8")
}

/// Snapshot `hpds <args...> --help` under the test's name.
macro_rules! help_snapshot {
    ($name:ident $(, $arg:literal)*) => {
        #[test]
        fn $name() {
            insta::assert_snapshot!(help_output(&[$($arg,)* "--help"]));
        }
    };
}

help_snapshot!(help_root);
help_snapshot!(help_format, "format");
help_snapshot!(help_lint, "lint");
help_snapshot!(help_init, "init");
help_snapshot!(help_project, "project");
help_snapshot!(help_project_init, "project", "init");
help_snapshot!(help_use, "use");
help_snapshot!(help_install, "install");
help_snapshot!(help_setup, "setup");
help_snapshot!(help_git, "git");
help_snapshot!(help_git_setup, "git", "setup");
help_snapshot!(help_git_vaccinate, "git", "vaccinate");
help_snapshot!(help_repo, "repo");
help_snapshot!(help_repo_create, "repo", "create");
help_snapshot!(help_audit, "audit");
help_snapshot!(help_audit_all, "audit", "all");
help_snapshot!(help_audit_report_github, "audit", "report-github");
help_snapshot!(help_tools, "tools");
help_snapshot!(help_tools_list, "tools", "list");
help_snapshot!(help_tools_update, "tools", "update");
help_snapshot!(help_tools_clean, "tools", "clean");
help_snapshot!(help_config, "config");
help_snapshot!(help_completions, "completions");
help_snapshot!(help_version, "version");
help_snapshot!(help_upgrade, "upgrade");

/// Every stubbed (sub)command renders a clean "not yet implemented" error on
/// stderr, says what to do next, and exits 2.
#[test]
fn stub_commands_exit_2_with_not_yet_implemented_error() {
    let stubs: &[&[&str]] = &[
        &["format"],
        &["lint"],
        &["install"],
        &["setup"],
        &["audit", "all"],
        &["audit", "report-github"],
        &["tools", "list"],
        &["tools", "update"],
        &["tools", "clean"],
        &["upgrade"],
    ];
    for args in stubs {
        hpds()
            .args(*args)
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty())
            .stderr(
                predicate::str::contains("not yet implemented")
                    .and(predicate::str::contains("error:"))
                    .and(predicate::str::contains("hpds --help")),
            );
    }
}

#[test]
fn version_command_prints_version() {
    hpds()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_flag_prints_version() {
    hpds()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn completions_generates_a_bash_script() {
    hpds()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_hpds"));
}

#[test]
fn completions_requires_a_shell_argument() {
    hpds().arg("completions").assert().code(2);
}

#[test]
fn global_flags_parse_before_the_subcommand() {
    hpds()
        .args([
            "--verbose",
            "--quiet",
            "--no-color",
            "--config",
            "hpds.toml",
            "version",
        ])
        .assert()
        .success();
}

#[test]
fn global_flags_parse_after_the_subcommand() {
    // Global flags must also be accepted in subcommand position; the command
    // itself is still a stub, so it exits 2 via the not-yet-implemented path
    // (not a usage-parse failure, which would not mention the stub error).
    hpds()
        .args(["format", "-v", "-q", "--no-color", "--config", "hpds.toml"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn no_arguments_shows_help_and_exits_2() {
    hpds()
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage:"));
}

#[test]
fn unknown_command_exits_2() {
    hpds().arg("frobnicate").assert().code(2);
}

#[test]
fn git_without_subcommand_exits_2() {
    hpds().arg("git").assert().code(2);
}
