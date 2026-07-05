//! `hpds init` — project setup wizard. Also reachable as
//! `hpds project init`.
//!
//! Interactive: prompts for name, description, language, a multi-select
//! of template components, and the primary author, then offers `git init`,
//! project `.gitignore` vaccination, and GitHub repo creation. Every
//! prompt has a flag-driven equivalent, so `--yes` (plus flags) runs the
//! whole thing without asking anything: defaults are the directory name,
//! an empty description, the detected language, the GitHub login gh is
//! authenticated as (else an empty author), and no components. Under
//! `--yes` the git-forward steps run only when their flags (`--git-init`,
//! `--vaccinate`, `--repo-create`) ask for them.
//!
//! Components come from the `hpds use` registry; `--use` accepts an
//! optional `:variant` per component (e.g. `pipeline:targets`). Without a
//! variant, `--yes` applies non-interactive defaults: pipeline renders the
//! `make` kind, container the `docker` kind, and gha every workflow.

use std::path::Path;

use anyhow::Context;
use clap::Args;

use crate::gitx;
use crate::templates::components::{self, ComponentCtx, container, gha, pipeline};
use crate::templates::{FileOutcome, Vars, write_rendered};
use crate::ui;

use super::r#use;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Accept the default for every unanswered question; never prompt
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Project name [default: the current directory's name]
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// One-line project description, recorded in hpds.toml
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Project language(s) [default under --yes: detected from project
    /// files, e.g. renv.lock or pyproject.toml]
    #[arg(long, value_parser = ["r", "python", "both"])]
    pub language: Option<String>,

    /// Components to apply (comma-separated): pipeline, readme, container,
    /// slurm, gha. Attach a variant with `:` — pipeline:make|targets|both,
    /// container:docker|apptainer|both, gha:pr-template+lint+audit-bot.
    /// Without a variant, --yes defaults pipeline to make, container to
    /// docker, and gha to every workflow, and reports each defaulted kind
    /// so the choice is never silent
    #[arg(long = "use", value_delimiter = ',', value_name = "COMPONENTS")]
    pub components: Option<Vec<String>>,

    /// Primary author (GitHub username) for hpds.toml [default: the login
    /// gh is authenticated as, else empty]
    #[arg(long, value_name = "AUTHOR")]
    pub author: Option<String>,

    /// Overwrite existing files that differ from what init would write
    #[arg(long)]
    pub force: bool,

    /// Initialize a git repository without asking (--yes otherwise skips it)
    #[arg(long)]
    pub git_init: bool,

    /// Add the lab ignore patterns to the repo's .gitignore without asking,
    /// like `hpds git vaccinate --project` (--yes otherwise skips it)
    #[arg(long)]
    pub vaccinate: bool,

    /// Create and push a GitHub repository at the end, like
    /// `hpds repo create` (never runs under --yes without this flag)
    #[arg(long)]
    pub repo_create: bool,
}

pub fn run(args: InitArgs) -> anyhow::Result<()> {
    // Belt and braces: everything below answers prompts from flags under
    // --yes, so any prompt that would still fire is a bug — make it fail
    // fast instead of hanging a scripted run.
    if args.yes {
        ui::set_non_interactive(true);
    }
    let cwd = std::env::current_dir().context("could not determine the current directory")?;

    let name = gitx::repo::resolve_with(
        args.name.clone(),
        args.yes,
        || default_project_name(&cwd),
        |d| ui::text("Project name", d),
    )?;
    let description = gitx::repo::resolve_with(
        args.description.clone(),
        args.yes,
        || Ok(String::new()),
        |d| ui::text("Project description (one line)", d),
    )?;
    let language = resolve_language(args.language.clone(), args.yes, &cwd)?;
    // Selections are validated before the author is resolved: bad flags
    // should fail fast, and the author default may ask gh (a subprocess,
    // possibly the network) for the login.
    let selections = resolve_selections(args.components.as_deref(), args.yes)?;
    ensure_language_for(&selections, language.as_deref())?;
    let author = gitx::repo::resolve_with(args.author.clone(), args.yes, default_author, |d| {
        ui::text("Primary author (GitHub username)", d)
    })?;

    let vars = Vars::standard(&name, language.as_deref(), &author);

    // hpds.toml first: the project metadata is the one thing init always
    // writes. Existing files go through the engine's conflict handling —
    // never overwritten without --force.
    let outcome = write_rendered(
        &cwd.join("hpds.toml"),
        hpds_toml(&name, &description, &author).as_bytes(),
        args.force,
    )?;
    let mut conflicts = r#use::report_outcomes(&[FileOutcome {
        path: "hpds.toml".into(),
        outcome,
    }]);

    for selection in &selections {
        let component = components::find(selection.spec.name)
            .expect("selectable init components are registered");
        announce_defaulted_kind(selection);
        let ctx = ComponentCtx {
            kind: selection.kind.as_deref(),
            workflows: selection.workflows.as_deref(),
            force: args.force,
            dest: &cwd,
            vars: vars.clone(),
            guidance: std::cell::RefCell::new(Vec::new()),
        };
        conflicts += r#use::report_outcomes(&(component.run)(&ctx)?);
        for line in ctx.guidance.borrow().iter() {
            ui::println(line);
        }
    }
    if conflicts > 0 {
        let plural = if conflicts == 1 { "file" } else { "files" };
        ui::println(&format!(
            "re-run the same `hpds init` command with --force to overwrite \
             the {conflicts} skipped {plural}"
        ));
    }

    git_forward(&args, &cwd, &name)?;
    if togi_next_step_applies(&selections, language.as_deref()) {
        ui::println(
            "next: format and lint with the lab's togi tool — `togi format` \
             (install it with `hpds install togi`)",
        );
    }
    ui::success(&format!("{name} is set up"));
    Ok(())
}

