//! Idempotent marker-comment blocks for appending to existing files
//!.
//!
//! A block is delimited by `<prefix> hpds:begin <id>` and
//! `<prefix> hpds:end <id>` lines. Appending the same block twice leaves
//! exactly one copy; appending changed content updates the block in place.
//! Content outside the block is preserved byte-for-byte where possible: the
//! file's dominant line ending (CRLF or LF) is kept and a missing final
//! newline is not "fixed" on update.
//! `hpds git vaccinate` uses the same marker convention but owns its
//! own implementation.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use super::TemplateError;

/// What happened to the marker block.
#[derive(Debug, PartialEq, Eq)]
// Tests-only until the `hpds use` components consume it.
#[allow(dead_code)]
pub enum AppendOutcome {
    /// No block with this id existed; it was appended (the file is created
    /// if missing).
    Appended,
    /// A block with this id existed with different content; it was replaced
    /// in place.
    Updated,
    /// A block with this id and identical content already existed; the file
    /// was not touched.
    AlreadyPresent,
}

/// Append (or update) the marker block `id` in `path`.
///
/// `comment_prefix` is the line-comment leader for the target file type
/// (`"#"` for Makefile/.gitignore). `body` is the block content between the
/// markers; a trailing newline is optional.
#[allow(dead_code)] // tests-only until the `hpds use` components consume it
pub fn append_block(
    path: &Path,
    id: &str,
    comment_prefix: &str,
    body: &str,
) -> Result<AppendOutcome, TemplateError> {
    let io_err = |action: &'static str| {
        move |source: std::io::Error| TemplateError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    };
    let existing = match fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(io_err("read")(err)),
    };
    let file_existed = existing.is_some();
    let existing = existing.unwrap_or_default();

    let begin = format!("{comment_prefix} hpds:begin {id}");
    let end = format!("{comment_prefix} hpds:end {id}");
    if body.lines().any(|line| line == begin || line == end) {
        // A body line identical to a marker line would make later
        // re-detection splice at the wrong place; refuse up front.
        return Err(TemplateError::MarkerInBody { id: id.to_string() });
    }
    let mut block: Vec<&str> = Vec::new();
    block.push(&begin);
    block.extend(body.lines());
    block.push(&end);

    // Preserve the file's own conventions outside the block: rejoin with its
    // dominant line ending and keep a missing final newline missing.
    let eol = dominant_eol(&existing);
    let had_trailing_newline = existing.is_empty() || existing.ends_with('\n');

    let mut lines: Vec<&str> = existing.lines().collect();
    let outcome = match lines.iter().position(|line| *line == begin) {
        Some(begin_idx) => {
            let end_idx = lines[begin_idx..]
                .iter()
                .position(|line| *line == end)
                .map(|offset| begin_idx + offset)
                .ok_or_else(|| TemplateError::UnterminatedMarkerBlock {
                    id: id.to_string(),
                    path: path.to_path_buf(),
                })?;
            if lines[begin_idx..=end_idx] == block[..] {
                // Identical block already present: leave the file untouched.
                return Ok(AppendOutcome::AlreadyPresent);
            }
            lines.splice(begin_idx..=end_idx, block);
            AppendOutcome::Updated
        }
        None => {
            if !lines.is_empty() {
                // Blank separator between existing content and our block.
                lines.push("");
            }
            lines.extend(block);
            AppendOutcome::Appended
        }
    };

    let mut updated = lines.join(eol);
    // A freshly appended block always ends with a newline (it is ours); an
    // update keeps whatever the file already did at its end.
    if outcome == AppendOutcome::Appended || had_trailing_newline {
        updated.push_str(eol);
    }
    if file_existed {
        // Rewriting the user's file: temp file + rename so an interrupted
        // write cannot truncate it.
        super::apply::replace_file_atomically(path, updated.as_bytes()).map_err(io_err("write"))?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err("create directory for"))?;
        }
        fs::write(path, updated).map_err(io_err("write"))?;
    }
    Ok(outcome)
}

