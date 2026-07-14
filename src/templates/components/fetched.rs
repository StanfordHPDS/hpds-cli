//! Fetched template components: slides, poster, thesis.
//!
//! Unlike the embedded templates, these live in GitHub repositories and are
//! fetched at apply time (network required). Quarto templates (slides,
//! poster) prefer `quarto use template` when quarto is on PATH; otherwise,
//! and always for the Typst thesis, they are shallow-cloned with `gh` (or
//! plain `git` when `gh` is absent) and the clone's `.git` directory is
//! stripped so the files join the user's own project history.
//!
//! Either way the template lands in a fresh subdirectory of the project
//! named after the repository: quarto refuses to install a template into a
//! non-empty directory, and `hpds use` runs inside an existing project, so
//! running quarto at the project root would always fail.
//!
//! A successful fetch is reported as one `Created` [`FileOutcome`] for the
//! new subdirectory; the what-to-do-next lines go into the context's
//! guidance buffer, which the command layer prints after the outcomes.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use super::{Component, ComponentCtx};
use crate::templates::{FileOutcome, WriteOutcome};
use crate::ui;

pub static SLIDES: Component = Component {
    name: "slides",
    description: "HPDS Quarto slides theme (fetches StanfordHPDS/hpds-slides-theme)",
    run: run_slides,
};

pub static POSTER: Component = Component {
    name: "poster",
    description: "HPDS conference poster template (fetches StanfordHPDS/hpds-poster)",
    run: run_poster,
};

pub static THESIS: Component = Component {
    name: "thesis",
    description: "Stanford Typst thesis template (fetches StanfordHPDS/typst-stanford-thesis)",
    run: run_thesis,
};

fn run_slides(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    fetch(
        FetchedTemplate::Slides,
        ctx,
        std::env::var_os("PATH").as_deref(),
    )
}

fn run_poster(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    fetch(
        FetchedTemplate::Poster,
        ctx,
        std::env::var_os("PATH").as_deref(),
    )
}

fn run_thesis(ctx: &ComponentCtx) -> anyhow::Result<Vec<FileOutcome>> {
    fetch(
        FetchedTemplate::Thesis,
        ctx,
        std::env::var_os("PATH").as_deref(),
    )
}

/// A template fetched from a GitHub repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchedTemplate {
    Slides,
    Poster,
    Thesis,
}

impl FetchedTemplate {
    /// The GitHub `owner/repo` slug.
    fn slug(self) -> &'static str {
        match self {
            FetchedTemplate::Slides => "StanfordHPDS/hpds-slides-theme",
            FetchedTemplate::Poster => "StanfordHPDS/hpds-poster",
            FetchedTemplate::Thesis => "StanfordHPDS/typst-stanford-thesis",
        }
    }

    /// The repository URL (used for `git clone` and in error messages).
    fn url(self) -> String {
        format!("https://github.com/{}", self.slug())
    }

    /// The bare repository name, used as the destination directory name.
    fn repo_name(self) -> &'static str {
        self.slug()
            .rsplit('/')
            .next()
            .expect("slug always contains `/`")
    }

    /// The `hpds use <component>` argument that fetches this template.
    fn component_name(self) -> &'static str {
        match self {
            FetchedTemplate::Slides => "slides",
            FetchedTemplate::Poster => "poster",
            FetchedTemplate::Thesis => "thesis",
        }
    }

    /// Whether the repository is a Quarto template usable with
    /// `quarto use template`. The Typst thesis is a whole document project
    /// and is always cloned instead.
    fn is_quarto_template(self) -> bool {
        matches!(self, FetchedTemplate::Slides | FetchedTemplate::Poster)
    }

    /// What to do after a successful fetch.
    fn next_steps(self, dest: &Path, strategy: Strategy) -> Vec<String> {
        match self {
            FetchedTemplate::Slides | FetchedTemplate::Poster => {
                // `quarto use template` renames the template's main `.qmd`
                // after the target directory; a plain clone keeps the
                // repository's `template.qmd` as-is.
                let main_qmd = match strategy {
                    Strategy::QuartoTemplate => format!(
                        "{}.qmd",
                        dest.file_name()
                            .and_then(OsStr::to_str)
                            .unwrap_or("template")
                    ),
                    Strategy::Clone(_) => "template.qmd".to_string(),
                };
                vec![format!(
                    "next: edit {} and render with `quarto render`",
                    dest.join(main_qmd).display()
                )]
            }
            FetchedTemplate::Thesis => vec![format!(
                "next: edit the chapters under {} and render with `quarto render`",
                dest.display()
            )],
        }
    }
}

