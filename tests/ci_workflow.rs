//! Structural checks on the repo's CI workflow.
//!
//! These tests keep the workflow honest: it must run the full gate set
//! (format, clippy, test, doc, typos) on the ubuntu/macos/windows matrix and
//! keep network-dependent tests in a separate allowed-to-fail job.

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

fn workflow_doc() -> serde_yaml::Value {
    serde_yaml::from_str(&workflow_contents()).expect("ci.yml must parse as YAML")
}

/// The steps of the `gates` job, as parsed YAML mappings.
fn gates_steps(doc: &serde_yaml::Value) -> &[serde_yaml::Value] {
    doc.get("jobs")
        .and_then(|j| j.get("gates"))
        .and_then(|g| g.get("steps"))
        .and_then(|s| s.as_sequence())
        .map(Vec::as_slice)
        .expect("ci.yml must have a `gates` job with steps")
}

/// The individual commands of a step's `run` script: one per line (with
/// backslash-continued lines joined), further split on `&&` and `;`, each as
/// whitespace-separated tokens.
fn step_commands(step: &serde_yaml::Value) -> Vec<Vec<String>> {
    let Some(run) = step.get("run").and_then(|r| r.as_str()) else {
        return Vec::new();
    };
    run.replace("\\\r\n", " ")
        .replace("\\\n", " ")
        .lines()
        .flat_map(|line| line.split("&&").map(str::to_string).collect::<Vec<_>>())
        .flat_map(|part| part.split(';').map(str::to_string).collect::<Vec<_>>())
        .map(|cmd| cmd.split_whitespace().map(str::to_string).collect())
        .filter(|tokens: &Vec<String>| !tokens.is_empty())
        .collect()
}

/// The first `gates` step with a command starting with `prefix` (such as
/// `["cargo", "doc"]`), together with the tokens of that single command.
fn find_command<'a>(
    steps: &'a [serde_yaml::Value],
    prefix: &[&str],
) -> Option<(&'a serde_yaml::Value, Vec<String>)> {
    steps.iter().find_map(|step| {
        step_commands(step)
            .into_iter()
            .find(|cmd| cmd.len() >= prefix.len() && cmd.iter().zip(prefix).all(|(a, b)| a == b))
            .map(|cmd| (step, cmd))
    })
}

fn check_has_flags(what: &str, tokens: &[String], flags: &[&str]) -> Result<(), String> {
    let missing: Vec<&&str> = flags
        .iter()
        .filter(|f| !tokens.iter().any(|t| t == **f))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{what} command in the gates job is missing {missing:?}; has {tokens:?}"
        ))
    }
}

/// `RUSTDOCFLAGS` as set by the step's env, else the job's, else the
/// workflow's.
fn rustdocflags<'a>(doc: &'a serde_yaml::Value, step: &'a serde_yaml::Value) -> Option<&'a str> {
    let job = doc.get("jobs").and_then(|j| j.get("gates"));
    [Some(step), job, Some(doc)]
        .into_iter()
        .flatten()
        .find_map(|scope| {
            scope
                .get("env")
                .and_then(|e| e.get("RUSTDOCFLAGS"))
                .and_then(|v| v.as_str())
        })
}

/// Checks that the `gates` job of `doc` runs the full gate set, describing
/// the first gate that is missing or incomplete.
fn check_gates(doc: &serde_yaml::Value) -> Result<(), String> {
    let steps = gates_steps(doc);

    let fmt = find_command(steps, &["cargo", "fmt"])
        .ok_or("gates job must run cargo fmt")?
        .1;
    check_has_flags("cargo fmt", &fmt, &["--check"])?;

    let clippy = find_command(steps, &["cargo", "clippy"])
        .ok_or("gates job must run cargo clippy")?
        .1;
    check_has_flags(
        "cargo clippy",
        &clippy,
        &[
            "--all-targets",
            "--all-features",
            "--locked",
            "-D",
            "warnings",
        ],
    )?;

    let test = find_command(steps, &["cargo", "test"])
        .ok_or("gates job must run cargo test")?
        .1;
    check_has_flags("cargo test", &test, &["--locked"])?;

    // RUSTDOCFLAGS must come from an `env:` block: an inline `VAR=value`
    // prefix does not work in the PowerShell default shell on Windows.
    let Some((doc_step, cargo_doc)) = find_command(steps, &["cargo", "doc"]) else {
        if find_command_after_assignment(steps, "RUSTDOCFLAGS", &["cargo", "doc"]) {
            return Err(
                "RUSTDOCFLAGS must not be set inline before cargo doc (it fails \
                 in PowerShell on Windows); set it through an `env:` block instead"
                    .to_string(),
            );
        }
        return Err("gates job must run cargo doc".to_string());
    };
    check_has_flags("cargo doc", &cargo_doc, &["--no-deps", "--locked"])?;
    let flags = rustdocflags(doc, doc_step).unwrap_or("");
    if !(flags.contains("-D warnings") || flags.contains("-Dwarnings")) {
        return Err(format!(
            "RUSTDOCFLAGS must be set to deny warnings in an env block on the cargo doc \
             step, the gates job, or the workflow, got {flags:?}"
        ));
    }

    let has_typos = steps.iter().any(|step| {
        step.get("uses")
            .and_then(|u| u.as_str())
            .is_some_and(|u| u.starts_with("crate-ci/typos"))
    });
    if !has_typos {
        return Err("gates job must have a step using a crate-ci/typos action".to_string());
    }
    Ok(())
}

