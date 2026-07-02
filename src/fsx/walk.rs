//! Gitignore-aware file walker (spec §5).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};

/// Errors from file discovery. Messages tell the user what to do next;
/// rendering is the caller's job (via `ui/`), never this module's.
#[derive(Debug, thiserror::Error)]
pub enum FsxError {
    /// A target path handed to [`walk`] does not exist.
    #[error(
        "path `{path}` does not exist; check the path for typos or run the \
         command from the project root"
    )]
    MissingPath { path: PathBuf },

    /// An exclude glob (config `exclude` or a CLI flag) failed to parse.
    #[error(
        "invalid exclude glob `{pattern}`; fix the pattern in your `exclude` \
         configuration (gitignore-style globs, e.g. `data/**` or `*.log`)"
    )]
    InvalidExclude {
        pattern: String,
        source: ignore::Error,
    },
}

/// What [`walk`] found: the surviving files plus non-fatal problems hit on
/// the way.
///
/// `fsx` returns data only; callers (the M2 format/lint runner) render
/// `warnings` through `ui/` so a permission-denied subtree never silently
/// shrinks the target set.
#[derive(Debug, Default)]
// TODO(M2): read by the format/lint batch runner; only tests use it until then.
#[allow(dead_code)]
pub struct WalkOutcome {
    /// Files that survived ignore filtering, sorted and deduplicated.
    pub files: Vec<PathBuf>,
    /// One human-readable message per entry that could not be read (e.g. an
    /// unreadable subdirectory whose contents were skipped). Sorted and
    /// deduplicated; each says what to do next.
    pub warnings: Vec<String>,
}

/// Walk `paths` and return every file that survives ignore filtering, plus
/// warnings for anything that could not be read.
///
/// Filtering respects `.gitignore` files (including nested ones, with normal
/// gitignore precedence) plus the additive gitignore-style `excludes` globs —
/// these come from config (`[format].exclude` / `[lint].exclude`, spec §3)
/// once M0.4 lands, so they are plain parameters here. Per-machine git
/// configuration (the global gitignore, `.git/info/exclude`) is deliberately
/// *not* consulted: spec §5 scopes discovery to `.gitignore` + config
/// excludes, and results must not vary across machines. Hidden files are
/// skipped, matching the underlying tools' conventions. Explicit file targets
/// are returned as-is, bypassing both `.gitignore` and `excludes` (ruff's
/// default behavior; spec is silent — M2 must confirm this UX and may want a
/// `--force-exclude` opt-out).
///
/// Exclude globs are rooted at each walked path, so anchored patterns like
/// `data/**` are relative to the *target*, not the project root. That is
/// correct while `excludes` are plain parameters, but the M2 integration must
/// anchor config excludes at the project root rather than passing them here
/// per-target unchanged.
// TODO(M2): consumed by the format/lint batch runner; only tests use it until then.
#[allow(dead_code)]
pub fn walk(paths: &[PathBuf], excludes: &[String]) -> Result<WalkOutcome, FsxError> {
    let mut found = BTreeSet::new();
    let mut warnings = BTreeSet::new();
    for path in paths {
        if !path.exists() {
            return Err(FsxError::MissingPath { path: path.clone() });
        }
        // Root the exclude globs where the walk starts so anchored patterns
        // (`data/**`) behave like a .gitignore at the target root. For a file
        // target, that root is its containing directory.
        let glob_root = if path.is_file() {
            path.parent().unwrap_or_else(|| Path::new("."))
        } else {
            path.as_path()
        };
        let overrides = build_exclude_overrides(glob_root, excludes)?;

        let walker = WalkBuilder::new(path)
            // Respect .gitignore even outside a git checkout (fixture dirs,
            // fresh projects before `git init`).
            .require_git(false)
            // Spec §5 scopes discovery to `.gitignore` + config exclude
            // globs. Per-machine git configuration — the user's global
            // ignore (`core.excludesFile`) and the clone-local
            // `.git/info/exclude` — must not change which files hpds
            // formats/lints, or results would differ across machines and CI.
            .git_global(false)
            .git_exclude(false)
            .overrides(overrides)
            .build();
        for entry in walker {
            match entry {
                Ok(entry) => {
                    if entry.file_type().is_some_and(|ft| ft.is_file()) {
                        found.insert(entry.into_path());
                    }
                }
                // Per-entry errors (e.g. unreadable subdirectories) are not
                // fatal — the roots were validated above — but they must not
                // pass silently either: anything beneath them is skipped.
                Err(err) => {
                    warnings.insert(format!(
                        "skipped {err}; files under this path were not \
                         included — fix its permissions or add it to your \
                         exclude globs"
                    ));
                }
            }
        }
    }
    Ok(WalkOutcome {
        files: found.into_iter().collect(),
        warnings: warnings.into_iter().collect(),
    })
}

