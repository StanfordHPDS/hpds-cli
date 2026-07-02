//! `lockfiles`: when the repo uses renv or uv, the corresponding lockfile
//! must be committed — an uncommitted lockfile means nobody else can
//! reproduce the environment.

use std::collections::HashSet;

use super::{Check, Finding, Severity, git_failed_finding, tracked_files};
use crate::audit::AuditCtx;

pub(super) struct Lockfiles;

impl Check for Lockfiles {
    fn id(&self) -> &str {
        "lockfiles"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let tracked = match tracked_files(&ctx.repo) {
            Ok(tracked) => tracked,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };
        let tracked_set: HashSet<&str> = tracked.iter().map(String::as_str).collect();

        let renv_detected = ctx.repo.join("renv").is_dir()
            || ctx.repo.join("renv.lock").is_file()
            || tracked_set.contains("renv.lock")
            || tracked.iter().any(|path| path.starts_with("renv/"));
        let uv_detected = ctx.repo.join("pyproject.toml").is_file()
            || ctx.repo.join("uv.lock").is_file()
            || tracked_set.contains("pyproject.toml")
            || tracked_set.contains("uv.lock");

        let mut findings = Vec::new();
        if renv_detected && !tracked_set.contains("renv.lock") {
            findings.push(Finding {
                check_id: self.id().to_string(),
                severity: Severity::Error,
                message: "the project uses renv but renv.lock is not committed".to_string(),
                remediation: "run `renv::snapshot()` in R, then commit renv.lock".to_string(),
            });
        }
        if uv_detected && !tracked_set.contains("uv.lock") {
            findings.push(Finding {
                check_id: self.id().to_string(),
                severity: Severity::Error,
                message: "the project uses uv but uv.lock is not committed".to_string(),
                remediation: "run `uv lock`, then commit uv.lock".to_string(),
            });
        }
        findings
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{commit_all, ctx, init_repo, write};
    use super::*;

    #[test]
    fn repos_without_renv_or_uv_need_no_lockfiles() {
        let (_tmp, repo) = init_repo();
        write(&repo, "analysis.R", "1 + 1\n");
        write(&repo, "script.py", "print(1)\n");
        commit_all(&repo, "add code", None);
        assert_eq!(Lockfiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn renv_dir_without_a_committed_lock_is_an_error() {
        let (_tmp, repo) = init_repo();
        write(&repo, "renv/activate.R", "# renv\n");
        write(&repo, "analysis.R", "1 + 1\n");
        commit_all(&repo, "add renv", None);

        let findings = Lockfiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("renv.lock"), "{findings:?}");
        assert!(
            findings[0].remediation.contains("renv::snapshot()"),
            "{findings:?}"
        );
    }

    #[test]
    fn renv_with_a_committed_lock_is_clean() {
        let (_tmp, repo) = init_repo();
        write(&repo, "renv/activate.R", "# renv\n");
        write(&repo, "renv.lock", "{}\n");
        commit_all(&repo, "add renv", None);
        assert_eq!(Lockfiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn a_lockfile_on_disk_but_not_committed_still_fails() {
        let (_tmp, repo) = init_repo();
        write(&repo, "kept.txt", "x\n");
        commit_all(&repo, "init", None);
        // The lockfiles exist locally but were never committed.
        write(&repo, "renv.lock", "{}\n");
        write(&repo, "uv.lock", "version = 1\n");

        let mut findings = Lockfiles.run(&ctx(&repo));
        findings.sort_by(|a, b| a.message.cmp(&b.message));
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].message.contains("renv.lock"), "{findings:?}");
        assert!(findings[1].message.contains("uv.lock"), "{findings:?}");
    }

    #[test]
    fn pyproject_without_a_committed_uv_lock_is_an_error() {
        let (_tmp, repo) = init_repo();
        write(&repo, "pyproject.toml", "[project]\nname = \"x\"\n");
        commit_all(&repo, "add pyproject", None);

        let findings = Lockfiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("uv.lock"), "{findings:?}");
        assert!(findings[0].remediation.contains("uv lock"), "{findings:?}");
    }

    #[test]
    fn pyproject_with_a_committed_uv_lock_is_clean() {
        let (_tmp, repo) = init_repo();
        write(&repo, "pyproject.toml", "[project]\nname = \"x\"\n");
        write(&repo, "uv.lock", "version = 1\n");
        commit_all(&repo, "add project", None);
        assert_eq!(Lockfiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn repo_with_no_commits_but_renv_on_disk_flags_without_panicking() {
        let (_tmp, repo) = init_repo();
        write(&repo, "renv/activate.R", "# renv\n");
        let findings = Lockfiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn plain_directory_yields_a_git_error_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let findings = Lockfiles.run(&ctx(tmp.path()));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
    }
}
