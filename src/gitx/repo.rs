//! `hpds repo create` — create and push a GitHub repo for the current
//! project, following the lab-manual gh flow.
//!
//! Flow: verify `gh` auth → resolve name/org/visibility (flags, or defaults
//! under `--yes`, or prompts) → `git init` if needed → ensure an initial
//! commit → `gh repo create <org>/<name> --private|--public --source=. --push`.
//!
//! Tests never call the real `gh` (see `tests/repo_create.rs`, which shims it
//! on PATH). Manual/online smoke path, for a human or the smoke agent only:
//!
//! ```text
//! mkdir smoke-hpds-repo && cd smoke-hpds-repo && echo "# smoke" > README.md
//! hpds repo create --org <your-gh-user> --name smoke-hpds-repo --visibility private --yes
//! gh repo view <your-gh-user>/smoke-hpds-repo          # verify it exists
//! gh repo delete <your-gh-user>/smoke-hpds-repo --yes  # clean up
//! ```

use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::Context;

use crate::ui::{self, HintExt};

/// The lab's GitHub organization, used when `--org` is not given.
pub const DEFAULT_ORG: &str = "StanfordHPDS";

/// Repository visibility on GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Visibility {
    Private,
    Public,
}

impl Visibility {
    fn gh_flag(self) -> &'static str {
        match self {
            Self::Private => "--private",
            Self::Public => "--public",
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Private => "private",
            Self::Public => "public",
        })
    }
}

/// Inputs for [`create`], mapped 1:1 from the `hpds repo create` flags.
pub struct CreateOptions {
    pub name: Option<String>,
    pub org: Option<String>,
    pub visibility: Option<Visibility>,
    /// Fully non-interactive: accept every default instead of prompting.
    pub yes: bool,
}

/// Create a GitHub repo for the project in the current directory and push it.
pub fn create(opts: CreateOptions) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    ensure_gh_auth(&cwd)?;

    let name = resolve_with(
        opts.name,
        opts.yes,
        || default_repo_name(&cwd),
        |d| ui::text("Repository name", d),
    )?;
    let org = resolve_with(
        opts.org,
        opts.yes,
        || Ok(DEFAULT_ORG.to_string()),
        |d| ui::text("GitHub organization (or user)", d),
    )?;
    let visibility = match opts.visibility {
        Some(v) => v,
        None if opts.yes => Visibility::Private,
        None => ui::select(
            "Repository visibility",
            vec![Visibility::Private, Visibility::Public],
        )?,
    };

    ensure_git_repo(&cwd)?;
    ensure_initial_commit(&cwd, opts.yes)?;
    gh_repo_create(&cwd, &org, &name, visibility)?;
    ui::success(&format!("created and pushed {org}/{name}"));

    // The design: offer `hpds use gha` afterward. The gha component is not
    // implemented yet (M3), so print a notice instead of prompting; once it
    // ships, prompt here (and skip silently under --yes). The notice itself
    // intentionally prints even under --yes: --yes suppresses prompts, not
    // informational output, and the AC only requires skipping the *prompt*.
    ui::println(
        "note: GitHub Actions setup (`hpds use gha`) is not available yet; \
         run it once it ships to add CI to this repo",
    );
    Ok(())
}

/// Resolve one prompt-or-flag value: an explicit flag always wins; `--yes`
/// takes the default; otherwise ask (which fails with an actionable error
/// when the session is non-interactive).
fn resolve_with(
    flag: Option<String>,
    yes: bool,
    default: impl FnOnce() -> anyhow::Result<String>,
    prompt: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    match flag {
        Some(value) => Ok(value),
        None => {
            let default = default()?;
            if yes { Ok(default) } else { prompt(&default) }
        }
    }
}

/// Default repo name: the current directory's basename.
fn default_repo_name(cwd: &Path) -> anyhow::Result<String> {
    match cwd.file_name().and_then(|n| n.to_str()) {
        Some(name) if !name.is_empty() => Ok(name.to_string()),
        _ => Err(anyhow::anyhow!(
            "could not derive a repository name from the current directory"
        ))
        .hint("pass one explicitly with --name <NAME>"),
    }
}

/// Fail early, before touching the working tree, unless `gh` is installed
/// and authenticated.
fn ensure_gh_auth(cwd: &Path) -> anyhow::Result<()> {
    let out = gh(cwd, &["auth", "status"])?;
    if out.status.success() {
        return Ok(());
    }
    Err(command_failure("gh auth status", &out))
        .context("not logged in to GitHub")
        .hint("run `gh auth login` to authenticate, then re-run `hpds repo create`")
}

