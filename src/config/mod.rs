//! Config discovery, parsing, and layering for `hpds.toml`.
//!
//! Layering: **built-in defaults ← user config ← project config ← CLI
//! flags**. Each file parses into a [`Layer`] (only the keys it actually
//! sets); layers are applied to [`Config::default`] in order, so later
//! layers win key-by-key.
//!
//! This module returns data only — it never prints. Unknown-key warnings are
//! returned on [`Loaded::warnings`] for the caller to report through `ui/`.

mod discover;
pub(crate) mod raw;

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::ui::HintExt;

/// Fully resolved configuration; `Default` is the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub project: ProjectConfig,
    pub audit: AuditConfig,
}

/// Valid `[project] status` values: the machine-readable lifecycle.
pub const PROJECT_STATUSES: &[&str] = &["active", "submitted", "published", "retired"];

/// `[project]`: lifecycle metadata used by `hpds audit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    /// active | submitted | published | retired
    pub status: String,
    /// GitHub username; audit checks they watch the repo.
    pub primary_author: String,
}

/// `[audit]`: knobs for `hpds audit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditConfig {
    /// Branches with no commits in more than this many days count as stale.
    pub stale_days: u32,
    /// GitHub logins that must watch every lab repo (the project's
    /// primary author is required in addition to these). Overridable via
    /// *user* config only — see [`strip_user_only_keys`].
    pub required_watchers: Vec<String>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            project: ProjectConfig {
                status: "active".to_string(),
                primary_author: String::new(),
            },
            audit: AuditConfig {
                stale_days: 90,
                required_watchers: strings(&["malcolmbarrett", "sherrirose"]),
            },
        }
    }
}

/// One configuration layer: only the keys this source actually set.
///
/// A parsed config file becomes a `Layer`, and CLI flags that override
/// config keys are expressed as a `Layer` too, so all four layers merge
/// through the same [`Config::apply`] path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub project_status: Option<String>,
    pub project_primary_author: Option<String>,
    pub audit_stale_days: Option<u32>,
    pub audit_required_watchers: Option<Vec<String>>,
}

impl Config {
    /// Apply a layer on top of `self`; every key the layer sets wins.
    pub fn apply(&mut self, layer: Layer) {
        if let Some(v) = layer.project_status {
            self.project.status = v;
        }
        if let Some(v) = layer.project_primary_author {
            self.project.primary_author = v;
        }
        if let Some(v) = layer.audit_stale_days {
            self.audit.stale_days = v;
        }
        if let Some(v) = layer.audit_required_watchers {
            self.audit.required_watchers = v;
        }
    }
}

/// Typed error for `--config` pointing at a file that does not exist: a
/// bad flag value, so `main` renders it as a usage error and exits 2.
#[derive(Debug, thiserror::Error)]
#[error("config file `{}` does not exist", path.display())]
pub struct MissingConfigFile {
    pub path: PathBuf,
}

impl MissingConfigFile {
    /// What to do next (every user-facing error must say).
    pub fn hint(&self) -> String {
        "check the path passed to --config, or drop the flag to discover \
         hpds.toml automatically"
            .to_string()
    }
}

/// The result of [`load`]: the resolved config, which files contributed,
/// and any unknown-key warnings for the caller to print via `ui::warn`.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// User config file, when it existed and was layered in.
    pub user_path: Option<PathBuf>,
    /// Project config file (`--config` or discovered `hpds.toml`).
    pub project_path: Option<PathBuf>,
    /// Human-readable warnings (unknown keys); print through `ui::warn`.
    pub warnings: Vec<String>,
}

/// Discover, parse, and layer configuration.
///
/// `explicit` is the global `--config <path>` flag: it replaces project-file
/// discovery and it is an error for it not to exist. `flags` carries any
/// CLI-flag overrides (the final layer).
pub fn load(cwd: &Path, explicit: Option<&Path>, flags: Layer) -> anyhow::Result<Loaded> {
    let mut config = Config::default();
    let mut warnings = Vec::new();

    let mut user_path = None;
    if let Some(path) = discover::user_config_path()
        && path.is_file()
    {
        config.apply(load_file(&path, &mut warnings)?);
        user_path = Some(path);
    }

    let project_path = match explicit {
        Some(path) => {
            if !path.is_file() {
                // Typed so `main` can exit 2: a bad flag value is a usage
                // error, not a runtime failure.
                return Err(anyhow::Error::new(MissingConfigFile {
                    path: path.to_path_buf(),
                }));
            }
            Some(path.to_path_buf())
        }
        None => discover::find_project_config(cwd),
    };
    if let Some(path) = &project_path {
        let mut layer = load_file(path, &mut warnings)?;
        strip_user_only_keys(&mut layer, path, &mut warnings);
        config.apply(layer);
    }

    config.apply(flags);

    Ok(Loaded {
        config,
        user_path,
        project_path,
        warnings,
    })
}

