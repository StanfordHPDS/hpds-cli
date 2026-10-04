//! `hpds use devcontainer`: editor-neutral Dev Container configuration.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::json;

use crate::templates::{FileOutcome, WriteOutcome, write_rendered};
use crate::ui::HintExt;

use super::{Component, ComponentCtx};

pub static COMPONENT: Component = Component {
    name: "devcontainer",
    description: "VS Code and Positron development container configuration",
    run,
};

const CONFIG_PATH: &str = ".devcontainer/devcontainer.json";
const README_PATH: &str = ".devcontainer/README.md";

fn run(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    if let Some(kind) = ctx.kind {
        return Err(crate::cli::usage_error(
            format!("the `devcontainer` component has no --kind variants (got `{kind}`)"),
            "drop --kind and re-run `hpds use devcontainer`",
        ));
    }
    if let Some(workflows) = ctx.workflows {
        return Err(crate::cli::usage_error(
            format!(
                "the `devcontainer` component does not take --workflows (got `{}`)",
                workflows.join(",")
            ),
            "drop --workflows and re-run `hpds use devcontainer`",
        ));
    }
    preflight(ctx.dest)?;

    let folder = checkout_basename(ctx.dest)?;
    let title = ctx.vars.get("project").unwrap_or(folder);
    let workspace = format!("/workspaces/{folder}");
    let config = json!({
        "name": title,
        "build": {
            "dockerfile": "../Dockerfile",
            "context": "..",
            "target": "hpds-dev",
            "args": {
                "HPDS_PROJECT_DIR": workspace,
                "HPDS_UID": "1000",
                "HPDS_GID": "1000"
            }
        },
        "workspaceFolder": workspace,
        "workspaceMount": format!(
            "source=${{localWorkspaceFolder}},target=/workspaces/{folder},type=bind"
        ),
        "remoteUser": "hpds",
        "updateRemoteUserUID": false
    });
    let mut config_text = serde_json::to_string_pretty(&config)?;
    config_text.push('\n');
    let readme = readme(folder, ctx.vars.get("language"));

    Ok(vec![
        outcome(
            CONFIG_PATH,
            write_rendered(
                &ctx.dest.join(CONFIG_PATH),
                config_text.as_bytes(),
                ctx.force,
            )?,
        ),
        outcome(
            README_PATH,
            write_rendered(&ctx.dest.join(README_PATH), readme.as_bytes(), ctx.force)?,
        ),
    ])
}

fn outcome(path: &str, outcome: WriteOutcome) -> FileOutcome {
    FileOutcome {
        path: PathBuf::from(path),
        outcome,
    }
}

fn checkout_basename(dest: &Path) -> anyhow::Result<&str> {
    dest.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("could not determine the checkout folder name"))
        .hint("run this command from a project directory with a UTF-8 folder name")
}

/// Validate everything that can fail before either owned file is written.
pub(crate) fn preflight(dest: &Path) -> anyhow::Result<()> {
    preflight_workspace(dest)?;
    preflight_competing(dest)?;
    require_dev_stage(&dest.join("Dockerfile"))
}

pub(crate) fn preflight_workspace(dest: &Path) -> anyhow::Result<()> {
    let folder = checkout_basename(dest)?;
    if folder
        .chars()
        .any(|character| character.is_control() || character == ',')
    {
        anyhow::bail!(
            "checkout folder name `{folder}` cannot be represented safely in a Dev Container mount; rename the folder to remove commas and control characters"
        );
    }
    Ok(())
}

pub(crate) fn preflight_competing(dest: &Path) -> anyhow::Result<()> {
    let root_config = dest.join(".devcontainer.json");
    if root_config.exists() {
        anyhow::bail!(
            "competing Dev Container configuration `{}` already exists; move or remove it before generating `{CONFIG_PATH}`",
            root_config.display()
        );
    }
    let dir = dest.join(".devcontainer");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("could not inspect `{}`", dir.display()));
        }
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() && path.join("devcontainer.json").exists() {
            anyhow::bail!(
                "competing Dev Container configuration `{}` already exists; move or remove it before generating `{CONFIG_PATH}`",
                path.join("devcontainer.json").display()
            );
        }
    }
    Ok(())
}

pub(crate) fn require_dev_stage(path: &Path) -> anyhow::Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| {
            format!(
                "a compatible root Dockerfile with an `hpds-dev` stage is required at `{}`",
                path.display()
            )
        })
        .hint("generate a Dockerfile with `hpds use container --kind docker`, then retry")?;
    if dockerfile_has_stage(&text, "hpds-dev") {
        return Ok(());
    }
    anyhow::bail!(
        "Dockerfile `{}` does not define an actual `hpds-dev` build stage; regenerate or update it before creating a Dev Container",
        path.display()
    )
}

