use assert_cmd::Command;

#[test]
fn hpds_binary_runs_and_exits_successfully() {
    Command::cargo_bin("hpds")
        .expect("hpds binary should build")
        .assert()
        .success();
}
