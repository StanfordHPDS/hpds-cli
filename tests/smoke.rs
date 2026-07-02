use assert_cmd::Command;

#[test]
fn hpds_binary_runs_and_exits_successfully() {
    // `hpds` with no arguments shows help and exits 2 (usage error),
    // so the smoke check drives `--help` instead.
    Command::cargo_bin("hpds")
        .expect("hpds binary should build")
        .arg("--help")
        .assert()
        .success();
}