fn dockerfile_has_stage(text: &str, wanted: &str) -> bool {
    let mut heredoc_ends: std::collections::VecDeque<String> = Default::default();
    let mut logical = String::new();
    for raw in text.lines() {
        if let Some(end) = heredoc_ends.front() {
            if raw.trim_start_matches([' ', '\t']) == end {
                heredoc_ends.pop_front();
            }
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let continued = line.ends_with('\\');
        logical.push_str(line.trim_end_matches('\\'));
        logical.push(' ');
        if continued {
            continue;
        }
        heredoc_ends.extend(heredoc_delimiters(&logical));
        let words: Vec<_> = logical.split_whitespace().collect();
        if !words
            .first()
            .is_some_and(|word| word.eq_ignore_ascii_case("FROM"))
        {
            logical.clear();
            continue;
        }
        if words
            .windows(2)
            .any(|pair| pair[0].eq_ignore_ascii_case("AS") && pair[1].eq_ignore_ascii_case(wanted))
        {
            return true;
        }
        logical.clear();
    }
    false
}

fn heredoc_delimiters(instruction_line: &str) -> Vec<String> {
    let Some(instruction) = instruction_line.split_whitespace().next() else {
        return Vec::new();
    };
    if !["RUN", "COPY", "ADD"]
        .iter()
        .any(|candidate| instruction.eq_ignore_ascii_case(candidate))
    {
        return Vec::new();
    }
    let mut delimiters = Vec::new();
    for (index, _) in instruction_line.match_indices("<<") {
        let prefix = &instruction_line[..index];
        let in_single_quotes = prefix
            .chars()
            .filter(|character| *character == '\'')
            .count()
            % 2
            == 1;
        let in_double_quotes =
            prefix.chars().filter(|character| *character == '"').count() % 2 == 1;
        if in_single_quotes || in_double_quotes {
            continue;
        }
        let Some(token) = instruction_line[index + 2..]
            .trim_start_matches('-')
            .split_whitespace()
            .next()
        else {
            continue;
        };
        let token = token.trim_matches(['\'', '"']);
        if !token.is_empty() {
            delimiters.push(token.to_string());
        }
    }
    delimiters
}

fn readme(folder: &str, language: Option<&str>) -> String {
    let interpreters = match language {
        Some("r") => {
            "Select `/usr/local/bin/R` as the R executable. R starts with the project's normal renv activation and uses the library under `/opt/renv/library`. Change dependencies with the usual `renv` commands, update `renv.lock`, and rebuild the container."
        }
        Some("python") => {
            "Run `Python: Select Interpreter` from the Command Palette and choose `/opt/venv/bin/python`. Change dependencies with the usual `uv` commands, update `uv.lock`, and rebuild the container."
        }
        _ => {
            "Run `Python: Select Interpreter` from the Command Palette and choose `/opt/venv/bin/python`; select `/usr/local/bin/R` as the R executable. R starts with the project's normal renv activation and uses the library under `/opt/renv/library`. Change dependencies with the usual `uv` or `renv` commands, update the lockfiles, and rebuild the container."
        }
    };
    format!(
        "# Development container\n\nOpen this project in Visual Studio Code with the Dev Containers extension or enable Positron's Dev Containers preview. Run `Dev Containers: Reopen in Container` from the Command Palette. After configuration changes, run `Dev Containers: Rebuild Container`. The shared configuration builds the `hpds-dev` target and mounts this checkout at `/workspaces/{folder}`.\n\n{interpreters} No packages are restored automatically when the editor starts.\n\nOn Linux, set the literal `HPDS_UID` and `HPDS_GID` build arguments in `devcontainer.json` to the output of `id -u` and `id -g`, then rebuild. `updateRemoteUserUID` is disabled so the account and `/opt` environments keep matching ownership.\n\nThe workspace path and build argument contain the checkout folder name literally for Positron compatibility. If the checkout folder is renamed, rerun `hpds use devcontainer --force` before reopening it.\n\nOpen the analysis image interactively outside an editor from the project root with `docker build --target hpds-analysis -t hpds-analysis .`, followed by `docker run --rm -it hpds-analysis`. Existing projects need a newly generated Dockerfile with `hpds-project`, `hpds-dev`, and final `hpds-analysis` stages before adding this configuration. Windows has not yet been validated.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_parser_ignores_comments_strings_and_heredocs() {
        for text in [
            "# FROM base AS hpds-dev\nFROM base AS other\n",
            "RUN echo 'FROM base AS hpds-dev'\nFROM base AS other\n",
            "RUN cat <<'EOF'\nFROM base AS hpds-dev\nEOF\nFROM base AS other\n",
            "RUN <<-EOF\n\tFROM base AS hpds-dev\n\tEOF\nFROM base AS other\n",
        ] {
            assert!(!dockerfile_has_stage(text, "hpds-dev"), "{text}");
        }
        assert!(dockerfile_has_stage(
            "RUN echo \"value <<EOF\"\nFROM base AS hpds-dev\n",
            "hpds-dev"
        ));
        assert!(dockerfile_has_stage("from base as HPDS-DEV\n", "hpds-dev"));
        assert!(dockerfile_has_stage(
            "FROM --platform=linux/amd64 base AS \\\nhpds-dev\n",
            "hpds-dev"
        ));
    }
}
