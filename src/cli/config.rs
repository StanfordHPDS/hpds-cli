//! `hpds config` — print the resolved, layered configuration.
//!
//! This is the debugging story for "why did it do that?": every contributing
//! file's path is shown alongside the fully resolved values.

use anyhow::Context;
use clap::{Args, ValueEnum};

use crate::config::{self, Layer, Loaded};
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
    // No CLI flags map onto config keys yet; when they do they become this
    // final layer.
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

/// TOML-shaped text: source comments up top, then the resolved values.
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
        "\n[audit]\nstale-days = {}\nrequired-watchers = {}\n",
        config.audit.stale_days,
        toml_array(&config.audit.required_watchers),
    ));
    out
}

fn render_json(loaded: &Loaded) -> anyhow::Result<String> {
    let config = &loaded.config;

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
            "audit": {
                "stale-days": config.audit.stale_days,
                "required-watchers": config.audit.required_watchers,
            },
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
    use crate::config::Config;
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
        assert!(out.contains(
            "[audit]\nstale-days = 90\nrequired-watchers = [\"malcolmbarrett\", \"sherrirose\"]"
        ));
        // The whole rendering (minus the comment header) is parseable TOML.
        let body: String = out
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        body.parse::<toml::Table>().expect("output is valid TOML");
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
        assert_eq!(value["config"]["audit"]["stale-days"], 90);
    }
}