fn ensure_git_repo(cwd: &Path) -> anyhow::Result<()> {
    match repo_position(cwd)? {
        RepoPosition::Toplevel => return Ok(()),
        RepoPosition::NestedInside(parent) => {
            // A user who merely cd'd one level too deep would otherwise
            // publish a partial repo named after the subdirectory without
            // realizing it; make the situation visible before proceeding.
            ui::warn(&format!(
                "the current directory is inside an existing git repository \
                 ({}); creating a separate new repository for this directory \
                 only — press Ctrl-C and `cd` to the repository root if you \
                 meant to publish that project instead",
                parent.display()
            ));
        }
        RepoPosition::Outside => {}
    }
    let init = git(cwd, &["init"])?;
    if !init.status.success() {
        return Err(command_failure("git init", &init))
            .hint("fix the git error above, then re-run `hpds repo create`");
    }
    ui::success("initialized a git repository");
    Ok(())
}

/// Where `cwd` sits relative to any existing git repository.
#[derive(Debug, PartialEq, Eq)]
enum RepoPosition {
    /// `cwd` is the toplevel of a repository: reuse it as-is.
    Toplevel,
    /// `cwd` is nested inside a repository rooted at the given path. A mere
    /// `git rev-parse --git-dir` check would call this "already a repo",
    /// skip `git init`, and push the PARENT's entire history — potentially
    /// unrelated, private content — to the new GitHub repo. The design says
    /// init runs "if needed", and it is needed here (with a warning).
    NestedInside(std::path::PathBuf),
    /// `cwd` is not inside any repository.
    Outside,
}

fn repo_position(cwd: &Path) -> anyhow::Result<RepoPosition> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"])?;
    if !out.status.success() {
        // Not inside any repo (or a bare repo, where init below is a no-op).
        return Ok(RepoPosition::Outside);
    }
    let toplevel = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // Canonicalize both sides: git prints a resolved path, while `cwd` may
    // reach the same place through symlinks (e.g. /tmp on macOS). If
    // canonicalization fails, fall back to comparing the raw paths.
    let same = match (toplevel.canonicalize(), cwd.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => toplevel == cwd,
    };
    if same {
        Ok(RepoPosition::Toplevel)
    } else {
        Ok(RepoPosition::NestedInside(toplevel))
    }
}

/// `gh repo create --push` needs at least one commit; create one when the
/// repo has none, prompting first unless `--yes`.
fn ensure_initial_commit(cwd: &Path, yes: bool) -> anyhow::Result<()> {
    if git(cwd, &["rev-parse", "--verify", "HEAD"])?
        .status
        .success()
    {
        return Ok(());
    }
    if !yes
        && !ui::confirm(
            "No commits yet — create an initial commit from the current files?",
            true,
        )?
    {
        return Err(anyhow::anyhow!(
            "the repository has no commits, so there is nothing to push"
        ))
        .hint("commit your files (`git add` + `git commit`), then re-run `hpds repo create`");
    }

    let add = git(cwd, &["add", "--all"])?;
    if !add.status.success() {
        return Err(command_failure("git add", &add))
            .hint("fix the git error above, then re-run `hpds repo create`");
    }
    // Never commit anything under .beads/ — the issue database is local
    // state, not project content (and we add no un-ignore rules for it).
    // Un-stage rather than an `:(exclude)` pathspec on `git add`: the
    // exclude form makes git exit 1 with an "ignored paths" complaint.
    // `--ignore-unmatch` keeps this a no-op when there is no .beads/.
    let unstage = git(
        cwd,
        &[
            "rm",
            "-r",
            "--cached",
            "--ignore-unmatch",
            "-q",
            "--",
            ".beads",
        ],
    )?;
    if !unstage.status.success() {
        return Err(command_failure("git rm --cached .beads", &unstage))
            .hint("fix the git error above, then re-run `hpds repo create`");
    }
    // --allow-empty: a bare directory (everything ignored, or no files yet)
    // still gets a pushable first commit.
    let commit = git(cwd, &["commit", "--allow-empty", "-m", "Initial commit"])?;
    if !commit.status.success() {
        return Err(command_failure("git commit", &commit)).hint(
            "if git complained about your identity, set it with \
             `git config --global user.name`/`user.email`, then re-run",
        );
    }
    ui::success("created an initial commit");
    Ok(())
}

fn gh_repo_create(cwd: &Path, org: &str, name: &str, visibility: Visibility) -> anyhow::Result<()> {
    let target = format!("{org}/{name}");
    let args = gh_create_args(&target, visibility);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gh(cwd, &arg_refs)?;
    if !out.status.success() {
        return Err(command_failure(&format!("gh repo create {target}"), &out)).hint(
            "check the gh message above — the repo may already exist, or you may \
             not have permission to create repos in that organization",
        );
    }
    // gh prints the new repo URL; pass it on.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let url = stdout.trim();
    if !url.is_empty() {
        ui::println(url);
    }
    Ok(())
}

/// Argv for `gh repo create` (after the `gh` program itself).
fn gh_create_args(target: &str, visibility: Visibility) -> Vec<String> {
    [
        "repo",
        "create",
        target,
        visibility.gh_flag(),
        "--source=.",
        "--push",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Run `gh` with `args`, capturing output. A missing binary is a distinct,
/// actionable error.
fn gh(cwd: &Path, args: &[&str]) -> anyhow::Result<Output> {
    match Command::new("gh").args(args).current_dir(cwd).output() {
        Ok(out) => Ok(out),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "the GitHub CLI (`gh`) is not installed or not on PATH"
        ))
        .hint("install it from https://cli.github.com/, then run `gh auth login`"),
        Err(err) => Err(err).context(format!("could not run `gh {}`", args.join(" "))),
    }
}

