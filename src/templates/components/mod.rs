//! The `hpds use` component registry.
//!
//! Each component contributes a [`Component`] entry: a name (the positional
//! argument to `hpds use`), a one-line description for the no-argument
//! listing, and a `run` function that renders the component into the
//! project. Components return their [`FileOutcome`]s as data; the `hpds use`
//! command layer prints them through `ui/`.
//!
//! To add a component: create its module here, give it a
//! `pub static COMPONENT: Component` (modules that provide several, like
//! `fetched`, name each static after its component), and append one line
//! to [`COMPONENTS`].

pub mod container;
pub mod fetched;
pub mod gha;
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
    /// The gha component's `--workflows` selection, unvalidated; every
    /// other component rejects the flag.
    pub workflows: Option<&'a [String]>,
    /// Overwrite files that conflict with the template.
    pub force: bool,
    /// Project root the component renders into.
    pub dest: &'a Path,
    /// Standard substitution variables (project, language, year, author).
    pub vars: Vars,
    /// What-to-do-next lines collected by the component. The command layer
    /// prints them after the file outcomes, so the guidance always follows
    /// the `created ...` report it refers to.
    pub guidance: std::cell::RefCell<Vec<String>>,
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
    container::COMPONENT,
    gha::COMPONENT,
    pipeline::COMPONENT,
    fetched::POSTER,
    readme::COMPONENT,
    fetched::SLIDES,
    slurm::COMPONENT,
    fetched::THESIS,
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

/// Fail with a do-this-next error when `--workflows` was passed to a
/// component other than gha.
pub(crate) fn reject_workflows(ctx: &ComponentCtx, component: &str) -> anyhow::Result<()> {
    if let Some(workflows) = ctx.workflows {
        anyhow::bail!(
            "the `{component}` component does not take --workflows \
             (got `--workflows {}`); drop the --workflows flag and re-run",
            workflows.join(",")
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
        workflows: None,
        force: false,
        dest,
        vars: Vars::standard("malaria-icu", Some(language), "HPDS Lab"),
        guidance: std::cell::RefCell::new(Vec::new()),
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
    fn find_returns_every_other_registered_component() {
        for name in [
            "readme",
            "slurm",
            "container",
            "gha",
            "slides",
            "poster",
            "thesis",
        ] {
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
    fn reject_workflows_errors_with_drop_the_flag_advice() {
        let tmp = tempfile::tempdir().unwrap();
        let workflows = vec!["pr-template".to_string(), "lint".to_string()];
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.workflows = Some(&workflows);
        let err = reject_workflows(&ctx, "readme").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--workflows pr-template,lint"),
            "names the bad flag: {msg}"
        );
        assert!(
            msg.contains("drop the --workflows flag"),
            "says what to do: {msg}"
        );
    }

    #[test]
    fn reject_workflows_passes_when_the_flag_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(tmp.path(), "r");
        assert!(reject_workflows(&ctx, "readme").is_ok());
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
