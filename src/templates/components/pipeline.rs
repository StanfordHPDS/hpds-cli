//! `hpds use pipeline` — Makefile and/or targets-based pipeline scaffolding.
//!
//! Kinds:
//! - `make`: a Makefile with `clean`, `deep-clean`, and `sync-mtimes`
//!   starter targets.
//! - `targets`: a `_targets.R` (targets + tarchetypes) with a starter
//!   pipeline and a renv note.
//! - `both`: the targets setup plus a Makefile whose default target runs
//!   `Rscript -e 'targets::tar_make()'`.

use std::fmt;

use crate::templates::{FileOutcome, TEMPLATES, apply_dir};
use crate::ui;
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "pipeline",
    description: "Makefile and/or targets-based R pipeline scaffolding",
    run,
};

/// Which pipeline scaffolding to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Make,
    Targets,
    Both,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Make, Kind::Targets, Kind::Both];

    fn name(self) -> &'static str {
        match self {
            Kind::Make => "make",
            Kind::Targets => "targets",
            Kind::Both => "both",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Kind> {
        Kind::ALL
            .into_iter()
            .find(|k| k.name() == value)
            .ok_or_else(|| anyhow::anyhow!("`{value}` is not a pipeline kind"))
            .hint("pass --kind make, --kind targets, or --kind both")
    }

    /// The embedded template directories this kind renders, in order.
    fn template_dirs(self) -> &'static [&'static str] {
        match self {
            Kind::Make => &["pipeline/make"],
            Kind::Targets => &["pipeline/targets"],
            Kind::Both => &["pipeline/targets", "pipeline/both"],
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Every kind name, in menu order. `hpds init` validates `--use
/// pipeline:<kind>` against this list before it writes anything.
pub(crate) fn kind_names() -> Vec<&'static str> {
    Kind::ALL.into_iter().map(Kind::name).collect()
}

/// `--kind` when given, otherwise an interactive choice (which fails with a
/// pointer at `--kind` when the process cannot prompt).
fn resolve_kind(flag: Option<&str>) -> anyhow::Result<Kind> {
    match flag {
        Some(value) => Kind::parse(value),
        None => ui::select("Which pipeline kind?", Kind::ALL.to_vec()),
    }
}

fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    super::reject_workflows(ctx, "pipeline")?;
    let kind = resolve_kind(ctx.kind)?;
    let mut outcomes = Vec::new();
    for dir in kind.template_dirs() {
        let source = TEMPLATES
            .get_dir(dir)
            // The pipeline templates are embedded at compile time; a missing
            // directory is a packaging bug, not a user error.
            .ok_or_else(|| anyhow::anyhow!("embedded template directory `{dir}` is missing"))
            .hint("this is a bug in hpds; please report it")?;
        outcomes.extend(apply_dir(source, ctx.dest, &ctx.vars, ctx.force)?);
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_parses_from_its_name() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_kind_errors_and_lists_the_valid_kinds() {
        let err = Kind::parse("cmake").unwrap_err();
        let rendered = ui::render_error(&err, false);
        assert!(rendered.contains("cmake"), "names the bad kind: {rendered}");
        for valid in ["make", "targets", "both"] {
            assert!(rendered.contains(valid), "lists `{valid}`: {rendered}");
        }
    }

    #[test]
    fn each_kind_maps_to_embedded_template_dirs() {
        for kind in Kind::ALL {
            for dir in kind.template_dirs() {
                assert!(
                    TEMPLATES.get_dir(dir).is_some(),
                    "{kind} needs embedded dir `{dir}`"
                );
            }
        }
    }

    #[test]
    fn both_renders_the_targets_setup_before_its_makefile() {
        assert_eq!(
            Kind::Both.template_dirs(),
            &["pipeline/targets", "pipeline/both"]
        );
    }
}