/// Whether the closing next steps should point at togi: only when
/// formatting is relevant — the project has a language whose code togi
/// formats, or CI was just set up to run togi (the gha lint workflow).
/// An interactive gha selection has no workflow list yet (the component
/// prompts for it), so it counts as possibly-lint.
fn togi_next_step_applies(selections: &[Selection], language: Option<&str>) -> bool {
    language.is_some()
        || selections.iter().any(|s| {
            s.spec.takes_workflows
                && s.workflows
                    .as_deref()
                    .is_none_or(|w| w.iter().any(|name| name == "lint"))
        })
}

/// Default project name: the current directory's basename.
fn default_project_name(cwd: &Path) -> anyhow::Result<String> {
    match cwd.file_name().and_then(|n| n.to_str()) {
        Some(name) if !name.is_empty() => Ok(name.to_string()),
        _ => Err(super::usage_error(
            "could not derive a project name from the current directory",
            "pass one explicitly with --name <NAME>",
        )),
    }
}

/// Default primary author: the login `gh` is authenticated as. The audit
/// watchers check needs a GitHub LOGIN — git's `user.name` is a display
/// name, so it is never used. Without a usable gh, the author stays empty
/// (and the generated hpds.toml says to fill it in).
fn default_author() -> anyhow::Result<String> {
    Ok(gitx::gh_login().unwrap_or_default())
}

