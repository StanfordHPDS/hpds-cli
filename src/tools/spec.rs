//! What hpds knows about a release-binary tool: where it comes from and
//! how its release assets are named per platform.

use crate::tools::platform::Platform;

/// One GitHub-released tool: its name, the version this hpds release
/// installs by default, and how its release assets are named. Patterns may
/// use the placeholders `{version}`, `{arch}`, `{alt-arch}`, `{os}`, and
/// `{ext}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub default_version: &'static str,
    /// `owner/repo` on GitHub.
    pub repo: &'static str,
    /// Release asset filename pattern for the archive.
    pub asset_pattern: &'static str,
    /// Release asset filename pattern for the sha256 checksum, when the
    /// project publishes one (verification is skipped with a warning when
    /// it does not).
    pub checksum_pattern: Option<&'static str>,
}

impl ToolSpec {
    /// The release archive filename for `version` on `platform`.
    pub fn asset_name(&self, platform: Platform, version: &str) -> String {
        resolve_pattern(self.asset_pattern, platform, version)
    }

    /// The checksum asset filename for `version` on `platform`; `None` for
    /// tools without published checksums.
    pub fn checksum_asset_name(&self, platform: Platform, version: &str) -> Option<String> {
        self.checksum_pattern
            .map(|pattern| resolve_pattern(pattern, platform, version))
    }
}

/// Expand `{version}`, `{arch}`, `{alt-arch}`, `{os}`, and `{ext}` in an
/// asset pattern.
fn resolve_pattern(pattern: &str, platform: Platform, version: &str) -> String {
    pattern
        .replace("{version}", version)
        .replace("{alt-arch}", platform.arch.alt_asset_str())
        .replace("{arch}", platform.arch.asset_str())
        .replace("{os}", platform.os.asset_str())
        .replace("{ext}", platform.os.archive_ext())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::platform::{Arch, Os};

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            repo: "example/tool",
            asset_pattern: "tool-{arch}-{os}.{ext}",
            checksum_pattern: Some("tool-{arch}-{os}.{ext}.sha256"),
        }
    }

    #[test]
    fn resolves_asset_names_across_all_six_platform_tuples() {
        let cases = [
            (Os::Mac, Arch::X86_64, "tool-x86_64-apple-darwin.tar.gz"),
            (Os::Mac, Arch::Aarch64, "tool-aarch64-apple-darwin.tar.gz"),
            (
                Os::Linux,
                Arch::X86_64,
                "tool-x86_64-unknown-linux-gnu.tar.gz",
            ),
            (
                Os::Linux,
                Arch::Aarch64,
                "tool-aarch64-unknown-linux-gnu.tar.gz",
            ),
            (Os::Windows, Arch::X86_64, "tool-x86_64-pc-windows-msvc.zip"),
            (
                Os::Windows,
                Arch::Aarch64,
                "tool-aarch64-pc-windows-msvc.zip",
            ),
        ];
        for (os, arch, want) in cases {
            let platform = Platform { os, arch };
            assert_eq!(
                tool().asset_name(platform, "1.2.3"),
                want,
                "{os:?}/{arch:?}"
            );
        }
    }

    #[test]
    fn resolves_checksum_asset_names() {
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        assert_eq!(
            tool().checksum_asset_name(platform, "1.2.3").as_deref(),
            Some("tool-x86_64-unknown-linux-gnu.tar.gz.sha256")
        );
    }

    #[test]
    fn substitutes_version_placeholder() {
        let spec = ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            repo: "example/tool",
            asset_pattern: "tool-{version}-{arch}-{os}.{ext}",
            checksum_pattern: None,
        };
        let platform = Platform {
            os: Os::Mac,
            arch: Arch::Aarch64,
        };
        assert_eq!(
            spec.asset_name(platform, "9.9.9"),
            "tool-9.9.9-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(spec.checksum_asset_name(platform, "9.9.9"), None);
    }

    #[test]
    fn substitutes_alt_arch_placeholder_with_go_style_names() {
        // Tools from the Go ecosystem (gh, duckdb) name assets with
        // `amd64`/`arm64` instead of the Rust-triple arch names.
        let spec = ToolSpec {
            name: "tool",
            default_version: "1.2.3",
            repo: "example/tool",
            asset_pattern: "tool_{version}_linux_{alt-arch}.tar.gz",
            checksum_pattern: None,
        };
        let cases = [
            (Arch::X86_64, "tool_1.2.3_linux_amd64.tar.gz"),
            (Arch::Aarch64, "tool_1.2.3_linux_arm64.tar.gz"),
        ];
        for (arch, want) in cases {
            let platform = Platform {
                os: Os::Linux,
                arch,
            };
            assert_eq!(spec.asset_name(platform, "1.2.3"), want, "{arch:?}");
        }
    }
}
