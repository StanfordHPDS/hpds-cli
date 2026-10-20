//! `hpds git`: git helpers: defaults + ignore vaccination.

use clap::{Args, Subcommand};

use crate::gitx;
use crate::ui::{self, HintExt};

#[derive(Debug, Args)]
pub struct GitArgs {
    #[command(subcommand)]
    pub command: GitCommand,
}

#[derive(Debug, Subcommand)]
pub enum GitCommand {
    /// Configure sensible git defaults and gh auth guidance
    ///
    /// Sets init.defaultBranch to main (unless you already chose one),
    /// ensures your git user.name and user.email are set (prompting, or from
    /// --name/--email), reports your gh authentication state, and offers to
    /// vaccinate the global git ignore.
    Setup(SetupArgs),
    /// Add R/Python/editor junk patterns to the global git ignore
    ///
    /// Appends the lab's ignore patterns (R/Python/editor droppings) to your
    /// global git ignore so they are never committed to any repo. Pass
    /// --project to write to this repo's .gitignore instead.
    Vaccinate(VaccinateArgs),
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Value for git user.name (skips the prompt; for scripted use)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Value for git user.email (skips the prompt; for scripted use)
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,

    /// Never prompt: vaccinate the global ignore without asking (identity
    /// must then come from --name/--email or existing config)
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct VaccinateArgs {
    /// Append to this repo's .gitignore instead of the global git ignore
    #[arg(long)]
    pub project: bool,
}

pub fn run(args: GitArgs) -> anyhow::Result<()> {
    match args.command {
        GitCommand::Setup(args) => setup(args, false),
        GitCommand::Vaccinate(args) => vaccinate(args),
    }
}

/// Run the `hpds git setup` flow with no identity flags, exactly as the
/// machine-setup bundle needs it. Under `yes`, an identity key that is not
/// set is reported as a warning and skipped, so the remaining steps still
/// run.
pub(super) fn run_setup_with_defaults(yes: bool) -> anyhow::Result<()> {
    setup(
        SetupArgs {
            name: None,
            email: None,
            yes,
        },
        true,
    )
}

/// `hpds git setup`: default branch, identity, gh auth guidance, then an
/// offer to vaccinate the global ignore. With `skip_missing_identity`, an
/// identity key that `--yes` leaves unset and no flag supplies is a warning
/// rather than an error.
fn setup(args: SetupArgs, skip_missing_identity: bool) -> anyhow::Result<()> {
    configure_default_branch()?;
    configure_identity(
        "user.name",
        "Your name (for git commits)",
        "--name",
        args.name,
        args.yes,
        skip_missing_identity,
    )?;
    configure_identity(
        "user.email",
        "Your email (for git commits)",
        "--email",
        args.email,
        args.yes,
        skip_missing_identity,
    )?;
    report_gh_auth()?;
    offer_vaccinate(args.yes)
}

/// Set `init.defaultBranch` to `main`, unless the user already chose a
/// value: an existing setting is reported, never clobbered.
fn configure_default_branch() -> anyhow::Result<()> {
    match gitx::global_config_get("init.defaultBranch")? {
        Some(current) if current == "main" => {
            ui::success("init.defaultBranch is already set to main");
        }
        Some(current) => {
            ui::println(&format!(
                "init.defaultBranch is already set to \"{current}\"; leaving it as is \
                 (run `git config --global init.defaultBranch main` to change it)"
            ));
        }
        None => {
            gitx::global_config_set("init.defaultBranch", "main")?;
            ui::success("set init.defaultBranch to main");
        }
    }
    Ok(())
}

/// How to obtain one identity value (user.name / user.email).
#[derive(Debug, PartialEq, Eq)]
enum IdentityAction {
    /// The flag matches what config already holds: report, write nothing.
    AlreadySet(String),
    /// An explicit flag always wins, even over existing config.
    SetFromFlag(String),
    /// Config already has a value and no flag was given: keep it.
    KeepExisting(String),
    /// Unset, no flag, prompting allowed: ask.
    Prompt,
    /// Unset, no flag, and --yes forbids prompting: the flag is required.
    NeedFlag,
}

/// Pure decision for one identity key, factored out so it is unit-testable.
fn identity_action(flag: Option<String>, current: Option<String>, yes: bool) -> IdentityAction {
    match (flag, current) {
        // Compare trimmed: the flag value is trimmed before any write. An
        // empty current value never counts as set: a blank flag must
        // still fall through to the cannot-be-empty error.
        (Some(value), Some(current)) if !current.is_empty() && value.trim() == current => {
            IdentityAction::AlreadySet(current)
        }
        (Some(value), _) => IdentityAction::SetFromFlag(value),
        (None, Some(current)) => IdentityAction::KeepExisting(current),
        (None, None) if yes => IdentityAction::NeedFlag,
        (None, None) => IdentityAction::Prompt,
    }
}

/// Ensure one global identity key is set, prompting only when needed.
fn configure_identity(
    key: &str,
    prompt: &str,
    flag_name: &str,
    flag: Option<String>,
    yes: bool,
    skip_missing: bool,
) -> anyhow::Result<()> {
    // The current value is read even when a flag is given: a flag that
    // matches it is reported as already set, not rewritten.
    let current = gitx::global_config_get(key)?;
    let value = match identity_action(flag, current, yes) {
        IdentityAction::AlreadySet(value) => {
            ui::success(&format!("{key} is already set to \"{value}\""));
            return Ok(());
        }
        IdentityAction::KeepExisting(current) => {
            ui::println(&format!(
                "{key} is already set to \"{current}\"; leaving it as is"
            ));
            return Ok(());
        }
        IdentityAction::SetFromFlag(value) => value,
        IdentityAction::Prompt => ui::text(prompt, "")?,
        IdentityAction::NeedFlag if skip_missing => {
            ui::warn(&format!(
                "{key} is not set; run `git config --global {key} <VALUE>` \
                 or `hpds git setup` to set it"
            ));
            return Ok(());
        }
        IdentityAction::NeedFlag => {
            return Err(anyhow::anyhow!(
                "{key} is not set, and --yes suppresses the prompt for it"
            ))
            .hint(format!(
                "pass {flag_name} <VALUE> to set it without prompting"
            ));
        }
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow::anyhow!("{key} cannot be empty")).hint(format!(
            "re-run and enter a value, or pass {flag_name} <VALUE>"
        ));
    }
    gitx::global_config_set(key, value)?;
    ui::success(&format!("set {key} to {value}"));
    Ok(())
}

