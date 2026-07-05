//! The installer registry: which tools `hpds install` knows about, and the
//! lookup that maps a tool name to its [`Installer`].
//!
//! Concrete installers register themselves in [`INSTALLERS`]; tools listed
//! in [`KNOWN_TOOLS`] without a registered installer produce a typed
//! "lands soon" error so the CLI can exit 2 with guidance.

use thiserror::Error;

use super::Installer;
use super::installers::{
    duckdb::DuckDb, gh::Gh, quarto::Quarto, r::R, rig::Rig, tinytex::TinyTex, togi::Togi, uv::Uv,
};

/// Every tool `hpds install` is meant to manage, implemented or not.
pub const KNOWN_TOOLS: [&str; 8] = [
    "r", "quarto", "uv", "gh", "rig", "tinytex", "duckdb", "togi",
];

/// The installers implemented so far. Each new installer is added here to
/// become reachable from `hpds install <tool>`.
static INSTALLERS: &[&(dyn Installer + Sync)] =
    &[&Uv, &Gh, &Rig, &DuckDb, &R, &Quarto, &TinyTex, &Togi];

/// A tool name that cannot be dispatched to an installer. Rendered by
/// `main` with its [`hint`](RegistryError::hint) and exit code 2.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The name is not a tool hpds manages at all.
    #[error("`{name}` is not a tool hpds can install")]
    UnknownTool { name: String },
    /// The tool is on the roster but its installer is not built yet.
    #[error("the installer for `{name}` lands soon")]
    NotImplemented { name: String },
}

impl RegistryError {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        match self {
            RegistryError::UnknownTool { .. } => {
                format!("known tools: {}", KNOWN_TOOLS.join(", "))
            }
            RegistryError::NotImplemented { name } => format!(
                "install {name} manually for now; `hpds install {name}` ships in an upcoming release"
            ),
        }
    }
}

/// Look up the installer for `name`.
pub fn find(name: &str) -> Result<&'static dyn Installer, RegistryError> {
    if let Some(installer) = INSTALLERS.iter().find(|i| i.name() == name) {
        return Ok(*installer);
    }
    if KNOWN_TOOLS.contains(&name) {
        Err(RegistryError::NotImplemented {
            name: name.to_string(),
        })
    } else {
        Err(RegistryError::UnknownTool {
            name: name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `expect_err` needs `T: Debug`, which a `&dyn Installer` is not.
    fn lookup_error(name: &str) -> RegistryError {
        match find(name) {
            Ok(installer) => panic!("`{name}` unexpectedly found `{}`", installer.name()),
            Err(err) => err,
        }
    }

    #[test]
    fn unknown_tool_error_lists_every_known_tool() {
        let err = lookup_error("frobnicate");
        assert!(matches!(err, RegistryError::UnknownTool { .. }));
        assert!(err.to_string().contains("frobnicate"), "{err}");
        let hint = err.hint();
        for tool in KNOWN_TOOLS {
            assert!(hint.contains(tool), "hint must list {tool}: {hint}");
        }
    }

    #[test]
    fn the_not_implemented_error_keeps_its_guidance() {
        // Every current tool has an installer; the variant stays for the
        // next tool added to KNOWN_TOOLS ahead of its installer.
        let err = RegistryError::NotImplemented {
            name: "newtool".to_string(),
        };
        assert!(err.to_string().contains("lands soon"), "{err}");
        assert!(err.hint().contains("manually"), "{}", err.hint());
    }

    #[test]
    fn every_known_tool_resolves_to_its_installer() {
        for tool in KNOWN_TOOLS {
            let installer = find(tool).unwrap_or_else(|e| panic!("{tool}: {e}"));
            assert_eq!(installer.name(), tool);
        }
    }

    #[test]
    fn lookup_is_exact_no_case_folding_or_prefixes() {
        for name in ["Quarto", "QUARTO", "quart", "quarto2"] {
            let err = lookup_error(name);
            assert!(matches!(err, RegistryError::UnknownTool { .. }), "{name}");
        }
    }
}
