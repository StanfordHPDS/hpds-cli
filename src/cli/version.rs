//! `hpds version` — print the hpds version and the tool versions baked
//! into this release.

use crate::tools::ToolSpec;
use crate::ui;

/// Print the hpds version followed by one indented line per managed tool,
/// naming the default version this release installs when a project pins
/// nothing.
pub fn run() -> anyhow::Result<()> {
    ui::println(&version_report(env!("CARGO_PKG_VERSION")));
    Ok(())
}

/// The full `hpds version` report: the hpds version on the first line, then
/// one `  <tool> <version>` line per managed tool default. Pure so the
/// formatting is unit-testable.
fn version_report(hpds_version: &str) -> String {
    let mut out = format!("hpds {hpds_version}");
    for spec in ToolSpec::builtins() {
        out.push_str(&format!("\n  {} {}", spec.name, spec.default_version));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::versions;

    #[test]
    fn report_starts_with_the_hpds_version() {
        let report = version_report("9.9.9");
        assert!(
            report.starts_with("hpds 9.9.9\n"),
            "the first line names the hpds version: {report}"
        );
    }

    #[test]
    fn report_lists_every_managed_tool_with_its_baked_default() {
        let report = version_report("0.1.0");
        for spec in ToolSpec::builtins() {
            let line = format!("  {} {}", spec.name, spec.default_version);
            assert!(
                report.contains(&line),
                "report must list `{line}`:\n{report}"
            );
        }
    }

    #[test]
    fn report_uses_the_baked_version_constants() {
        // The lines are the constants from tools/versions.rs, not hardcoded
        // duplicates: a bump there flows straight into `hpds version`.
        let report = version_report("0.1.0");
        assert!(
            report.contains(&format!("air {}", versions::AIR)),
            "{report}"
        );
        assert!(
            report.contains(&format!("ruff {}", versions::RUFF)),
            "{report}"
        );
        assert!(report.contains(&format!("uv {}", versions::UV)), "{report}");
    }
}