/// The project language: the flag wins; `--yes` falls back to detection
/// (possibly none); otherwise ask.
fn resolve_language(flag: Option<String>, yes: bool, cwd: &Path) -> anyhow::Result<Option<String>> {
    match flag {
        Some(language) => Ok(Some(language)),
        None if yes => Ok(r#use::detect_language(cwd).map(str::to_string)),
        None => Ok(Some(
            ui::select("Project language", vec!["r", "python", "both"])?.to_string(),
        )),
    }
}

/// One init-selectable component (the embedded ones only — fetched
/// templates like slides land in a subdirectory and are `hpds use`'s job).
#[derive(Debug)]
struct Selectable {
    name: &'static str,
    /// The component refuses to render without a language.
    needs_language: bool,
    /// The `--kind` value `--yes` assumes when no `:variant` is given;
    /// `None` for components without kinds.
    default_kind: Option<&'static str>,
    /// The component's valid kinds (from the component itself), used to
    /// validate a `:variant` before anything is written; `None` for
    /// components without kinds.
    kinds: Option<fn() -> Vec<&'static str>>,
    /// The component takes workflows (gha) instead of a kind.
    takes_workflows: bool,
}

const SELECTABLE: &[Selectable] = &[
    Selectable {
        name: "pipeline",
        needs_language: false,
        default_kind: Some("make"),
        kinds: Some(pipeline::kind_names),
        takes_workflows: false,
    },
    Selectable {
        name: "readme",
        needs_language: true,
        default_kind: None,
        kinds: None,
        takes_workflows: false,
    },
    Selectable {
        name: "container",
        needs_language: true,
        default_kind: Some("docker"),
        kinds: Some(container::kind_names),
        takes_workflows: false,
    },
    Selectable {
        name: "slurm",
        needs_language: true,
        default_kind: None,
        kinds: None,
        takes_workflows: false,
    },
    Selectable {
        name: "gha",
        needs_language: false,
        default_kind: None,
        kinds: None,
        takes_workflows: true,
    },
];

/// Comma-separated selectable names for error hints.
fn selectable_names() -> String {
    SELECTABLE
        .iter()
        .map(|s| s.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One resolved `--use` entry (or multi-select pick).
#[derive(Debug)]
struct Selection {
    spec: &'static Selectable,
    /// `--kind` to hand the component; `None` lets it prompt.
    kind: Option<String>,
    /// Whether `kind` was assumed from the component's default (under
    /// `--yes`, without an explicit `:variant`) rather than chosen by the
    /// user. When true the run loop announces the choice so the default is
    /// never silent.
    kind_defaulted: bool,
    /// gha's workflow list; `None` lets it prompt.
    workflows: Option<Vec<String>>,
}

/// Turn `--use` (or, interactively, a multi-select) into selections.
/// Under `--yes`, missing variants take their non-interactive defaults.
fn resolve_selections(flag: Option<&[String]>, yes: bool) -> anyhow::Result<Vec<Selection>> {
    let mut selections = match flag {
        Some(items) => items
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(parse_selection)
            .collect::<anyhow::Result<Vec<_>>>()?,
        None if yes => Vec::new(),
        None => prompt_components()?,
    };
    let mut seen = Vec::new();
    for selection in &selections {
        if seen.contains(&selection.spec.name) {
            return Err(super::usage_error(
                format!(
                    "component `{}` is listed more than once",
                    selection.spec.name
                ),
                "list each component once in --use",
            ));
        }
        seen.push(selection.spec.name);
    }
    if yes {
        for selection in &mut selections {
            if selection.kind.is_none()
                && let Some(default) = selection.spec.default_kind
            {
                selection.kind = Some(default.to_string());
                selection.kind_defaulted = true;
            }
            if selection.spec.takes_workflows && selection.workflows.is_none() {
                selection.workflows = Some(gha::workflow_names());
            }
        }
    }
    Ok(selections)
}

/// Announce a kind that `--yes` assumed from the component's default, so
/// the choice is never silent. Only fires for a defaulted kind on a
/// variant-bearing component; an explicit `:variant` (the user's own
/// choice) and kindless components say nothing.
fn announce_defaulted_kind(selection: &Selection) {
    if !selection.kind_defaulted {
        return;
    }
    let (Some(kind), Some(kinds)) = (selection.kind.as_deref(), selection.spec.kinds) else {
        return;
    };
    let name = selection.spec.name;
    let choices = kinds().join("|");
    ui::println(&format!(
        "{name}: using kind \"{kind}\" (pass --use {name}:{choices} to choose)"
    ));
}

/// Parse one `--use` entry: `name` or `name:variant`, where the variant is
/// a kind (pipeline, container) or a `+`-separated workflow list (gha).
/// Variants are validated here — before init writes anything — and bad
/// ones get hints in init's own `--use name:variant` syntax, not the
/// `--kind`/`--workflows` flags that only `hpds use` has.
fn parse_selection(item: &str) -> anyhow::Result<Selection> {
    let (name, variant) = match item.split_once(':') {
        Some((name, variant)) => (name.trim(), Some(variant.trim())),
        None => (item, None),
    };
    let Some(spec) = SELECTABLE.iter().find(|s| s.name == name) else {
        // A real component that init does not scaffold (the fetched
        // templates) gets a pointer at `hpds use`; anything else is a typo.
        if components::find(name).is_some() {
            return Err(super::usage_error(
                format!("`{name}` is fetched from GitHub and not part of init"),
                format!(
                    "run `hpds use {name}` inside the project once init is done \
                     (init scaffolds: {})",
                    selectable_names()
                ),
            ));
        }
        return Err(super::usage_error(
            format!("`{name}` is not an init component"),
            format!("pass --use with any of: {}", selectable_names()),
        ));
    };
    let mut selection = Selection {
        spec,
        kind: None,
        kind_defaulted: false,
        workflows: None,
    };
    if let Some(variant) = variant {
        if spec.takes_workflows {
            let known = gha::workflow_names();
            let workflows: Vec<String> = variant
                .split('+')
                .map(str::trim)
                .map(str::to_string)
                .collect();
            if let Some(bad) = workflows.iter().find(|w| !known.contains(w)) {
                return Err(super::usage_error(
                    format!("`{bad}` is not a {name} workflow (in `--use {item}`)"),
                    format!(
                        "join workflows with `+`, e.g. `--use {name}:{}`",
                        known.join("+")
                    ),
                ));
            }
            selection.workflows = Some(workflows);
        } else if let Some(kinds) = spec.kinds {
            let kinds = kinds();
            if !kinds.contains(&variant) {
                return Err(super::usage_error(
                    format!("`{variant}` is not a `{name}` variant (in `--use {item}`)"),
                    format!(
                        "pass one of: {}",
                        kinds
                            .iter()
                            .map(|kind| format!("`--use {name}:{kind}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            selection.kind = Some(variant.to_string());
        } else {
            return Err(super::usage_error(
                format!("the `{name}` component has no `:variant` (got `{item}`)"),
                format!("pass `--use {name}` without a variant"),
            ));
        }
    }
    Ok(selection)
}

/// The interactive component multi-select. Each pick's own prompts (e.g.
/// the pipeline kind) run later, when the component does.
fn prompt_components() -> anyhow::Result<Vec<Selection>> {
    struct MenuItem(&'static Selectable, &'static str);
    impl std::fmt::Display for MenuItem {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} — {}", self.0.name, self.1)
        }
    }
    let options = SELECTABLE
        .iter()
        .map(|spec| {
            let description = components::find(spec.name)
                .expect("selectable init components are registered")
                .description;
            MenuItem(spec, description)
        })
        .collect();
    let picked = ui::multiselect("Components to set up (space to toggle)", options)?;
    Ok(picked
        .into_iter()
        .map(|item| Selection {
            spec: item.0,
            kind: None,
            kind_defaulted: false,
            workflows: None,
        })
        .collect())
}

/// Fail before writing anything when a selected component needs a language
/// and none was given or detected.
fn ensure_language_for(selections: &[Selection], language: Option<&str>) -> anyhow::Result<()> {
    if language.is_some() {
        return Ok(());
    }
    match selections.iter().find(|s| s.spec.needs_language) {
        Some(selection) => Err(super::usage_error(
            format!(
                "the `{}` component needs a project language, and none was \
                 given or detected",
                selection.spec.name
            ),
            "pass --language r, --language python, or --language both",
        )),
        None => Ok(()),
    }
}

/// The `hpds.toml` init writes: the documented `[project]` shape, with the
/// name and description as header comments (they are not config keys).
fn hpds_toml(name: &str, description: &str, author: &str) -> String {
    let mut out = format!("# hpds.toml — hpds configuration for {}\n", one_line(name));
    let description = one_line(description);
    if !description.is_empty() {
        out.push_str(&format!("# {description}\n"));
    }
    out.push_str("\n[project]\n");
    out.push_str("# active | submitted | published | retired\n");
    out.push_str("status = \"active\"\n");
    out.push_str("# GitHub username; `hpds audit` checks they watch the repo\n");
    let author = one_line(author);
    if author.is_empty() {
        out.push_str("# fill in your GitHub username (no gh login was detected)\n");
    }
    out.push_str(&format!("primary-author = {}\n", toml_string(&author)));
    out
}

/// Collapse any line breaks so user input cannot escape a comment line.
fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

/// Quote a value as a TOML basic string.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The git-forward steps at the end of init: `git init` (when not already
/// a repo), project vaccination, and GitHub repo creation. Interactive
/// runs offer each one; `--yes` runs only what the flags request.
fn git_forward(args: &InitArgs, cwd: &Path, name: &str) -> anyhow::Result<()> {
    let already_repo = cwd.join(".git").exists();
    let mut have_repo = already_repo;
    if already_repo {
        if args.git_init {
            ui::println("already a git repository; skipping git init");
        }
    } else {
        let do_init =
            args.git_init || (!args.yes && ui::confirm("Initialize a git repository here?", true)?);
        if do_init {
            gitx::git_init(cwd)?;
            ui::success("initialized a git repository");
            have_repo = true;
        }
    }

    // Only offer vaccination when there is a repo to vaccinate; an explicit
    // --vaccinate still runs (and errors actionably) without one.
    let do_vaccinate = args.vaccinate
        || (!args.yes
            && have_repo
            && ui::confirm(
                "Add the lab ignore patterns to this repo's .gitignore \
                 (`hpds git vaccinate --project`)?",
                true,
            )?);
    if do_vaccinate {
        // `hpds git vaccinate`'s NotARepo message suggests dropping
        // `--project`, a flag init does not have; point at init's own
        // fix instead.
        let report = gitx::vaccinate_project().map_err(|err| match err {
            gitx::GitxError::NotARepo => super::usage_error(
                "cannot vaccinate: this directory is not a git repository",
                "add --git-init so init creates the repository first, \
                 or run `git init` yourself and re-run",
            ),
            other => other.into(),
        })?;
        super::git::report_vaccination(&report);
    }

    let do_repo_create = args.repo_create
        || (!args.yes
            && ui::confirm(
                "Create and push a GitHub repository now (`hpds repo create`)?",
                false,
            )?);
    if do_repo_create {
        gitx::repo::create(gitx::repo::CreateOptions {
            name: Some(name.to_string()),
            org: None,
            visibility: None,
            yes: args.yes,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Message + hint of a [`super::super::UsageError`], the way `main`
    /// renders them (the hint rides on the type, not the anyhow chain).
    fn usage_parts(err: &anyhow::Error) -> String {
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("init flag mistakes are usage errors");
        format!("{usage}\nhint: {}", usage.hint())
    }

    #[test]
    fn every_selectable_component_is_in_the_registry() {
        for spec in SELECTABLE {
            assert!(
                components::find(spec.name).is_some(),
                "{} must be a registered component",
                spec.name
            );
        }
    }

    #[test]
    fn parse_selection_accepts_a_bare_name() {
        let selection = parse_selection("pipeline").unwrap();
        assert_eq!(selection.spec.name, "pipeline");
        assert!(selection.kind.is_none());
        assert!(selection.workflows.is_none());
    }

    #[test]
    fn parse_selection_splits_a_kind_variant() {
        let selection = parse_selection("pipeline:targets").unwrap();
        assert_eq!(selection.spec.name, "pipeline");
        assert_eq!(selection.kind.as_deref(), Some("targets"));
    }

    #[test]
    fn parse_selection_splits_gha_workflows_on_plus() {
        let selection = parse_selection("gha:pr-template+lint").unwrap();
        assert_eq!(selection.spec.name, "gha");
        assert_eq!(
            selection.workflows.as_deref(),
            Some(&["pr-template".to_string(), "lint".to_string()][..])
        );
    }

    #[test]
    fn parse_selection_rejects_an_unknown_kind_with_init_syntax() {
        let err = parse_selection("pipeline:bogus").unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("bogus"), "{out}");
        assert!(out.contains("--use pipeline:make"), "{out}");
        assert!(out.contains("--use pipeline:targets"), "{out}");
        assert!(!out.contains("--kind"), "no `hpds use`-only flags: {out}");
    }

    #[test]
    fn parse_selection_rejects_an_unknown_gha_workflow_with_init_syntax() {
        let err = parse_selection("gha:pr-template+bogus").unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("bogus"), "{out}");
        assert!(out.contains("--use gha:"), "{out}");
        assert!(out.contains("pr-template"), "{out}");
        assert!(
            !out.contains("--workflows"),
            "no `hpds use`-only flags: {out}"
        );
    }

    #[test]
    fn parse_selection_accepts_every_kind_its_component_accepts() {
        for spec in SELECTABLE {
            let Some(kinds) = spec.kinds else { continue };
            for kind in kinds() {
                let selection = parse_selection(&format!("{}:{kind}", spec.name)).unwrap();
                assert_eq!(selection.kind.as_deref(), Some(kind));
            }
        }
    }

    #[test]
    fn every_default_kind_is_one_the_component_accepts() {
        for spec in SELECTABLE {
            let Some(default) = spec.default_kind else {
                continue;
            };
            let kinds = spec.kinds.expect("a default kind implies kinds")();
            assert!(
                kinds.contains(&default),
                "{}'s default `{default}` must be in {kinds:?}",
                spec.name
            );
        }
    }

    #[test]
    fn parse_selection_rejects_a_variant_on_readme() {
        let err = parse_selection("readme:qmd").unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("readme"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn parse_selection_points_fetched_components_at_hpds_use() {
        let err = parse_selection("thesis").unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("hpds use thesis"), "{out}");
    }

    #[test]
    fn parse_selection_lists_valid_names_for_a_typo() {
        let err = parse_selection("frobnicate").unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("frobnicate"), "{out}");
        assert!(out.contains("pipeline"), "{out}");
        assert!(out.contains("gha"), "{out}");
    }

    #[test]
    fn yes_fills_the_documented_variant_defaults() {
        let flag = [
            "pipeline".to_string(),
            "container".to_string(),
            "gha".to_string(),
        ];
        let selections = resolve_selections(Some(&flag), true).unwrap();
        assert_eq!(selections[0].kind.as_deref(), Some("make"));
        assert_eq!(selections[1].kind.as_deref(), Some("docker"));
        assert_eq!(
            selections[2].workflows.as_deref(),
            Some(&gha::workflow_names()[..])
        );
    }

    #[test]
    fn without_yes_variants_stay_unset_so_components_can_prompt() {
        let flag = ["pipeline".to_string()];
        let selections = resolve_selections(Some(&flag), false).unwrap();
        assert!(selections[0].kind.is_none());
    }

    #[test]
    fn yes_marks_an_assumed_default_kind_as_defaulted() {
        // A bare component under --yes takes its default kind, and that
        // choice is flagged so the run loop can announce it.
        let flag = ["pipeline".to_string(), "container".to_string()];
        let selections = resolve_selections(Some(&flag), true).unwrap();
        assert!(
            selections[0].kind_defaulted,
            "pipeline's make default is marked defaulted"
        );
        assert!(
            selections[1].kind_defaulted,
            "container's docker default is marked defaulted"
        );
    }

    #[test]
    fn yes_does_not_mark_an_explicit_variant_as_defaulted() {
        let flag = ["pipeline:targets".to_string()];
        let selections = resolve_selections(Some(&flag), true).unwrap();
        assert_eq!(selections[0].kind.as_deref(), Some("targets"));
        assert!(
            !selections[0].kind_defaulted,
            "an explicit variant is the user's own choice, not a default"
        );
    }

    #[test]
    fn yes_does_not_mark_kindless_components_as_defaulted() {
        // gha and readme have no kind to default, so nothing is announced.
        let flag = ["gha".to_string(), "readme".to_string()];
        let selections = resolve_selections(Some(&flag), true).unwrap();
        assert!(selections.iter().all(|s| !s.kind_defaulted));
    }

    #[test]
    fn duplicate_components_are_rejected() {
        let flag = ["readme".to_string(), "readme".to_string()];
        let err = resolve_selections(Some(&flag), true).unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("more than once"), "{out}");
    }

    #[test]
    fn empty_use_entries_are_ignored() {
        let flag = ["".to_string(), " pipeline ".to_string()];
        let selections = resolve_selections(Some(&flag), true).unwrap();
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].spec.name, "pipeline");
    }

    #[test]
    fn hpds_toml_has_the_documented_project_shape() {
        let toml = hpds_toml("malaria-icu", "ICU malaria outcomes", "malcolm");
        assert!(toml.contains("[project]"), "{toml}");
        assert!(toml.contains("status = \"active\""), "{toml}");
        assert!(toml.contains("primary-author = \"malcolm\""), "{toml}");
        assert!(toml.contains("# hpds.toml — hpds configuration for malaria-icu"));
        assert!(toml.contains("# ICU malaria outcomes"));
    }

    #[test]
    fn hpds_toml_parses_back_through_the_config_loader() {
        let toml = hpds_toml("p", "d", "malcolm");
        let parsed: toml::Value = toml::from_str(&toml).expect("valid TOML");
        assert_eq!(
            parsed["project"]["primary-author"].as_str(),
            Some("malcolm")
        );
        assert_eq!(parsed["project"]["status"].as_str(), Some("active"));
    }

    #[test]
    fn hpds_toml_with_an_empty_author_says_to_fill_in_the_username() {
        let toml = hpds_toml("p", "", "");
        assert!(
            toml.contains("fill in your GitHub username"),
            "says what to do: {toml}"
        );
        assert!(toml.contains("primary-author = \"\""), "{toml}");
    }

    #[test]
    fn hpds_toml_with_an_author_has_no_fill_in_comment() {
        let toml = hpds_toml("p", "", "octocat");
        assert!(!toml.contains("fill in"), "{toml}");
    }

    #[test]
    fn hpds_toml_escapes_quotes_and_newlines_in_the_author() {
        let toml = hpds_toml("p", "", "a\"b\\c\nd");
        let parsed: toml::Value = toml::from_str(&toml).expect("valid TOML");
        assert_eq!(
            parsed["project"]["primary-author"].as_str(),
            Some("a\"b\\c d")
        );
    }

    #[test]
    fn empty_description_adds_no_comment_line() {
        let toml = hpds_toml("p", "   ", "a");
        assert_eq!(toml.matches('#').count(), 3, "{toml}");
    }

    #[test]
    fn ensure_language_passes_when_no_component_needs_it() {
        let selections = vec![parse_selection("pipeline").unwrap()];
        assert!(ensure_language_for(&selections, None).is_ok());
    }

    #[test]
    fn ensure_language_errors_actionably_for_readme_without_language() {
        let selections = vec![parse_selection("readme").unwrap()];
        let err = ensure_language_for(&selections, None).unwrap_err();
        let out = usage_parts(&err);
        assert!(out.contains("readme"), "{out}");
        assert!(out.contains("--language"), "{out}");
    }

    #[test]
    fn togi_next_step_applies_whenever_the_project_has_a_language() {
        assert!(togi_next_step_applies(&[], Some("r")));
        assert!(!togi_next_step_applies(&[], None));
    }

    #[test]
    fn togi_next_step_follows_the_lint_workflow_without_a_language() {
        let with_lint = vec![parse_selection("gha:lint").unwrap()];
        assert!(togi_next_step_applies(&with_lint, None));

        let without_lint = vec![parse_selection("gha:pr-template").unwrap()];
        assert!(!togi_next_step_applies(&without_lint, None));

        // Interactive gha has no workflow list yet — the multi-select may
        // still pick lint, so the next step stays on.
        let undecided = vec![parse_selection("gha").unwrap()];
        assert!(togi_next_step_applies(&undecided, None));
    }

    #[test]
    fn default_project_name_is_the_directory_basename() {
        let name = default_project_name(Path::new("/home/user/projects/my-study")).unwrap();
        assert_eq!(name, "my-study");
    }
}
