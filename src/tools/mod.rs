//! Release-binary download plumbing shared by `hpds install` and
//! `hpds upgrade`: platform detection, asset-name resolution, the on-disk
//! download cache, and the checksum-verified, atomic downloader.
//!
//! Terminal output (progress bars, warnings) always goes through `ui/`.

mod cache;
mod download;
mod manifest;
mod platform;
mod spec;
#[cfg(test)]
pub(crate) mod test_support;
pub mod versions;

pub use cache::ToolCache;
pub(crate) use download::extract_binary;
pub(crate) use download::github_agent;
pub use download::{Downloader, InstallContext};
pub use platform::{Os, Platform};
pub use spec::ToolSpec;

// `Arch` only appears in platform-matrix unit tests; production code
// always goes through `Platform::current()`.
#[cfg(test)]
pub(crate) use platform::Arch;
