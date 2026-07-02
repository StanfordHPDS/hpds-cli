//! Managed tool installs: which tools hpds knows about, where their
//! binaries live on disk, what platform archive to fetch for each, and the
//! downloader that installs them.
//!
//! Terminal output (progress bars, warnings) always goes through `ui/`.

// NOTE: dead_code allowed per module because nothing outside `tools/` calls
// in yet; the `hpds tools` subcommands and the format/lint adapters are the
// consumers. Until then everything here is exercised by unit tests only.
#[allow(dead_code)]
mod cache;
#[allow(dead_code)]
mod download;
#[allow(dead_code)]
mod manifest;
#[allow(dead_code)]
mod platform;
#[allow(dead_code)]
mod spec;
#[cfg(test)]
pub(crate) mod test_support;
#[allow(dead_code)]
mod uv_tool;
#[allow(dead_code)]
pub mod versions;

use std::path::PathBuf;

// NOTE: re-exports are consumed by the `hpds tools` subcommands and the
// format/lint adapters; until those land they are only exercised by unit
// tests.
#[allow(unused_imports)]
pub use cache::ToolCache;
#[allow(unused_imports)]
pub use download::{Downloader, InstallContext};
#[allow(unused_imports)]
pub use manifest::Manifest;
#[allow(unused_imports)]
pub use platform::{Arch, Os, Platform, UnsupportedPlatform};
#[allow(unused_imports)]
pub use spec::{ToolKind, ToolSpec};
#[allow(unused_imports)]
pub use uv_tool::UvToolInstaller;

/// Install `spec` at `version` into the default cache for this machine and
/// return the path to its binary, dispatching on the tool's kind so callers
/// never care how a tool is installed. Cached tools are returned with zero
/// network.
#[allow(dead_code)] // consumed by the `hpds tools` subcommands and adapters
pub fn ensure_installed(
    spec: &ToolSpec,
    version: &str,
    ctx: &InstallContext,
) -> anyhow::Result<PathBuf> {
    let cache = ToolCache::from_env()?;
    let platform = Platform::current()?;
    match spec.kind {
        ToolKind::GithubBinary { .. } => {
            Downloader::new(cache, platform).ensure_installed(spec, version, ctx)
        }
        ToolKind::UvTool { .. } => {
            UvToolInstaller::new(cache, platform).ensure_installed(spec, version, ctx)
        }
    }
}