/// How a template will be fetched. Both strategies fill a fresh
/// subdirectory of the project named after the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strategy {
    /// `quarto use template <slug> --no-prompt` inside the (empty) new
    /// subdirectory; quarto refuses non-empty directories.
    QuartoTemplate,
    /// Shallow clone into the subdirectory, then strip `.git`.
    Clone(CloneTool),
}

/// Which tool performs the shallow clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneTool {
    Gh,
    Git,
}

impl CloneTool {
    /// The executable name, as looked up on PATH.
    fn name(self) -> &'static str {
        match self {
            CloneTool::Gh => "gh",
            CloneTool::Git => "git",
        }
    }
}

/// Errors from fetching a template. Every message says what to do next.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("could not run `{tool}`: {source}; check that {tool} is installed and on PATH")]
    Spawn {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "could not fetch {url}{}; fix the reported problem (usually network connectivity or repository access), then re-run",
        render_stderr(stderr)
    )]
    FetchFailed { url: String, stderr: String },

    #[error(
        "{} already exists (the {component} template may already be fetched); move or delete it, then re-run `hpds use {component}`",
        path.display()
    )]
    DestExists {
        path: PathBuf,
        component: &'static str,
    },

    #[error(
        "fetched the template but could not remove {}: {source}; delete that directory by hand",
        path.display()
    )]
    StripGit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn render_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

/// Reduce a tool's stderr dump to its one meaningful line: strip ANSI
/// escape codes, prefer the first line that mentions an error, and fall
/// back to the first non-empty line. Quarto failures arrive as multi-line
/// Deno stack traces with color codes; only the error line helps the user.
fn first_error_line(stderr: &str) -> String {
    let cleaned = strip_ansi(stderr);
    let mut lines = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    std::iter::once(first)
        .chain(lines)
        .find(|line| line.to_lowercase().contains("error"))
        .unwrap_or(first)
        .to_string()
}

/// Remove ANSI escape sequences (CSI like `\x1b[31m`, OSC like terminal
/// hyperlinks, and lone two-byte escapes), keeping the plain text.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte in @..~.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: runs to a BEL or an ESC-backslash string terminator.
            Some(']') => {
                while let Some(next) = chars.next() {
                    match next {
                        '\x07' => break,
                        '\x1b' => {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                        _ => {}
                    }
                }
            }
            // Any other two-byte escape (or a trailing lone ESC): dropped.
            _ => {}
        }
    }
    out
}

/// Fetch `template` into a fresh subdirectory of the project at `ctx.dest`,
/// using the tools found on `path_var` (the run functions pass the live
/// `PATH`; tests pass a fake one).
fn fetch(
    template: FetchedTemplate,
    ctx: &ComponentCtx,
    path_var: Option<&OsStr>,
) -> anyhow::Result<Vec<FileOutcome>> {
    let component = template.component_name();
    super::reject_kind(ctx, component)?;
    super::reject_workflows(ctx, component)?;
    let strategy = choose_strategy(template, path_var);
    let rel_dest = PathBuf::from(template.repo_name());
    let dest = ctx.dest.join(&rel_dest);
    ensure_dest_absent(&dest, component)?;
    match strategy {
        Strategy::QuartoTemplate => {
            let quarto = resolve_program("quarto", path_var);
            fetch_via_quarto(&quarto, template, &dest)?;
        }
        Strategy::Clone(tool) => {
            let program = resolve_program(tool.name(), path_var);
            clone_and_strip(tool, &program, template.slug(), &template.url(), &dest)?;
        }
    }
    // Guidance, not a file outcome: hand it to the command layer, which
    // prints it after the `created ...` report it refers to. The relative
    // path keeps the advice short; `hpds use` runs from the project root.
    ctx.guidance
        .borrow_mut()
        .extend(template.next_steps(&rel_dest, strategy));
    Ok(vec![FileOutcome {
        path: rel_dest,
        outcome: WriteOutcome::Created,
    }])
}

