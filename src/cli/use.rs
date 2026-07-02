//! `hpds use` — apply a template component to the current project.
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

    /// Component-specific variant (e.g. make, targets, or both for pipeline)
    #[arg(long, value_name = "VALUE")]
    pub kind: Option<String>,

    /// Overwrite existing files that differ from the template
    #[arg(long)]
    pub force: bool,
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
    let ctx = ComponentCtx {
        kind: args.kind.as_deref(),
        force: args.force,
        dest: &cwd,
        vars: standard_vars(&cwd, global)?,
    };
    let outcomes = (component.run)(&ctx)?;
    report(&name, &outcomes);
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
/// directory, author from the resolved config, plus language and year.
fn standard_vars(dest: &Path, global: &GlobalArgs) -> anyhow::Result<Vars> {
    let project = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let loaded = config::load(dest, global.config.as_deref(), config::Layer::default())?;
    for warning in &loaded.warnings {
        ui::warn(warning);
    }
    // Project-level language metadata lands with `hpds init`; until a
    // component needs to distinguish, R is the lab default.
    Ok(Vars::standard(
        project,
        "r",
        &loaded.config.project.primary_author,
    ))
}

/// Report one line per file through `ui/`, with a diff preview and a
/// `--force` pointer for every conflict that was skipped.
fn report(component: &str, outcomes: &[FileOutcome]) {
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
    if conflicts > 0 {
        let plural = if conflicts == 1 { "file" } else { "files" };
        ui::println(&format!(
            "re-run `hpds use {component} --force` to overwrite the {conflicts} skipped {plural}"
        ));
    }
}
