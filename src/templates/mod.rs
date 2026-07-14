//! Embedded project templates and the engine that renders them.
//!
//! The engine is pure logic: `{{variable}}` substitution ([`render`]), file
//! writes with conflict handling ([`write_rendered`], [`apply_dir`]: files
//! are NEVER overwritten without `force`; conflicts carry a diff-style
//! preview and are skipped), and idempotent marker-comment blocks for
//! appending to existing files like `Makefile`/`.gitignore`
//! ([`append_block`]).
//!
//! This module returns outcomes as data and never prints; the `hpds use`
//! command layer renders every outcome through `ui/`.

mod apply;
pub mod components;
mod markers;
mod render;

pub use apply::{FileOutcome, WriteOutcome, apply_dir, write_rendered};
// NOTE: re-export consumed by later commands; until then it is only
// exercised by unit tests.
#[allow(unused_imports)]
pub use apply::diff_preview;
// The append-to-existing-files half of the engine; no shipped component
// appends blocks, so only its unit tests exercise it.
#[allow(unused_imports)]
pub use markers::{AppendOutcome, append_block};
pub use render::Vars;
#[allow(unused_imports)]
pub use render::render;

use std::path::PathBuf;

use include_dir::{Dir, include_dir};

/// Every template component, embedded at compile time from `templates/` at
/// the repo root.
pub static TEMPLATES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/templates");

/// Fixture templates for the engine's own unit tests. Kept under
/// `tests/fixtures/templates/` so they never ship in the production binary
/// or show up in the `hpds use` component listing.
#[cfg(test)]
pub(crate) static TEST_TEMPLATES: Dir<'static> =
    include_dir!("$CARGO_MANIFEST_DIR/tests/fixtures/templates");

/// Errors from the template engine. Messages say what to do next; rendering
/// them is the caller's job (via `ui/`), never this module's.
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    /// A template referenced a `{{variable}}` that is not in the
    /// substitution map, almost always a typo in the template itself.
    #[error(
        "unknown template variable `{{{{{name}}}}}` in `{template}` (available: {available}); \
         fix the typo in the template or add the variable to the substitution map"
    )]
    UnknownVariable {
        name: String,
        template: String,
        available: String,
    },

    /// An `hpds:begin` marker line has no matching `hpds:end` line, so the
    /// block cannot be updated safely.
    #[error(
        "marker block `{id}` in `{}` has an `hpds:begin` line but no matching `hpds:end` line; \
         repair or remove the block by hand, then re-run", path.display()
    )]
    UnterminatedMarkerBlock { id: String, path: PathBuf },

    /// A block body contains a line identical to the block's own
    /// `hpds:begin`/`hpds:end` marker line, which would corrupt later
    /// re-detection of the block.
    #[error(
        "the body for marker block `{id}` contains its own `hpds:begin`/`hpds:end` marker line; \
         remove that line from the block body so the block can be updated safely later"
    )]
    MarkerInBody { id: String },

    /// A filesystem operation failed.
    #[error(
        "could not {action} `{}`; check that the path is writable and try again", path.display()
    )]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_templates_do_not_embed_test_fixtures() {
        // `TEMPLATES` ships inside the release binary and feeds
        // the `hpds use` component listing; fixtures live in
        // `tests/fixtures/templates/` so they can never leak into either.
        assert!(TEMPLATES.get_dir("test-fixture").is_none());
        assert!(
            TEMPLATES.get_dir("pipeline").is_some(),
            "pipeline templates are embedded"
        );
    }

    #[test]
    fn production_templates_embed_the_shipped_components() {
        assert!(TEMPLATES.get_dir("readme").is_some());
        assert!(TEMPLATES.get_dir("hpds-toml").is_some());
        assert!(TEMPLATES.get_dir("slurm").is_some());
        assert!(TEMPLATES.get_dir("container").is_some());
        assert!(TEMPLATES.get_dir("gha").is_some());
    }

    #[test]
    fn test_templates_include_the_test_fixture_tree() {
        let fixture = TEST_TEMPLATES
            .get_dir("test-fixture")
            .expect("tests/fixtures/templates/test-fixture is embedded");
        assert!(fixture.get_file("test-fixture/hello.txt").is_some());
        assert!(fixture.get_file("test-fixture/nested/note.md").is_some());
    }

    #[test]
    fn unknown_variable_error_says_what_to_do_next() {
        let err = TemplateError::UnknownVariable {
            name: "projct".into(),
            template: "test-fixture/hello.txt".into(),
            available: "author, language, project, year".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("{{projct}}"), "names the bad variable: {msg}");
        assert!(msg.contains("test-fixture/hello.txt"));
        assert!(msg.contains("author, language, project, year"));
        assert!(msg.contains("fix the typo"), "tells the user what to do");
    }
}
