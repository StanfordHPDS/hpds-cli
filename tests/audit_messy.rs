//! End-to-end `hpds audit` UX on a deliberately messy repo.
//!
//! The repo is built in a tempdir from the plain-file fixture tree in
//! `tests/fixtures/audit-messy/` (no `.git` directory is ever committed to
//! this repository): the helper git-inits it with pinned commit dates,
//! plants junk and secrets files, leaves a merged branch, a stale rendered
//! artifact, a dirty tracked file, and an untracked file. Everything is
//! deterministic — there is no `origin` remote, so the GitHub checks never
//! probe `gh` and the report is stable offline.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// Pinned commit dates (epoch seconds), old enough to be in the past
/// forever, ordered T1 < T2 so "source newer than artifact" holds.
const T1: u64 = 1_600_000_000;
const T2: u64 = 1_600_100_000;

/// The messy repo under audit plus an isolated user-config directory.
struct MessyRepo {
    _root: tempfile::TempDir,
    repo: PathBuf,
    user_dir: PathBuf,
}

impl MessyRepo {
    /// Build the messy repo in a tempdir. The final state trips, at
    /// minimum: dirty-files, untracked-files, stale-branches,
    /// stale-artifacts, junk-files, gitignore-hygiene, readme,
    /// lifecycle-metadata, and lockfiles.
    fn build() -> Self {
        let root = tempfile::tempdir().expect("create tempdir");
        // Fixed directory name: the repo name appears in the report.
        let repo = root.path().join("audit-messy");
        let user_dir = root.path().join("user-config");
        fs::create_dir_all(&user_dir).expect("create user config dir");

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("audit-messy");
        copy_tree(&fixture, &repo);

        let messy = MessyRepo {
            _root: root,
            repo,
            user_dir,
        };
        messy.git(&["init", "--quiet"], None);
        messy.git(&["symbolic-ref", "HEAD", "refs/heads/main"], None);

        // Junk that should never be committed: Finder droppings (warn)
        // and a secrets-looking file (error). Written here rather than
        // stored in the fixture tree so they can never collide with
        // ignore rules or secret scanning on this repository itself.
        messy.write(".DS_Store", "finder junk\n");
        messy.write(".env", "SECRET=hunter2\n");
        messy.git(&["add", "-A"], None);
        messy.git(&["commit", "--quiet", "-m", "initial mess"], Some(T1));

        // A branch left behind at the first commit: fully merged once
        // main moves on (deterministic, unlike age-based staleness).
        messy.git(&["branch", "old-work"], None);

        // Edit the .qmd source without re-rendering: report.html is now
        // older than report.qmd by commit date.
        messy.append("report.qmd", "\nNewer words the HTML never saw.\n");
        messy.git(&["add", "-A"], None);
        messy.git(
            &["commit", "--quiet", "-m", "edit report source only"],
            Some(T2),
        );

        // Uncommitted change to a tracked file, plus an untracked file.
        messy.append("analysis.R", "y <- 2\n");
        messy.write("scratch-notes.txt", "todo\n");
        messy
    }

    /// Run git in the messy repo with a fully isolated identity/config
    /// and, when building history, pinned commit dates.
    fn git(&self, args: &[&str], epoch: Option<u64>) {
        let excludes = format!(
            "core.excludesFile={}",
            self.repo.join("no-such-excludes").display()
        );
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            // The default excludes file (~/.config/git/ignore) applies even
            // with GIT_CONFIG_GLOBAL unset, so pin it somewhere empty too.
            .args(["-c", &excludes])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", self.repo.join("no-such-global-config"))
            .env("GIT_CONFIG_SYSTEM", self.repo.join("no-such-system-config"));
        if let Some(epoch) = epoch {
            let date = format!("{epoch} +0000");
            cmd.env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date);
        }
        let output = cmd.output().expect("run git in messy repo");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write(&self, rel: &str, content: &str) {
        fs::write(self.repo.join(rel), content).expect("write repo file");
    }

    fn append(&self, rel: &str, content: &str) {
        let path = self.repo.join(rel);
        let mut text = fs::read_to_string(&path).expect("read repo file");
        text.push_str(content);
        fs::write(&path, text).expect("append to repo file");
    }

