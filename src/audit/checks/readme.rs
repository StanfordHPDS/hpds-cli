//! `readme`: the repo has a README.
//!
//! The check deliberately treats README contents as opaque. Projects vary
//! too much for the audit to prescribe headings, prose, or a rendering model.

use super::{Check, Finding, Severity};
use crate::audit::AuditCtx;

pub(super) struct Readme;

/// README files the check recognizes.
const README_NAMES: &[&str] = &["README.md", "README.qmd", "README.Rmd"];

impl Check for Readme {
    fn id(&self) -> &str {
        "readme"
    }

    /// Reads files only, never git — it runs even outside a repository.
    fn needs_repo(&self) -> bool {
        false
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        if README_NAMES
            .iter()
            .any(|name| ctx.repo.join(name).is_file())
        {
            Vec::new()
        } else {
            vec![Finding {
                check_id: self.id().to_string(),
                severity: Severity::Error,
                message: "the repo has no README".to_string(),
                remediation: "add one, for example with `hpds use readme`".to_string(),
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ctx, init_repo, write};
    use super::*;

    #[test]
    fn missing_readme_is_an_error_pointing_at_hpds_use() {
        let (_tmp, repo) = init_repo();
        let findings = Readme.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("README"), "{findings:?}");
        assert!(
            findings[0].remediation.contains("hpds use readme"),
            "{findings:?}"
        );
    }

    #[test]
    fn any_recognized_readme_passes_regardless_of_contents() {
        for name in README_NAMES {
            let (_tmp, repo) = init_repo();
            write(&repo, name, "anything at all\n");
            assert_eq!(Readme.run(&ctx(&repo)), Vec::new(), "{name}");
        }
    }

    #[test]
    fn works_outside_a_git_repo_because_it_never_calls_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("README.md"), "notes\n").expect("write");
        assert_eq!(Readme.run(&ctx(tmp.path())), Vec::new());
    }
}
