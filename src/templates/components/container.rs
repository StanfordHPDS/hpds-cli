//! `hpds use container` — Dockerfile and/or Apptainer definition scaffolds
//! based on slim upstream images plus the pinned hpds release image.
//!
//! `--kind docker|apptainer|both` picks the file(s); the project language
//! (`--language` or detection) picks the upstream runtime and dependency
//! restore steps. Python is provisioned from project metadata by the
//! release-pinned official uv image; R uses the current release resolved
//! from R-hub and rendered as a literal Rocker tag. The generating hpds
//! version is likewise rendered into the release image reference.

use std::fmt;
use std::io::Read;

use anyhow::Context;
use serde::Deserialize;

use crate::templates::{FileOutcome, TEMPLATES, apply_dir};
use crate::ui;
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "container",
    description: "slim Dockerfile and/or Apptainer definition with pinned runtimes",
    run,
};

const R_RELEASE_URL: &str = "https://api.r-hub.io/rversions/r-release";
const USER_AGENT: &str = concat!("hpds/", env!("CARGO_PKG_VERSION"));

#[derive(Deserialize)]
struct RRelease {
    version: String,
}

/// Parse and validate the R-hub release response before it becomes part of
/// a container image reference.
fn parse_r_release(body: &str) -> anyhow::Result<String> {
    let release: RRelease =
        serde_json::from_str(body).context("R-hub's R release response was not valid JSON")?;
    let parts: Vec<_> = release.version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()))
    {
        anyhow::bail!(
            "R-hub returned an invalid R release version `{}`",
            release.version
        );
    }
    Ok(release.version)
}

