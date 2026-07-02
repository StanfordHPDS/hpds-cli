//! `readme`: the repo has a README carrying the lab-manual minimum
//! sections. The section list is the one the readme template component
//! generates, so the two can never drift apart.

use super::{Check, Finding, Severity};
use crate::audit::AuditCtx;
use crate::templates::components::readme::LAB_MANUAL_SECTIONS;

pub(super) struct Readme;

/// README files the check recognizes, most-authoritative first (the `.md`
/// is a rendered artifact of the other two, so sources win).
const README_NAMES: &[&str] = &["README.qmd", "README.Rmd", "README.md"];

impl Check for Readme {
    fn id(&self) -> &str {
        "readme"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let Some(name) = README_NAMES
            .iter()
            .find(|name| ctx.repo.join(name).is_file())
        else {
            return vec![Finding {
                check_id: self.id().to_string(),
                severity: Severity::Error,
                message: "the repo has no README".to_string(),
                remediation: "add one with `hpds use readme` and fill in the lab-manual \
                              sections"
                    .to_string(),
            }];
        };

        let text = match std::fs::read_to_string(ctx.repo.join(name)) {
            Ok(text) => text,
            Err(err) => {
                return vec![Finding {
                    check_id: self.id().to_string(),
                    severity: Severity::Error,
                    message: format!("could not read `{name}`: {err}"),
                    remediation: "check the file's permissions".to_string(),
                }];
            }
        };

        let missing: Vec<&str> = LAB_MANUAL_SECTIONS
            .iter()
            .copied()
            .filter(|section| !has_heading(&text, section))
            .collect();
        if missing.is_empty() {
            return Vec::new();
        }
        vec![Finding {
            check_id: self.id().to_string(),
            severity: Severity::Warn,
            message: format!(
                "`{name}` is missing the lab-manual sections: {}",
                missing.join(", ")
            ),
            remediation: "add the missing `## <section>` headings (`hpds use readme` \
                          generates the full structure)"
                .to_string(),
        }]
    }
}

/// Does the text contain a Markdown heading (any level) whose text is
/// `section`, case-insensitively?
fn has_heading(text: &str, section: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let stripped = line.trim_start_matches('#');
        line.len() > stripped.len() && stripped.trim().eq_ignore_ascii_case(section)
    })
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{ctx, init_repo, write};
    use super::*;

    fn full_readme() -> String {
        let mut text = String::from("# demo\n");
        for section in LAB_MANUAL_SECTIONS {
            text.push_str(&format!("\n## {section}\n\ncontent\n"));
        }
        text
    }

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
    fn readme_with_all_sections_passes_even_uncommitted() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.md", &full_readme());
        assert_eq!(Readme.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn missing_sections_warn_and_are_named() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.md", "# demo\n\n## Description\n\nwords\n");
        let findings = Readme.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(!findings[0].message.contains("Description"), "{findings:?}");
        for section in ["File structure", "How to run", "Dependencies"] {
            assert!(findings[0].message.contains(section), "{findings:?}");
        }
    }

    #[test]
    fn heading_matching_ignores_case_and_level() {
        let (_tmp, repo) = init_repo();
        write(
            &repo,
            "README.md",
            "# DESCRIPTION\n### file structure\n## How to run\n#Dependencies\n",
        );
        assert_eq!(Readme.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn section_names_in_prose_do_not_count_as_headings() {
        let (_tmp, repo) = init_repo();
        write(
            &repo,
            "README.md",
            "# demo\nDescription, File structure, How to run and Dependencies are \
             all explained elsewhere.\n",
        );
        let findings = Readme.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn a_qmd_source_outranks_a_bare_md_next_to_it() {
        let (_tmp, repo) = init_repo();
        // The .qmd is complete; the rendered .md being incomplete is
        // stale-artifacts' business, not this check's.
        write(&repo, "README.qmd", &full_readme());
        write(&repo, "README.md", "# demo\n");
        assert_eq!(Readme.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn works_outside_a_git_repo_because_it_never_calls_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("README.md"), full_readme()).expect("write");
        assert_eq!(Readme.run(&ctx(tmp.path())), Vec::new());
    }
}
