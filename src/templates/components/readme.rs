//! `hpds use readme` — a lab-manual structured Markdown README.
//!
//! Every project gets `README.md` directly, with starter content tailored
//! to R, Python, or a project using both. Every variant carries suggested
//! sections for description, file structure, how to run, and dependencies.

use crate::templates::{FileOutcome, TEMPLATES, apply_dir};
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "readme",
    description: "README.md with suggested lab-manual sections",
    run,
};

/// Suggested README sections shared by the language-specific templates.
#[cfg(test)]
const STARTER_SECTIONS: &[&str] = &[
    "Description",
    "File structure",
    "How to run",
    "Dependencies",
];

/// Render the README template for the project's language into the
/// project root.
fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    super::reject_kind(ctx, "readme")?;
    super::reject_workflows(ctx, "readme")?;
    let language = super::require_language(ctx, "readme")?;
    // Every variant writes README.md; only the starter content differs.
    let variant = match language {
        "r" => "readme/r",
        "both" => "readme/both",
        _ => "readme/python",
    };
    let source = TEMPLATES
        .get_dir(variant)
        // The readme templates are embedded at compile time; a missing
        // directory is a packaging bug, not a user error.
        .ok_or_else(|| anyhow::anyhow!("embedded template directory `{variant}` is missing"))
        .hint("this is a bug in hpds; please report it")?;
    Ok(apply_dir(source, ctx.dest, &ctx.vars, ctx.force)?)
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;
    use crate::templates::{Vars, WriteOutcome};
    use std::fs;

    fn run_in(language: &str) -> (tempfile::TempDir, Vec<FileOutcome>) {
        let tmp = tempfile::tempdir().unwrap();
        let outcomes = run(&test_ctx(tmp.path(), language)).unwrap();
        (tmp, outcomes)
    }

    #[test]
    fn r_project_gets_readme_md_without_a_render_step() {
        let (tmp, outcomes) = run_in("r");
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("README.md"));
        assert_eq!(outcomes[0].outcome, WriteOutcome::Created);
        let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(!tmp.path().join("README.qmd").exists());
        assert!(
            !text.contains("quarto render") && !text.starts_with("---"),
            "README is plain Markdown with no render step: {text}"
        );
        assert!(
            text.contains("malaria-icu"),
            "project name substituted: {text}"
        );
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn python_project_gets_readme_md() {
        let (tmp, outcomes) = run_in("python");
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("README.md"));
        let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(!tmp.path().join("README.qmd").exists());
        assert!(text.contains("# malaria-icu"), "titled by project: {text}");
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn mixed_language_project_gets_readme_md() {
        let (tmp, outcomes) = run_in("both");
        assert_eq!(outcomes[0].path.to_str(), Some("README.md"));
        assert!(tmp.path().join("README.md").exists());
        assert!(!tmp.path().join("README.qmd").exists());
        let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
        for expected in ["renv::restore()", "uv sync", "make", "renv.lock", "uv.lock"] {
            assert!(
                text.contains(expected),
                "mixed README documents `{expected}`: {text}"
            );
        }
    }

    #[test]
    fn python_readme_runs_the_public_make_pipeline() {
        let (tmp, _) = run_in("python");
        let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(
            text.contains("\nmake\n"),
            "run block uses the project pipeline entry point: {text}"
        );
    }

    #[test]
    fn every_language_readme_carries_the_starter_sections() {
        for language in ["r", "python", "both"] {
            let (tmp, _) = run_in(language);
            let file = "README.md";
            let text = fs::read_to_string(tmp.path().join(file)).unwrap();
            for section in STARTER_SECTIONS {
                let heading = format!("## {section}");
                assert!(text.contains(&heading), "{file} for {language}: {heading}");
            }
        }
    }

    #[test]
    fn directory_tables_are_explicitly_optional_examples() {
        for language in ["r", "python", "both"] {
            let (tmp, _) = run_in(language);
            let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
            assert!(text.contains("Example only"), "{language}: {text}");
            assert!(text.contains("replace or remove"), "{language}: {text}");
        }
    }

    #[test]
    fn existing_readme_is_never_overwritten_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("README.md"), "my notes\n").unwrap();
        let outcomes = run(&test_ctx(tmp.path(), "python")).unwrap();
        assert!(
            matches!(outcomes[0].outcome, WriteOutcome::SkippedConflict { .. }),
            "{outcomes:?}"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("README.md")).unwrap(),
            "my notes\n"
        );
    }

    #[test]
    fn kind_flag_is_rejected_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "python");
        ctx.kind = Some("make");
        let err = run(&ctx).unwrap_err();
        assert!(err.to_string().contains("--kind"), "{err}");
        assert!(err.to_string().contains("drop the --kind flag"), "{err}");
        assert!(!tmp.path().join("README.md").exists());
    }

    #[test]
    fn missing_language_is_rejected_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "python");
        ctx.vars = Vars::standard("malaria-icu", None, "HPDS Lab");
        let err = run(&ctx).unwrap_err();
        let rendered = crate::ui::render_error(&err, false);
        assert!(rendered.contains("--language"), "{rendered}");
        assert!(!tmp.path().join("README.md").exists());
        assert!(!tmp.path().join("README.qmd").exists());
    }

    #[test]
    fn force_replaces_a_conflicting_readme() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("README.md"), "my notes\n").unwrap();
        let mut ctx = test_ctx(tmp.path(), "python");
        ctx.force = true;
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes[0].outcome, WriteOutcome::Overwritten);
        let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(text.contains("# malaria-icu"));
    }
}
