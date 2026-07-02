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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::ui::HintExt;

/// Fully resolved configuration; `Default` is the the design built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub project: ProjectConfig,
    pub format: FileSelection,
    pub lint: FileSelection,
    pub sql: SqlConfig,
    pub audit: AuditConfig,
    pub tools: ToolsConfig,
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

/// `[format]` / `[lint]`: which languages to include and what to skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelection {
    pub languages: Vec<String>,
    /// gitignore-style globs, additive to `.gitignore`.
    pub exclude: Vec<String>,
}

/// `[sql]`: passed through to sqlfluff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlConfig {
    pub dialect: String,
}

/// `[audit]`: knobs for `hpds audit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditConfig {
    /// A branch with no commits in more than this many days is stale.
    pub stale_days: u32,
}

/// `[tools]`: version pins plus per-tool passthrough args.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolsConfig {
    /// `air = "0.10.0"` style pins (also `[tools.air] version = "0.10.0"`).
    pub pins: BTreeMap<String, String>,
    /// `[tools.air] args = [...]` escape-hatch passthrough args.
    pub args: BTreeMap<String, Vec<String>>,
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
            format: FileSelection {
                languages: strings(&["r", "python", "quarto", "sql", "markdown"]),
                exclude: Vec::new(),
            },
            lint: FileSelection {
                languages: strings(&["r", "python", "quarto", "sql"]),
                exclude: Vec::new(),
            },
            sql: SqlConfig {
                dialect: "bigquery".to_string(),
            },
            audit: AuditConfig { stale_days: 90 },
            tools: ToolsConfig::default(),
        }
    }
}

/// One configuration layer: only the keys this source actually set.
///
/// A parsed config file becomes a `Layer`, and CLI flags that override
/// config keys are expressed as a `Layer` too, so all four spec layers merge
/// through the same [`Config::apply`] path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub project_status: Option<String>,
    pub project_primary_author: Option<String>,
    pub format_languages: Option<Vec<String>>,
    pub format_exclude: Option<Vec<String>>,
    pub lint_languages: Option<Vec<String>>,
    pub lint_exclude: Option<Vec<String>>,
    pub sql_dialect: Option<String>,
    pub audit_stale_days: Option<u32>,
    pub tool_pins: BTreeMap<String, String>,
    pub tool_args: BTreeMap<String, Vec<String>>,
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
        if let Some(v) = layer.format_languages {
            self.format.languages = v;
        }
        if let Some(v) = layer.format_exclude {
            self.format.exclude = v;
        }
        if let Some(v) = layer.lint_languages {
            self.lint.languages = v;
        }
        if let Some(v) = layer.lint_exclude {
            self.lint.exclude = v;
        }
        if let Some(v) = layer.sql_dialect {
            self.sql.dialect = v;
        }
        if let Some(v) = layer.audit_stale_days {
            self.audit.stale_days = v;
        }
        self.tools.pins.extend(layer.tool_pins);
        self.tools.args.extend(layer.tool_args);
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
                return Err(anyhow::anyhow!(
                    "config file `{}` does not exist",
                    path.display()
                ))
                .hint(
                    "check the path passed to --config, or drop the flag to \
                     discover hpds.toml automatically",
                );
            }
            Some(path.to_path_buf())
        }
        None => discover::find_project_config(cwd),
    };
    if let Some(path) = &project_path {
        config.apply(load_file(path, &mut warnings)?);
    }

    config.apply(flags);

    Ok(Loaded {
        config,
        user_path,
        project_path,
        warnings,
    })
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
    fn defaults_match_spec_section_3() {
        let config = Config::default();
        assert_eq!(config.project.status, "active");
        assert_eq!(config.project.primary_author, "");
        assert_eq!(
            config.format.languages,
            strings(&["r", "python", "quarto", "sql", "markdown"])
        );
        assert!(config.format.exclude.is_empty());
        assert_eq!(
            config.lint.languages,
            strings(&["r", "python", "quarto", "sql"])
        );
        assert!(config.lint.exclude.is_empty());
        assert_eq!(config.sql.dialect, "bigquery");
        assert_eq!(config.audit.stale_days, 90);
        assert!(config.tools.pins.is_empty());
        assert!(config.tools.args.is_empty());
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
        // default < user < project < flag, per the design. Each layer overrides
        // only the keys it sets; everything else shines through.
        let user = Layer {
            sql_dialect: Some("duckdb".to_string()),
            project_status: Some("submitted".to_string()),
            project_primary_author: Some("malcolm".to_string()),
            ..Layer::default()
        };
        let project = Layer {
            sql_dialect: Some("postgres".to_string()),
            project_status: Some("published".to_string()),
            ..Layer::default()
        };
        let flags = Layer {
            sql_dialect: Some("sqlite".to_string()),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        config.apply(project);
        config.apply(flags);

        // flag beat project beat user for sql.dialect
        assert_eq!(config.sql.dialect, "sqlite");
        // project beat user for status
        assert_eq!(config.project.status, "published");
        // user's value survives where nothing above set the key
        assert_eq!(config.project.primary_author, "malcolm");
        // untouched keys keep built-in defaults
        assert_eq!(
            config.lint.languages,
            strings(&["r", "python", "quarto", "sql"])
        );
    }

    #[test]
    fn user_layer_alone_overrides_defaults() {
        let mut config = Config::default();
        config.apply(Layer {
            format_languages: Some(strings(&["r"])),
            format_exclude: Some(strings(&["renv/**"])),
            ..Layer::default()
        });
        assert_eq!(config.format.languages, strings(&["r"]));
        assert_eq!(config.format.exclude, strings(&["renv/**"]));
        // lint untouched
        assert_eq!(
            config.lint.languages,
            strings(&["r", "python", "quarto", "sql"])
        );
    }

    #[test]
    fn tool_pins_and_args_merge_per_tool_across_layers() {
        let user = Layer {
            tool_pins: BTreeMap::from([
                ("air".to_string(), "0.9.0".to_string()),
                ("ruff".to_string(), "0.14.0".to_string()),
            ]),
            tool_args: BTreeMap::from([("air".to_string(), strings(&["--old"]))]),
            ..Layer::default()
        };
        let project = Layer {
            tool_pins: BTreeMap::from([("air".to_string(), "0.10.0".to_string())]),
            tool_args: BTreeMap::from([("air".to_string(), strings(&["--new"]))]),
            ..Layer::default()
        };

        let mut config = Config::default();
        config.apply(user);
        config.apply(project);

        // project pin wins for air; user pin for ruff survives
        assert_eq!(config.tools.pins["air"], "0.10.0");
        assert_eq!(config.tools.pins["ruff"], "0.14.0");
        // args replace wholesale per tool, they do not concatenate
        assert_eq!(config.tools.args["air"], strings(&["--new"]));
    }

    #[test]
    fn load_reports_missing_explicit_config_with_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let err = load(dir.path(), Some(&missing), Layer::default())
            .expect_err("missing --config file must be an error");
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("nope.toml"), "names the file: {rendered}");
        assert!(rendered.contains("hint:"), "says what to do: {rendered}");
    }
}