/// Refuse to fetch over an existing destination directory.
fn ensure_dest_absent(dest: &Path, component: &'static str) -> Result<(), FetchError> {
    if dest.exists() {
        return Err(FetchError::DestExists {
            path: dest.to_path_buf(),
            component,
        });
    }
    Ok(())
}

/// Pick the fetch strategy from the template kind and which tools are on
/// `path_var` (the caller passes the live `PATH`; tests pass a fake one).
fn choose_strategy(template: FetchedTemplate, path_var: Option<&OsStr>) -> Strategy {
    if template.is_quarto_template() && tool_on_path("quarto", path_var) {
        Strategy::QuartoTemplate
    } else if tool_on_path("gh", path_var) {
        Strategy::Clone(CloneTool::Gh)
    } else {
        Strategy::Clone(CloneTool::Git)
    }
}

/// Whether an executable named `name` exists in any directory of `path_var`.
fn tool_on_path(name: &str, path_var: Option<&OsStr>) -> bool {
    find_tool(name, path_var).is_some()
}

/// Find the executable named `name` in the directories of `path_var`,
/// returning its full path. Spawning the resolved path keeps detection and
/// execution in agreement: on Windows, `Command::new("quarto")` resolves
/// only `quarto.exe`, but a `quarto.cmd` on PATH is a real installation and
/// can be spawned by its full path.
fn find_tool(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let paths = path_var?;
    std::env::split_paths(paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            candidate_names(name)
                .into_iter()
                .map(|candidate| dir.join(candidate))
                .find(|path| is_executable(path))
        })
}

/// The program to spawn for `name`: the resolved PATH entry when found,
/// otherwise the bare name (letting the OS report a clear spawn error).
fn resolve_program(name: &str, path_var: Option<&OsStr>) -> PathBuf {
    find_tool(name, path_var).unwrap_or_else(|| PathBuf::from(name))
}

