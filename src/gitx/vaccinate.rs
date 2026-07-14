//! `hpds git vaccinate`: append R/Python/editor junk patterns to
//! the global git ignore (or the repo's `.gitignore` with `--project`),
//! idempotently via a marker block.

use std::path::{Path, PathBuf};

use super::{GitxError, git_expect_success, git_output, global_config_get, home_dir};

/// Markers delimiting the hpds-managed block; re-runs never duplicate it.
const MARKER_BEGIN: &str = "# >>> hpds vaccinate >>>";
const MARKER_END: &str = "# <<< hpds vaccinate <<<";

/// R ignore patterns (usethis::git_vaccinate). Also consumed by the audit's
/// gitignore-hygiene check, so the list lives in exactly one place.
pub(crate) const R_PATTERNS: &[&str] = &[
    ".Rhistory",
    ".RData",
    ".Rproj.user",
    ".Rdata",
    ".httr-oauth",
    ".DS_Store",
];

/// Curated Python ignore patterns (from github/gitignore Python.gitignore).
/// Also consumed by the audit's gitignore-hygiene check.
pub(crate) const PYTHON_PATTERNS: &[&str] = &[
    "__pycache__/",
    "*.py[cod]",
    ".venv/",
    ".ipynb_checkpoints/",
    ".env",
    "*.egg-info/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
];

/// General editor junk, vaccinated but not language-specific.
const EDITOR_PATTERNS: &[&str] = &["*.swp", "*.swo", "*~", ".idea/", ".vscode/", "Thumbs.db"];

/// Every pattern vaccination manages, in the order it writes them.
fn all_patterns() -> impl Iterator<Item = &'static str> {
    R_PATTERNS
        .iter()
        .chain(PYTHON_PATTERNS)
        .chain(EDITOR_PATTERNS)
        .copied()
}

/// What a vaccination run did, for the caller to render via `ui/`.
#[derive(Debug)]
pub struct VaccinateReport {
    /// The ignore file that was (or would have been) updated.
    pub path: PathBuf,
    /// Patterns appended by this run.
    pub added: Vec<&'static str>,
    /// Patterns that were already present and therefore skipped.
    pub already_present: Vec<&'static str>,
    /// True when this run set `core.excludesFile` (it was previously unset).
    pub set_excludes_file: bool,
}

/// Vaccinate the global git ignore: resolve `core.excludesFile`, defaulting
/// to `~/.gitignore` (and setting the config) when unset.
pub fn vaccinate_global() -> Result<VaccinateReport, GitxError> {
    let (path, set_excludes_file) = resolve_global_excludes_file()?;
    let mut report = vaccinate_file(&path)?;
    report.set_excludes_file = set_excludes_file;
    Ok(report)
}

/// Vaccinate the current repository's root `.gitignore`.
pub fn vaccinate_project() -> Result<VaccinateReport, GitxError> {
    let output = git_output(&["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err(GitxError::NotARepo);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        // e.g. inside a bare repo or .git dir: no work tree to vaccinate.
        return Err(GitxError::NotARepo);
    }
    vaccinate_file(&PathBuf::from(root).join(".gitignore"))
}

/// Resolve the global excludes file, setting `core.excludesFile` to
/// `~/.gitignore` when unset. Returns the path and whether we set the config.
fn resolve_global_excludes_file() -> Result<(PathBuf, bool), GitxError> {
    if let Some(value) = global_config_get("core.excludesFile")? {
        return Ok((expand_tilde(&value, &home_dir()?), false));
    }
    let path = home_dir()?.join(".gitignore");
    git_expect_success(&[
        "config".as_ref(),
        "--global".as_ref(),
        "core.excludesFile".as_ref(),
        path.as_os_str(),
    ])?;
    Ok((path, true))
}

/// Expand a leading `~` (git allows `core.excludesFile = ~/.gitignore`).
/// `~user` forms are left as-is.
fn expand_tilde(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    }
}

