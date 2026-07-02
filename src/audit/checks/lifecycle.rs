//! `lifecycle-metadata`: the repo's `hpds.toml` declares its lifecycle —
//! a `[project]` table with a valid `status` and a `primary-author`.
//!
//! The check reads the repo's own `hpds.toml` (not the layered config),
//! because a status inherited from defaults or user config is exactly the
//! problem it exists to catch.

use super::{Check, Finding, Severity};
use crate::audit::AuditCtx;
use crate::config::PROJECT_STATUSES;

pub(super) struct LifecycleMetadata;

impl Check for LifecycleMetadata {
    fn id(&self) -> &str {
        "lifecycle-metadata"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let path = ctx.repo.join("hpds.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return vec![self.finding(
                    "the repo has no hpds.toml",
                    "create hpds.toml with a [project] table setting `status` and \
                     `primary-author`",
                )];
            }
            Err(err) => {
                return vec![self.finding(
                    &format!("could not read hpds.toml: {err}"),
                    "check the file's permissions",
                )];
            }
        };

        let layer = match crate::config::raw::parse(&text) {
            Ok(parsed) => parsed.layer,
            Err(err) => {
                return vec![self.finding(
                    &format!("hpds.toml could not be parsed: {err:#}"),
                    "fix the TOML so the [project] lifecycle metadata is readable",
                )];
            }
        };

        let mut findings = Vec::new();
        match layer.project_status.as_deref() {
            None => findings.push(self.finding(
                "hpds.toml does not set `project.status`",
                &format!(
                    "add `status = \"active\"` (one of {}) under [project] in hpds.toml",
                    PROJECT_STATUSES.join(" | ")
                ),
            )),
            Some(status) if !PROJECT_STATUSES.contains(&status) => {
                findings.push(self.finding(
                    &format!("`{status}` is not a valid `project.status` in hpds.toml"),
                    &format!("set status to one of {}", PROJECT_STATUSES.join(" | ")),
                ));
            }
            Some(_) => {}
        }
        if layer
            .project_primary_author
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            findings.push(self.finding(
                "hpds.toml does not set `project.primary-author`",
                "add `primary-author = \"<github-username>\"` under [project] in hpds.toml",
            ));
        }
        findings
    }
}

impl LifecycleMetadata {
    /// Every lifecycle problem is an error: the audit bot and the GitHub
    /// checks depend on this metadata being present and valid.
    fn finding(&self, message: &str, remediation: &str) -> Finding {
        Finding {
            check_id: self.id().to_string(),
            severity: Severity::Error,
            message: message.to_string(),
            remediation: remediation.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ctx, init_repo, write};
    use super::*;

    #[test]
    fn complete_metadata_passes_for_every_valid_status() {
        for status in PROJECT_STATUSES {
            let (_tmp, repo) = init_repo();
            write(
                &repo,
                "hpds.toml",
                &format!("[project]\nstatus = \"{status}\"\nprimary-author = \"malcolm\"\n"),
            );
            assert_eq!(LifecycleMetadata.run(&ctx(&repo)), Vec::new(), "{status}");
        }
    }

    #[test]
    fn missing_hpds_toml_is_one_error() {
        let (_tmp, repo) = init_repo();
        let findings = LifecycleMetadata.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("hpds.toml"), "{findings:?}");
        assert!(
            findings[0].remediation.contains("[project]"),
            "{findings:?}"
        );
    }

    #[test]
    fn missing_status_and_author_are_reported_separately() {
        let (_tmp, repo) = init_repo();
        write(&repo, "hpds.toml", "[format]\nlanguages = [\"r\"]\n");
        let findings = LifecycleMetadata.run(&ctx(&repo));
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].message.contains("status"), "{findings:?}");
        assert!(
            findings[1].message.contains("primary-author"),
            "{findings:?}"
        );
        for finding in &findings {
            assert_eq!(finding.severity, Severity::Error);
        }
    }

    #[test]
    fn invalid_status_is_an_error_listing_the_valid_ones() {
        let (_tmp, repo) = init_repo();
        write(
            &repo,
            "hpds.toml",
            "[project]\nstatus = \"frozen\"\nprimary-author = \"malcolm\"\n",
        );
        let findings = LifecycleMetadata.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("frozen"), "{findings:?}");
        assert!(findings[0].remediation.contains("retired"), "{findings:?}");
    }

    #[test]
    fn blank_primary_author_counts_as_missing() {
        let (_tmp, repo) = init_repo();
        write(
            &repo,
            "hpds.toml",
            "[project]\nstatus = \"active\"\nprimary-author = \"  \"\n",
        );
        let findings = LifecycleMetadata.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains("primary-author"),
            "{findings:?}"
        );
    }

    #[test]
    fn unparseable_hpds_toml_is_an_error_not_a_panic() {
        let (_tmp, repo) = init_repo();
        write(&repo, "hpds.toml", "not valid toml [\n");
        let findings = LifecycleMetadata.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("parsed"), "{findings:?}");
    }

    #[test]
    fn works_outside_a_git_repo_because_it_never_calls_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let findings = LifecycleMetadata.run(&ctx(tmp.path()));
        assert_eq!(findings.len(), 1, "{findings:?}");
    }
}
