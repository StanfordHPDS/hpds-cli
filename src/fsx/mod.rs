//! File discovery for format/lint targets: gitignore-aware walking
//! with additive exclude globs, plus the extension → language registry used
//! to batch files per adapter.
//!
//! This module returns data only. It never prints to the terminal — all
//! output goes through `ui/`.

mod registry;
mod walk;

// NOTE: re-exports are consumed by the M2 format/lint commands; until then
// they are only exercised by unit tests.
#[allow(unused_imports)]
pub use registry::{ExtensionRegistry, Language, group_by_language};
#[allow(unused_imports)]
pub use walk::{FsxError, WalkOutcome, walk};
