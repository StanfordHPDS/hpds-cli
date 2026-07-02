//! Integration tests for `hpds init` (and its `hpds project init` alias).
//!
//! These drive the flag-driven, non-interactive path only. The interactive
//! wizard cannot be driven under the test harness (stdin is not a TTY), so
//! it is verified by hand with this manual script:
//!
//! ```text
//! # Manual smoke script for the interactive wizard (run in a terminal):
//! mkdir smoke-hpds-init && cd smoke-hpds-init
//! hpds init
//! #  1. "Project name" defaults to smoke-hpds-init — accept it
//! #  2. "Project description" — type anything
//! #  3. "Project language" — pick r
//! #  4. Component multi-select — pick pipeline and readme
//! #  5. "Primary author (GitHub username)" — defaults to the login gh is
//! #     authenticated as (empty without gh); accept
//! #  6. "Which pipeline kind?" — pick make
//! #  7. "Initialize a git repository here?" — answer yes
//! #  8. "Add the lab ignore patterns ...?" — answer yes
//! #  9. "Create a GitHub repository ...?" — answer no
//! # Verify: hpds.toml ([project] with status/primary-author), Makefile,
//! # README.qmd, .git/, and .gitignore containing the vaccinate block.
//! cd .. && rm -rf smoke-hpds-init
//! ```

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

fn hpds() -> Command {
    Command::cargo_bin("hpds").expect("hpds binary should build")
}

fn read(dir: &tempfile::TempDir, rel: &str) -> String {
    fs::read_to_string(dir.path().join(rel)).unwrap_or_else(|e| panic!("{rel} should exist: {e}"))
}

// --- --yes produces a complete project ---------------------------------------

#[test]
fn init_yes_full_options_produces_a_complete_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args([
            "init",
            "--yes",
            "--name",
            "malaria-icu",
            "--description",
            "ICU malaria outcomes study",
            "--language",
            "r",
            "--author",
            "malcolm",
            "--use",
            "pipeline:make,readme,container:docker,slurm,gha",
        ])
        .current_dir(tmp.path())
        .assert()
        .success();

    // hpds.toml carries the [project] metadata.
    let toml = read(&tmp, "hpds.toml");
    assert!(toml.contains("[project]"), "{toml}");
    assert!(toml.contains("status = \"active\""), "{toml}");
    assert!(toml.contains("primary-author = \"malcolm\""), "{toml}");
    assert!(toml.contains("malaria-icu"), "name in header: {toml}");
    assert!(
        toml.contains("ICU malaria outcomes study"),
        "description in header: {toml}"
    );

    // Every selected component landed its files.
    let makefile = read(&tmp, "Makefile");
    assert!(makefile.contains("clean:"), "{makefile}");
    let readme = read(&tmp, "README.qmd");
    assert!(readme.contains("malaria-icu"), "{readme}");
    assert!(read(&tmp, "Dockerfile").contains("stanfordhpds"));
    assert!(tmp.path().join("scripts/slurm_job.sh").exists());
    assert!(tmp.path().join("docs/slurm.md").exists());
    assert!(tmp.path().join(".github/pull_request_template.md").exists());
    assert!(tmp.path().join(".github/workflows/hpds-lint.yml").exists());
}

#[test]
fn project_init_alias_accepts_the_same_flags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args([
            "project", "init", "--yes", "--name", "aliased", "--author", "malcolm",
        ])
        .current_dir(tmp.path())
        .assert()
        .success();
    let toml = read(&tmp, "hpds.toml");
    assert!(toml.contains("aliased"), "{toml}");
}

// --- defaults under --yes ----------------------------------------------------

#[test]
fn init_yes_alone_writes_hpds_toml_and_nothing_else() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--author", "malcolm"])
        .current_dir(tmp.path())
        .assert()
        .success();
    assert!(tmp.path().join("hpds.toml").exists());
    // No components requested, no git flags: nothing else appears.
    assert!(!tmp.path().join("Makefile").exists());
    assert!(!tmp.path().join(".git").exists());
    assert!(!tmp.path().join(".gitignore").exists());
}

#[test]
fn init_yes_name_defaults_to_the_directory_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path().join("cool-study");
    fs::create_dir(&project).expect("create project dir");
    hpds()
        .args(["init", "--yes", "--author", "malcolm"])
        .current_dir(&project)
        .assert()
        .success();
    let toml = fs::read_to_string(project.join("hpds.toml")).expect("hpds.toml");
    assert!(toml.contains("cool-study"), "{toml}");
}

#[test]
fn init_yes_pipeline_without_variant_defaults_to_make() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--author", "malcolm", "--use", "pipeline"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let makefile = read(&tmp, "Makefile");
    assert!(makefile.contains("clean:"), "{makefile}");
    assert!(
        !tmp.path().join("_targets.R").exists(),
        "default pipeline kind is make, not targets"
    );
}

