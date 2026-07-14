//! `hpds use`: apply a template component to the current project.
//!
//! With no component, lists every registered component with its one-line
//! description. With a component, builds the [`ComponentCtx`] (kind/force
//! flags plus project metadata) and hands off to the component's `run`,
//! then reports each file outcome through `ui/`.

use std::path::Path;

use anyhow::Context;
use clap::Args;

use crate::config;
use crate::templates::components::{self, ComponentCtx};
use crate::templates::{FileOutcome, Vars, WriteOutcome};
use crate::ui;

use super::GlobalArgs;

#[derive(Debug, Args)]
pub struct UseArgs {
    /// Component to apply; omit to list the available components
    #[arg(value_name = "COMPONENT")]
    pub component: Option<String>,

    /// Component-specific variant (e.g. make, targets, or both for
    /// pipeline); omit to choose interactively (pipeline's default is make
    /// under `hpds init --yes`)
    #[arg(long, value_name = "VALUE")]
    pub kind: Option<String>,

    /// Overwrite existing files that differ from the template
    #[arg(long)]
    pub force: bool,

    /// Project language; detected from project files (renv.lock,
    /// pyproject.toml, ...) when omitted
    #[arg(long, value_parser = ["r", "python", "both"])]
    pub language: Option<String>,

    /// gha workflows to add without prompting (comma-separated or repeated:
    /// pr-template, lint, audit-bot)
    #[arg(long, value_delimiter = ',', value_name = "NAMES")]
    pub workflows: Option<Vec<String>>,
}

pub fn run(args: UseArgs, global: &GlobalArgs) -> anyhow::Result<()> {
    let Some(name) = args.component else {
        list_components();
        return Ok(());
    };
    let component = components::find(&name).ok_or_else(|| {
        super::usage_error(
            format!("`{name}` is not a template component"),
            format!(
                "run `hpds use <component>` with one of: {}",
                components::names()
            ),
        )
    })?;

    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let language = args
        .language
        .or_else(|| detect_language(&cwd).map(str::to_string));
    let ctx = ComponentCtx {
        kind: args.kind.as_deref(),
        workflows: args.workflows.as_deref(),
        force: args.force,
        dest: &cwd,
        vars: standard_vars(&cwd, global, language.as_deref())?,
        guidance: std::cell::RefCell::new(Vec::new()),
    };
    let outcomes = (component.run)(&ctx)?;
    let conflicts = report_outcomes(&outcomes);
    if conflicts > 0 {
        let plural = if conflicts == 1 { "file" } else { "files" };
        ui::println(&format!(
            "re-run `hpds use {name} --force` to overwrite the {conflicts} skipped {plural}"
        ));
    }
    // What-to-do-next lines the component collected; after the outcomes,
    // so the advice follows the `created ...` report it refers to.
    for line in ctx.guidance.borrow().iter() {
        ui::println(line);
    }
    Ok(())
}

/// Print every registered component with its description.
fn list_components() {
    ui::println("Available components (apply with `hpds use <component>`):");
    ui::println("");
    let width = components::COMPONENTS
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    for component in components::COMPONENTS {
        ui::println(&format!(
            "  {:width$}  {}",
            component.name, component.description
        ));
    }
}

/// The standard substitution variables for `dest`: project name from the
/// directory, author from the resolved config, the current year, and the
/// project language when known (`--language` or detection); components
/// that need the language ask for the flag when it is absent.
fn standard_vars(dest: &Path, global: &GlobalArgs, language: Option<&str>) -> anyhow::Result<Vars> {
    let project = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let loaded = config::load(dest, global.config.as_deref(), config::Layer::default())?;
    for warning in &loaded.warnings {
        ui::warn(warning);
    }
    Ok(Vars::standard(
        project,
        language,
        &loaded.config.project.primary_author,
    ))
}

/// Best-effort language detection from well-known project files. `None`
/// when nothing identifies the project; components that care ask the user
/// for `--language`. Also used by `hpds init` under `--yes`.
pub(crate) fn detect_language(root: &Path) -> Option<&'static str> {
    let r = ["renv.lock", "renv", "DESCRIPTION", "_targets.R"]
        .iter()
        .any(|marker| root.join(marker).exists())
        || has_rproj_file(root);
    let python = ["pyproject.toml", "uv.lock", "requirements.txt"]
        .iter()
        .any(|marker| root.join(marker).exists());
    match (r, python) {
        (true, true) => Some("both"),
        (true, false) => Some("r"),
        (false, true) => Some("python"),
        (false, false) => None,
    }
}

/// Whether the directory holds an RStudio `*.Rproj` file.
fn has_rproj_file(root: &Path) -> bool {
    std::fs::read_dir(root)
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("Rproj"))
            })
        })
        .unwrap_or(false)
}

/// Report one line per file through `ui/`, with a diff preview for every
/// conflict that was skipped. Returns the number of skipped conflicts so
/// the caller can point at its own `--force` re-run command. Shared with
/// `hpds init`, which applies the same components.
pub(crate) fn report_outcomes(outcomes: &[FileOutcome]) -> usize {
    let mut conflicts = 0usize;
    for FileOutcome { path, outcome } in outcomes {
        match outcome {
            WriteOutcome::Created => ui::success(&format!("created {}", path.display())),
            WriteOutcome::Overwritten => ui::success(&format!("overwrote {}", path.display())),
            WriteOutcome::Unchanged => {
                ui::println(&format!("{} is already up to date", path.display()));
            }
            WriteOutcome::SkippedConflict { diff } => {
                conflicts += 1;
                ui::warn(&format!(
                    "skipped {}: it already exists with different content",
                    path.display()
                ));
                ui::println(diff);
            }
        }
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_directory_detects_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(detect_language(tmp.path()), None);
    }

    #[test]
    fn renv_lock_marks_an_r_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("renv.lock"), "{}").unwrap();
        assert_eq!(detect_language(tmp.path()), Some("r"));
    }

    #[test]
    fn rproj_file_marks_an_r_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("analysis.Rproj"), "Version: 1.0\n").unwrap();
        assert_eq!(detect_language(tmp.path()), Some("r"));
    }

    #[test]
    fn pyproject_marks_a_python_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_language(tmp.path()), Some("python"));
    }

    #[test]
    fn r_and_python_markers_together_mean_both() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("renv.lock"), "{}").unwrap();
        std::fs::write(tmp.path().join("uv.lock"), "").unwrap();
        assert_eq!(detect_language(tmp.path()), Some("both"));
    }
}