#[cfg(unix)]
fn candidate_names(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

#[cfg(windows)]
fn candidate_names(name: &str) -> Vec<String> {
    // Windows resolves executables via extensions; cover the common ones.
    ["exe", "cmd", "bat"]
        .iter()
        .map(|ext| format!("{name}.{ext}"))
        .collect()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Create `dest` and run `quarto use template` inside it (quarto refuses to
/// install into a non-empty directory, so it gets a fresh one, mirroring
/// where the clone strategy lands). If quarto fails, `dest`, including any
/// partial output, is removed again: a stale directory would make every
/// retry fail with `DestExists` instead of repeating the actionable fetch
/// error. git/gh clean up their own failed clones, so only this path needs
/// explicit cleanup.
fn fetch_via_quarto(quarto: &Path, template: FetchedTemplate, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest).with_context(|| {
        format!(
            "could not create {}; check that you can write to the project directory",
            dest.display()
        )
    })?;
    if let Err(err) = quarto_use_template(quarto, template, dest) {
        // Best-effort: the fetch error is the actionable one. If cleanup
        // fails too, say so, or the next run's "already exists" error would
        // be baffling.
        if remove_dir_all_including_readonly(dest).is_err() {
            ui::warn(&format!(
                "could not clean up {}; delete it before re-running `hpds use {}`",
                dest.display(),
                template.component_name()
            ));
        }
        return Err(err.into());
    }
    Ok(())
}

/// Run `<quarto> use template <slug> --no-prompt` inside `dir` (which must
/// be empty: quarto refuses to install a template into a non-empty
/// directory).
fn quarto_use_template(
    quarto: &Path,
    template: FetchedTemplate,
    dir: &Path,
) -> Result<(), FetchError> {
    let output = Command::new(quarto)
        .args(["use", "template", template.slug(), "--no-prompt"])
        .current_dir(dir)
        .output()
        .map_err(|source| FetchError::Spawn {
            tool: "quarto",
            source,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        // Quarto fails with a whole ANSI-colored stack trace on stderr;
        // keep only the line that says what went wrong.
        Err(FetchError::FetchFailed {
            url: template.url(),
            stderr: first_error_line(&String::from_utf8_lossy(&output.stderr)),
        })
    }
}

/// Shallow-clone `git_url` (or the `slug` via `gh`) into `dest` using the
/// resolved `program`, then strip the clone's `.git` directory.
fn clone_and_strip(
    tool: CloneTool,
    program: &Path,
    slug: &str,
    git_url: &str,
    dest: &Path,
) -> Result<(), FetchError> {
    let spawn_error = |source| FetchError::Spawn {
        tool: tool.name(),
        source,
    };
    let output = match tool {
        CloneTool::Gh => Command::new(program)
            .args(["repo", "clone", slug])
            .arg(dest)
            .args(["--", "--depth", "1"])
            .output()
            .map_err(spawn_error)?,
        CloneTool::Git => Command::new(program)
            .args(["clone", "--depth", "1", git_url])
            .arg(dest)
            .output()
            .map_err(spawn_error)?,
    };
    if !output.status.success() {
        return Err(FetchError::FetchFailed {
            url: git_url.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    strip_git(dest)
}

/// Remove the `.git` directory from a fresh clone so the fetched files can
/// join the user's own repository.
fn strip_git(dest: &Path) -> Result<(), FetchError> {
    let git_dir = dest.join(".git");
    if git_dir.is_dir() {
        remove_dir_all_including_readonly(&git_dir).map_err(|source| FetchError::StripGit {
            path: git_dir.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Like `std::fs::remove_dir_all`, but tolerates read-only files.
///
/// Git marks fresh-clone pack files read-only, and on Windows
/// `remove_dir_all` fails with `PermissionDenied` on read-only files (on
/// unix, deletion only needs write permission on the parent directory, so
/// no attribute clearing is required there).
fn remove_dir_all_including_readonly(dir: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    clear_readonly_recursively(dir)?;
    std::fs::remove_dir_all(dir)
}

/// Clear the read-only attribute on `path` and everything under it.
#[cfg(windows)]
fn clear_readonly_recursively(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut perms = metadata.permissions();
    if perms.readonly() {
        // Windows-only: this clears FILE_ATTRIBUTE_READONLY, not unix mode
        // bits, so the clippy concern about world-writable files is moot.
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            clear_readonly_recursively(&entry?.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A temp dir posing as a PATH entry, holding fake executables.
    struct ShimPath {
        _tmp: tempfile::TempDir,
        path_var: std::ffi::OsString,
    }

    /// Build a PATH containing ONLY a shim directory with the named fake
    /// tools, so lookups are hermetic (the real PATH is never consulted).
    fn shim_path(tools: &[&str]) -> ShimPath {
        let tmp = tempfile::tempdir().expect("create tempdir");
        for tool in tools {
            let file = tmp.path().join(shim_file_name(tool));
            fs::write(&file, "#!/bin/sh\nexit 0\n").expect("write shim");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod shim");
            }
        }
        let path_var = std::env::join_paths([tmp.path()]).expect("join PATH");
        ShimPath {
            _tmp: tmp,
            path_var,
        }
    }

    #[cfg(unix)]
    fn shim_file_name(tool: &str) -> String {
        tool.to_string()
    }

    #[cfg(windows)]
    fn shim_file_name(tool: &str) -> String {
        format!("{tool}.exe")
    }

    #[test]
    fn slides_and_poster_prefer_quarto_when_on_path() {
        let shims = shim_path(&["quarto", "gh", "git"]);
        for template in [FetchedTemplate::Slides, FetchedTemplate::Poster] {
            assert_eq!(
                choose_strategy(template, Some(&shims.path_var)),
                Strategy::QuartoTemplate,
                "{template:?} should use `quarto use template` when quarto is present"
            );
        }
    }

    #[test]
    fn thesis_always_clones_even_with_quarto_on_path() {
        let shims = shim_path(&["quarto", "gh", "git"]);
        assert_eq!(
            choose_strategy(FetchedTemplate::Thesis, Some(&shims.path_var)),
            Strategy::Clone(CloneTool::Gh),
        );
    }

    #[test]
    fn slides_fall_back_to_gh_clone_without_quarto() {
        let shims = shim_path(&["gh", "git"]);
        assert_eq!(
            choose_strategy(FetchedTemplate::Slides, Some(&shims.path_var)),
            Strategy::Clone(CloneTool::Gh),
        );
    }

    #[test]
    fn clone_falls_back_to_git_when_gh_is_absent() {
        let shims = shim_path(&["git"]);
        assert_eq!(
            choose_strategy(FetchedTemplate::Poster, Some(&shims.path_var)),
            Strategy::Clone(CloneTool::Git),
        );
        assert_eq!(
            choose_strategy(FetchedTemplate::Thesis, Some(&shims.path_var)),
            Strategy::Clone(CloneTool::Git),
        );
    }

    #[test]
    fn missing_path_means_no_tools_found() {
        assert_eq!(
            choose_strategy(FetchedTemplate::Slides, None),
            Strategy::Clone(CloneTool::Git),
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_on_path_does_not_count() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        fs::write(tmp.path().join("quarto"), "not a program").expect("write file");
        let path_var = std::env::join_paths([tmp.path()]).expect("join PATH");
        assert!(!tool_on_path("quarto", Some(path_var.as_os_str())));
    }

    /// Detection and spawning must agree: `find_tool` returns the resolved
    /// path so we spawn exactly the file we detected (on Windows, a
    /// `quarto.cmd` found on PATH would not be resolved by `Command::new`,
    /// which only finds `.exe`).
    #[test]
    fn find_tool_returns_the_full_path_to_the_detected_executable() {
        let shims = shim_path(&["quarto"]);
        let found = find_tool("quarto", Some(&shims.path_var)).expect("quarto shim is found");
        assert!(
            found.is_absolute(),
            "resolved to a full path: {}",
            found.display()
        );
        assert_eq!(
            found.file_name().and_then(OsStr::to_str),
            Some(shim_file_name("quarto").as_str())
        );
    }

    /// Pin the exact quarto invocation: `use template <slug> --no-prompt`,
    /// run inside the destination directory.
    #[cfg(unix)]
    #[test]
    fn quarto_is_invoked_with_use_template_no_prompt_inside_the_dest() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let capture = tmp.path().join("capture.txt");
        let shim = tmp.path().join("quarto");
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\n{{ pwd; printf '%s\\n' \"$@\"; }} > '{}'\n",
                capture.display()
            ),
        )
        .expect("write shim");
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod shim");
        let dest = tmp.path().join("hpds-slides-theme");
        fs::create_dir(&dest).expect("create dest");

        quarto_use_template(&shim, FetchedTemplate::Slides, &dest).expect("shim exits 0");

        let recorded = fs::read_to_string(&capture).expect("shim recorded its invocation");
        let mut lines = recorded.lines();
        let cwd = PathBuf::from(lines.next().expect("recorded cwd"));
        assert_eq!(
            cwd.canonicalize().expect("canonicalize recorded cwd"),
            dest.canonicalize().expect("canonicalize dest"),
            "quarto runs inside the destination directory"
        );
        assert_eq!(
            lines.collect::<Vec<_>>(),
            [
                "use",
                "template",
                "StanfordHPDS/hpds-slides-theme",
                "--no-prompt"
            ],
        );
    }

    /// Regression: a failed `quarto use template` must not leave the freshly
    /// created destination (or quarto's partial output) behind: a stale
    /// directory would make every retry fail with `DestExists` ("may already
    /// be fetched", which is untrue) instead of repeating the actionable
    /// fetch error with the URL and connectivity hint.
    #[cfg(unix)]
    #[test]
    fn failed_quarto_fetch_removes_the_destination_so_retries_are_not_poisoned() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let shim = tmp.path().join("quarto");
        // Simulate quarto dying mid-fetch: partial output, then failure.
        fs::write(
            &shim,
            "#!/bin/sh\necho partial > _partial.qmd\necho 'no route to host' >&2\nexit 1\n",
        )
        .expect("write shim");
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod shim");
        let dest = tmp.path().join("hpds-slides-theme");

        let err =
            fetch_via_quarto(&shim, FetchedTemplate::Slides, &dest).expect_err("shim exits 1");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("https://github.com/StanfordHPDS/hpds-slides-theme"),
            "names the repo URL: {msg}"
        );
        assert!(
            !dest.exists(),
            "failed fetch left {} behind",
            dest.display()
        );
        ensure_dest_absent(&dest, "slides").expect("a retry passes the pre-check");
    }

    /// Regression: an offline `quarto use template` dies with a multi-line
    /// Deno stack trace full of raw ANSI codes. The hpds error must keep
    /// only the first meaningful error line (ANSI stripped) alongside the
    /// repo URL and the connectivity hint, not the whole dump.
    #[cfg(unix)]
    #[test]
    fn failed_quarto_fetch_truncates_stderr_to_the_first_error_line_without_ansi() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let shim = tmp.path().join("quarto");
        fs::write(
            &shim,
            concat!(
                "#!/bin/sh\n",
                "printf '\\033[2K\\r[1/2] downloading template\\n' >&2\n",
                "printf '\\033[31mERROR: error sending request for url ",
                "(https://api.github.com/repos/StanfordHPDS/hpds-slides-theme/tarball): ",
                "dns error\\033[0m\\n' >&2\n",
                "printf 'Stack trace:\\n' >&2\n",
                "printf '    at async fetch (ext:deno_fetch/26_fetch.js:170:7)\\n' >&2\n",
                "printf '    at async mainFetch (ext:deno_fetch/26_fetch.js:181:5)\\n' >&2\n",
                "exit 1\n"
            ),
        )
        .expect("write shim");
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod shim");
        let dest = tmp.path().join("hpds-slides-theme");

        let err =
            fetch_via_quarto(&shim, FetchedTemplate::Slides, &dest).expect_err("shim exits 1");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("ERROR: error sending request"),
            "keeps the meaningful error line: {msg}"
        );
        assert!(!msg.contains('\x1b'), "ANSI codes are stripped: {msg}");
        assert!(
            !msg.contains("Stack trace") && !msg.contains("at async fetch"),
            "the stack trace is dropped: {msg}"
        );
        assert!(
            msg.contains("https://github.com/StanfordHPDS/hpds-slides-theme"),
            "names the repo URL: {msg}"
        );
        assert!(
            msg.contains("network connectivity"),
            "keeps the connectivity hint: {msg}"
        );
    }

    #[test]
    fn first_error_line_prefers_the_line_mentioning_an_error() {
        let stderr = "[1/2] downloading\nERROR: dns error\nStack trace:\n  at foo\n";
        assert_eq!(first_error_line(stderr), "ERROR: dns error");
    }

    #[test]
    fn first_error_line_falls_back_to_the_first_non_empty_line() {
        assert_eq!(
            first_error_line("\n\n  fatal problem  \nmore\n"),
            "fatal problem"
        );
    }

    #[test]
    fn first_error_line_of_nothing_is_empty() {
        assert_eq!(first_error_line(""), "");
        assert_eq!(first_error_line("\x1b[31m\x1b[0m \n"), "");
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("\x1b[2K\rline"), "\rline");
        assert_eq!(
            strip_ansi("\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(strip_ansi("no escapes"), "no escapes");
    }

    /// Pin the exact gh invocation: `repo clone <slug> <dest> -- --depth 1`
    /// (everything after `--` is passed through to git, so the placement
    /// matters).
    #[cfg(unix)]
    #[test]
    fn gh_is_invoked_with_repo_clone_slug_dest_and_shallow_passthrough() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let capture = tmp.path().join("capture.txt");
        let shim = tmp.path().join("gh");
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                capture.display()
            ),
        )
        .expect("write shim");
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod shim");
        let dest = tmp.path().join("hpds-poster");

        clone_and_strip(
            CloneTool::Gh,
            &shim,
            "StanfordHPDS/hpds-poster",
            "https://github.com/StanfordHPDS/hpds-poster",
            &dest,
        )
        .expect("shim exits 0");

        let recorded = fs::read_to_string(&capture).expect("shim recorded its invocation");
        assert_eq!(
            recorded.lines().collect::<Vec<_>>(),
            [
                "repo",
                "clone",
                "StanfordHPDS/hpds-poster",
                dest.to_str().expect("utf-8 temp path"),
                "--",
                "--depth",
                "1"
            ],
        );
    }

    /// Create a local git repository with one committed file, for clone
    /// tests that never touch the network.
    fn fixture_repo(dir: &Path) -> PathBuf {
        let repo = dir.join("fixture-repo");
        fs::create_dir(&repo).expect("create fixture repo dir");
        fs::write(repo.join("template.qmd"), "---\ntitle: demo\n---\n").expect("write file");
        for args in [
            vec!["init", "--initial-branch", "main"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "seed",
            ],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", dir.join("nonexistent-gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("run git");
            assert!(
                status.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
        repo
    }

    #[test]
    fn clone_and_strip_removes_the_git_directory_but_keeps_files() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo = fixture_repo(tmp.path());
        let dest = tmp.path().join("cloned");

        clone_and_strip(
            CloneTool::Git,
            Path::new("git"),
            "fake-org/fixture-repo",
            repo.to_str().expect("utf-8 temp path"),
            &dest,
        )
        .expect("clone succeeds");

        assert!(dest.join("template.qmd").is_file(), "clone kept the files");
        assert!(!dest.join(".git").exists(), ".git was stripped");
    }

    #[test]
    fn clone_failure_names_the_url_and_says_what_to_do() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let missing = tmp.path().join("no-such-repo");
        let dest = tmp.path().join("cloned");

        let err = clone_and_strip(
            CloneTool::Git,
            Path::new("git"),
            "fake-org/no-such-repo",
            missing.to_str().expect("utf-8 temp path"),
            &dest,
        )
        .expect_err("clone of a missing source fails");

        let msg = err.to_string();
        assert!(
            msg.contains(missing.to_str().unwrap()),
            "names the repo URL: {msg}"
        );
        assert!(msg.contains("re-run"), "says what to do next: {msg}");
        // The hint must not blame the network unconditionally: the real
        // cause (captured stderr) may be something else entirely.
        assert!(
            !msg.contains("check your network connection"),
            "does not hard-claim a network problem: {msg}"
        );
    }

    #[test]
    fn existing_destination_errors_before_any_fetch_and_says_what_to_do() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dest = tmp.path().join("hpds-poster");
        fs::create_dir(&dest).expect("create pre-existing dest");

        let err = ensure_dest_absent(&dest, "poster").expect_err("existing dest is refused");

        let msg = err.to_string();
        assert!(
            msg.contains(dest.to_str().unwrap()),
            "names the directory: {msg}"
        );
        assert!(msg.contains("hpds use poster"), "says how to re-run: {msg}");
    }

    #[test]
    fn absent_destination_passes_the_pre_check() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        ensure_dest_absent(&tmp.path().join("hpds-poster"), "poster").expect("absent dest is fine");
    }

    /// `quarto use template` renames the template's main `.qmd` after the
    /// target directory, so the printed next step must use that name.
    #[test]
    fn next_steps_name_the_directory_named_qmd_for_the_quarto_strategy() {
        let dest = Path::new("proj").join("hpds-slides-theme");
        let steps = FetchedTemplate::Slides.next_steps(&dest, Strategy::QuartoTemplate);
        assert!(
            steps[0].contains("hpds-slides-theme.qmd"),
            "names the renamed main file: {steps:?}"
        );
        assert!(
            !steps[0].contains("template.qmd"),
            "does not name a file quarto renamed away: {steps:?}"
        );
    }

    /// A plain clone keeps the repository's `template.qmd` untouched.
    #[test]
    fn next_steps_name_template_qmd_for_the_clone_strategy() {
        let dest = Path::new("proj").join("hpds-poster");
        let steps = FetchedTemplate::Poster.next_steps(&dest, Strategy::Clone(CloneTool::Git));
        assert!(
            steps[0].contains("template.qmd"),
            "names the cloned main file: {steps:?}"
        );
    }

    #[test]
    fn strip_git_is_a_no_op_when_there_is_no_git_directory() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        strip_git(tmp.path()).expect("nothing to strip");
    }

    /// Regression: git marks fresh-clone pack files read-only, and on Windows
    /// `std::fs::remove_dir_all` fails with `PermissionDenied` on read-only
    /// files. `strip_git` must clear the attribute first.
    #[test]
    fn strip_git_removes_read_only_files_like_gits_pack_files() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let pack_dir = tmp.path().join(".git").join("objects").join("pack");
        fs::create_dir_all(&pack_dir).expect("create pack dir");
        let pack = pack_dir.join("pack-0000.pack");
        fs::write(&pack, "pack data").expect("write pack file");
        let mut perms = fs::metadata(&pack).expect("stat pack file").permissions();
        perms.set_readonly(true);
        fs::set_permissions(&pack, perms).expect("mark pack file read-only");

        strip_git(tmp.path()).expect("read-only files inside .git are removed");

        assert!(!tmp.path().join(".git").exists(), ".git was stripped");
    }

    #[test]
    fn kind_flag_is_rejected_before_anything_is_fetched() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut ctx = super::super::test_ctx(tmp.path(), "r");
        ctx.kind = Some("fancy");
        let err = (SLIDES.run)(&ctx).expect_err("--kind has no meaning here");
        let msg = err.to_string();
        assert!(msg.contains("--kind"), "names the bad flag: {msg}");
        assert!(
            !tmp.path().join("hpds-slides-theme").exists(),
            "nothing was fetched"
        );
    }

    #[test]
    fn workflows_flag_is_rejected_before_anything_is_fetched() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let workflows = vec!["lint".to_string()];
        let mut ctx = super::super::test_ctx(tmp.path(), "r");
        ctx.workflows = Some(&workflows);
        let err = (THESIS.run)(&ctx).expect_err("--workflows has no meaning here");
        let msg = err.to_string();
        assert!(msg.contains("--workflows"), "names the bad flag: {msg}");
        assert!(
            !tmp.path().join("typst-stanford-thesis").exists(),
            "nothing was fetched"
        );
    }

    /// A successful fetch reports one `Created` outcome for the new
    /// subdirectory, ready for the command layer to render.
    #[cfg(unix)]
    #[test]
    fn successful_fetch_returns_a_created_outcome_for_the_subdirectory() {
        let shims = shim_path(&["quarto"]);
        let tmp = tempfile::tempdir().expect("create tempdir");
        let ctx = super::super::test_ctx(tmp.path(), "r");

        let outcomes =
            fetch(FetchedTemplate::Slides, &ctx, Some(&shims.path_var)).expect("shim exits 0");

        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        assert_eq!(outcomes[0].path, Path::new("hpds-slides-theme"));
        assert!(matches!(outcomes[0].outcome, WriteOutcome::Created));
        assert!(
            tmp.path().join("hpds-slides-theme").is_dir(),
            "the subdirectory was created for quarto to fill"
        );
        let guidance = ctx.guidance.borrow();
        assert_eq!(guidance.len(), 1, "{guidance:?}");
        assert!(
            guidance[0].starts_with("next:"),
            "the next steps land in the guidance buffer: {guidance:?}"
        );
    }

    #[test]
    fn template_urls_point_at_the_stanford_hpds_org() {
        for template in [
            FetchedTemplate::Slides,
            FetchedTemplate::Poster,
            FetchedTemplate::Thesis,
        ] {
            assert!(
                template
                    .url()
                    .starts_with("https://github.com/StanfordHPDS/"),
                "{template:?}"
            );
        }
    }
}

