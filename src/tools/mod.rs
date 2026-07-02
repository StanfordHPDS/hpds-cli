//! Managed tool installs: which tools hpds knows about, where their
//! binaries live on disk, and what platform archive to fetch for each.
//!
//! This module returns data only. It never prints to the terminal — all
//! output goes through `ui/`.

// NOTE: dead_code allowed per module because nothing outside `tools/` calls
// in yet; the downloader and the `hpds tools` subcommands are the consumers.
// Until then everything here is exercised by unit tests only.
#[allow(dead_code)]
mod cache;
#[allow(dead_code)]
mod manifest;
#[allow(dead_code)]
mod platform;
#[allow(dead_code)]
mod spec;
#[allow(dead_code)]
pub mod versions;

// NOTE: re-exports are consumed by the tool downloader and the `hpds tools`
// subcommands; until those land they are only exercised by unit tests.
#[allow(unused_imports)]
pub use cache::ToolCache;
#[allow(unused_imports)]
pub use manifest::Manifest;
#[allow(unused_imports)]
pub use platform::{Arch, Os, Platform, UnsupportedPlatform};
#[allow(unused_imports)]
pub use spec::{ToolKind, ToolSpec};
