//! `gitignore-hygiene`: the repo's `.gitignore` is missing vaccinate
//! patterns for the languages the repo actually uses. The pattern lists
//! are the ones `hpds git vaccinate` writes, defined once, in `gitx`.

use std::collections::HashSet;

use super::{Check, Finding, Severity, git_failed_finding, tracked_files};
use crate::audit::AuditCtx;
use crate::gitx::{PYTHON_PATTERNS, R_PATTERNS};

pub(super) struct GitignoreHygiene;

impl Check for GitignoreHygiene {
    fn id(&self) -> &str {
        "gitignore-hygiene"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let tracked = match tracked_files(&ctx.repo) {
            Ok(tracked) => tracked,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };

        let languages: Vec<(&str, &[&str])> = [
            ("R", R_PATTERNS, tracked.iter().any(|p| is_r_file(p))),
            (
                "Python",
                PYTHON_PATTERNS,
                tracked.iter().any(|p| is_python_file(p)),
            ),
        ]
        .into_iter()
        .filter_map(|(name, patterns, detected)| detected.then_some((name, patterns)))
        .collect();
        if languages.is_empty() {
            return Vec::new();
        }

        let gitignore = std::fs::read_to_string(ctx.repo.join(".gitignore")).unwrap_or_default();
        // Trailing-slash-insensitive: `__pycache__` covers `__pycache__/`.
        let present: HashSet<&str> = gitignore
            .lines()
            .map(|line| line.trim().trim_end_matches('/'))
            .collect();

        let mut findings = Vec::new();
        for (language, patterns) in languages {
            let missing: Vec<&str> = patterns
                .iter()
                .copied()
                .filter(|pattern| !present.contains(pattern.trim_end_matches('/')))
                .collect();
            if missing.is_empty() {
                continue;
            }
            findings.push(Finding {
                check_id: self.id().to_string(),
                severity: Severity::Warn,
                message: format!(
                    ".gitignore is missing the recommended {language} patterns: {}",
                    missing.join(", ")
                ),
                remediation: "run `hpds git vaccinate --project` to add them".to_string(),
            });
        }
        findings
    }
}

/// The extension of a slash-separated path's file name, if any.
fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, ext)| ext)
}

/// Does this tracked path indicate the repo uses R?
fn is_r_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = extension(path).unwrap_or("");
    name == "renv.lock"
        || name == "DESCRIPTION"
        || ext.eq_ignore_ascii_case("r")
        || ext.eq_ignore_ascii_case("rmd")
        || ext.eq_ignore_ascii_case("rproj")
}

/// Does this tracked path indicate the repo uses Python?
fn is_python_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "pyproject.toml"
        || name == "uv.lock"
        || name == "requirements.txt"
        || matches!(extension(path), Some("py" | "ipynb"))
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{commit_all, ctx, init_repo, write};
    use super::*;

    /// A `.gitignore` body containing every pattern in `patterns`.
    fn full_ignore(patterns: &[&str]) -> String {
        let mut body = patterns.join("\n");
        body.push('\n');
        body
    }

    #[test]
    fn repos_without_r_or_python_need_no_gitignore_at_all() {
        let (_tmp, repo) = init_repo();
        write(&repo, "notes.md", "hello\n");
        write(&repo, "query.sql", "select 1;\n");
        commit_all(&repo, "add docs", None);
        assert_eq!(GitignoreHygiene.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn r_repo_without_the_r_patterns_warns_and_names_them() {
        let (_tmp, repo) = init_repo();
        write(&repo, "analysis.R", "1 + 1\n");
        commit_all(&repo, "add analysis", None);

        let findings = GitignoreHygiene.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("R patterns"), "{findings:?}");
        assert!(findings[0].message.contains(".Rhistory"), "{findings:?}");
        assert!(
            findings[0]
                .remediation
                .contains("hpds git vaccinate --project"),
            "{findings:?}"
        );
    }

    #[test]
    fn r_repo_with_all_r_patterns_is_clean() {
        let (_tmp, repo) = init_repo();
        write(&repo, "analysis.R", "1 + 1\n");
        write(&repo, ".gitignore", &full_ignore(R_PATTERNS));
        commit_all(&repo, "add analysis", None);
        assert_eq!(GitignoreHygiene.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn python_repo_missing_patterns_warns_only_for_python() {
        let (_tmp, repo) = init_repo();
        write(&repo, "script.py", "print(1)\n");
        commit_all(&repo, "add script", None);

        let findings = GitignoreHygiene.run(&ctx(&repo));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains("Python patterns"),
            "{findings:?}"
        );
        assert!(findings[0].message.contains("__pycache__"), "{findings:?}");
    }

    #[test]
    fn trailing_slash_differences_do_not_count_as_missing() {
        let (_tmp, repo) = init_repo();
        write(&repo, "script.py", "print(1)\n");
        // Same patterns, but written without the trailing slashes.
        let body: String = PYTHON_PATTERNS
            .iter()
            .map(|p| format!("{}\n", p.trim_end_matches('/')))
            .collect();
        write(&repo, ".gitignore", &body);
        commit_all(&repo, "add script", None);
        assert_eq!(GitignoreHygiene.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn mixed_repo_missing_everything_warns_once_per_language() {
        let (_tmp, repo) = init_repo();
        write(&repo, "analysis.Rmd", "---\n---\n");
        write(&repo, "pyproject.toml", "[project]\nname = \"x\"\n");
        commit_all(&repo, "add both", None);

        let findings = GitignoreHygiene.run(&ctx(&repo));
        assert_eq!(findings.len(), 2, "{findings:?}");
    }

    #[test]
    fn plain_directory_yields_a_git_error_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let findings = GitignoreHygiene.run(&ctx(tmp.path()));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
    }
}