/// Resolve the current R release. The override is an internal offline-test
/// seam; normal runs use R-hub's hourly-updated release endpoint.
fn resolve_r_release() -> anyhow::Result<String> {
    if let Ok(version) = std::env::var("HPDS_R_VERSION") {
        return parse_r_release(&format!(r#"{{"version":"{version}"}}"#))
            .context("HPDS_R_VERSION is not a full numeric R version");
    }

    let url = std::env::var("HPDS_R_RELEASE_URL").unwrap_or_else(|_| R_RELEASE_URL.to_string());
    let agent = crate::tools::github_agent();
    let mut response = agent
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("could not resolve the current R release from `{url}`"))
        .hint("check your network connection and retry `hpds use container`")?;
    let mut body = String::new();
    response
        .body_mut()
        .as_reader()
        .read_to_string(&mut body)
        .with_context(|| format!("could not read R-hub's response from `{url}`"))?;
    parse_r_release(&body)
        .hint("R-hub returned an unexpected response; retry, or report the response to hpds")
}

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
        // An unknown --kind value is a usage error (exit 2), like an
        // unknown component name.
        Kind::ALL
            .into_iter()
            .find(|k| k.name() == value)
            .ok_or_else(|| {
                crate::cli::usage_error(
                    format!("`{value}` is not a container kind"),
                    "pass --kind docker, --kind apptainer, or --kind both",
                )
            })
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

/// Every kind name, in menu order. `hpds init` validates `--use
/// container:<kind>` against this list before it writes anything.
pub(crate) fn kind_names() -> Vec<&'static str> {
    Kind::ALL.into_iter().map(Kind::name).collect()
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
    let mut vars = ctx.vars.clone();
    if matches!(language, "python" | "both") {
        vars = vars.with("uv_version", crate::tools::versions::UV);
    }
    if matches!(language, "r" | "both") && vars.get("r_version").is_none() {
        vars = vars.with("r_version", resolve_r_release()?);
    }
    let mut outcomes = Vec::new();
    for format in kind.formats() {
        let dir = format!("container/{format}/{language}");
        let source = TEMPLATES
            .get_dir(&dir)
            // The container templates are embedded at compile time; a
            // missing directory is a packaging bug, not a user error.
            .ok_or_else(|| anyhow::anyhow!("embedded template directory `{dir}` is missing"))
            .hint("this is a bug in hpds; please report it")?;
        outcomes.extend(apply_dir(source, ctx.dest, &vars, ctx.force)?);
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;
    use crate::templates::WriteOutcome;
    use std::fs;

    fn container_ctx<'a>(dest: &'a std::path::Path, language: &str) -> ComponentCtx<'a> {
        let mut ctx = test_ctx(dest, language);
        ctx.vars = ctx.vars.with("r_version", "4.6.1");
        ctx
    }

    #[test]
    fn parses_the_r_hub_release_response() {
        let body = r#"{"version":"4.6.1","date":"2026-06-24T07:14:42Z","semver":"4.6.1"}"#;
        assert_eq!(parse_r_release(body).unwrap(), "4.6.1");
    }

    #[test]
    fn rejects_an_r_hub_response_without_a_semantic_version() {
        for body in [r#"{"date":"2026-06-24"}"#, r#"{"version":"release"}"#] {
            assert!(parse_r_release(body).is_err(), "{body}");
        }
    }

    #[test]
    fn every_kind_parses_from_its_name() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_kind_is_a_usage_error_listing_the_valid_kinds() {
        let err = Kind::parse("podman").unwrap_err();
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("an unknown --kind value is a usage error (exit 2)");
        let out = format!("{usage}\nhint: {}", usage.hint());
        assert!(out.contains("podman"), "names the bad kind: {out}");
        for valid in ["docker", "apptainer", "both"] {
            assert!(out.contains(valid), "lists `{valid}`: {out}");
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
    fn docker_r_uses_the_resolved_r_and_generating_hpds_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = container_ctx(tmp.path(), "r");
        ctx.kind = Some("docker");
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("Dockerfile"));
        assert_eq!(outcomes[0].outcome, WriteOutcome::Created);
        let text = fs::read_to_string(tmp.path().join("Dockerfile")).unwrap();
        assert!(text.contains("FROM rocker/r-ver:4.6.1"), "{text}");
        assert!(
            text.contains(&format!(
                "FROM ghcr.io/stanfordhpds/hpds-cli:{} AS hpds",
                env!("CARGO_PKG_VERSION")
            )),
            "{text}"
        );
        assert!(text.contains("renv::restore()"), "{text}");
        assert!(text.contains("malaria-icu"), "project substituted: {text}");
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn apptainer_python_copies_pinned_hpds_and_syncs_uv() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = container_ctx(tmp.path(), "python");
        ctx.kind = Some("apptainer");
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path.to_str(), Some("container.def"));
        let text = fs::read_to_string(tmp.path().join("container.def")).unwrap();
        assert!(
            text.contains(&format!(
                "From: ghcr.io/stanfordhpds/hpds-cli:{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{text}"
        );
        assert!(text.contains("From: debian:trixie-slim"), "{text}");
        assert!(
            text.contains(&format!(
                "From: ghcr.io/astral-sh/uv:{}",
                crate::tools::versions::UV
            )),
            "{text}"
        );
        assert!(text.contains("%files from uv"), "{text}");
        assert!(!text.contains("hpds install uv"), "{text}");
        assert!(text.contains("uv sync"), "{text}");
        assert!(!text.contains("{{"), "no unrendered variables: {text}");
    }

    #[test]
    fn kind_both_writes_the_dockerfile_and_the_def() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = container_ctx(tmp.path(), "both");
        ctx.kind = Some("both");
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        for file in ["Dockerfile", "container.def"] {
            let text = fs::read_to_string(tmp.path().join(file)).unwrap();
            assert!(
                text.contains("rocker/r-ver:4.6.1"),
                "{file} uses the resolved R image: {text}"
            );
            assert!(text.contains("renv::restore()"), "{file}: {text}");
            assert!(text.contains("uv sync"), "{file}: {text}");
        }
    }

    #[test]
    fn missing_language_is_rejected_and_nothing_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = container_ctx(tmp.path(), "r");
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
        let mut ctx = container_ctx(tmp.path(), "r");
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
        let mut ctx = container_ctx(tmp.path(), "r");
        ctx.kind = Some("docker");
        ctx.force = true;
        let outcomes = run(&ctx).unwrap();
        assert_eq!(outcomes[0].outcome, WriteOutcome::Overwritten);
        let text = fs::read_to_string(tmp.path().join("Dockerfile")).unwrap();
        assert!(text.contains("FROM rocker/r-ver:4.6.1"), "{text}");
    }
}