#[test]
fn init_yes_pipeline_variant_targets_is_honored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args([
            "init",
            "--yes",
            "--author",
            "malcolm",
            "--use",
            "pipeline:targets",
        ])
        .current_dir(tmp.path())
        .assert()
        .success();
    assert!(tmp.path().join("_targets.R").exists());
    assert!(!tmp.path().join("Makefile").exists());
}

#[test]
fn init_yes_gha_without_variant_applies_every_workflow() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--author", "malcolm", "--use", "gha"])
        .current_dir(tmp.path())
        .assert()
        .success();
    assert!(tmp.path().join(".github/pull_request_template.md").exists());
    assert!(tmp.path().join(".github/workflows/hpds-lint.yml").exists());
}

#[test]
fn init_yes_gha_variant_selects_workflows_with_plus() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args([
            "init",
            "--yes",
            "--author",
            "malcolm",
            "--use",
            "gha:pr-template",
        ])
        .current_dir(tmp.path())
        .assert()
        .success();
    assert!(tmp.path().join(".github/pull_request_template.md").exists());
    assert!(
        !tmp.path().join(".github/workflows/hpds-lint.yml").exists(),
        "only the requested workflow lands"
    );
}

#[test]
fn init_yes_python_project_gets_readme_md() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args([
            "init",
            "--yes",
            "--author",
            "malcolm",
            "--language",
            "python",
            "--use",
            "readme",
        ])
        .current_dir(tmp.path())
        .assert()
        .success();
    assert!(tmp.path().join("README.md").exists());
    assert!(!tmp.path().join("README.qmd").exists());
}

// --- primary-author default: a GitHub login, never git user.name ------------

/// The default primary author must be the login `gh` is authenticated as
/// (`gh api user -q .login`) — the audit watchers check needs a GitHub
/// LOGIN, and git's user.name is a display name. Driven with a fake `gh`
/// on PATH. Unix-only: a script shim cannot intercept `Command::new` on
/// Windows, which resolves only `.exe`.
#[cfg(unix)]
#[test]
fn init_yes_author_defaults_to_the_gh_login() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let shim_dir = tmp.path().join("bin");
    let project = tmp.path().join("proj");
    fs::create_dir_all(&shim_dir).expect("create shim dir");
    fs::create_dir_all(&project).expect("create project dir");
    let log = tmp.path().join("gh.log");
    let gh = shim_dir.join("gh");
    fs::write(
        &gh,
        format!(
            "#!/bin/sh\nprintf 'gh %s\\n' \"$*\" >> '{}'\necho octocat\n",
            log.display()
        ),
    )
    .expect("write gh shim");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod gh shim");

    hpds()
        .args(["init", "--yes"])
        .current_dir(&project)
        .env("PATH", &shim_dir)
        .assert()
        .success();

    let toml = fs::read_to_string(project.join("hpds.toml")).expect("hpds.toml");
    assert!(
        toml.contains("primary-author = \"octocat\""),
        "the gh login is the default author: {toml}"
    );
    let recorded = fs::read_to_string(&log).expect("the shim was invoked");
    assert!(
        recorded.contains("gh api user -q .login"),
        "the login comes from `gh api user`: {recorded}"
    );
}

#[test]
fn init_yes_author_stays_empty_without_gh_and_the_toml_says_to_fill_it_in() {
    // No gh on PATH (and no fallback to git user.name): primary-author is
    // left blank, and the generated hpds.toml tells the user to fill in
    // their GitHub username.
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty_path = tmp.path().join("empty-bin");
    let project = tmp.path().join("proj");
    fs::create_dir_all(&empty_path).expect("create empty PATH dir");
    fs::create_dir_all(&project).expect("create project dir");

    hpds()
        .args(["init", "--yes"])
        .current_dir(&project)
        .env("PATH", &empty_path)
        .assert()
        .success();

    let toml = fs::read_to_string(project.join("hpds.toml")).expect("hpds.toml");
    assert!(
        toml.contains("primary-author = \"\""),
        "the author stays empty: {toml}"
    );
    assert!(
        toml.contains("fill in your GitHub username"),
        "the toml says what to do: {toml}"
    );
}

// --- errors: every one says what to do next ---------------------------------

#[test]
fn init_yes_language_component_without_language_errors_actionably() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--use", "readme"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("readme")
                .and(predicate::str::contains("--language"))
                .and(predicate::str::contains("hint:")),
        );
    // Nothing half-written: the failure happens before any file lands.
    assert!(!tmp.path().join("README.md").exists());
    assert!(!tmp.path().join("README.qmd").exists());
}

#[test]
fn init_yes_unknown_component_exits_2_and_lists_valid_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--use", "frobnicate"])
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("frobnicate")
                .and(predicate::str::contains("pipeline"))
                .and(predicate::str::contains("hint:")),
        );
}

