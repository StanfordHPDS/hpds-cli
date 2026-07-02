//! The `hpds use` component registry.
//!
//! Each component contributes a [`Component`] entry: a name (the positional
//! argument to `hpds use`), a one-line description for the no-argument
//! listing, and a `run` function that renders the component into the
//! project. Components return their [`FileOutcome`]s as data; the `hpds use`
//! command layer prints them through `ui/`.
//!
//! To add a component: create its module here, give it a
//! `pub static COMPONENT: Component`, and append one line to [`COMPONENTS`].

pub mod pipeline;
pub mod readme;
pub mod slurm;

use std::path::Path;

use crate::ui::HintExt;

use super::{FileOutcome, Vars};

/// Everything a component's `run` needs.
pub struct ComponentCtx<'a> {
    /// The component-specific `--kind` flag, unvalidated; each component
    /// defines (and checks) its own accepted values.
    pub kind: Option<&'a str>,
    /// Overwrite files that conflict with the template.
    pub force: bool,
    /// Project root the component renders into.
    pub dest: &'a Path,
    /// Standard substitution variables (project, language, year, author).
    pub vars: Vars,
}

/// One `hpds use` component.
#[derive(Clone, Copy)]
pub struct Component {
    /// The positional argument to `hpds use`.
    pub name: &'static str,
    /// One line for the no-argument component listing.
    pub description: &'static str,
    /// Render the component into `ctx.dest`, returning what happened to
    /// each file. May prompt (via `ui`) for anything `--kind` did not
    /// answer.
    pub run: fn(&ComponentCtx) -> anyhow::Result<Vec<FileOutcome>>,
}

/// All registered components, in listing order.
pub static COMPONENTS: &[Component] = &[
    pipeline::COMPONENT,
    readme::COMPONENT,
    slurm::COMPONENT,
    // next component here (one line each)
];

/// Look a component up by its `hpds use` name.
pub fn find(name: &str) -> Option<&'static Component> {
    COMPONENTS.iter().find(|c| c.name == name)
}

/// Sorted, comma-separated component names for error hints.
pub fn names() -> String {
    COMPONENTS
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Fail with a do-this-next error when `--kind` was passed to a component
/// that has no variants.
pub(crate) fn reject_kind(ctx: &ComponentCtx, component: &str) -> anyhow::Result<()> {
    if let Some(kind) = ctx.kind {
        anyhow::bail!(
            "the `{component}` component has no --kind variants \
             (got `--kind {kind}`); drop the --kind flag and re-run"
        );
    }
    Ok(())
}

/// The project language from the standard vars, for components that render
/// differently per language. Absent (no `--language` flag and nothing
/// detectable in the project) is an error pointing at the flag.
pub(crate) fn require_language<'v>(
    ctx: &'v ComponentCtx<'_>,
    component: &str,
) -> anyhow::Result<&'v str> {
    ctx.vars
        .get("language")
        .ok_or_else(|| anyhow::anyhow!("could not detect the project language"))
        .hint(format!(
            "the `{component}` component renders differently per language; \
             pass it explicitly, e.g. `hpds use {component} --language r` \
             (r, python, or both)"
        ))
}

#[cfg(test)]
pub(crate) fn test_ctx<'a>(dest: &'a Path, language: &str) -> ComponentCtx<'a> {
    ComponentCtx {
        kind: None,
        force: false,
        dest,
        vars: Vars::standard("malaria-icu", Some(language), "HPDS Lab"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui;

    #[test]
    fn find_returns_the_pipeline_component() {
        let component = find("pipeline").expect("pipeline is registered");
        assert_eq!(component.name, "pipeline");
        assert!(!component.description.is_empty());
    }

    #[test]
    fn find_returns_the_readme_and_slurm_components() {
        for name in ["readme", "slurm"] {
            let component = find(name).unwrap_or_else(|| panic!("{name} is registered"));
            assert_eq!(component.name, name);
        }
    }

    #[test]
    fn find_returns_none_for_an_unknown_name() {
        assert!(find("frobnicate").is_none());
    }

    #[test]
    fn every_component_has_a_unique_name_and_a_description() {
        let mut names: Vec<_> = COMPONENTS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "component names must be unique");
        for component in COMPONENTS {
            assert!(
                !component.description.is_empty(),
                "{} needs a description for the listing",
                component.name
            );
        }
    }

    #[test]
    fn reject_kind_errors_with_drop_the_flag_advice() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("make");
        let err = reject_kind(&ctx, "readme").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--kind make"), "names the bad flag: {msg}");
        assert!(
            msg.contains("drop the --kind flag"),
            "says what to do: {msg}"
        );
    }

    #[test]
    fn require_language_error_points_at_the_language_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.vars = Vars::standard("malaria-icu", None, "HPDS Lab");
        let err = require_language(&ctx, "readme").unwrap_err();
        let rendered = ui::render_error(&err, false);
        assert!(rendered.contains("could not detect"), "{rendered}");
        assert!(rendered.contains("--language"), "{rendered}");
    }

    #[test]
    fn require_language_returns_the_language_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path(), "python");
        assert_eq!(require_language(&ctx, "readme").unwrap(), "python");
    }
}