/// Append the missing patterns to `path` inside the marker block, creating
/// the file (and parent directories) if needed.
fn vaccinate_file(path: &Path) -> Result<VaccinateReport, GitxError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(GitxError::ReadIgnore {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let outcome = apply_patterns(&content);
    if !outcome.added.is_empty() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| GitxError::WriteIgnore {
                path: path.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(path, &outcome.content).map_err(|source| GitxError::WriteIgnore {
            path: path.to_path_buf(),
            source,
        })?;
    }

    Ok(VaccinateReport {
        path: path.to_path_buf(),
        added: outcome.added,
        already_present: outcome.already_present,
        set_excludes_file: false,
    })
}

/// Result of applying the patterns to ignore-file content, before any I/O.
struct ApplyOutcome {
    content: String,
    added: Vec<&'static str>,
    already_present: Vec<&'static str>,
}

/// Pure core of vaccination: add every missing pattern to `content` inside
/// the marker block. Patterns already present anywhere in the file (any
/// line, trimmed) are skipped, so re-running never duplicates.
fn apply_patterns(content: &str) -> ApplyOutcome {
    let existing: std::collections::HashSet<&str> = content.lines().map(str::trim).collect();
    let mut added = Vec::new();
    let mut already_present = Vec::new();
    for pattern in all_patterns() {
        if existing.contains(pattern) {
            already_present.push(pattern);
        } else {
            added.push(pattern);
        }
    }

    if added.is_empty() {
        return ApplyOutcome {
            content: content.to_string(),
            added,
            already_present,
        };
    }

    let content = if existing.contains(MARKER_END) {
        // A previous block exists: insert the missing patterns before its
        // end marker instead of appending a second block.
        let mut lines: Vec<&str> = content.lines().collect();
        let end = lines
            .iter()
            .position(|line| line.trim() == MARKER_END)
            .expect("end marker is in `existing`");
        lines.splice(end..end, added.iter().copied());
        let mut rebuilt = lines.join("\n");
        rebuilt.push('\n');
        rebuilt
    } else {
        let mut out = content.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(MARKER_BEGIN);
        out.push('\n');
        for pattern in &added {
            out.push_str(pattern);
            out.push('\n');
        }
        out.push_str(MARKER_END);
        out.push('\n');
        out
    };

    ApplyOutcome {
        content,
        added,
        already_present,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_count(content: &str, needle: &str) -> usize {
        content.lines().filter(|l| l.trim() == needle).count()
    }

    #[test]
    fn empty_content_gets_a_full_marker_block() {
        let outcome = apply_patterns("");
        assert_eq!(outcome.added.len(), all_patterns().count());
        assert!(outcome.already_present.is_empty());
        assert!(outcome.content.starts_with(MARKER_BEGIN));
        assert!(outcome.content.ends_with(&format!("{MARKER_END}\n")));
        for pattern in all_patterns() {
            assert_eq!(line_count(&outcome.content, pattern), 1);
        }
    }

    #[test]
    fn applying_twice_changes_nothing() {
        let first = apply_patterns("");
        let second = apply_patterns(&first.content);
        assert!(second.added.is_empty());
        assert_eq!(second.already_present.len(), all_patterns().count());
        assert_eq!(second.content, first.content);
    }

    #[test]
    fn existing_patterns_anywhere_in_the_file_are_skipped() {
        let outcome = apply_patterns(".Rhistory\nnode_modules/\n");
        assert!(!outcome.added.contains(&".Rhistory"));
        assert_eq!(outcome.already_present, vec![".Rhistory"]);
        assert_eq!(line_count(&outcome.content, ".Rhistory"), 1);
        // Pre-existing content is preserved.
        assert!(outcome.content.starts_with(".Rhistory\nnode_modules/\n"));
    }

    #[test]
    fn content_without_trailing_newline_stays_well_formed() {
        let outcome = apply_patterns("target/");
        assert!(outcome.content.starts_with("target/\n\n"));
        assert_eq!(line_count(&outcome.content, MARKER_BEGIN), 1);
    }

    #[test]
    fn missing_patterns_are_inserted_into_an_existing_block() {
        // Simulate an older hpds block missing a newer pattern.
        let content = format!("{MARKER_BEGIN}\n.Rhistory\n{MARKER_END}\n");
        let outcome = apply_patterns(&content);
        assert!(outcome.added.contains(&"__pycache__/"));
        assert_eq!(line_count(&outcome.content, MARKER_BEGIN), 1);
        assert_eq!(line_count(&outcome.content, MARKER_END), 1);
        assert_eq!(line_count(&outcome.content, "__pycache__/"), 1);
        // New patterns land inside the block, before the end marker.
        let end_pos = outcome.content.find(MARKER_END).unwrap();
        let py_pos = outcome.content.find("__pycache__/").unwrap();
        assert!(py_pos < end_pos);
    }

    #[test]
    fn tilde_expansion_covers_bare_and_slash_forms() {
        let home = Path::new("/home/hpds");
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/hpds"));
        assert_eq!(
            expand_tilde("~/.gitignore", home),
            PathBuf::from("/home/hpds/.gitignore")
        );
        assert_eq!(
            expand_tilde("/abs/ignore", home),
            PathBuf::from("/abs/ignore")
        );
        // `~user` is not expanded, left for git/the OS to interpret.
        assert_eq!(
            expand_tilde("~other/ignore", home),
            PathBuf::from("~other/ignore")
        );
    }
}
