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
fn install_rejects_a_version_pin_for_tools_that_cannot_pin() {
    // rig installs through package managers and tinytex through quarto's
    // bundled version, so `--version` is a usage error for both (caught
    // before anything touches the system).
    for tool in ["rig", "tinytex"] {
        hpds()
            .args(["install", tool, "--version", "1.0.0"])
            .assert()
            .code(2)
            .stderr(
                predicate::str::contains("does not support")
                    .and(predicate::str::contains("--version")),
            );
    }
}

/// The tinytex installer needs quarto; with an empty `PATH` the error
/// must say to install quarto first (and touch nothing).
#[test]
fn install_tinytex_without_quarto_says_install_quarto_first() {
    let empty = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["install", "tinytex", "--yes"])
        .env("PATH", empty.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("needs quarto")
                .and(predicate::str::contains("hpds install quarto")),
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
    // (hpds tool name, executable name, --version output, detected version)
    let fake_tools = [
        ("uv", "uv", "uv 0.9.0 (39b688653 2025-10-07)", "0.9.0"),
        ("gh", "gh", "gh version 2.95.0 (2026-06-17)", "2.95.0"),
        (
            "rig",
            "rig",
            "RIG -- The R Installation Manager 0.8.1",
            "0.8.1",
        ),
        ("duckdb", "duckdb", "v1.5.4 (Variegata) 08e34c447b", "1.5.4"),
        ("r", "R", "R version 4.6.0 (2026-04-24)", "4.6.0"),
        ("togi", "togi", "togi 0.1.0", "0.1.0"),
    ];
    for (_, exe, version_output, _) in fake_tools {
        let path = bin.path().join(exe);
        std::fs::write(&path, format!("#!/bin/sh\necho '{version_output}'\n"))
            .expect("write fake tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("mark fake tool executable");
    }

    // quarto answers both `--version` (its own detection) and `list tools`
    // (tinytex detection), covering the last two installers.
    let quarto = bin.path().join("quarto");
    std::fs::write(
        &quarto,
        "#!/bin/sh\n\
         if [ \"$1\" = list ]; then\n\
         printf 'Tool     Status      Installed  Latest\\n'\n\
         printf 'tinytex  Up to date  v2026.07   v2026.07\\n'\n\
         else\n\
         echo '1.9.36'\n\
         fi\n",
    )
    .expect("write fake quarto");
    std::fs::set_permissions(&quarto, std::fs::Permissions::from_mode(0o755))
        .expect("mark fake quarto executable");

    let no_ops = fake_tools
        .into_iter()
        .map(|(tool, _, _, version)| (tool, version))
        .chain([("quarto", "1.9.36"), ("tinytex", "2026.07")]);
    for (tool, version) in no_ops {
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
    // The flags must parse; rig cannot pin, so the run stops at the usage
    // error before anything touches the system.
    hpds()
        .args(["install", "rig", "--version", "0.8.1", "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does not support"));
}

#[test]
fn install_accepts_short_yes_flag() {
    hpds()
        .args(["install", "-y", "rig", "--version", "0.8.1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does not support"));
}

/// A pinned install of an already-pinned-version tool is a no-op, so the
/// full `--version` + `--yes` path is exercised without touching anything.
#[cfg(unix)]
#[test]
fn install_pinned_to_the_installed_version_is_a_no_op() {
    use std::os::unix::fs::PermissionsExt;

    let bin = tempfile::tempdir().expect("tempdir");
    let path = bin.path().join("quarto");
    std::fs::write(&path, "#!/bin/sh\necho '1.9.36'\n").expect("write fake quarto");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("mark fake quarto executable");

    hpds()
        .args(["install", "quarto", "--version", "1.9.36", "--yes"])
        .env("PATH", bin.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("already installed"));
}

/// Not installed + non-interactive + no --yes: the plan is printed and
/// the run refuses before any command executes.
#[cfg(unix)]
#[test]
fn install_without_yes_non_interactively_refuses_before_running_anything() {
    use std::os::unix::fs::PermissionsExt;

    let bin = tempfile::tempdir().expect("tempdir");
    let marker = bin.path().join("brew-ran");
    let brew = bin.path().join("brew");
    std::fs::write(&brew, format!("#!/bin/sh\ntouch {}\n", marker.display()))
        .expect("write fake brew");
    std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755))
        .expect("mark fake brew executable");

    hpds()
        .args(["install", "uv"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("installing uv will:")
                .and(predicate::str::contains("brew install uv")),
        )
        .stderr(predicate::str::contains("--yes"));

    assert!(!marker.exists(), "no command may run without approval");
}

/// The togi plan must say where the binary comes from (its GitHub
/// releases), not just that "a release binary" will be downloaded. The
/// non-interactive refusal path prints the plan without running anything.
#[test]
fn install_togi_plan_names_the_source_release_repo() {
    let empty = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["install", "togi"])
        .env("PATH", empty.path())
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("installing togi will:")
                .and(predicate::str::contains("github.com/StanfordHPDS/togi")),
        )
        .stderr(predicate::str::contains("--yes"));
}

/// --yes prints the plan and runs the strategy without prompting; the
/// fake brew "installs" a uv shim so post-install verification passes.
#[cfg(unix)]
#[test]
fn install_with_yes_prints_the_plan_and_runs_the_strategy() {
    use std::os::unix::fs::PermissionsExt;

    let bin = tempfile::tempdir().expect("tempdir");
    let brew = bin.path().join("brew");
    std::fs::write(
        &brew,
        format!(
            "#!/bin/sh\n\
             printf '#!/bin/sh\\necho uv 0.9.0\\n' > {dir}/uv\n\
             chmod +x {dir}/uv\n",
            dir = bin.path().display()
        ),
    )
    .expect("write fake brew");
    std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755))
        .expect("mark fake brew executable");

    hpds()
        .args(["install", "uv", "--yes"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("installing uv will:")
                .and(predicate::str::contains("brew install uv"))
                .and(predicate::str::contains("uv 0.9.0 installed")),
        );
}
