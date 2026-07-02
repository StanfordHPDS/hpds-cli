//! `hpds version` — print the hpds version.

/// Print the version. Baked tool versions are appended once the toolchain
/// manager defines its version constants.
///
/// TODO: route this print through `src/ui/`.
pub fn run() -> anyhow::Result<()> {
    println!("hpds {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
