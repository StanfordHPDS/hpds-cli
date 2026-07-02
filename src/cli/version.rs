//! `hpds version` — print the hpds version.

use crate::ui;

/// Print the version. Baked tool versions are appended once that lands
/// `tools/versions.rs`.
pub fn run() -> anyhow::Result<()> {
    ui::println(&format!("hpds {}", env!("CARGO_PKG_VERSION")));
    Ok(())
}