/// Whether some command in `steps` starts with an assignment to `var`
/// (whose value may be quoted and contain spaces) followed by the tokens
/// `prefix`.
fn find_command_after_assignment(steps: &[serde_yaml::Value], var: &str, prefix: &[&str]) -> bool {
    let assignment = format!("{var}=");
    steps.iter().any(|step| {
        step_commands(step).iter().any(|cmd| {
            let Some(first) = cmd.first().and_then(|t| t.strip_prefix(&assignment)) else {
                return false;
            };
            let rest = match first.chars().next().filter(|c| matches!(c, '"' | '\'')) {
                Some(quote) if !(first.len() > 1 && first.ends_with(quote)) => {
                    let close = cmd[1..].iter().position(|t| t.ends_with(quote));
                    close.map_or(&[][..], |i| &cmd[i + 2..])
                }
                _ => &cmd[1..],
            };
            rest.len() >= prefix.len() && rest.iter().zip(prefix).all(|(a, b)| a == b)
        })
    })
}

#[test]
fn ci_workflow_runs_the_full_gate_set() {
    if let Err(message) = check_gates(&workflow_doc()) {
        panic!("{message}");
    }
}

fn gates_doc(steps_yaml: &str) -> serde_yaml::Value {
    let yml = format!("jobs:\n  gates:\n    steps:\n      - uses: crate-ci/typos@v1\n{steps_yaml}");
    serde_yaml::from_str(&yml).expect("test yaml must parse")
}

const FMT_TEST_STEPS: &str = "      - run: cargo fmt --all --check
      - run: cargo test --locked
";

#[test]
fn gate_check_joins_backslash_continued_lines() {
    let doc = gates_doc(&format!(
        "{FMT_TEST_STEPS}      - run: |
          cargo clippy --all-targets \\
            --all-features --locked -- -D warnings
      - run: cargo doc --no-deps --locked
        env:
          RUSTDOCFLAGS: -D warnings
"
    ));
    assert_eq!(check_gates(&doc), Ok(()));
}

#[test]
fn gate_check_rejects_an_inline_rustdocflags_assignment() {
    let doc = gates_doc(&format!(
        "{FMT_TEST_STEPS}      - run: cargo clippy --all-targets --all-features --locked -- -D warnings
      - run: RUSTDOCFLAGS=\"-D warnings\" cargo doc --no-deps --locked
"
    ));
    let err = check_gates(&doc).expect_err("inline RUSTDOCFLAGS must be rejected");
    assert!(
        err.contains("inline") && err.contains("env:"),
        "message must explain the inline form and point at env:, got {err:?}"
    );
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
fn ci_workflow_declares_least_privilege_permissions() {
    let yml = workflow_contents();
    let doc: serde_yaml::Value = serde_yaml::from_str(&yml).expect("ci.yml must parse as YAML");

    let permissions = doc
        .get("permissions")
        .expect("top-level permissions block must exist")
        .as_mapping()
        .expect("permissions must be a mapping");
    assert_eq!(
        permissions.len(),
        1,
        "permissions must grant exactly one scope, got {permissions:?}"
    );
    assert_eq!(
        permissions.get("contents").and_then(|v| v.as_str()),
        Some("read"),
        "permissions must be exactly contents: read, got {permissions:?}"
    );

    let jobs = doc
        .get("jobs")
        .and_then(|j| j.as_mapping())
        .expect("jobs section must exist");
    for (name, job) in jobs {
        let Some(job_permissions) = job.get("permissions") else {
            continue;
        };
        let job_permissions = job_permissions
            .as_mapping()
            .unwrap_or_else(|| panic!("job {name:?} permissions must be a mapping"));
        for (scope, level) in job_permissions {
            assert_ne!(
                level.as_str(),
                Some("write"),
                "job {name:?} must not grant write on scope {scope:?}"
            );
        }
    }
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
