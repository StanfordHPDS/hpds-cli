//! `hpds use gha`: GitHub Actions scaffolding under `.github/`.
//!
//! Offers a menu of workflows (a pull request template, a lint workflow,
//! and the audit bot) chosen interactively via multi-select or
//! non-interactively via `--workflows pr-template,lint,audit-bot`.

use std::fmt;

use anyhow::Context as _;

use crate::templates::{FileOutcome, TEMPLATES, apply_dir};
use crate::ui;
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "gha",
    description: "GitHub Actions scaffolding: PR template, lint workflow, audit bot",
    run,
};

/// One selectable entry in the gha menu.
#[derive(Debug)]
struct Workflow {
    name: &'static str,
    description: &'static str,
    /// Template directory under the embedded `templates/` tree.
    dir: &'static str,
}

const WORKFLOWS: &[Workflow] = &[
    Workflow {
        name: "pr-template",
        description: ".github/pull_request_template.md",
        dir: "gha/pr-template",
    },
    Workflow {
        name: "lint",
        description: "workflow running `togi lint` + `togi format --check`",
        dir: "gha/lint",
    },
    Workflow {
        name: "audit-bot",
        description: "workflow running `hpds audit` weekly and on PRs, reporting to GitHub",
        dir: "gha/audit-bot",
    },
    // New workflows are appended here and picked up by the menu,
    // `--workflows`, and the tests automatically.
];

/// Every workflow name, in menu order. `hpds init --yes` uses this as the
/// default selection when `gha` is requested without an explicit list.
pub(crate) fn workflow_names() -> Vec<String> {
    WORKFLOWS.iter().map(|w| w.name.to_string()).collect()
}

/// Comma-separated workflow names, for errors and hints.
fn available() -> String {
    WORKFLOWS
        .iter()
        .map(|w| w.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    super::reject_kind(ctx, "gha")?;
    let selected = match ctx.workflows {
        Some(names) => resolve(names)?,
        None => prompt()?,
    };

    let mut outcomes = Vec::new();
    for workflow in selected {
        let source = TEMPLATES
            .get_dir(workflow.dir)
            // The gha templates are embedded at compile time; a missing
            // directory is a packaging bug, not a user error.
            .ok_or_else(|| {
                anyhow::anyhow!("embedded template directory `{}` is missing", workflow.dir)
            })
            .hint("this is a bug in hpds; please report it")?;
        outcomes.extend(apply_dir(source, ctx.dest, &ctx.vars, ctx.force)?);
    }
    Ok(outcomes)
}

/// Map `--workflows` names to workflows, rejecting unknown names. Bad
/// values are usage errors (exit 2), like an unknown component name.
fn resolve(names: &[String]) -> anyhow::Result<Vec<&'static Workflow>> {
    if names.is_empty() {
        return Err(crate::cli::usage_error(
            "no workflows given",
            format!("pass --workflows with any of: {}", available()),
        ));
    }
    names
        .iter()
        .map(|name| {
            WORKFLOWS.iter().find(|w| w.name == name).ok_or_else(|| {
                crate::cli::usage_error(
                    format!("unknown gha workflow `{name}`"),
                    format!("available workflows: {}", available()),
                )
            })
        })
        .collect()
}

/// A menu row: workflow name plus what it adds.
struct Choice(&'static Workflow);

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.0.name, self.0.description)
    }
}

