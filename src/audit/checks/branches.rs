//! `stale-branches`: local branches that are fully merged into `HEAD`, or
//! whose last commit is older than the configured threshold
//! (`[audit] stale-days`, default 90).
//!
//! The current branch and the default branch (`main`/`master`) are never
//! flagged: they are not deletable cruft, whatever their age.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Check, Finding, Severity, git, git_failed_finding, git_maybe};
use crate::audit::AuditCtx;

pub(super) struct StaleBranches;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

impl Check for StaleBranches {
    fn id(&self) -> &str {
        "stale-branches"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let refs = match git(
            &ctx.repo,
            &[
                "for-each-ref",
                "refs/heads",
                "--format=%(refname:short)%00%(committerdate:unix)",
            ],
        ) {
            Ok(refs) => refs,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };

        // Fails when HEAD is detached; then no branch is "current".
        let current = git_maybe(&ctx.repo, &["symbolic-ref", "--short", "-q", "HEAD"])
            .map(|out| out.trim().to_string());

        // Fails when HEAD has no commits yet; then nothing is merged.
        let merged: HashSet<String> = git_maybe(
            &ctx.repo,
            &[
                "for-each-ref",
                "refs/heads",
                "--merged",
                "HEAD",
                "--format=%(refname:short)",
            ],
        )
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let stale_days = u64::from(ctx.config.audit.stale_days);

        let mut findings = Vec::new();
        for line in refs.lines() {
            let Some((name, epoch)) = line.split_once('\0') else {
                continue;
            };
            if Some(name) == current.as_deref() || name == "main" || name == "master" {
                continue;
            }
            if merged.contains(name) {
                findings.push(Finding {
                    check_id: self.id().to_string(),
                    severity: Severity::Info,
                    message: format!("local branch `{name}` is fully merged"),
                    remediation: format!("delete it with `git branch -d {name}`"),
                });
                continue;
            }
            let Ok(epoch) = epoch.parse::<u64>() else {
                continue;
            };
            if now.saturating_sub(epoch) > stale_days * SECONDS_PER_DAY {
                findings.push(Finding {
                    check_id: self.id().to_string(),
                    severity: Severity::Info,
                    message: format!(
                        "local branch `{name}` has had no commits in over {stale_days} days"
                    ),
                    remediation: format!(
                        "pick the work back up, or delete the branch with `git branch -D {name}`"
                    ),
                });
            }
        }
        findings
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{commit_all, ctx, git, init_repo, write};
    use super::*;

    /// Epoch seconds `days` ago, relative to the wall clock.
    fn days_ago(days: u64) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs()
            - days * SECONDS_PER_DAY
    }

    #[test]
    fn repo_with_only_a_fresh_main_branch_is_clean() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        assert_eq!(StaleBranches.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn fully_merged_branch_is_flagged_but_main_is_not() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        git(&repo, &["branch", "feature"]);

        let findings = StaleBranches.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].message.contains("feature"), "{findings:?}");
        assert!(
            findings[0].remediation.contains("git branch -d feature"),
            "{findings:?}"
        );
    }

    #[test]
    fn unmerged_branch_with_old_commits_is_stale_under_the_default_threshold() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        git(&repo, &["checkout", "--quiet", "-b", "old-work"]);
        write(&repo, "b.txt", "b\n");
        commit_all(&repo, "old work", Some(days_ago(100)));
        git(&repo, &["checkout", "--quiet", "main"]);

        let findings = StaleBranches.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("old-work"), "{findings:?}");
        assert!(findings[0].message.contains("90"), "{findings:?}");
    }

    #[test]
    fn stale_threshold_comes_from_config() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        git(&repo, &["checkout", "--quiet", "-b", "old-work"]);
        write(&repo, "b.txt", "b\n");
        commit_all(&repo, "old work", Some(days_ago(100)));
        git(&repo, &["checkout", "--quiet", "main"]);

        let mut ctx = ctx(&repo);
        ctx.config.audit.stale_days = 365;
        assert_eq!(StaleBranches.run(&ctx), Vec::new());
    }

    #[test]
    fn recent_unmerged_branch_is_not_stale() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        git(&repo, &["checkout", "--quiet", "-b", "fresh-work"]);
        write(&repo, "b.txt", "b\n");
        commit_all(&repo, "fresh work", None);
        git(&repo, &["checkout", "--quiet", "main"]);
        assert_eq!(StaleBranches.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn the_current_branch_is_never_flagged_even_when_old() {
        let (_tmp, repo) = init_repo();
        git(&repo, &["checkout", "--quiet", "-b", "solo-work"]);
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "long ago", Some(days_ago(400)));
        assert_eq!(StaleBranches.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn detached_head_does_not_panic_and_spares_main() {
        let (_tmp, repo) = init_repo();
        write(&repo, "a.txt", "a\n");
        commit_all(&repo, "add a", None);
        git(&repo, &["checkout", "--quiet", "--detach"]);
        assert_eq!(StaleBranches.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn repo_with_no_commits_yet_is_clean_not_a_panic() {
        let (_tmp, repo) = init_repo();
        assert_eq!(StaleBranches.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn plain_directory_yields_a_git_error_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let findings = StaleBranches.run(&ctx(tmp.path()));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
    }
}