/// Drop keys only the *user* config layer may set, warning about each.
///
/// `[audit].required-watchers` is the auditor's requirement, not the
/// project's: the audited repo must not be able to rewrite the lab-lead
/// watcher list for everyone who audits it by committing an override in
/// its own `hpds.toml`, so the key is honored only from user config.
fn strip_user_only_keys(layer: &mut Layer, path: &Path, warnings: &mut Vec<String>) {
    if layer.audit_required_watchers.take().is_some() {
        warnings.push(format!(
            "ignoring `audit.required-watchers` in {}: project config cannot change \
             the required watcher list; set it in your user config instead \
             (`hpds config` shows its path)",
            path.display()
        ));
    }
}

/// Read and parse one config file into a layer, converting its unknown keys
/// into warnings that name the file.
fn load_file(path: &Path, warnings: &mut Vec<String>) -> anyhow::Result<Layer> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read config file `{}`", path.display()))
        .hint("check the file's permissions, or remove it if it should not exist")?;
    let parsed = raw::parse(&text)
        .with_context(|| format!("could not parse `{}`", path.display()))
        .hint("fix the TOML shown above; run `hpds config --help` and see hpds.toml docs for the supported keys")?;
    for key in parsed.unknown_keys {
        warnings.push(format!(
            "ignoring unknown key `{key}` in {}",
            path.display()
        ));
    }
    Ok(parsed.layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_project_and_audit() {
        let config = Config::default();
        assert_eq!(config.project.status, "active");
        assert_eq!(config.project.primary_author, "");
        assert_eq!(config.audit.stale_days, 90);
        assert_eq!(
            config.audit.required_watchers,
            strings(&["malcolmbarrett", "sherrirose"])
        );
    }

    #[test]
    fn audit_stale_days_layers_like_any_other_key() {
        let user = Layer {
            audit_stale_days: Some(30),
            ..Layer::default()
        };
        let project = Layer {
            audit_stale_days: Some(45),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        assert_eq!(config.audit.stale_days, 30);
        config.apply(project);
        assert_eq!(config.audit.stale_days, 45);
        // a layer that does not set the key leaves it alone
        config.apply(Layer::default());
        assert_eq!(config.audit.stale_days, 45);
    }

    #[test]
    fn layering_defaults_then_user_then_project_then_flags() {
        // default < user < project < flag. Each layer overrides only the
        // keys it sets; everything else shines through.
        let user = Layer {
            audit_stale_days: Some(30),
            project_status: Some("submitted".to_string()),
            project_primary_author: Some("malcolm".to_string()),
            ..Layer::default()
        };
        let project = Layer {
            audit_stale_days: Some(45),
            project_status: Some("published".to_string()),
            ..Layer::default()
        };
        let flags = Layer {
            audit_stale_days: Some(7),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        config.apply(project);
        config.apply(flags);

        // flag beat project beat user for audit.stale-days
        assert_eq!(config.audit.stale_days, 7);
        // project beat user for status
        assert_eq!(config.project.status, "published");
        // user's value survives where nothing above set the key
        assert_eq!(config.project.primary_author, "malcolm");
        // untouched keys keep built-in defaults
        assert_eq!(
            config.audit.required_watchers,
            strings(&["malcolmbarrett", "sherrirose"])
        );
    }

    #[test]
    fn audit_keys_layer_like_everything_else() {
        // User config overrides the built-in lab leads; a later layer wins.
        let user = Layer {
            audit_required_watchers: Some(strings(&["lead1", "lead2"])),
            audit_stale_days: Some(30),
            ..Layer::default()
        };
        let project = Layer {
            audit_stale_days: Some(45),
            ..Layer::default()
        };
        let mut config = Config::default();
        config.apply(user);
        config.apply(project);
        assert_eq!(config.audit.stale_days, 45);
        assert_eq!(config.audit.required_watchers, strings(&["lead1", "lead2"]));
    }

    #[test]
    fn strip_user_only_keys_drops_required_watchers_with_a_warning() {
        let mut layer = Layer {
            audit_required_watchers: Some(vec![]),
            audit_stale_days: Some(30),
            ..Layer::default()
        };
        let mut warnings = Vec::new();
        strip_user_only_keys(&mut layer, Path::new("/repo/hpds.toml"), &mut warnings);

        assert_eq!(layer.audit_required_watchers, None);
        // stale-days is an ordinary per-project knob and survives.
        assert_eq!(layer.audit_stale_days, Some(30));
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("audit.required-watchers"),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("/repo/hpds.toml"), "{warnings:?}");
        assert!(warnings[0].contains("user config"), "{warnings:?}");
    }

    #[test]
    fn strip_user_only_keys_is_silent_when_the_key_is_absent() {
        let mut layer = Layer {
            audit_stale_days: Some(30),
            ..Layer::default()
        };
        let mut warnings = Vec::new();
        strip_user_only_keys(&mut layer, Path::new("/repo/hpds.toml"), &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn load_reports_missing_explicit_config_as_a_typed_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let err = load(dir.path(), Some(&missing), Layer::default())
            .expect_err("missing --config file must be an error");
        assert!(
            err.to_string().contains("nope.toml"),
            "names the file: {err}"
        );
        let typed = err
            .downcast_ref::<MissingConfigFile>()
            .expect("typed so main can exit 2 (usage error)");
        assert!(typed.hint().contains("--config"), "hint: {}", typed.hint());
    }
}