/// Report the GitHub CLI's auth state. Guidance only: whatever the state,
/// setup carries on (the user can authenticate later).
fn report_gh_auth() -> anyhow::Result<()> {
    match gitx::gh_auth()? {
        gitx::GhAuth::Authenticated => {
            ui::success("gh is installed and authenticated with GitHub");
        }
        gitx::GhAuth::Unauthenticated(_) => {
            ui::println(
                "gh is installed but not logged in to GitHub; run `gh auth login` to authenticate",
            );
        }
        gitx::GhAuth::NotInstalled => {
            ui::println(
                "the GitHub CLI (gh) is not installed; run `hpds install gh` to install it, \
                 then `gh auth login` to authenticate",
            );
        }
    }
    Ok(())
}

/// Offer to vaccinate the global git ignore; `--yes` runs it without asking.
fn offer_vaccinate(yes: bool) -> anyhow::Result<()> {
    if !yes
        && !ui::confirm(
            "Vaccinate the global git ignore against R/Python/editor junk files?",
            true,
        )?
    {
        ui::println("skipping vaccination; run `hpds git vaccinate` any time to do it later");
        return Ok(());
    }
    let report = gitx::vaccinate_global()?;
    report_vaccination(&report);
    Ok(())
}

fn vaccinate(args: VaccinateArgs) -> anyhow::Result<()> {
    let report = if args.project {
        gitx::vaccinate_project()?
    } else {
        gitx::vaccinate_global()?
    };
    report_vaccination(&report);
    Ok(())
}

