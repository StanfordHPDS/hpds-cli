//! `hpds use container` — Dockerfile and/or Apptainer definition scaffolds
//! based on the lab base images.
//!
//! `--kind docker|apptainer|both` picks the file(s); the project language
//! (`--language` or detection) picks the base image (`stanfordhpds/r-renv`,
//! `stanfordhpds/python-uv`, or `stanfordhpds/base`) and the dependency
//! restore steps written into it (`renv::restore()` and/or `uv sync`).

use std::fmt;

use crate::templates::{FileOutcome, TEMPLATES, apply_dir};
use crate::ui;
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "container",
    description: "Dockerfile and/or Apptainer definition on the lab base images",
    run,
};

/// Which container file(s) to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Docker,
    Apptainer,
    Both,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Docker, Kind::Apptainer, Kind::Both];

    fn name(self) -> &'static str {
        match self {
            Kind::Docker => "docker",
            Kind::Apptainer => "apptainer",
            Kind::Both => "both",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Kind> {
        Kind::ALL
            .into_iter()
            .find(|k| k.name() == value)
            .ok_or_else(|| anyhow::anyhow!("`{value}` is not a container kind"))
            .hint("pass --kind docker, --kind apptainer, or --kind both")
    }

    /// The container formats this kind renders, in order. Each format is
    /// one embedded directory per language: `container/<format>/<language>`.
    fn formats(self) -> &'static [&'static str] {
        match self {
            Kind::Docker => &["docker"],
            Kind::Apptainer => &["apptainer"],
            Kind::Both => &["docker", "apptainer"],
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// `--kind` when given, otherwise an interactive choice (which fails with a
/// pointer at `--kind` when the process cannot prompt).
fn resolve_kind(flag: Option<&str>) -> anyhow::Result<Kind> {
    match flag {
        Some(value) => Kind::parse(value),
        None => ui::select("Which container format?", Kind::ALL.to_vec()),
    }
}

fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    super::reject_workflows(ctx, "container")?;
    let kind = resolve_kind(ctx.kind)?;
    let language = super::require_language(ctx, "container")?;
    let mut outcomes = Vec::new();
    for format in kind.formats() {
        let dir = format!("container/{format}/{language}");
        let source = TEMPLATES
            .get_dir(&dir)
            // The container templates are embedded at compile time; a
            // missing directory is a packaging bug, not a user error.
            .ok_or_else(|| anyhow::anyhow!("embedded template directory `{dir}` is missing"))
            .hint("this is a bug in hpds; please report it")?;
        outcomes.extend(apply_dir(source, ctx.dest, &ctx.vars, ctx.force)?);
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;
    use crate::templates::WriteOutcome;
    use std::fs;

    #[test]
    fn every_kind_parses_from_its_name() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_kind_errors_and_lists_the_valid_kinds() {
        let err = Kind::parse("podman").unwrap_err();
        let rendered = ui::render_error(&err, false);
        assert!(
            rendered.contains("podman"),
            "names the bad kind: {rendered}"
        );
        for valid in ["docker", "apptainer", "both"] {
            assert!(rendered.contains(valid), "lists `{valid}`: {rendered}");
        }
    }

    #[test]
    fn both_renders_docker_before_apptainer() {
        assert_eq!(Kind::Both.formats(), &["docker", "apptainer"]);
    }

    #[test]
    fn every_kind_and_language_maps_to_an_embedded_template_dir() {
        for kind in Kind::ALL {
            for language in ["r", "python", "both"] {
                for format in kind.formats() {
                    let dir = format!("container/{format}/{language}");
                    assert!(
                        TEMPLATES.get_dir(&dir).is_some(),
                        "{kind}/{language} needs embedded dir `{dir}`"
                    );
                }
            }
        }
    }

    #[test]
    fn docker_r_uses_the_lab_r_image_and_restores_renv() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("docker");
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("Dockerfile"));
        assert_eq!(outcomes[0].outcome, WriteOutcome::Created);
        let text = fs::read_to_string(tmp.path().join("Dockerfile")).unwrap();
        assert!(text.contains("FROM stanfordhpds/r-renv"), "{text}");
        assert!(text.contains("renv::restore()"), "{text}");
        assert!(text.contains("malaria-icu"), "project substituted: {text}");
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn apptainer_python_bootstraps_the_lab_python_image_and_syncs_uv() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "python");
        ctx.kind = Some("apptainer");
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("container.def"));
        let text = fs::read_to_string(tmp.path().join("container.def")).unwrap();
        assert!(text.contains("From: stanfordhpds/python-uv"), "{text}");
        assert!(text.contains("uv sync"), "{text}");
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn kind_both_writes_the_dockerfile_and_the_def() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "both");
        ctx.kind = Some("both");
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        for file in ["Dockerfile", "container.def"] {
            let text = fs::read_to_string(tmp.path().join(file)).unwrap();
            assert!(
                text.contains("stanfordhpds/base"),
                "{file} uses the base image: {text}"
            );
            assert!(text.contains("renv::restore()"), "{file}: {text}");
            assert!(text.contains("uv sync"), "{file}: {text}");
        }
    }

    #[test]
    fn missing_language_is_rejected_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("docker");
        ctx.vars = crate::templates::Vars::standard("malaria-icu", None, "HPDS Lab");
        let err = run(&ctx).unwrap_err();
        let rendered = ui::render_error(&err, false);
        assert!(rendered.contains("--language"), "{rendered}");
        assert!(!tmp.path().join("Dockerfile").exists());
    }

    #[test]
    fn existing_dockerfile_is_never_overwritten_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM my-own-image\n").unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("docker");
        let outcomes = run(&ctx).unwrap();
        assert!(
            matches!(outcomes[0].outcome, WriteOutcome::SkippedConflict { .. }),
            "{outcomes:?}"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("Dockerfile")).unwrap(),
            "FROM my-own-image\n"
        );
    }

    #[test]
    fn force_replaces_a_conflicting_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Dockerfile"), "FROM my-own-image\n").unwrap();
        let mut ctx = test_ctx(tmp.path(), "r");
        ctx.kind = Some("docker");
        ctx.force = true;
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes[0].outcome, WriteOutcome::Overwritten);
        let text = fs::read_to_string(tmp.path().join("Dockerfile")).unwrap();
        assert!(text.contains("FROM stanfordhpds/r-renv"), "{text}");
    }
}
