//! End-to-end walkthrough: drive the real `hpds` binary through a full lab
//! workflow in a throwaway directory and assert the exit code and the files
//! at every step.
//!
//! The workflow is: scaffold a project (`hpds init --yes` with a full option
//! set) → commit it so the audit has a clean baseline → audit it
//! (`hpds audit`) → resolve its config (`hpds config`). Formatting and
//! linting belong to the separate togi tool, so they are not part of the
//! hpds walkthrough.
//!
//! Isolation: every step redirects HOME, the git global/system config, and
//! the hpds config and data directories into the tempdir, so the test never
//! reads or writes the developer's real HOME, git config, or downloads.

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// A throwaway project directory plus the isolated HOME, git config, and
/// hpds config/data directories the walkthrough runs against.
struct Walkthrough {
    _root: tempfile::TempDir,
    home: PathBuf,
    user_dir: PathBuf,
    data_dir: PathBuf,
    git_global: PathBuf,
    git_system: PathBuf,
    project: PathBuf,
}

impl Walkthrough {
    /// Fresh sandbox with an empty project directory named `lab-study` (the
    /// audit names the repo after this directory).
    fn new() -> Self {
        let root = tempfile::tempdir().expect("create walkthrough tempdir");
        let home = root.path().join("home");
        let user_dir = root.path().join("hpds-config");
        let data_dir = root.path().join("hpds-data");
        let project = root.path().join("lab-study");
        // These two never exist on disk: git treats a missing config file as
        // empty, which is exactly the isolation we want.
        let git_global = root.path().join("no-such-global-gitconfig");
        let git_system = root.path().join("no-such-system-gitconfig");
        for dir in [&home, &user_dir, &data_dir, &project] {
            fs::create_dir_all(dir).expect("create sandbox dir");
        }
        Walkthrough {
            _root: root,
            home,
            user_dir,
            data_dir,
            git_global,
            git_system,
            project,
        }
    }

    /// The project directory's basename — the name the audit reports.
    fn project_name(&self) -> String {
        self.project
            .file_name()
            .expect("project dir has a name")
            .to_string_lossy()
            .into_owned()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.project.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel))
            .unwrap_or_else(|e| panic!("read {rel} in the sandbox: {e}"))
    }

    /// A fully isolated `hpds <args...>` invocation from the project dir. All
    /// state — HOME, git config, hpds config, and the download cache —
    /// points into the tempdir, and the release-download host points at a
    /// closed port so no step can ever touch the network.
    fn hpds(&self, args: &[&str]) -> Command {
        let mut cmd = Command::cargo_bin("hpds").expect("hpds binary should build");
        cmd.current_dir(&self.project)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("HPDS_CONFIG_DIR", &self.user_dir)
            .env("HPDS_DATA_DIR", &self.data_dir)
            .env("GIT_CONFIG_GLOBAL", &self.git_global)
            .env("GIT_CONFIG_SYSTEM", &self.git_system)
            .env("HPDS_RELEASE_BASE_URL", dead_url())
            .args(args);
        cmd
    }

    /// Run git in the project dir with an isolated identity and config, so
    /// commits never depend on (or touch) the developer's git setup.
    fn git(&self, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.project)
            .args([
                "-c",
                "user.name=Lab Tester",
                "-c",
                "user.email=lab@example.com",
            ])
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.git_global)
            .env("GIT_CONFIG_SYSTEM", &self.git_system)
            .output()
            .expect("run git in the sandbox");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // --- shared workflow steps ------------------------------------------

    /// Step 1: scaffold the project with a full option set and assert the
    /// exit code, the `hpds.toml` metadata, and every component's files.
    fn init(&self) {
        self.hpds(&[
            "init",
            "--yes",
            "--name",
            "lab-study",
            "--description",
            "End-to-end walkthrough study",
            "--language",
            "r",
            "--author",
            "malcolm",
            "--use",
            "pipeline:make,readme,gha",
        ])
        .assert()
        .success();

        // hpds.toml carries the [project] lifecycle metadata.
        let toml = self.read("hpds.toml");
        assert!(toml.contains("[project]"), "{toml}");
        assert!(toml.contains(r#"status = "active""#), "{toml}");
        assert!(toml.contains(r#"primary-author = "malcolm""#), "{toml}");
        assert!(toml.contains("lab-study"), "name in header: {toml}");
        assert!(
            toml.contains("End-to-end walkthrough study"),
            "description in header: {toml}"
        );

        // Every selected component landed its files.
        assert!(self.read("Makefile").contains("clean:"), "make pipeline");
        assert!(
            self.read("README.md").contains("lab-study"),
            "readme component"
        );
        assert!(
            !self.path("README.qmd").exists(),
            "no generated README source"
        );
        assert!(
            self.path(".github/pull_request_template.md").exists(),
            "gha pr-template"
        );
        assert!(
            self.path(".github/workflows/togi-lint.yml").exists(),
            "gha lint workflow"
        );
        assert!(
            self.path(".github/workflows/hpds-audit.yml").exists(),
            "gha audit workflow"
        );
    }

    /// Step 2: make a clean git baseline so the audit's dirty/untracked
    /// checks start from nothing outstanding.
    fn commit_baseline(&self) {
        self.git(&["init", "--quiet"]);
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", "scaffold project"]);
    }

    /// Step 3: audit the committed project. On a freshly scaffolded repo the
    /// only finding is the Info notice that the GitHub checks were skipped
    /// (no origin remote), so the audit passes.
    fn audit(&self) {
        self.hpds(&["audit"]).assert().success().stdout(
            predicate::str::contains(self.project_name())
                .and(predicate::str::contains("0 errors, 0 warnings"))
                .and(predicate::str::contains("across 9 checks")),
        );
    }

    /// Step 4: the resolved, layered config prints and reflects the
    /// scaffolded `hpds.toml`.
    fn config(&self) {
        self.hpds(&["config"]).assert().success().stdout(
            predicate::str::contains("[project]")
                .and(predicate::str::contains(r#"status = "active""#))
                .and(predicate::str::contains(r#"primary-author = "malcolm""#))
                .and(predicate::str::contains("stale-days = 90")),
        );
    }
}

/// A `http://127.0.0.1:<port>` URL nothing listens on: any download attempt
/// fails with connection refused, like a machine with no network.
fn dead_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// The whole walkthrough: scaffold → commit → audit → resolve config.
/// Needs no external tools, so it runs in the default offline suite.
#[test]
fn walkthrough_offline() {
    let wt = Walkthrough::new();
    wt.init();
    wt.commit_baseline();
    wt.audit();
    wt.config();
}
