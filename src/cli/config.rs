//! `hpds config` — print the resolved, layered configuration.
//!
//! This is the debugging story for "why did it do that?": every contributing
//! file's path is shown alongside the fully resolved values.

use anyhow::Context;
use clap::{Args, ValueEnum};

use crate::config::{self, Config, Layer, Loaded};
use crate::ui;

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

pub fn run(args: ConfigArgs, global: &super::GlobalArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    // No CLI flags map onto config keys yet; when they do (e.g. future
    // format/lint flags) they become this final layer.
    let loaded = config::load(&cwd, global.config.as_deref(), Layer::default())?;

    for warning in &loaded.warnings {
        ui::warn(warning);
    }

    match args.format {
        OutputFormat::Text => ui::println(&render_text(&loaded)),
        OutputFormat::Json => ui::data(&render_json(&loaded)?),
    }
    Ok(())
}

/// TOML-shaped text: source comments up top, then the resolved values in
/// the design order.
fn render_text(loaded: &Loaded) -> String {
    let config = &loaded.config;
    let mut out = String::from("# resolved hpds config\n# sources: built-in defaults\n");
    if let Some(path) = &loaded.user_path {
        out.push_str(&format!("#          user config    {}\n", path.display()));
    }
    if let Some(path) = &loaded.project_path {
        out.push_str(&format!("#          project config {}\n", path.display()));
    }

    out.push_str(&format!(
        "\n[project]\nstatus = {}\nprimary-author = {}\n",
        toml_string(&config.project.status),
        toml_string(&config.project.primary_author),
    ));
    out.push_str(&format!(
        "\n[format]\nlanguages = {}\nexclude = {}\n",
        toml_array(&config.format.languages),
        toml_array(&config.format.exclude),
    ));
    out.push_str(&format!(
        "\n[lint]\nlanguages = {}\nexclude = {}\n",
        toml_array(&config.lint.languages),
        toml_array(&config.lint.exclude),
    ));
    out.push_str(&format!(
        "\n[sql]\ndialect = {}\n",
        toml_string(&config.sql.dialect)
    ));
    out.push_str(&render_tools_text(config));
    out
}

/// `[tools]`: pin-only tools render as `name = "version"`; tools with args
/// get a `[tools.<name>]` table (with `version` inside when also pinned, so
/// the output stays valid TOML).
fn render_tools_text(config: &Config) -> String {
    let tools = &config.tools;
    let mut out = String::new();

    let pin_only: Vec<_> = tools
        .pins
        .iter()
        .filter(|(name, _)| !tools.args.contains_key(*name))
        .collect();
    if !pin_only.is_empty() {
        out.push_str("\n[tools]\n");
        for (name, version) in pin_only {
            out.push_str(&format!("{name} = {}\n", toml_string(version)));
        }
    }
    for (name, args) in &tools.args {
        out.push_str(&format!("\n[tools.{name}]\n"));
        if let Some(version) = tools.pins.get(name) {
            out.push_str(&format!("version = {}\n", toml_string(version)));
        }
        out.push_str(&format!("args = {}\n", toml_array(args)));
    }
    out
}

fn render_json(loaded: &Loaded) -> anyhow::Result<String> {
    let config = &loaded.config;

    let mut tools = serde_json::Map::new();
    let tool_names = config.tools.pins.keys().chain(config.tools.args.keys());
    for name in tool_names {
        // BTreeMap keys iterate sorted; chaining may repeat a name, but the
        // insert below writes the same object twice, so that is harmless.
        let mut tool = serde_json::Map::new();
        if let Some(version) = config.tools.pins.get(name) {
            tool.insert("version".to_string(), serde_json::json!(version));
        }
        if let Some(args) = config.tools.args.get(name) {
            tool.insert("args".to_string(), serde_json::json!(args));
        }
        tools.insert(name.clone(), serde_json::Value::Object(tool));
    }

    let path_or_null = |path: &Option<std::path::PathBuf>| {
        path.as_ref()
            .map(|p| serde_json::json!(p.display().to_string()))
            .unwrap_or(serde_json::Value::Null)
    };

    let value = serde_json::json!({
        "sources": {
            "user": path_or_null(&loaded.user_path),
            "project": path_or_null(&loaded.project_path),
        },
        "config": {
            "project": {
                "status": config.project.status,
                "primary-author": config.project.primary_author,
            },
            "format": {
                "languages": config.format.languages,
                "exclude": config.format.exclude,
            },
            "lint": {
                "languages": config.lint.languages,
                "exclude": config.lint.exclude,
            },
            "sql": { "dialect": config.sql.dialect },
            "tools": tools,
        },
    });
    serde_json::to_string_pretty(&value).context("could not serialize the resolved config to JSON")
}

/// Quote a string as a TOML value (handles escaping).
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

/// Format a string list as a TOML array value.
fn toml_array(items: &[String]) -> String {
    toml::Value::Array(
        items
            .iter()
            .map(|item| toml::Value::String(item.clone()))
            .collect(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn loaded_with(config: Config) -> Loaded {
        Loaded {
            config,
            user_path: Some(PathBuf::from("/home/x/.config/hpds/config.toml")),
            project_path: Some(PathBuf::from("/repo/hpds.toml")),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn text_output_lists_all_contributing_sources() {
        let out = render_text(&loaded_with(Config::default()));
        assert!(out.contains("built-in defaults"));
        assert!(out.contains("/home/x/.config/hpds/config.toml"));
        assert!(out.contains("/repo/hpds.toml"));
    }

    #[test]
    fn text_output_renders_defaults_as_valid_toml_sections() {
        let out = render_text(&loaded_with(Config::default()));
        assert!(out.contains("[project]\nstatus = \"active\"\nprimary-author = \"\""));
        assert!(out.contains("languages = [\"r\", \"python\", \"quarto\", \"sql\", \"markdown\"]"));
        assert!(out.contains("[sql]\ndialect = \"bigquery\""));
        // no tools configured -> no tools section at all
        assert!(!out.contains("[tools"));
    }

    #[test]
    fn text_output_splits_pins_and_arg_tables() {
        let mut config = Config::default();
        config.tools.pins = BTreeMap::from([
            ("air".to_string(), "0.10.0".to_string()),
            ("ruff".to_string(), "0.14.0".to_string()),
        ]);
        config.tools.args = BTreeMap::from([("ruff".to_string(), vec!["--fast".to_string()])]);

        let out = render_text(&loaded_with(config));
        // pin-only tool under [tools]; pinned tool with args gets a table
        // with version inside so the output stays valid TOML.
        assert!(out.contains("[tools]\nair = \"0.10.0\""));
        assert!(out.contains("[tools.ruff]\nversion = \"0.14.0\"\nargs = [\"--fast\"]"));
    }

    #[test]
    fn json_output_round_trips_and_marks_missing_sources_null() {
        let loaded = Loaded {
            config: Config::default(),
            user_path: None,
            project_path: None,
            warnings: Vec::new(),
        };
        let out = render_json(&loaded).expect("render json");
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert!(value["sources"]["user"].is_null());
        assert!(value["sources"]["project"].is_null());
        assert_eq!(value["config"]["project"]["status"], "active");
        assert_eq!(value["config"]["sql"]["dialect"], "bigquery");
    }
}