#[test]
fn init_yes_fetched_component_is_rejected_with_a_pointer_at_use() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--use", "slides"])
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("hpds use slides").and(predicate::str::contains("hint:")));
}

#[test]
fn init_yes_variant_on_a_variantless_component_exits_2() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--use", "readme:qmd"])
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("readme").and(predicate::str::contains("hint:")));
}

#[test]
fn init_yes_unknown_pipeline_variant_fails_before_any_write_with_init_syntax() {
    // The pipeline component's own `--kind` hint would be wrong here:
    // init has no --kind flag. The variant must be validated up front,
    // before hpds.toml lands, and the hint must use init's `--use` syntax.
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--use", "pipeline:bogus"])
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("bogus")
                .and(predicate::str::contains("pipeline:make"))
                .and(predicate::str::contains("--kind").not())
                .and(predicate::str::contains("hint:")),
        );
    assert!(
        !tmp.path().join("hpds.toml").exists(),
        "the bad variant fails before anything is written"
    );
}

#[test]
fn init_yes_unknown_container_variant_fails_before_any_write_with_init_syntax() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--language", "r", "--use", "container:oci"])
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("oci")
                .and(predicate::str::contains("container:docker"))
                .and(predicate::str::contains("--kind").not())
                .and(predicate::str::contains("hint:")),
        );
    assert!(!tmp.path().join("hpds.toml").exists());
}

#[test]
fn init_yes_unknown_gha_workflow_fails_before_any_write_with_init_syntax() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--use", "gha:bogus"])
        .current_dir(tmp.path())
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("bogus")
                .and(predicate::str::contains("gha:"))
                .and(predicate::str::contains("pr-template"))
                .and(predicate::str::contains("--workflows").not())
                .and(predicate::str::contains("hint:")),
        );
    assert!(!tmp.path().join("hpds.toml").exists());
}

#[test]
fn init_yes_vaccinate_outside_a_repo_points_at_git_init_flag() {
    // `hpds git vaccinate`'s own NotARepo hint talks about `--project`,
    // which init does not have; init's hint must point at --git-init.
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args(["init", "--yes", "--author", "malcolm", "--vaccinate"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("--git-init")
                .and(predicate::str::contains("--project").not())
                .and(predicate::str::contains("hint:")),
        );
}

#[test]
fn init_without_yes_cannot_prompt_off_a_tty_and_says_so() {
    // stdin is not a TTY under the test harness, so the wizard's first
    // prompt must refuse with a pointer at the flag-driven path.
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .arg("init")
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("non-interactively").and(predicate::str::contains("hint:")),
        );
}

// --- conflict handling: never overwrite without the engine ------------------

#[test]
fn init_never_overwrites_an_existing_hpds_toml_without_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("hpds.toml"), "# mine\n").expect("seed hpds.toml");
    let assert = hpds()
        .args(["init", "--yes", "--author", "malcolm"])
        .current_dir(tmp.path())
        .assert()
        .success();
    // The user's file is untouched and the output points at --force.
    assert_eq!(read(&tmp, "hpds.toml"), "# mine\n");
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("--force"), "points at --force: {stdout}");
}

#[test]
fn init_force_overwrites_a_conflicting_hpds_toml() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("hpds.toml"), "# mine\n").expect("seed hpds.toml");
    hpds()
        .args(["init", "--yes", "--author", "malcolm", "--force"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let toml = read(&tmp, "hpds.toml");
    assert!(toml.contains("[project]"), "{toml}");
}

#[test]
fn init_is_idempotent_on_a_second_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let args = [
        "init",
        "--yes",
        "--name",
        "twice",
        "--author",
        "malcolm",
        "--use",
        "pipeline:make",
    ];
    hpds().args(args).current_dir(tmp.path()).assert().success();
    hpds()
        .args(args)
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("--force").not());
}

// --- git-forward flags -------------------------------------------------------

#[test]
fn init_yes_git_init_and_vaccinate_flags_run_both_steps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    hpds()
        .args([
            "init",
            "--yes",
            "--author",
            "malcolm",
            "--git-init",
            "--vaccinate",
        ])
        .current_dir(tmp.path())
        .assert()
        .success();
    assert!(tmp.path().join(".git").exists(), "git init ran");
    let gitignore = read(&tmp, ".gitignore");
    assert!(
        gitignore.contains(".Rhistory"),
        "vaccinate --project ran: {gitignore}"
    );
}

#[test]
fn init_yes_git_init_in_an_existing_repo_is_a_friendly_no_op() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = std::process::Command::new("git")
        .arg("init")
        .current_dir(tmp.path())
        .output()
        .expect("git init");
    assert!(out.status.success());
    hpds()
        .args(["init", "--yes", "--author", "malcolm", "--git-init"])
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("already a git repository"));
}