/// Compile exclude globs into an [`Override`] set rooted at `root`.
///
/// `Override` globs are whitelists by default; negating each pattern turns
/// them into ignores, giving the additive gitignore-style semantics of §3.
fn build_exclude_overrides(root: &Path, excludes: &[String]) -> Result<Override, FsxError> {
    let mut builder = OverrideBuilder::new(root);
    for pattern in excludes {
        builder
            .add(&format!("!{pattern}"))
            .map_err(|source| FsxError::InvalidExclude {
                pattern: pattern.clone(),
                source,
            })?;
    }
    builder.build().map_err(|source| FsxError::InvalidExclude {
        pattern: excludes.join(", "),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Create `rel` (and any parent directories) under `root`.
    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Root-relative, forward-slashed names for platform-stable assertions.
    fn rel_names(files: &[PathBuf], root: &Path) -> Vec<String> {
        files
            .iter()
            .map(|f| {
                f.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walk_respects_root_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\ndata/\n");
        write(root, "analysis.R", "");
        write(root, "run.log", "");
        write(root, "data/raw.csv", "");
        write(root, "query.sql", "");

        let files = walk(&[root.to_path_buf()], &[]).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["analysis.R", "query.sql"]);
    }

    #[test]
    fn walk_respects_nested_gitignore_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Root ignores all logs; the nested .gitignore ignores a file of its
        // own and un-ignores one specific log, exercising real gitignore
        // precedence between levels.
        write(root, ".gitignore", "*.log\n");
        write(root, "analysis.R", "");
        write(root, "run.log", "");
        write(root, "sub/.gitignore", "local.R\n!keep.log\n");
        write(root, "sub/local.R", "");
        write(root, "sub/model.py", "");
        write(root, "sub/keep.log", "");
        write(root, "sub/other.log", "");
        write(root, "sub/deeper/.gitignore", "*.py\n");
        write(root, "sub/deeper/scratch.py", "");
        write(root, "sub/deeper/notes.md", "");

        let files = walk(&[root.to_path_buf()], &[]).unwrap().files;

        assert_eq!(
            rel_names(&files, root),
            vec![
                "analysis.R",
                "sub/deeper/notes.md",
                "sub/keep.log",
                "sub/model.py",
            ]
        );
    }

    #[test]
    fn walk_applies_exclude_globs_additively_to_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\n");
        write(root, "analysis.R", "");
        write(root, "run.log", "");
        write(root, "query.sql", "");
        write(root, "data/raw.csv", "");
        write(root, "src/model.py", "");

        let excludes = ["*.sql".to_string(), "data/**".to_string()];
        let files = walk(&[root.to_path_buf()], &excludes).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["analysis.R", "src/model.py"]);
    }

    #[test]
    fn walk_accepts_explicit_file_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "model.py", "");

        let files = walk(&[root.join("analysis.R")], &[]).unwrap().files;

        assert_eq!(files, vec![root.join("analysis.R")]);
    }

    /// Pins the documented policy that explicit file targets bypass both
    /// `.gitignore` and exclude globs (ruff's default): "format this exact
    /// file" is an intentional request. The spec is silent here, so M2 must
    /// confirm the UX — if it decides otherwise, this test should change
    /// with it deliberately, not by accident.
    #[test]
    fn walk_returns_explicit_file_targets_even_if_ignored_or_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".gitignore", "*.log\n");
        write(root, "run.log", "");
        write(root, "query.sql", "");

        let outcome = walk(
            &[root.join("run.log"), root.join("query.sql")],
            &["*.sql".to_string()],
        )
        .unwrap();

        assert_eq!(
            rel_names(&outcome.files, root),
            vec!["query.sql", "run.log"]
        );
    }

    #[test]
    fn walk_dedupes_overlapping_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "sub/model.py", "");

        let files = walk(&[root.to_path_buf(), root.join("sub")], &[])
            .unwrap()
            .files;

        assert_eq!(files, vec![root.join("sub").join("model.py")]);
    }

    #[test]
    fn walk_skips_hidden_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, ".Rhistory", "");
        write(root, ".Rproj.user/settings", "");
        write(root, "analysis.R", "");

        let files = walk(&[root.to_path_buf()], &[]).unwrap().files;

        assert_eq!(rel_names(&files, root), vec!["analysis.R"]);
    }

    /// A permission-denied subtree must not silently shrink the target set:
    /// the files are skipped, but the caller gets a warning to render.
    #[cfg(unix)]
    #[test]
    fn walk_warns_on_unreadable_directories() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "analysis.R", "");
        write(root, "locked/inner.R", "");
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = walk(&[root.to_path_buf()], &[]);

        // Restore before asserting so the tempdir cleans up even on failure.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = outcome.unwrap();
        assert_eq!(rel_names(&outcome.files, root), vec!["analysis.R"]);
        assert_eq!(
            outcome.warnings.len(),
            1,
            "warnings: {:?}",
            outcome.warnings
        );
        let warning = &outcome.warnings[0];
        assert!(warning.contains("locked"), "warning: {warning}");
        // Project rule: every user-facing message says what to do next.
        assert!(warning.contains("exclude"), "warning: {warning}");
    }

    /// Regression test: the user's *global* gitignore (`~/.config/git/ignore`
    /// or `core.excludesFile`) must not leak into discovery — spec §5 names
    /// `.gitignore` + config exclude globs only, and honoring per-machine
    /// global excludes would make format/lint targets differ across machines.
    ///
    /// Env vars cannot be mutated safely in a threaded test process, so the
    /// parent branch re-runs just this test in a child process whose
    /// HOME/XDG_CONFIG_HOME point at a hermetic global ignore dropping `*.R`.
    #[test]
    fn walk_ignores_users_global_gitignore() {
        if std::env::var_os("HPDS_TEST_GLOBAL_IGNORE_CHILD").is_some() {
            // Child: a hostile global ignore is in place; R files must survive.
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            write(root, "analysis.R", "");
            write(root, "query.sql", "");

            let files = walk(&[root.to_path_buf()], &[]).unwrap().files;

            assert_eq!(rel_names(&files, root), vec!["analysis.R", "query.sql"]);
            return;
        }

        let fake_home = tempfile::tempdir().unwrap();
        write(fake_home.path(), "git/ignore", "*.R\n");
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "fsx::walk::tests::walk_ignores_users_global_gitignore",
            ])
            .env("HPDS_TEST_GLOBAL_IGNORE_CHILD", "1")
            .env("HOME", fake_home.path())
            .env("USERPROFILE", fake_home.path())
            .env("XDG_CONFIG_HOME", fake_home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "walk honored the user's global gitignore:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn walk_returns_empty_for_no_paths() {
        let outcome = walk(&[], &[]).unwrap();
        assert!(outcome.files.is_empty());
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn walk_errors_on_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");

        let err = walk(std::slice::from_ref(&missing), &[]).unwrap_err();

        assert!(matches!(err, FsxError::MissingPath { .. }));
        let message = err.to_string();
        assert!(message.contains("no-such-dir"), "message: {message}");
    }

    #[test]
    fn walk_errors_on_invalid_exclude_glob() {
        let tmp = tempfile::tempdir().unwrap();

        let err = walk(&[tmp.path().to_path_buf()], &["a[".to_string()]).unwrap_err();

        assert!(matches!(err, FsxError::InvalidExclude { .. }));
        let message = err.to_string();
        assert!(message.contains("a["), "message: {message}");
    }
}
