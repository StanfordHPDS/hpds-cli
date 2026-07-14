//! `hpds version`: print the hpds version.

use crate::ui;

/// Print `hpds <version>`, the same value as `hpds --version`, provided
/// as a subcommand for scripts.
pub fn run() -> anyhow::Result<()> {
    ui::println(&version_report(env!("CARGO_PKG_VERSION")));
    Ok(())
}

/// The `hpds version` line. Pure so the formatting is unit-testable.
fn version_report(hpds_version: &str) -> String {
    format!("hpds {hpds_version}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_the_plain_hpds_version() {
        assert_eq!(version_report("9.9.9"), "hpds 9.9.9");
    }
}