/// Network tests: shallow-clone each real repository into a temp dir.
/// Run with `cargo test --features online-tests -- --ignored`.
#[cfg(all(test, feature = "online-tests"))]
mod online_tests {
    use super::*;

    fn fetch_real_repo(template: FetchedTemplate, expected_files: &[&str]) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dest = tmp.path().join(template.repo_name());
        clone_and_strip(
            CloneTool::Git,
            Path::new("git"),
            template.slug(),
            &template.url(),
            &dest,
        )
        .expect("clone the real repository");
        for file in expected_files {
            assert!(
                dest.join(file).exists(),
                "{template:?} clone contains `{file}`"
            );
        }
        assert!(!dest.join(".git").exists(), ".git was stripped");
    }

    #[test]
    #[ignore = "hits the network"]
    fn online_fetches_the_slides_repo() {
        fetch_real_repo(FetchedTemplate::Slides, &["template.qmd", "_extensions"]);
    }

    #[test]
    #[ignore = "hits the network"]
    fn online_fetches_the_poster_repo() {
        fetch_real_repo(FetchedTemplate::Poster, &["template.qmd", "_extensions"]);
    }

    #[test]
    #[ignore = "hits the network"]
    fn online_fetches_the_thesis_repo() {
        fetch_real_repo(
            FetchedTemplate::Thesis,
            &["_quarto.yml", "stanford-thesis.typ"],
        );
    }
}