/// Interactive multi-select over the workflow menu (which fails with a
/// pointer at `--workflows` when the process cannot prompt).
fn prompt() -> anyhow::Result<Vec<&'static Workflow>> {
    let options = WORKFLOWS.iter().map(Choice).collect();
    let chosen = ui::multiselect_all("Which GitHub Actions pieces should be added?", options)
        .with_context(|| {
            format!(
                "could not choose gha workflows; non-interactively, pass \
                 --workflows with any of: {}",
                available()
            )
        })?;
    Ok(chosen.into_iter().map(|c| c.0).collect())
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;
    use crate::templates::{Vars, WriteOutcome};
    use std::fs;

    #[test]
    fn workflow_template_dirs_are_embedded() {
        for workflow in WORKFLOWS {
            assert!(
                TEMPLATES.get_dir(workflow.dir).is_some(),
                "templates/{} is embedded in the binary",
                workflow.dir
            );
        }
    }

    #[test]
    fn lint_workflow_template_is_valid_yaml() {
        let file = TEMPLATES
            .get_file("gha/lint/.github/workflows/togi-lint.yml")
            .expect("lint workflow template is embedded");
        let text = file.contents_utf8().expect("workflow template is UTF-8");
        // The template must render cleanly with the standard variables and
        // the result must be parseable YAML.
        let rendered =
            crate::templates::render(text, "togi-lint.yml", &Vars::standard("p", Some("r"), "a"))
                .expect("template renders with the standard variables");
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("rendered workflow parses as YAML");
        assert!(doc.get("jobs").is_some(), "workflow has jobs: {rendered}");
        assert!(rendered.contains("togi lint"));
        assert!(rendered.contains("togi format --check"));
    }

    #[test]
    fn audit_workflow_template_is_valid_yaml_with_bot_triggers_and_permissions() {
        let file = TEMPLATES
            .get_file("gha/audit-bot/.github/workflows/hpds-audit.yml")
            .expect("audit workflow template is embedded");
        let text = file.contents_utf8().expect("workflow template is UTF-8");
        let rendered =
            crate::templates::render(text, "hpds-audit.yml", &Vars::standard("p", Some("r"), "a"))
                .expect("template renders with the standard variables");
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&rendered).expect("rendered workflow parses as YAML");

        // Triggers: a weekly cron plus pull_request.
        let on = doc.get("on").expect("workflow has triggers");
        let cron = on
            .get("schedule")
            .and_then(|s| s.get(0))
            .and_then(|entry| entry.get("cron"))
            .and_then(|c| c.as_str())
            .expect("schedule trigger carries a cron expression");
        assert_eq!(
            cron.split_whitespace().count(),
            5,
            "cron has five fields: {cron}"
        );
        assert!(
            on.get("pull_request").is_some(),
            "pull_request trigger present: {rendered}"
        );

        // The bot needs to comment on PRs and manage issues; nothing more.
        let perms = doc.get("permissions").expect("permissions block present");
        let perm = |key: &str| perms.get(key).and_then(|v| v.as_str());
        assert_eq!(perm("contents"), Some("read"));
        assert_eq!(perm("issues"), Some("write"));
        assert_eq!(perm("pull-requests"), Some("write"));

        // Steps: audit to JSON (continuing on findings), then the reporter
        // with the Actions token.
        assert!(doc.get("jobs").is_some(), "workflow has jobs: {rendered}");
        assert!(
            rendered.contains("hpds audit --format json > audit.json"),
            "writes the audit JSON: {rendered}"
        );
        assert!(
            rendered.contains("hpds audit report-github --input audit.json"),
            "feeds the JSON to the reporter: {rendered}"
        );
        assert!(
            rendered.contains("GITHUB_TOKEN"),
            "reporter step gets the Actions token: {rendered}"
        );
    }

    #[test]
    fn resolve_accepts_every_advertised_workflow() {
        let names: Vec<String> = WORKFLOWS.iter().map(|w| w.name.to_string()).collect();
        let resolved = resolve(&names).unwrap();
        assert_eq!(resolved.len(), WORKFLOWS.len());
    }

    /// Message + hint of a usage error, the way `main` renders them (the
    /// hint rides on the type, not the anyhow chain).
    fn usage_parts(err: &anyhow::Error) -> String {
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("a bad --workflows value is a usage error (exit 2)");
        format!("{usage}\nhint: {}", usage.hint())
    }

    #[test]
    fn resolve_rejects_an_unknown_workflow_as_usage_error_naming_the_real_ones() {
        let err = resolve(&["definitely-not-a-workflow".to_string()]).unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("definitely-not-a-workflow"), "out: {out}");
        assert!(out.contains("pr-template"), "out: {out}");
        assert!(out.contains("lint"), "out: {out}");
        assert!(out.contains("audit-bot"), "out: {out}");
    }

    #[test]
    fn resolve_rejects_an_empty_selection_with_a_flag_hint() {
        let err = resolve(&[]).unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("--workflows"), "out: {out}");
        assert!(out.contains("pr-template, lint, audit-bot"), "out: {out}");
    }

    #[test]
    fn gha_rejects_the_kind_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let workflows = vec!["lint".to_string()];
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("make");
        ctx.workflows = Some(&workflows);
        let err = run(&ctx).unwrap_err();
        assert!(err.to_string().contains("--kind"), "{err}");
    }

    #[test]
    fn selected_workflows_render_under_dot_github() {
        let tmp = tempfile::tempdir().unwrap();
        let workflows = vec!["pr-template".to_string(), "lint".to_string()];
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.workflows = Some(&workflows);
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert!(
            outcomes.iter().all(|o| o.outcome == WriteOutcome::Created),
            "{outcomes:?}"
        );
        let pr = tmp.path().join(".github/pull_request_template.md");
        let wf = tmp.path().join(".github/workflows/togi-lint.yml");
        let pr_text = fs::read_to_string(pr).unwrap();
        let wf_text = fs::read_to_string(wf).unwrap();
        assert!(
            !pr_text.contains("{{"),
            "no unrendered variables: {pr_text}"
        );
        assert!(
            wf_text.contains("togi lint"),
            "lint step present: {wf_text}"
        );
    }
}