    /// `hpds audit <args...>` from inside the messy repo, with the user
    /// config and the git config the audit's own git calls see both
    /// isolated from the developer's machine.
    fn audit_cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(&self.repo)
            .env("HPDS_CONFIG_DIR", &self.user_dir)
            .env("GIT_CONFIG_GLOBAL", self.repo.join("no-such-global-config"))
            .env("GIT_CONFIG_SYSTEM", self.repo.join("no-such-system-config"))
            // Neutralize any default excludes file (~/.config/git/ignore)
            // for the git processes the audit itself spawns.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.excludesFile")
            .env("GIT_CONFIG_VALUE_0", self.repo.join("no-such-excludes"))
            .arg("audit")
            .args(args);
        cmd
    }

    /// Replace every spelling of the repo's tempdir path (raw and
    /// canonicalized) so snapshots carry no machine-specific paths.
    fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_string();
        let mut spellings = vec![self.repo.display().to_string()];
        if let Ok(canonical) = self.repo.canonicalize() {
            spellings.push(canonical.display().to_string());
        }
        for spelling in spellings {
            redacted = redacted.replace(&spelling, "[REPO]");
        }
        redacted
    }
}

/// Recursively copy the fixture tree (plain files only) into `to`.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination dir");
    for entry in fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("read fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

#[test]
fn messy_repo_human_report_groups_findings_by_severity() {
    let messy = MessyRepo::build();
    let assert = messy.audit_cmd(&[]).assert().code(1);
    let output = assert.get_output();
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout should be UTF-8");
    insta::assert_snapshot!("messy_human_report", messy.redact(&stdout));

    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr should be UTF-8");
    assert!(
        stderr.contains("audit found"),
        "failure reason on stderr: {stderr}"
    );
}

#[test]
fn messy_repo_json_report_is_exact_and_alone_on_stdout() {
    let messy = MessyRepo::build();
    let assert = messy.audit_cmd(&["--format", "json"]).assert().code(1);
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");

    // Piped stdout must be exactly one JSON document (plus the trailing
    // newline) — parse the whole thing, not a fished-out substring.
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("entire stdout parses as one JSON document");
    assert_eq!(value["repo"], "audit-messy");

    insta::assert_snapshot!("messy_json_report", messy.redact(&stdout));
}

#[test]
fn messy_repo_trips_at_least_five_distinct_checks() {
    let messy = MessyRepo::build();
    let assert = messy.audit_cmd(&["--format", "json"]).assert().code(1);
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("stdout should be UTF-8");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");

    let mut check_ids: Vec<&str> = value["findings"]
        .as_array()
        .expect("findings is an array")
        .iter()
        .map(|f| f["check_id"].as_str().expect("check_id is a string"))
        .collect();
    check_ids.sort_unstable();
    check_ids.dedup();
    assert!(
        check_ids.len() >= 5,
        "the fixture must stay messy enough to trip at least 5 checks, got {check_ids:?}"
    );
}

#[test]
fn messy_repo_fails_under_strict_for_warnings_alone() {
    // Prove --strict is wired end to end: with every error-severity
    // problem fixed, the leftover warnings pass normally but fail --strict.
    let messy = MessyRepo::build();
    fs::remove_file(messy.repo.join(".env")).expect("delete .env");
    messy.write(
        "hpds.toml",
        "[project]\nstatus = \"active\"\nprimary-author = \"malcolm\"\n",
    );
    messy.write("renv.lock", "{}\n");
    messy.write(
        "README.md",
        "# messy\n\n## Description\n\n## File structure\n\n## How to run\n\n## Dependencies\n",
    );
    messy.git(&["add", "-A"], None);
    messy.git(&["commit", "--quiet", "-m", "fix the errors"], None);
    // Leave a warning behind: report.qmd is still newer than report.html.

    messy.audit_cmd(&[]).assert().success();
    let assert = messy.audit_cmd(&["--strict"]).assert().code(1);
    let stderr =
        String::from_utf8(assert.get_output().stderr.clone()).expect("stderr should be UTF-8");
    assert!(
        stderr.contains("--strict"),
        "failure names --strict: {stderr}"
    );
}
