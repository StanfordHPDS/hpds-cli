//! Structural checks on the repo's CI workflow.
//!
//! These tests keep the workflow honest: it must run the three quality gates
//! on the ubuntu/macos/windows matrix and keep network-dependent tests in a
//! separate allowed-to-fail job.

use std::path::Path;

fn workflow_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ci.yml")
}

fn workflow_contents() -> String {
    let path = workflow_path();
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

#[test]
fn ci_workflow_exists_at_badge_path() {
    // The README badge points at workflows/ci.yml; the name is load-bearing.
    assert!(
        workflow_path().is_file(),
        "expected .github/workflows/ci.yml to exist"
    );
}

#[test]
fn ci_workflow_triggers_on_pr_and_main_push() {
    let yml = workflow_contents();
    assert!(yml.contains("pull_request"), "must trigger on pull_request");
    assert!(yml.contains("push"), "must trigger on push");
    assert!(yml.contains("main"), "push trigger must target main");
}

#[test]
fn ci_workflow_runs_the_three_gates() {
    let yml = workflow_contents();
    assert!(
        yml.contains("cargo fmt --check"),
        "must run cargo fmt --check"
    );
    assert!(
        yml.contains("cargo clippy --all-targets -- -D warnings"),
        "must run clippy with -D warnings on all targets"
    );
    assert!(yml.contains("cargo test"), "must run cargo test");
}

#[test]
fn ci_workflow_tests_on_three_os_matrix() {
    let yml = workflow_contents();
    for os in ["ubuntu-latest", "macos-latest", "windows-latest"] {
        assert!(yml.contains(os), "matrix must include {os}");
    }
}

#[test]
fn ci_workflow_isolates_online_tests_in_allowed_to_fail_job() {
    let yml = workflow_contents();
    assert!(
        yml.contains("--features online-tests"),
        "online tests must run via --features online-tests"
    );
    assert!(
        yml.contains("continue-on-error: true"),
        "online-tests job must be allowed to fail"
    );

    // The default gate job must not enable the online-tests feature: the only
    // occurrence of the feature flag must live after the online job starts.
    let online_job = yml
        .find("online-tests:")
        .expect("expected a job named online-tests");
    let feature_flag = yml
        .find("--features online-tests")
        .expect("checked above; qed");
    assert!(
        feature_flag > online_job,
        "--features online-tests must only appear inside the online-tests job"
    );
}

#[test]
fn pull_request_template_exists_and_prompts_for_decisions() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("pull_request_template.md");
    let md = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let lower = md.to_lowercase();
    assert!(
        lower.contains("change"),
        "template must ask contributors to describe changes"
    );
    assert!(
        lower.contains("decision"),
        "template must ask contributors to document important decisions"
    );
}
