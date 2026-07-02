//! Working-tree checks: uncommitted changes to tracked files and
//! untracked non-ignored files, both read from `git status --porcelain`.

use super::{Check, Finding, Severity, git, git_failed_finding, preview};
use crate::audit::AuditCtx;

/// One `git status --porcelain` pass, split into the two kinds of paths
/// the workspace checks care about.
struct StatusReport {
    dirty: Vec<String>,
    untracked: Vec<String>,
}

/// Parse `git status --porcelain`: `?? ` lines are untracked, everything
/// else is a change to a tracked file (staged or not). Renames report the
/// new name.
fn status(ctx: &AuditCtx) -> Result<StatusReport, super::GitError> {
    let out = git(&ctx.repo, &["status", "--porcelain"])?;
    let mut report = StatusReport {
        dirty: Vec::new(),
        untracked: Vec::new(),
    };
    for line in out.lines() {
        let Some(path) = line.get(3..).filter(|p| !p.is_empty()) else {
            continue;
        };
        let path = match path.split_once(" -> ") {
            Some((_old, new)) => new,
            None => path,
        };
        if line.starts_with("??") {
            report.untracked.push(path.to_string());
        } else {
            report.dirty.push(path.to_string());
        }
    }
    Ok(report)
}

/// `dirty-files`: tracked files with uncommitted (staged or unstaged)
/// changes.
pub(super) struct DirtyFiles;

impl Check for DirtyFiles {
    fn id(&self) -> &str {
        "dirty-files"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let report = match status(ctx) {
            Ok(report) => report,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };
        if report.dirty.is_empty() {
            return Vec::new();
        }
        vec![Finding {
            check_id: self.id().to_string(),
            severity: Severity::Warn,
            message: format!(
                "{} tracked {} uncommitted changes ({})",
                report.dirty.len(),
                if report.dirty.len() == 1 {
                    "file has"
                } else {
                    "files have"
                },
                preview(&report.dirty)
            ),
            remediation: "commit the changes (`git add` / `git commit`) or stash them \
                          (`git stash`)"
                .to_string(),
        }]
    }
}

/// `untracked-files`: files that are neither tracked nor ignored.
pub(super) struct UntrackedFiles;

impl Check for UntrackedFiles {
    fn id(&self) -> &str {
        "untracked-files"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let report = match status(ctx) {
            Ok(report) => report,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };
        if report.untracked.is_empty() {
            return Vec::new();
        }
        vec![Finding {
            check_id: self.id().to_string(),
            severity: Severity::Info,
            message: format!(
                "{} {} neither tracked nor ignored ({})",
                report.untracked.len(),
                if report.untracked.len() == 1 {
                    "file is"
                } else {
                    "files are"
                },
                preview(&report.untracked)
            ),
            remediation: "commit each file, add it to .gitignore, or delete it".to_string(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{commit_all, ctx, init_repo, write};
    use super::*;
    use crate::audit::Severity;

    #[test]
    fn clean_repo_has_no_workspace_findings() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        assert_eq!(DirtyFiles.run(&ctx(&repo)), Vec::new());
        assert_eq!(UntrackedFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn modified_tracked_file_is_dirty_not_untracked() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        write(&repo, "a.txt", "changed\n");

        let findings = DirtyFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].check_id, "dirty-files");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("a.txt"), "{findings:?}");
        assert!(findings[0].remediation.contains("git"), "{findings:?}");

        assert_eq!(UntrackedFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn staged_change_counts_as_dirty() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        write(&repo, "a.txt", "staged\n");
        super::super::testutil::git(&repo, &["add", "a.txt"]);
        let findings = DirtyFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn new_file_is_untracked_not_dirty() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        write(&repo, "notes.txt", "scratch\n");

        assert_eq!(DirtyFiles.run(&ctx(&repo)), Vec::new());
        let findings = UntrackedFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].check_id, "untracked-files");
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].message.contains("notes.txt"), "{findings:?}");
        assert!(
            findings[0].remediation.contains(".gitignore"),
            "{findings:?}"
        );
    }

    #[test]
    fn ignored_files_are_not_reported() {
        let (_tmp, repo) = init_repo();
        write(&repo, ".gitignore", "scratch/\n");
        commit_all(&repo, "ignore scratch", None);
        write(&repo, "scratch/tmp.txt", "tmp\n");
        assert_eq!(UntrackedFiles.run(&ctx(&repo)), Vec::new());
        assert_eq!(DirtyFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn repo_with_no_commits_reports_untracked_without_panicking() {
        let (_tmp, repo) = init_repo();
        write(&repo, "notes.txt", "scratch\n");
        assert_eq!(DirtyFiles.run(&ctx(&repo)), Vec::new());
        let findings = UntrackedFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn many_dirty_files_fold_into_one_finding() {
        let (_tmp, repo) = init_repo();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
            write(&repo, name, "x\n");
        }
        commit_all(&repo, "add files", None);
        for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
            write(&repo, name, "changed\n");
        }
        let findings = DirtyFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("5"), "{findings:?}");
        assert!(findings[0].message.contains("more"), "{findings:?}");
    }

    #[test]
    fn plain_directory_yields_a_git_error_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for finding in [
            DirtyFiles.run(&ctx(tmp.path())),
            UntrackedFiles.run(&ctx(tmp.path())),
        ] {
            assert_eq!(finding.len(), 1, "{finding:?}");
            assert_eq!(finding[0].severity, Severity::Error);
            assert!(finding[0].remediation.contains("git"), "{finding:?}");
        }
    }
}
