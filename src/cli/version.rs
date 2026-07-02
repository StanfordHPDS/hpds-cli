//! `hpds version` — print the hpds version (spec §10).

/// Print the version. Baked tool versions are appended once M1.4 lands
/// `tools/versions.rs`.
///
/// TODO(M0.3): route through `src/ui/` once it exists (bd-2di.3); until then
/// this plain print is the version command's whole behavior.
pub fn run() -> anyhow::Result<()> {
    println!("hpds {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
