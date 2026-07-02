//! Writing rendered templates to disk with conflict handling.
//!
//! Files are NEVER overwritten without `force`. On conflict the outcome
//! carries a diff-style preview and the file is skipped; the command layer
//! decides how to show it (via `ui/`). This module never prints.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use include_dir::Dir;

use super::TemplateError;
use super::render::{Vars, render};

/// What happened to one destination file.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file did not exist and was written.
    Created,
    /// The file already had exactly the rendered content; nothing written.
    Unchanged,
    /// The file existed with different content and `force` was set.
    Overwritten,
    /// The file existed with different content and `force` was NOT set: the
    /// file was left untouched. `diff` is a unified diff-style preview of
    /// what would change, ready for the command layer to render.
    SkippedConflict { diff: String },
}

/// One entry of an [`apply_dir`] run: the template-relative path and what
/// happened at the destination.
#[derive(Debug)]
pub struct FileOutcome {
    pub path: PathBuf,
    pub outcome: WriteOutcome,
}

/// Write `content` to `dest`, creating parent directories as needed and
/// honoring the never-overwrite-without-`force` rule.
pub fn write_rendered(
    dest: &Path,
    content: &[u8],
    force: bool,
) -> Result<WriteOutcome, TemplateError> {
    let io_err = |action: &'static str| {
        move |source: std::io::Error| TemplateError::Io {
            action,
            path: dest.to_path_buf(),
            source,
        }
    };
    match fs::read(dest) {
        Ok(existing) => {
            if existing == content {
                Ok(WriteOutcome::Unchanged)
            } else if force {
                // Overwriting user data: go through a temp file + rename so
                // an interrupted write cannot truncate the existing file.
                replace_file_atomically(dest, content).map_err(io_err("write"))?;
                Ok(WriteOutcome::Overwritten)
            } else {
                // Existing files are user data: never overwrite without
                // force. Carry a preview so the caller can show what the
                // template would have changed.
                let diff = diff_preview(
                    &dest.display().to_string(),
                    &String::from_utf8_lossy(&existing),
                    &String::from_utf8_lossy(content),
                );
                Ok(WriteOutcome::SkippedConflict { diff })
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(io_err("create directory for"))?;
            }
            fs::write(dest, content).map_err(io_err("write"))?;
            Ok(WriteOutcome::Created)
        }
        Err(err) => Err(io_err("read")(err)),
    }
}

/// Replace the existing file at `path` with `contents` atomically: the bytes
/// land in a temp file in the same directory, which then renames over
/// `path`, so a crash mid-write can never leave the user's file truncated.
/// The destination's permissions are preserved. Only for paths that already
/// exist — creating a brand-new file destroys nothing and uses `fs::write`.
pub(super) fn replace_file_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(contents)?;
    if let Ok(meta) = fs::metadata(path) {
        // Keep the user's mode bits (e.g. an executable script) across the
        // rename; the temp file starts out owner-only.
        tmp.as_file().set_permissions(meta.permissions())?;
    }
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// A unified diff-style preview of replacing `old` with `new` in the file
/// labelled `label`. Returned as plain data; styling is the caller's job.
pub fn diff_preview(label: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(
            &format!("{label} (existing)"),
            &format!("{label} (template)"),
        )
        .to_string()
}

/// Render every file in the embedded `source` dir into `dest_root`,
/// substituting `vars` in each UTF-8 file (binary files are copied
/// verbatim). Returns one [`FileOutcome`] per file, sorted by path.
pub fn apply_dir(
    source: &Dir<'_>,
    dest_root: &Path,
    vars: &Vars,
    force: bool,
) -> Result<Vec<FileOutcome>, TemplateError> {
    let mut files = Vec::new();
    collect_files(source, &mut files);
    files.sort_by_key(|f| f.path());

    let mut outcomes = Vec::with_capacity(files.len());
    for file in files {
        // Paths inside the embedded dir carry the component prefix (e.g.
        // `pipeline/Makefile`); destinations are relative to `dest_root`.
        let rel = file
            .path()
            .strip_prefix(source.path())
            .unwrap_or_else(|_| file.path());
        let dest = dest_root.join(rel);
        let outcome = match file.contents_utf8() {
            Some(text) => {
                let rendered = render(text, &rel.display().to_string(), vars)?;
                write_rendered(&dest, rendered.as_bytes(), force)?
            }
            // Binary assets (images etc.) are copied verbatim: `{{var}}`
            // substitution only makes sense in text.
            None => write_rendered(&dest, file.contents(), force)?,
        };
        outcomes.push(FileOutcome {
            path: rel.to_path_buf(),
            outcome,
        });
    }
    Ok(outcomes)
}

