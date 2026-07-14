//! `junk-files`: committed files that should never be in a repo: editor
//! and session droppings, caches, and secrets-looking files.
//!
//! The pattern list lives in exactly one place: [`JUNK_PATTERNS`].

use super::{Check, Finding, Severity, git_failed_finding, tracked_files};
use crate::audit::AuditCtx;

pub(super) struct JunkFiles;

/// How one junk pattern recognizes a committed path.
enum Matcher {
    /// The file's name is exactly this.
    Basename(&'static str),
    /// Any directory component of the path is exactly this.
    Dir(&'static str),
    /// The file's name ends with this suffix (and is longer than it).
    Suffix(&'static str),
}

/// One junk pattern: how to match, how bad it is, and what the file is.
struct JunkPattern {
    matcher: Matcher,
    severity: Severity,
    label: &'static str,
}

/// Everything the check flags. Secrets-looking files are errors; the rest
/// is hygiene.
const JUNK_PATTERNS: &[JunkPattern] = &[
    JunkPattern {
        matcher: Matcher::Basename(".DS_Store"),
        severity: Severity::Warn,
        label: "macOS Finder junk",
    },
    JunkPattern {
        matcher: Matcher::Basename(".Rhistory"),
        severity: Severity::Warn,
        label: "R session history",
    },
    JunkPattern {
        matcher: Matcher::Basename(".RData"),
        severity: Severity::Warn,
        label: "R workspace image",
    },
    JunkPattern {
        matcher: Matcher::Dir("__pycache__"),
        severity: Severity::Warn,
        label: "Python bytecode cache",
    },
    JunkPattern {
        matcher: Matcher::Dir(".ipynb_checkpoints"),
        severity: Severity::Warn,
        label: "Jupyter checkpoint",
    },
    JunkPattern {
        matcher: Matcher::Suffix(".pem"),
        severity: Severity::Error,
        label: "looks like a private key",
    },
    JunkPattern {
        matcher: Matcher::Basename(".env"),
        severity: Severity::Error,
        label: "looks like an environment/secrets file",
    },
];

impl Check for JunkFiles {
    fn id(&self) -> &str {
        "junk-files"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let tracked = match tracked_files(&ctx.repo) {
            Ok(tracked) => tracked,
            Err(err) => return vec![git_failed_finding(self.id(), &err)],
        };
        let mut findings = Vec::new();
        for path in &tracked {
            let Some(pattern) = JUNK_PATTERNS
                .iter()
                .find(|pattern| matches(&pattern.matcher, path))
            else {
                continue;
            };
            let remediation = match pattern.severity {
                Severity::Error => format!(
                    "run `git rm --cached {path}`, add it to .gitignore, and rotate the \
                     secret if the repo was ever pushed"
                ),
                _ => format!(
                    "run `git rm --cached {path}` and ignore it \
                     (`hpds git vaccinate --project` adds the common patterns)"
                ),
            };
            findings.push(Finding {
                check_id: self.id().to_string(),
                severity: pattern.severity,
                message: format!("`{path}` is committed ({})", pattern.label),
                remediation,
            });
        }
        findings
    }
}

/// Does `path` (slash-separated, repo-relative) match this pattern?
fn matches(matcher: &Matcher, path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match matcher {
        Matcher::Basename(base) => name == *base,
        Matcher::Dir(dir) => path.split('/').rev().skip(1).any(|part| part == *dir),
        Matcher::Suffix(suffix) => name.len() > suffix.len() && name.ends_with(suffix),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{commit_all, ctx, init_repo, write};
    use super::*;

    #[test]
    fn clean_repo_has_no_junk_findings() {
        let (_tmp, repo) = init_repo();
        write(&repo, "analysis.R", "1 + 1\n");
        write(&repo, "envelope.txt", "not a .env file\n");
        commit_all(&repo, "add code", None);
        assert_eq!(JunkFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn committed_session_droppings_warn_wherever_they_are() {
        let (_tmp, repo) = init_repo();
        write(&repo, ".DS_Store", "x");
        write(&repo, "data/.DS_Store", "x");
        write(&repo, ".Rhistory", "x");
        write(&repo, ".RData", "x");
        commit_all(&repo, "oops", None);

        let findings = JunkFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 4, "{findings:?}");
        for finding in &findings {
            assert_eq!(finding.severity, Severity::Warn);
            assert_eq!(finding.check_id, "junk-files");
            assert!(
                finding.remediation.contains("git rm --cached"),
                "{finding:?}"
            );
        }
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("data/.DS_Store")),
            "{findings:?}"
        );
    }

    #[test]
    fn cache_directories_match_on_any_path_component() {
        let (_tmp, repo) = init_repo();
        write(&repo, "src/__pycache__/mod.cpython-312.pyc", "x");
        write(
            &repo,
            "notebooks/.ipynb_checkpoints/nb-checkpoint.ipynb",
            "x",
        );
        commit_all(&repo, "oops", None);

        let findings = JunkFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 2, "{findings:?}");
    }

    #[test]
    fn a_file_merely_named_like_a_cache_dir_is_not_junk() {
        let (_tmp, repo) = init_repo();
        write(&repo, "notes/__pycache__", "a FILE named like the dir\n");
        commit_all(&repo, "odd name", None);
        assert_eq!(JunkFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn rendered_outputs_are_not_junk() {
        let (_tmp, repo) = init_repo();
        write(&repo, "report.qmd", "src\n");
        write(&repo, "report.html", "rendered\n");
        write(&repo, "report.pdf", "rendered\n");
        write(&repo, "standalone.html", "hand-written\n");
        commit_all(&repo, "add", None);

        assert_eq!(JunkFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn committed_secrets_are_errors_with_rotation_advice() {
        let (_tmp, repo) = init_repo();
        write(&repo, "deploy/key.pem", "SECRET");
        write(&repo, ".env", "TOKEN=x\n");
        commit_all(&repo, "oops", None);

        let findings = JunkFiles.run(&ctx(&repo));
        assert_eq!(findings.len(), 2, "{findings:?}");
        for finding in &findings {
            assert_eq!(finding.severity, Severity::Error);
            assert!(finding.remediation.contains("rotate"), "{finding:?}");
        }
    }

    #[test]
    fn a_bare_pem_extension_alone_is_not_a_match() {
        let (_tmp, repo) = init_repo();
        // A file literally named ".pem" has no stem; leave it alone.
        write(&repo, ".pem", "odd\n");
        commit_all(&repo, "odd", None);
        assert_eq!(JunkFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn untracked_junk_on_disk_is_not_reported() {
        let (_tmp, repo) = init_repo();
        write(&repo, "kept.txt", "x\n");
        commit_all(&repo, "add", None);
        write(&repo, ".DS_Store", "x");
        write(&repo, ".env", "TOKEN=x\n");
        assert_eq!(JunkFiles.run(&ctx(&repo)), Vec::new());
    }

    #[test]
    fn plain_directory_yields_a_git_error_finding() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let findings = JunkFiles.run(&ctx(tmp.path()));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Error);
    }
}