/// Run `git` with `args`, capturing output. A missing binary is a distinct,
/// actionable error.
fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<Output> {
    match Command::new("git").args(args).current_dir(cwd).output() {
        Ok(out) => Ok(out),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Err(anyhow::anyhow!("`git` is not installed or not on PATH"))
                .hint("install git from https://git-scm.com/, then re-run `hpds repo create`")
        }
        Err(err) => Err(err).context(format!("could not run `git {}`", args.join(" "))),
    }
}

/// Describe a failed subprocess, folding in its stderr when it said anything.
fn command_failure(what: &str, out: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        anyhow::anyhow!("`{what}` failed ({})", out.status)
    } else {
        anyhow::anyhow!("`{what}` failed ({}): {detail}", out.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render_error;

    #[test]
    fn flag_value_wins_without_consulting_default_or_prompt() {
        let got = resolve_with(
            Some("from-flag".into()),
            false,
            || panic!("default should not be computed"),
            |_| panic!("prompt should not run"),
        )
        .unwrap();
        assert_eq!(got, "from-flag");
    }

    #[test]
    fn yes_takes_the_default_without_prompting() {
        let got = resolve_with(
            None,
            true,
            || Ok("the-default".into()),
            |_| panic!("prompt should not run under --yes"),
        )
        .unwrap();
        assert_eq!(got, "the-default");
    }

    #[test]
    fn without_flag_or_yes_the_prompt_is_asked_with_the_default() {
        let got = resolve_with(
            None,
            false,
            || Ok("the-default".into()),
            |default| Ok(format!("answered:{default}")),
        )
        .unwrap();
        assert_eq!(got, "answered:the-default");
    }

    #[test]
    fn gh_create_args_private() {
        assert_eq!(
            gh_create_args("StanfordHPDS/myproj", Visibility::Private),
            [
                "repo",
                "create",
                "StanfordHPDS/myproj",
                "--private",
                "--source=.",
                "--push"
            ]
        );
    }

    #[test]
    fn gh_create_args_public() {
        assert_eq!(
            gh_create_args("malco/cool-study", Visibility::Public),
            [
                "repo",
                "create",
                "malco/cool-study",
                "--public",
                "--source=.",
                "--push"
            ]
        );
    }

    #[test]
    fn default_repo_name_is_the_directory_basename() {
        let name = default_repo_name(Path::new("/home/user/projects/my-study")).unwrap();
        assert_eq!(name, "my-study");
    }

    #[test]
    fn default_repo_name_errors_actionably_at_a_root_path() {
        // A filesystem root has no basename; the error must point at --name.
        let err = default_repo_name(Path::new("/")).unwrap_err();
        let out = render_error(&err, false);
        assert!(out.contains("--name"), "out was: {out}");
        assert!(out.contains("hint:"), "out was: {out}");
    }

    #[test]
    fn visibility_displays_as_lowercase_words() {
        assert_eq!(Visibility::Private.to_string(), "private");
        assert_eq!(Visibility::Public.to_string(), "public");
    }

    /// Real `git init`-based checks for [`repo_position`]; these need no
    /// commits or user config, only `git` on PATH (a hard requirement of the
    /// feature anyway).
    #[test]
    fn repo_position_distinguishes_toplevel_nested_and_outside() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).expect("create dirs");

        // Outside any repo.
        assert_eq!(repo_position(&outer).unwrap(), RepoPosition::Outside);

        let init = git(&outer, &["init"]).expect("run git init");
        assert!(init.status.success(), "git init failed");

        // The repo root is a toplevel; a plain subdirectory of it is nested
        // (this is the case that must trigger `git init` + a warning, not
        // silently reuse the parent repo).
        assert_eq!(repo_position(&outer).unwrap(), RepoPosition::Toplevel);
        match repo_position(&inner).unwrap() {
            RepoPosition::NestedInside(parent) => {
                assert_eq!(
                    parent.canonicalize().unwrap(),
                    outer.canonicalize().unwrap(),
                    "the reported parent must be the enclosing repo root"
                );
            }
            other => panic!("expected NestedInside, got {other:?}"),
        }
    }

    #[test]
    fn command_failure_includes_stderr_detail() {
        let out = Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: b"fatal: something broke\n".to_vec(),
        };
        let msg = command_failure("git add", &out).to_string();
        assert!(msg.contains("git add"), "msg was: {msg}");
        assert!(msg.contains("fatal: something broke"), "msg was: {msg}");
    }

    #[test]
    fn command_failure_without_stderr_still_names_the_command() {
        let out = Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let msg = command_failure("gh repo create x/y", &out).to_string();
        assert!(msg.contains("gh repo create x/y"), "msg was: {msg}");
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }
}
