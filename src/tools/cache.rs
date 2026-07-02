//! Where installed tools live on disk:
//! `<data_dir>/tools/<name>/<version>/<binary>` plus a `manifest.json` next
//! to each installed binary.

use std::path::{Path, PathBuf};

use crate::tools::platform::Platform;
use crate::ui::HintExt;

/// The on-disk tool cache, rooted at `<data_dir>/tools`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCache {
    root: PathBuf,
}

impl ToolCache {
    /// Cache under the platform data directory for hpds
    /// (`~/Library/Application Support/hpds`, `~/.local/share/hpds`,
    /// `%APPDATA%\hpds`), honoring the internal `HPDS_DATA_DIR` override
    /// used for test isolation.
    pub fn from_env() -> anyhow::Result<ToolCache> {
        match data_dir(std::env::var_os("HPDS_DATA_DIR")) {
            Some(dir) => Ok(ToolCache::at(&dir)),
            None => Err(anyhow::anyhow!(
                "could not determine a data directory for hpds tool installs"
            ))
            .hint(
                "make sure your home directory is set (HOME on macOS/Linux, \
                 APPDATA on Windows)",
            ),
        }
    }

    /// Cache rooted under an explicit hpds data directory.
    pub fn at(data_dir: &Path) -> ToolCache {
        ToolCache {
            root: data_dir.join("tools"),
        }
    }

    /// `<data_dir>/tools`: parent of every per-tool directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<data_dir>/tools/<name>/<version>`: one installed tool version.
    pub fn tool_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(name).join(version)
    }

    /// The installed binary for one tool version on `platform`.
    pub fn binary_path(&self, name: &str, version: &str, platform: Platform) -> PathBuf {
        self.tool_dir(name, version)
            .join(platform.binary_name(name))
    }

    /// The `manifest.json` describing one installed tool version.
    pub fn manifest_path(&self, name: &str, version: &str) -> PathBuf {
        self.tool_dir(name, version).join("manifest.json")
    }
}

/// The hpds data directory: `override_dir` when set (the internal
/// `HPDS_DATA_DIR` escape hatch, mirroring `HPDS_CONFIG_DIR` for config),
/// else the platform location from `directories`. `None` when no home
/// directory can be determined.
fn data_dir(override_dir: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "hpds").map(|dirs| dirs.data_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::platform::{Arch, Os};

    #[test]
    fn layout_is_tools_name_version_under_the_data_dir() {
        let data = Path::new("data");
        let cache = ToolCache::at(data);
        assert_eq!(cache.root(), data.join("tools"));
        assert_eq!(
            cache.tool_dir("air", "0.10.0"),
            data.join("tools").join("air").join("0.10.0")
        );
    }

    #[test]
    fn binary_path_uses_the_platform_binary_name() {
        let cache = ToolCache::at(Path::new("data"));
        let unix = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        let windows = Platform {
            os: Os::Windows,
            arch: Arch::Aarch64,
        };
        assert_eq!(
            cache.binary_path("ruff", "0.14.0", unix),
            cache.tool_dir("ruff", "0.14.0").join("ruff")
        );
        assert_eq!(
            cache.binary_path("ruff", "0.14.0", windows),
            cache.tool_dir("ruff", "0.14.0").join("ruff.exe")
        );
    }

    #[test]
    fn manifest_lives_next_to_the_binary() {
        let cache = ToolCache::at(Path::new("data"));
        assert_eq!(
            cache.manifest_path("air", "0.10.0"),
            cache.tool_dir("air", "0.10.0").join("manifest.json")
        );
    }

    #[test]
    fn data_dir_override_wins() {
        let dir = data_dir(Some("/isolated/hpds-data".into())).expect("override is set");
        assert_eq!(dir, PathBuf::from("/isolated/hpds-data"));
    }

    #[test]
    fn data_dir_falls_back_to_the_platform_location() {
        // On dev/CI machines a home directory always exists. The exact path
        // is platform-specific, but it is always inside an `hpds` directory.
        let dir = data_dir(None).expect("platform data dir");
        assert!(dir.iter().any(|part| part == "hpds"), "{}", dir.display());
    }
}
