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

use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_returns_the_pipeline_component() {
        let component = find("pipeline").expect("pipeline is registered");
        assert_eq!(component.name, "pipeline");
        assert!(!component.description.is_empty());
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
}