/// Render a vaccination result (shared by `git vaccinate`, the offer at
/// the end of `git setup`, and `hpds init`).
pub(crate) fn report_vaccination(report: &gitx::VaccinateReport) {
    if report.set_excludes_file {
        ui::println(&format!(
            "set global git core.excludesFile to {}",
            report.path.display()
        ));
    }
    let path = report.path.display();
    if report.added.is_empty() {
        ui::success(&format!(
            "{path} already has all {} hpds ignore patterns; nothing to add",
            report.already_present.len()
        ));
    } else if report.already_present.is_empty() {
        ui::success(&format!(
            "added {} ignore pattern(s) to {path}",
            report.added.len()
        ));
    } else {
        ui::success(&format!(
            "added {} ignore pattern(s) to {path} ({} already present)",
            report.added.len(),
            report.already_present.len()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render_error;

    #[test]
    fn flag_wins_even_over_existing_config() {
        assert_eq!(
            identity_action(Some("New".into()), Some("Old".into()), false),
            IdentityAction::SetFromFlag("New".into())
        );
    }

    #[test]
    fn flag_equal_to_existing_config_is_already_set() {
        // Re-running setup with the same identity must read as a no-op
        // ("already set"), not as a fresh write ("set").
        assert_eq!(
            identity_action(Some("Same".into()), Some("Same".into()), false),
            IdentityAction::AlreadySet("Same".into())
        );
    }

    #[test]
    fn a_blank_flag_never_matches_an_empty_config_value() {
        // Both trim to nothing, but "already set to nothing" is nonsense:
        // the blank flag must fall through to the cannot-be-empty error.
        assert_eq!(
            identity_action(Some("   ".into()), Some(String::new()), false),
            IdentityAction::SetFromFlag("   ".into())
        );
    }

    #[test]
    fn flag_comparison_ignores_surrounding_whitespace() {
        // The value is trimmed before it is written, so an untrimmed
        // match is still the same identity.
        assert_eq!(
            identity_action(Some("  Same ".into()), Some("Same".into()), false),
            IdentityAction::AlreadySet("Same".into())
        );
    }

    #[test]
    fn existing_config_is_kept_when_no_flag_is_given() {
        assert_eq!(
            identity_action(None, Some("Old".into()), true),
            IdentityAction::KeepExisting("Old".into())
        );
    }

    #[test]
    fn unset_without_flag_prompts_when_allowed() {
        assert_eq!(identity_action(None, None, false), IdentityAction::Prompt);
    }

    #[test]
    fn unset_without_flag_under_yes_requires_the_flag() {
        assert_eq!(identity_action(None, None, true), IdentityAction::NeedFlag);
    }

    #[test]
    fn empty_identity_value_errors_with_the_flag_in_the_hint() {
        let err = configure_identity(
            "user.name",
            "Your name",
            "--name",
            Some("   ".into()),
            true,
            false,
        )
        .unwrap_err();
        let out = render_error(&err, false);
        assert!(out.contains("user.name"), "out was: {out}");
        assert!(out.contains("--name"), "out was: {out}");
        assert!(out.contains("hint:"), "out was: {out}");
    }

    // The tests below exercise the flow end to end against real `git`,
    // which reads the global config from the process environment. Rather
    // than mutate this process's environment, each one re-executes this
    // test binary, filtered to a child test, with HOME and the global git
    // config pointed into a temporary directory. A child test does nothing
    // unless it is running inside such a sandbox, so in a normal run it
    // passes as a no-op.

    /// Holds the sandbox HOME path in a re-executed child test process.
    const SANDBOX_CHILD: &str = "HPDS_TEST_GIT_SETUP_SANDBOX_CHILD";

    /// Whether this process is a sandboxed child: SANDBOX_CHILD must equal
    /// HOME, and GIT_CONFIG_GLOBAL must point inside that directory. A
    /// stray SANDBOX_CHILD in a developer shell fails these checks, so the
    /// child tests cannot touch the real global git config.
    fn in_sandbox_child() -> bool {
        let (Some(marker), Some(home), Some(global)) = (
            std::env::var_os(SANDBOX_CHILD),
            std::env::var_os("HOME"),
            std::env::var_os("GIT_CONFIG_GLOBAL"),
        ) else {
            return false;
        };
        let home = std::path::Path::new(&home);
        !home.as_os_str().is_empty()
            && marker == home.as_os_str()
            && std::path::Path::new(&global).starts_with(home)
    }

    struct SandboxRun {
        tmp: tempfile::TempDir,
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
    }

    impl SandboxRun {
        fn home(&self) -> std::path::PathBuf {
            self.tmp.path().join("home")
        }

        fn config(&self, key: &str) -> Option<String> {
            let output = std::process::Command::new("git")
                .args(["config", "--file"])
                .arg(self.home().join(".gitconfig"))
                .args(["--get", key])
                .output()
                .unwrap();
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        }

        fn transcript(&self) -> String {
            format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
        }
    }

    /// Run the child test `name` (a test in this module) in a fresh
    /// process with a sandboxed HOME, an empty global git config, and no
    /// `gh`.
    fn run_child(name: &str) -> SandboxRun {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let work = tmp.path().join("work");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&work).unwrap();
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, rest)| rest);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                format!("{module}::{name}").as_str(),
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(&work)
            .env(SANDBOX_CHILD, &home)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HPDS_GH", tmp.path().join("no-such-gh"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("EMAIL")
            .output()
            .unwrap();
        let run = SandboxRun {
            tmp,
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        // A filter that matches nothing also exits successfully, so confirm
        // that exactly one child test ran and passed.
        assert!(
            run.stdout.contains("test result: ok. 1 passed;"),
            "the child test `{name}` must run and pass exactly once\n{}",
            run.transcript()
        );
        run
    }

    #[test]
    fn child_configure_identity_skipping_missing() {
        if !in_sandbox_child() {
            return;
        }
        configure_identity("user.name", "Your name", "--name", None, true, true).unwrap();
    }

    #[test]
    fn child_configure_identity_requiring_flag() {
        if !in_sandbox_child() {
            return;
        }
        let err =
            configure_identity("user.name", "Your name", "--name", None, true, false).unwrap_err();
        let out = render_error(&err, false);
        assert!(out.contains("--name"), "out was: {out}");
    }

    #[test]
    fn child_bundle_setup() {
        if !in_sandbox_child() {
            return;
        }
        run_setup_with_defaults(true).unwrap();
    }

    #[test]
    fn missing_identity_under_yes_warns_and_continues_when_skipping() {
        let run = run_child("child_configure_identity_skipping_missing");
        let transcript = run.transcript();
        assert!(run.status.success(), "{transcript}");
        assert!(run.stderr.contains("warning:"), "{transcript}");
        assert!(run.stderr.contains("user.name"), "{transcript}");
        assert!(
            run.stderr.contains("git config --global user.name")
                || run.stderr.contains("hpds git setup"),
            "the warning must say what to run next\n{transcript}"
        );
        assert_eq!(run.config("user.name"), None, "{transcript}");
    }

    #[test]
    fn missing_identity_under_yes_still_errors_without_skipping() {
        let run = run_child("child_configure_identity_requiring_flag");
        assert!(run.status.success(), "{}", run.transcript());
    }

    #[test]
    fn bundle_setup_without_identity_still_runs_the_remaining_steps() {
        let run = run_child("child_bundle_setup");
        let transcript = run.transcript();
        assert!(run.status.success(), "{transcript}");
        for key in ["user.name", "user.email"] {
            assert!(
                run.stderr
                    .lines()
                    .any(|line| line.contains("warning:") && line.contains(key)),
                "{key} must be reported as a warning\n{transcript}"
            );
            assert_eq!(run.config(key), None, "{transcript}");
        }
        assert!(
            run.stdout.contains("gh) is not installed"),
            "the gh auth report must still run\n{transcript}"
        );
        assert_eq!(run.config("init.defaultBranch").as_deref(), Some("main"));
        let ignore = std::fs::read_to_string(run.home().join(".gitignore"))
            .unwrap_or_else(|err| panic!("global ignore not written ({err})\n{transcript}"));
        assert!(ignore.contains("# >>> hpds vaccinate >>>"), "{ignore}");
        assert!(ignore.contains(".Rhistory"), "{ignore}");
    }
}