/// Depth-first collection of every file under `dir`.
fn collect_files<'a>(dir: &'a Dir<'a>, out: &mut Vec<&'a include_dir::File<'a>>) {
    out.extend(dir.files());
    for sub in dir.dirs() {
        collect_files(sub, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::TEST_TEMPLATES;

    fn vars() -> Vars {
        Vars::new()
            .with("project", "malaria-icu")
            .with("language", "r")
            .with("year", "2026")
            .with("author", "HPDS Lab")
    }

    #[test]
    fn creates_a_missing_file_and_its_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("nested").join("deep").join("out.txt");
        let outcome = write_rendered(&dest, b"content\n", false).unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "content\n");
    }

    #[test]
    fn identical_existing_file_is_reported_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.txt");
        fs::write(&dest, "same\n").unwrap();
        let outcome = write_rendered(&dest, b"same\n", false).unwrap();
        assert_eq!(outcome, WriteOutcome::Unchanged);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "same\n");
    }

    #[test]
    fn conflict_without_force_skips_and_carries_a_diff_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("Makefile");
        fs::write(&dest, "old line\n").unwrap();
        let outcome = write_rendered(&dest, b"new line\n", false).unwrap();
        match outcome {
            WriteOutcome::SkippedConflict { diff } => {
                assert!(diff.contains("-old line"), "diff shows removal: {diff}");
                assert!(diff.contains("+new line"), "diff shows addition: {diff}");
            }
            other => panic!("expected SkippedConflict, got {other:?}"),
        }
        // The file must be untouched.
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old line\n");
    }

    #[test]
    fn conflict_with_force_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.txt");
        fs::write(&dest, "old\n").unwrap();
        let outcome = write_rendered(&dest, b"new\n", true).unwrap();
        assert_eq!(outcome, WriteOutcome::Overwritten);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
    }

    #[test]
    fn replace_file_atomically_swaps_content_and_leaves_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.txt");
        fs::write(&dest, "old\n").unwrap();
        replace_file_atomically(&dest, b"new\n").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "new\n");
        let entries: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("out.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn replace_file_atomically_preserves_the_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("script.sh");
        fs::write(&dest, "#!/bin/sh\nold\n").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)).unwrap();
        replace_file_atomically(&dest, b"#!/bin/sh\nnew\n").unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "executable bit survives the overwrite");
    }

    #[test]
    fn force_overwrite_leaves_no_temp_files_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.txt");
        fs::write(&dest, "old\n").unwrap();
        write_rendered(&dest, b"new\n", true).unwrap();
        let entries: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("out.txt")]);
    }

    #[test]
    fn diff_preview_is_a_unified_diff_labelled_with_the_file() {
        let diff = diff_preview("Makefile", "a\nb\nc\n", "a\nB\nc\n");
        assert!(diff.contains("Makefile"), "labels the file: {diff}");
        assert!(diff.contains("-b"));
        assert!(diff.contains("+B"));
        assert!(diff.contains("@@"), "has hunk headers: {diff}");
    }

    #[test]
    fn apply_dir_renders_the_whole_fixture_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = TEST_TEMPLATES.get_dir("test-fixture").unwrap();
        let outcomes = apply_dir(fixture, tmp.path(), &vars(), false).unwrap();

        let paths: Vec<_> = outcomes.iter().map(|o| o.path.clone()).collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("hello.txt"), PathBuf::from("nested/note.md")]
        );
        assert!(
            outcomes.iter().all(|o| o.outcome == WriteOutcome::Created),
            "{outcomes:?}"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
            "Hello malaria-icu!\nlanguage: r\nyear: 2026\nauthor: HPDS Lab\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("nested").join("note.md")).unwrap(),
            "# malaria-icu\n"
        );
    }

    #[test]
    fn apply_dir_is_idempotent_on_a_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = TEST_TEMPLATES.get_dir("test-fixture").unwrap();
        apply_dir(fixture, tmp.path(), &vars(), false).unwrap();
        let second = apply_dir(fixture, tmp.path(), &vars(), false).unwrap();
        assert!(
            second.iter().all(|o| o.outcome == WriteOutcome::Unchanged),
            "{second:?}"
        );
    }

    #[test]
    fn apply_dir_skips_a_conflicting_file_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("hello.txt"), "user edits\n").unwrap();
        let fixture = TEST_TEMPLATES.get_dir("test-fixture").unwrap();
        let outcomes = apply_dir(fixture, tmp.path(), &vars(), false).unwrap();
        let hello = outcomes
            .iter()
            .find(|o| o.path == Path::new("hello.txt"))
            .unwrap();
        assert!(
            matches!(&hello.outcome, WriteOutcome::SkippedConflict { .. }),
            "{hello:?}"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
            "user edits\n"
        );
    }

    #[test]
    fn apply_dir_propagates_unknown_variable_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = TEST_TEMPLATES.get_dir("test-fixture").unwrap();
        let err = apply_dir(fixture, tmp.path(), &Vars::new(), false).unwrap_err();
        assert!(matches!(err, TemplateError::UnknownVariable { .. }));
    }
}
