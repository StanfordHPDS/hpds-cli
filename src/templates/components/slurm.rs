//! `hpds use slurm` — an sbatch job script plus a short submitting guide.
//!
//! Writes `scripts/slurm_job.sh` (job name from the project, logs under
//! `logs/`, mail flags commented out, module loads, the pipeline running
//! inside the project's Apptainer image), `docs/slurm.md` on how to
//! submit, and `logs/.gitkeep` so the log directory exists before the
//! first submission (Slurm does not create it).

use std::path::Path;

use crate::templates::{FileOutcome, TEMPLATES, WriteOutcome, apply_dir};
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "slurm",
    description: "sbatch job script running inside the Apptainer image, plus docs/slurm.md on submitting",
    run,
};

/// Render the Slurm templates into the project root.
fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    super::reject_kind(ctx, "slurm")?;
    let language = super::require_language(ctx, "slurm")?;
    let source = TEMPLATES
        .get_dir("slurm")
        // The slurm templates are embedded at compile time; a missing
        // directory is a packaging bug, not a user error.
        .ok_or_else(|| anyhow::anyhow!("embedded template directory `slurm` is missing"))
        .hint("this is a bug in hpds; please report it")?;
    let vars = ctx.vars.clone().with("run_command", run_command(language));
    let outcomes = apply_dir(source, ctx.dest, &vars, ctx.force)?;
    for outcome in &outcomes {
        if outcome.path == Path::new("scripts/slurm_job.sh")
            && matches!(
                outcome.outcome,
                WriteOutcome::Created | WriteOutcome::Overwritten
            )
        {
            make_executable(&ctx.dest.join(&outcome.path));
        }
    }
    Ok(outcomes)
}

/// The default pipeline command for the job script, by project language.
/// A comment in the script tells users to edit it.
fn run_command(language: &str) -> &'static str {
    match language {
        // R and mixed projects: the targets pipeline is the lab default.
        "r" | "both" => "Rscript -e 'targets::tar_make()'",
        // `scripts/` is where the README's file-structure table puts
        // analysis code; keep the two templates telling the same story.
        "python" => "uv run python scripts/analysis.py",
        // Unknown language selections still get a working script.
        _ => "make",
    }
}

/// Set the executable bits on the job script so `./scripts/slurm_job.sh`
/// works alongside `sbatch`. Best-effort: `sbatch` does not need the bits,
/// so a chmod failure is not worth failing the whole component over.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o111);
        let _ = fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {
    // Windows has no executable bit; nothing to do.
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;
    use crate::templates::WriteOutcome;
    use std::fs;
    use std::path::PathBuf;

    fn run_in(language: &str) -> (tempfile::TempDir, Vec<FileOutcome>) {
        let tmp = tempfile::tempdir().unwrap();
        let outcomes = run(&test_ctx(tmp.path(), language)).unwrap();
        (tmp, outcomes)
    }

    fn script(tmp: &tempfile::TempDir) -> String {
        fs::read_to_string(tmp.path().join("scripts").join("slurm_job.sh")).unwrap()
    }

    #[test]
    fn writes_the_job_script_the_docs_and_the_logs_dir() {
        let (tmp, outcomes) = run_in("r");
        let mut paths: Vec<_> = outcomes.iter().map(|o| o.path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("docs/slurm.md"),
                PathBuf::from("logs/.gitkeep"),
                PathBuf::from("scripts/slurm_job.sh"),
            ]
        );
        assert!(
            outcomes.iter().all(|o| o.outcome == WriteOutcome::Created),
            "{outcomes:?}"
        );
        assert!(tmp.path().join("logs").is_dir());
    }

    #[test]
    fn job_name_defaults_to_the_project_and_logs_go_under_logs() {
        let (tmp, _) = run_in("r");
        let text = script(&tmp);
        assert!(
            text.contains("#SBATCH --job-name=malaria-icu"),
            "job name from project: {text}"
        );
        assert!(text.contains("logs/"), "logs directory in use: {text}");
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn mail_flags_are_present_but_commented_out() {
        let (tmp, _) = run_in("r");
        let text = script(&tmp);
        for flag in ["--mail-type", "--mail-user"] {
            let line = text
                .lines()
                .find(|l| l.contains(flag))
                .unwrap_or_else(|| panic!("{flag} line present"));
            assert!(
                line.trim_start().starts_with("##SBATCH"),
                "{flag} is commented out: {line}"
            );
        }
    }

    #[test]
    fn script_loads_modules_and_runs_inside_the_apptainer_image() {
        let (tmp, _) = run_in("r");
        let text = script(&tmp);
        assert!(text.contains("module load"), "module loads: {text}");
        assert!(text.contains("apptainer exec"), "apptainer run: {text}");
    }

    #[test]
    fn run_command_matches_the_project_language() {
        let (tmp, _) = run_in("r");
        assert!(script(&tmp).contains("targets::tar_make()"));
        let (tmp, _) = run_in("both");
        assert!(script(&tmp).contains("targets::tar_make()"));
        // The path must match the `scripts/` layout the README's file
        // structure table documents.
        let (tmp, _) = run_in("python");
        assert!(script(&tmp).contains("uv run python scripts/analysis.py"));
    }

    #[cfg(unix)]
    #[test]
    fn rendered_script_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, _) = run_in("r");
        let mode = fs::metadata(tmp.path().join("scripts").join("slurm_job.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits set: {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn rendered_script_passes_bash_syntax_check() {
        for language in ["r", "python"] {
            let (tmp, _) = run_in(language);
            let status = std::process::Command::new("bash")
                .arg("-n")
                .arg(tmp.path().join("scripts").join("slurm_job.sh"))
                .status()
                .expect("bash is available on unix");
            assert!(status.success(), "bash -n fails for {language}");
        }
    }

    #[test]
    fn docs_explain_how_to_submit() {
        let (tmp, _) = run_in("r");
        let docs = fs::read_to_string(tmp.path().join("docs").join("slurm.md")).unwrap();
        assert!(
            docs.contains("sbatch scripts/slurm_job.sh"),
            "submit command: {docs}"
        );
        assert!(docs.contains("squeue"), "monitoring: {docs}");
        assert!(!docs.contains("{{"), "no unrendered variables: {docs}");
    }

    #[test]
    fn existing_script_is_never_overwritten_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("scripts").join("slurm_job.sh");
        fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        fs::write(&script_path, "#!/bin/bash\n# customized\n").unwrap();
        let outcomes = run(&test_ctx(tmp.path(), "r")).unwrap();
        let script_outcome = outcomes
            .iter()
            .find(|o| o.path == Path::new("scripts/slurm_job.sh"))
            .unwrap();
        assert!(
            matches!(script_outcome.outcome, WriteOutcome::SkippedConflict { .. }),
            "{script_outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&script_path).unwrap(),
            "#!/bin/bash\n# customized\n"
        );
    }

    #[test]
    fn kind_flag_is_rejected_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("targets");
        let err = run(&ctx).unwrap_err();
        assert!(err.to_string().contains("--kind"), "{err}");
        assert!(!tmp.path().join("scripts").exists());
    }

    #[test]
    fn second_run_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        run(&test_ctx(tmp.path(), "r")).unwrap();
        let second = run(&test_ctx(tmp.path(), "r")).unwrap();
        assert!(
            second.iter().all(|o| o.outcome == WriteOutcome::Unchanged),
            "{second:?}"
        );
    }
}