/// The dominant line ending in `text` (`"\n"` for empty or LF-majority
/// content), so edits keep a CRLF file CRLF.
fn dominant_eol(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf { "\r\n" } else { "\n" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_a_block_to_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        fs::write(&path, "target/\n").unwrap();
        let outcome = append_block(&path, "hpds-ignores", "#", ".Rhistory\n.DS_Store\n").unwrap();
        assert_eq!(outcome, AppendOutcome::Appended);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "target/\n\
             \n\
             # hpds:begin hpds-ignores\n\
             .Rhistory\n\
             .DS_Store\n\
             # hpds:end hpds-ignores\n"
        );
    }

    #[test]
    fn creates_the_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub").join(".gitignore");
        let outcome = append_block(&path, "block", "#", "line\n").unwrap();
        assert_eq!(outcome, AppendOutcome::Appended);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# hpds:begin block\nline\n# hpds:end block\n"
        );
    }

    #[test]
    fn appending_twice_leaves_exactly_one_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Makefile");
        fs::write(&path, "all:\n\techo hi\n").unwrap();

        assert_eq!(
            append_block(&path, "clean", "#", "clean:\n\trm -rf output\n").unwrap(),
            AppendOutcome::Appended
        );
        let after_first = fs::read_to_string(&path).unwrap();

        assert_eq!(
            append_block(&path, "clean", "#", "clean:\n\trm -rf output\n").unwrap(),
            AppendOutcome::AlreadyPresent
        );
        let after_second = fs::read_to_string(&path).unwrap();

        assert_eq!(after_first, after_second, "second append changed the file");
        assert_eq!(
            after_second.matches("# hpds:begin clean").count(),
            1,
            "exactly one block: {after_second}"
        );
    }

    #[test]
    fn changed_body_updates_the_block_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        fs::write(&path, "target/\n").unwrap();
        append_block(&path, "b", "#", "old\n").unwrap();
        let outcome = append_block(&path, "b", "#", "new\n").unwrap();
        assert_eq!(outcome, AppendOutcome::Updated);
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("new"), "{content}");
        assert!(!content.contains("old"), "{content}");
        assert_eq!(content.matches("# hpds:begin b").count(), 1);
        assert!(content.starts_with("target/\n"), "keeps prior content");
    }

    #[test]
    fn distinct_ids_get_distinct_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        append_block(&path, "one", "#", "a\n").unwrap();
        append_block(&path, "two", "#", "b\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# hpds:begin one"));
        assert!(content.contains("# hpds:begin two"));
    }

    #[test]
    fn begin_without_end_is_a_hard_error_naming_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        fs::write(&path, "# hpds:begin broken\nline\n").unwrap();
        let err = append_block(&path, "broken", "#", "line\n").unwrap_err();
        match &err {
            TemplateError::UnterminatedMarkerBlock { id, path: p } => {
                assert_eq!(id, "broken");
                assert_eq!(p, &path);
            }
            other => panic!("expected UnterminatedMarkerBlock, got {other:?}"),
        }
        assert!(err.to_string().contains("by hand"), "says what to do next");
    }

    #[test]
    fn preserves_crlf_line_endings_on_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        fs::write(&path, "target/\r\nnode_modules/\r\n").unwrap();
        let outcome = append_block(&path, "hpds-ignores", "#", ".Rhistory\n").unwrap();
        assert_eq!(outcome, AppendOutcome::Appended);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "target/\r\n\
             node_modules/\r\n\
             \r\n\
             # hpds:begin hpds-ignores\r\n\
             .Rhistory\r\n\
             # hpds:end hpds-ignores\r\n"
        );
    }

    #[test]
    fn preserves_crlf_line_endings_on_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        fs::write(&path, "target/\r\n").unwrap();
        append_block(&path, "b", "#", "old\n").unwrap();
        assert_eq!(
            append_block(&path, "b", "#", "new\n").unwrap(),
            AppendOutcome::Updated
        );
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            !content.replace("\r\n", "").contains('\n'),
            "no bare LF line endings crept in: {content:?}"
        );
        assert!(content.starts_with("target/\r\n"), "{content:?}");
        assert!(content.contains("new\r\n"), "{content:?}");
    }

    #[test]
    fn identical_crlf_block_is_already_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        fs::write(&path, "target/\r\n").unwrap();
        append_block(&path, "b", "#", "line\n").unwrap();
        let before = fs::read_to_string(&path).unwrap();
        assert_eq!(
            append_block(&path, "b", "#", "line\n").unwrap(),
            AppendOutcome::AlreadyPresent
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn update_preserves_a_missing_trailing_newline_outside_the_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Makefile");
        // User content after the block, with no final newline.
        fs::write(&path, "# hpds:begin b\nold\n# hpds:end b\ntail").unwrap();
        assert_eq!(
            append_block(&path, "b", "#", "new\n").unwrap(),
            AppendOutcome::Updated
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# hpds:begin b\nnew\n# hpds:end b\ntail",
            "content outside the block is untouched"
        );
    }

    #[test]
    fn rejects_a_body_containing_its_own_marker_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".gitignore");
        let err = append_block(&path, "b", "#", "line\n# hpds:end b\n").unwrap_err();
        match &err {
            TemplateError::MarkerInBody { id } => assert_eq!(id, "b"),
            other => panic!("expected MarkerInBody, got {other:?}"),
        }
        assert!(
            err.to_string().contains("remove"),
            "says what to do next: {err}"
        );
        assert!(!path.exists(), "nothing was written");
    }

    #[test]
    fn respects_a_non_hash_comment_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("script.R");
        append_block(&path, "setup", "##", "library(targets)\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "## hpds:begin setup\nlibrary(targets)\n## hpds:end setup\n"
        );
    }
}
