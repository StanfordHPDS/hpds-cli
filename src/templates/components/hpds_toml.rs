//! `hpds use hpds.toml`: project lifecycle metadata used by the audit.
//!
//! The same embedded template also feeds `hpds init`, keeping both creation
//! paths on one documented `[project]` shape.

use crate::gitx;
use crate::templates::{FileOutcome, TEMPLATES, Vars, apply_dir, render};
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

const TEMPLATE_DIR: &str = "hpds-toml";
const TEMPLATE_FILE: &str = "hpds-toml/hpds.toml";

pub static COMPONENT: Component = Component {
    name: "hpds.toml",
    description: "hpds.toml with project lifecycle metadata",
    run,
};

fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    super::reject_kind(ctx, "hpds.toml")?;
    super::reject_workflows(ctx, "hpds.toml")?;

    // A configured author is an explicit default. Otherwise use the same
    // best-effort authenticated GitHub login as `hpds init`.
    let author = ctx
        .vars
        .get("author")
        .filter(|author| !author.trim().is_empty())
        .map(str::to_string)
        .or_else(gitx::gh_login)
        .unwrap_or_default();
    let vars = ctx.vars.clone().with("author", author.clone());
    let vars = template_vars(&vars, "");

    let source = TEMPLATES
        .get_dir(TEMPLATE_DIR)
        .ok_or_else(|| anyhow::anyhow!("embedded template directory `{TEMPLATE_DIR}` is missing"))
        .hint("this is a bug in hpds; please report it")?;
    let outcomes = apply_dir(source, ctx.dest, &vars, ctx.force)?;
    if author.is_empty() {
        ctx.guidance.borrow_mut().push(
            "next: set `project.primary-author` in hpds.toml to your GitHub username".to_string(),
        );
    }
    Ok(outcomes)
}

/// Render the metadata template for `hpds init`.
pub(crate) fn render_config(vars: &Vars, description: &str) -> anyhow::Result<String> {
    let file = TEMPLATES
        .get_file(TEMPLATE_FILE)
        .ok_or_else(|| anyhow::anyhow!("embedded template file `{TEMPLATE_FILE}` is missing"))
        .hint("this is a bug in hpds; please report it")?;
    let text = file
        .contents_utf8()
        .ok_or_else(|| anyhow::anyhow!("embedded template file `{TEMPLATE_FILE}` is not UTF-8"))
        .hint("this is a bug in hpds; please report it")?;
    Ok(render(
        text,
        TEMPLATE_FILE,
        &template_vars(vars, description),
    )?)
}

fn template_vars(vars: &Vars, description: &str) -> Vars {
    let project = one_line(vars.get("project").unwrap_or("project"));
    let author = one_line(vars.get("author").unwrap_or(""));
    let description = one_line(description);
    vars.clone()
        .with("project", project)
        .with(
            "description_note",
            if description.is_empty() {
                String::new()
            } else {
                format!("# {description}")
            },
        )
        .with(
            "author_note",
            if author.is_empty() {
                "# fill in your GitHub username (no gh login was detected)\n".to_string()
            } else {
                String::new()
            },
        )
        .with("author_toml", toml_string(&author))
}

/// Collapse line breaks so substituted values cannot escape comment lines.
fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

/// Quote a value as a TOML basic string.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
