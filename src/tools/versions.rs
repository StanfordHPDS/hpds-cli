//! Release-pinned default tool versions.
//!
//! These are the versions `hpds install` uses when no `--version` pin is
//! given. They are updated (and re-verified against the upstream releases)
//! as part of cutting an hpds release, not at runtime.

pub const UV: &str = "0.9.5";
pub const GH: &str = "2.96.0";
pub const DUCKDB: &str = "1.5.4";
pub const QUARTO: &str = "1.9.36";
pub const TOGI: &str = "0.1.0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_versions_are_bare_semver() {
        // Bare `X.Y.Z` — no `v` prefix, so they can drop straight into
        // asset patterns.
        for (name, version) in [
            ("uv", UV),
            ("gh", GH),
            ("duckdb", DUCKDB),
            ("quarto", QUARTO),
            ("togi", TOGI),
        ] {
            let parts: Vec<&str> = version.split('.').collect();
            assert_eq!(parts.len(), 3, "{name}: {version}");
            for part in parts {
                part.parse::<u64>()
                    .unwrap_or_else(|_| panic!("{name}: {version}"));
            }
        }
    }
}
