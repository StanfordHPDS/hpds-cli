//! `hpds use readme` — a lab-manual structured README.
//!
//! R projects get `README.qmd` (rendered to `README.md` with quarto;
//! the file says so at the top); everything else gets `README.md`
//! directly. Both carry the lab-manual minimum sections: description,
//! file structure, how to run, dependencies.

use crate::templates::{FileOutcome, TEMPLATES, apply_dir};
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "readme",
    description: "README with the lab-manual sections (README.qmd for R projects, README.md otherwise)",
    run,
};

/// The lab-manual minimum README sections, as heading text (the templates
/// render each as `## <section>`). The audit's readme check enforces the
/// same list, so it lives in exactly one place.
pub const LAB_MANUAL_SECTIONS: &[&str] = &[
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
    // R projects (including mixed R + Python ones) get a Quarto source
    // that renders to README.md; everything else gets README.md directly.
    let variant = if project_uses_r(language) {
        "readme/qmd"
    } else {
        "readme/md"
    };
    let source = TEMPLATES
        .get_dir(variant)
        // The readme templates are embedded at compile time; a missing
        // directory is a packaging bug, not a user error.
        .ok_or_else(|| anyhow::anyhow!("embedded template directory `{variant}` is missing"))
        .hint("this is a bug in hpds; please report it")?;
    Ok(apply_dir(source, ctx.dest, &ctx.vars, ctx.force)?)
}

/// `true` when the project's language selection includes R.
fn project_uses_r(language: &str) -> bool {
    matches!(language, "r" | "both")
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
    fn r_project_gets_readme_qmd_with_a_render_note() {
        let (tmp, outcomes) = run_in("r");
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("README.qmd"));
        assert_eq!(outcomes[0].outcome, WriteOutcome::Created);
        let text = fs::read_to_string(tmp.path().join("README.qmd")).unwrap();
        assert!(!tmp.path().join("README.md").exists());
        assert!(
            text.contains("quarto render README.qmd"),
            "says how to render: {text}"
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
    fn mixed_language_project_counts_as_r_and_gets_qmd() {
        let (tmp, outcomes) = run_in("both");
        assert_eq!(outcomes[0].path.to_str(), Some("README.qmd"));
        assert!(tmp.path().join("README.qmd").exists());
    }

    #[test]
    fn md_readme_run_commands_agree_with_its_file_structure_table() {
        // The table says analysis code lives in `scripts/`; the "How to
        // run" block must not contradict it by running from the root.
        let (tmp, _) = run_in("python");
        let text = fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(
            text.contains("uv run python scripts/analysis.py"),
            "run block uses the scripts/ layout: {text}"
        );
    }

    #[test]
    fn both_variants_carry_the_lab_manual_minimum_sections() {
        for language in ["r", "python"] {
            let (tmp, _) = run_in(language);
            let file = if language == "r" {
                "README.qmd"
            } else {
                "README.md"
            };
            let text = fs::read_to_string(tmp.path().join(file)).unwrap();
            for section in LAB_MANUAL_SECTIONS {
                let heading = format!("## {section}");
                assert!(text.contains(&heading), "{file} for {language}: {heading}");
            }
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
