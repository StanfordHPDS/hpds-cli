//! `stale-artifacts`: committed rendered outputs older than their sources —
//! `README.md` older than a `README.qmd`/`README.Rmd` next to it, and
//! committed `.html`/`.pdf` older than the same-stem `.qmd` beside them.
//!
//! "Older" compares last-commit times, so the check works on fresh clones
//! where file mtimes say nothing.

use std::collections::HashSet;

use super::{Check, Finding, Severity, git_failed_finding, last_commit_epoch, tracked_files};
use crate::audit::AuditCtx;

pub(super) struct StaleArtifacts;

impl Check for StaleArtifacts {
    fn id(&self) -> &str {
        "stale-artifacts"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let tracked = match tracked_files(&ctx.repo) {
            Ok(tracked) => tracked,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };
        let tracked_set: HashSet<&str> = tracked.iter().map(String::as_str).collect();

        let mut findings = Vec::new();
        for output in &tracked {
            let (dir, name) = split_dir(output);
            let sources: Vec<String> = if name == "README.md" {
                ["README.qmd", "README.Rmd"]
                    .iter()
                    .map(|src| format!("{dir}{src}"))
                    .collect()
            } else if let Some(stem) = name.strip_suffix(".html").or(name.strip_suffix(".pdf")) {
                vec![format!("{dir}{stem}.qmd")]
            } else {
                continue;
            };
            for source in sources {
                if !tracked_set.contains(source.as_str()) {
                    continue;
                }
                if let Some(finding) = self.compare(ctx, output, &source) {
                    findings.push(finding);
                }
            }
        }
        findings
    }
}

impl StaleArtifacts {
    /// A finding when `source` was committed more recently than `output`.
    fn compare(&self, ctx: &AuditCtx, output: &str, source: &str) -> Option<Finding> {
        let output_at = last_commit_epoch(&ctx.repo, output)?;
        let source_at = last_commit_epoch(&ctx.repo, source)?;
        (source_at > output_at).then(|| Finding {
            check_id: self.id().to_string(),
            severity: Severity::Warn,
            message: format!("`{output}` is older than its source `{source}`"),
            remediation: format!("re-render it (`quarto render {source}`) and commit the result"),
        })
    }
}

/// Split a slash-separated path into its directory prefix (empty or ending
/// in `/`) and file name.
fn split_dir(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (&path[..dir.len() + 1], name),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{commit_all, ctx, init_repo, write};
    use super::*;

    const T1: u64 = 1_600_000_000;
    const T2: u64 = 1_600_100_000;
    const T3: u64 = 1_600_200_000;

    #[test]
    fn split_dir_handles_root_and_nested_paths() {
        assert_eq!(split_dir("README.md"), ("", "README.md"));
        assert_eq!(split_dir("a/b/paper.html"), ("a/b/", "paper.html"));
    }

    #[test]
    fn outputs_committed_with_their_sources_are_fresh() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.qmd", "---\n---\n");
        write(&repo, "README.md", "rendered\n");
        write(&repo, "report.qmd", "---\n---\n");
        write(&repo, "report.html", "<html></html>\n");
        commit_all(&repo, "add all", Some(T1));
        assert_eq!(StaleArtifacts.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn readme_md_older_than_readme_qmd_is_stale() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.qmd", "v1\n");
        write(&repo, "README.md", "rendered v1\n");
        commit_all(&repo, "add readme", Some(T1));
        write(&repo, "README.qmd", "v2\n");
        commit_all(&repo, "edit source only", Some(T2));

        let findings = StaleArtifacts.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("README.md"), "{findings:?}");
        assert!(findings[0].message.contains("README.qmd"), "{findings:?}");
        assert!(
            findings[0].remediation.contains("quarto render README.qmd"),
            "{findings:?}"
        );
    }

    #[test]
    fn re_rendering_the_readme_clears_the_finding() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.qmd", "v1\n");
        write(&repo, "README.md", "rendered v1\n");
        commit_all(&repo, "add readme", Some(T1));
        write(&repo, "README.qmd", "v2\n");
        commit_all(&repo, "edit source", Some(T2));
        write(&repo, "README.md", "rendered v2\n");
        commit_all(&repo, "re-render", Some(T3));
        assert_eq!(StaleArtifacts.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn readme_rmd_counts_as_a_readme_source() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.Rmd", "v1\n");
        write(&repo, "README.md", "rendered v1\n");
        commit_all(&repo, "add readme", Some(T1));
        write(&repo, "README.Rmd", "v2\n");
        commit_all(&repo, "edit source", Some(T2));

        let findings = StaleArtifacts.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].message.contains("README.Rmd"), "{findings:?}");
    }

    #[test]
    fn html_and_pdf_older_than_their_qmd_are_stale_in_subdirs_too() {
        let (_tmp, repo) = init_repo();
        write(&repo, "reports/paper.qmd", "v1\n");
        write(&repo, "reports/paper.html", "html v1\n");
        write(&repo, "reports/paper.pdf", "pdf v1\n");
        commit_all(&repo, "add report", Some(T1));
        write(&repo, "reports/paper.qmd", "v2\n");
        commit_all(&repo, "edit source", Some(T2));

        let mut findings = StaleArtifacts.run(&ctx(&repo));
        findings.sort_by(|a, b| a.message.cmp(&b.message));
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings[0].message.contains("reports/paper.html"),
            "{findings:?}"
        );
        assert!(
            findings[1].message.contains("reports/paper.pdf"),
            "{findings:?}"
        );
    }

    #[test]
    fn outputs_without_a_committed_source_are_ignored() {
        let (_tmp, repo) = init_repo();
        write(&repo, "slides.html", "standalone\n");
        write(&repo, "README.md", "plain readme\n");
        commit_all(&repo, "add files", Some(T1));
        // A source that exists on disk but is not committed does not count.
        write(&repo, "README.qmd", "uncommitted\n");
        assert_eq!(StaleArtifacts.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn sources_are_only_matched_in_the_same_directory() {
        let (_tmp, repo) = init_repo();
        write(&repo, "paper.qmd", "v1\n");
        write(&repo, "out/paper.html", "html\n");
        commit_all(&repo, "add", Some(T1));
        write(&repo, "paper.qmd", "v2\n");
        commit_all(&repo, "edit", Some(T2));
        // out/paper.html has no out/paper.qmd next to it.
        assert_eq!(StaleArtifacts.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn repo_with_no_commits_is_clean_not_a_panic() {
        let (_tmp, repo) = init_repo();
        write(&repo, "README.md", "x\n");
        assert_eq!(StaleArtifacts.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn plain_directory_yields_a_git_error_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let findings = StaleArtifacts.run(&ctx(tmp.path()));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
    }
}
