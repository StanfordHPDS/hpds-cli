//! The engine behind `hpds setup`: profile step tables, plan rendering,
//! and the step executor.
//!
//! A profile is a fixed, ordered list of [`Step`]s. Each step is one line
//! in the plan and the checklist, and runs one or more actions: an
//! installer from the `hpds install` registry, the `hpds git setup` flow,
//! or a command plan executed through the [`CommandRunner`] seam (with
//! sudo steps gated by the installer framework's sudo discipline). A
//! failing step is reported and the remaining steps still run; the caller
//! gets every step's outcome back for the summary and the exit code.
//!
//! [`CommandRunner`]: crate::install::CommandRunner

use std::path::Path;

use anyhow::anyhow;

use crate::install::{self, InstallCtx, registry};
use crate::ui::{self, HintExt};

/// Which bundle of steps `hpds setup` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Developer machine (any OS): the lab toolchain plus git defaults.
    Dev,
    /// Lab compute server (Linux only): full server provisioning.
    Server,
}

impl Profile {
    /// The profile name as typed on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Dev => "dev",
            Profile::Server => "server",
        }
    }
}

/// One checklist entry: a title plus the actions that realize it.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    /// What the checklist, plan, and summary call this step.
    pub title: &'static str,
    actions: &'static [Action],
}

/// One thing a step does.
#[derive(Debug)]
enum Action {
    /// Run the named tool's installer from the `hpds install` registry.
    Install(&'static str),
    /// Run the `hpds git setup` flow (via the seam on [`SetupDeps`]).
    GitSetup,
    /// Run one external command through the runner seam.
    Run(Cmd),
}

/// A planned external command: announced, logged at `-v`, and executed
/// through [`InstallCtx::run_step`] (or [`InstallCtx::run_sudo_step`]
/// when `sudo` is set, which prompts unless `--yes` was given).
#[derive(Debug)]
struct Cmd {
    what: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    sudo: bool,
}

/// Everything step execution needs: the installer context (OS, runner,
/// fetcher, sudo policy) and a seam for the `hpds git setup` flow so
/// tests never touch real git config.
pub struct SetupDeps<'a> {
    pub install: InstallCtx<'a>,
    pub git_setup: &'a dyn Fn() -> anyhow::Result<()>,
}

/// How one executed step ended.
#[derive(Debug)]
pub struct StepResult {
    pub title: &'static str,
    /// `None` on success; the rendered error on failure.
    pub error: Option<String>,
}

/// The developer-machine bundle: the toolchain installers plus git
/// defaults, in dependency-friendly order (rig before tinytex is not
/// required; r installs rig itself when missing, and tinytex needs the
/// quarto installed two steps earlier).
const DEV_STEPS: &[Step] = &[
    Step {
        title: "r",
        actions: &[Action::Install("r")],
    },
    Step {
        title: "quarto",
        actions: &[Action::Install("quarto")],
    },
    Step {
        title: "uv",
        actions: &[Action::Install("uv")],
    },
    Step {
        title: "gh",
        actions: &[Action::Install("gh")],
    },
    Step {
        title: "rig",
        actions: &[Action::Install("rig")],
    },
    Step {
        title: "tinytex",
        actions: &[Action::Install("tinytex")],
    },
    Step {
        title: "git setup",
        actions: &[Action::GitSetup],
    },
];

/// The Debian/Ubuntu system libraries R and Python packages routinely
/// compile against, plus gdebi for installing the RStudio Server deb.
const APT_SYSTEM_LIBRARIES: &[&str] = &[
    "install",
    "-y",
    "build-essential",
    "gdebi-core",
    "libcurl4-openssl-dev",
    "libfontconfig1-dev",
    "libfreetype6-dev",
    "libfribidi-dev",
    "libgit2-dev",
    "libharfbuzz-dev",
    "libjpeg-dev",
    "libpng-dev",
    "libssl-dev",
    "libtiff5-dev",
    "libxml2-dev",
    "unixodbc-dev",
    "zlib1g-dev",
];

/// Where the RStudio Server deb is downloaded before installation.
const RSTUDIO_DEB: &str = "/tmp/rstudio-server-amd64.deb";

/// The pinned RStudio Server release the server profile installs.
const RSTUDIO_URL: &str =
    "https://download2.rstudio.org/server/jammy/amd64/rstudio-server-2025.05.1-513-amd64.deb";

/// The lab-server bundle (Linux only): system libraries, R/Python wired
/// to Posit Package Manager, the IDE servers, and the same toolchain the
/// dev profile installs. Every external command is a static plan run
/// through the runner seam; nothing shells out behind the plan's back.
const SERVER_STEPS: &[Step] = &[
    Step {
        title: "system libraries (apt)",
        actions: &[
            Action::Run(Cmd {
                what: "refreshing apt package lists",
                program: "apt-get",
                args: &["update"],
                sudo: true,
            }),
            Action::Run(Cmd {
                what: "installing system libraries",
                program: "apt-get",
                args: APT_SYSTEM_LIBRARIES,
                sudo: true,
            }),
        ],
    },
    Step {
        title: "R + CRAN/PPM repositories",
        actions: &[
            Action::Install("r"),
            Action::Run(Cmd {
                what: "pointing R at Posit Package Manager for CRAN packages",
                program: "sh",
                args: &[
                    "-c",
                    "echo 'options(repos = c(P3M = \"https://packagemanager.posit.co/cran/__linux__/noble/latest\", CRAN = \"https://cloud.r-project.org\"))' >> /etc/R/Rprofile.site",
                ],
                sudo: true,
            }),
        ],
    },
    Step {
        title: "Python + PPM pip configuration",
        actions: &[
            Action::Run(Cmd {
                what: "installing Python",
                program: "apt-get",
                args: &["install", "-y", "python3", "python3-pip", "python3-venv"],
                sudo: true,
            }),
            Action::Run(Cmd {
                what: "pointing pip at Posit Package Manager",
                program: "sh",
                args: &[
                    "-c",
                    "printf '[global]\\nindex-url = https://packagemanager.posit.co/pypi/latest/simple\\n' > /etc/pip.conf",
                ],
                sudo: true,
            }),
        ],
    },
    Step {
        title: "quarto",
        actions: &[Action::Install("quarto")],
    },
    Step {
        title: "tinytex",
        actions: &[Action::Install("tinytex")],
    },
    Step {
        title: "RStudio Server",
        actions: &[
            Action::Run(Cmd {
                what: "downloading RStudio Server",
                program: "curl",
                args: &["-fsSL", "-o", RSTUDIO_DEB, RSTUDIO_URL],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "installing RStudio Server",
                program: "gdebi",
                args: &["-n", RSTUDIO_DEB],
                sudo: true,
            }),
        ],
    },
    Step {
        title: "code-server + extensions",
        actions: &[
            Action::Run(Cmd {
                what: "installing code-server",
                // The install script is code-server's supported path; it
                // detects the distro and picks the right package.
                program: "sh",
                args: &["-c", "curl -fsSL https://code-server.dev/install.sh | sh"],
                sudo: true,
            }),
            Action::Run(Cmd {
                what: "installing the Python extension",
                program: "code-server",
                args: &["--install-extension", "ms-python.python"],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "installing the Jupyter extension",
                program: "code-server",
                args: &["--install-extension", "ms-toolsai.jupyter"],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "installing the Quarto extension",
                program: "code-server",
                args: &["--install-extension", "quarto.quarto"],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "installing the Ruff extension",
                program: "code-server",
                args: &["--install-extension", "charliermarsh.ruff"],
                sudo: false,
            }),
        ],
    },
    Step {
        title: "gh",
        actions: &[Action::Install("gh")],
    },
    Step {
        title: "duckdb",
        actions: &[Action::Install("duckdb")],
    },
    Step {
        title: "uv + ruff + sqlfluff",
        actions: &[
            Action::Install("uv"),
            Action::Run(Cmd {
                what: "installing ruff as a uv tool",
                program: "uv",
                args: &["tool", "install", "ruff"],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "installing sqlfluff as a uv tool",
                program: "uv",
                args: &["tool", "install", "sqlfluff"],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "putting uv tools on PATH",
                program: "uv",
                args: &["tool", "update-shell"],
                sudo: false,
            }),
        ],
    },
    Step {
        title: "rig",
        actions: &[Action::Install("rig")],
    },
    Step {
        title: "git defaults",
        actions: &[Action::GitSetup],
    },
];

/// The ordered steps a profile runs. The server table is Linux-shaped by
/// construction (it never consults the host OS), which is what makes
/// `--plan --profile server` meaningful on every OS.
pub fn steps(profile: Profile) -> &'static [Step] {
    match profile {
        Profile::Dev => DEV_STEPS,
        Profile::Server => SERVER_STEPS,
    }
}

/// The numbered plan for a profile: every step, and under each step the
/// exact commands it will run.
pub fn plan(profile: Profile) -> String {
    let steps = steps(profile);
    let mut out = format!(
        "hpds setup --profile {} runs these {} steps:\n",
        profile.name(),
        steps.len()
    );
    for (index, step) in steps.iter().enumerate() {
        out.push_str(&format!("{:>3}. {}\n", index + 1, step.title));
        for action in step.actions {
            out.push_str(&format!("       {}\n", describe(action)));
        }
    }
    out
}

/// One plan line for an action: the command a user could run themselves.
fn describe(action: &Action) -> String {
    match action {
        Action::Install(tool) => format!("hpds install {tool}"),
        Action::GitSetup => "hpds git setup".to_string(),
        Action::Run(cmd) => {
            let line = format!("{} {}", cmd.program, cmd.args.join(" "));
            if cmd.sudo {
                format!("sudo {line}")
            } else {
                line
            }
        }
    }
}

/// Reduce the full step list to what should actually run: `--yes` takes
/// everything, an interactive session gets the opt-out checklist, and a
/// non-interactive session without `--yes` refuses before anything runs.
pub fn choose_steps(all: &[Step], yes: bool, interactive: bool) -> anyhow::Result<Vec<Step>> {
    if yes {
        return Ok(all.to_vec());
    }
    if !interactive {
        return Err(anyhow!(
            "hpds setup needs a terminal to ask which steps to run"
        ))
        .hint("re-run with --yes to run every step without prompting, or run from an interactive terminal");
    }
    let titles: Vec<&'static str> = all.iter().map(|step| step.title).collect();
    let chosen = ui::multiselect_all("Which setup steps should run? (all pre-selected)", titles)?;
    Ok(all
        .iter()
        .filter(|step| chosen.contains(&step.title))
        .copied()
        .collect())
}

/// Run every selected step in order, reporting each failure as it happens
/// and carrying on with the rest.
pub fn execute(steps: &[Step], deps: &SetupDeps) -> Vec<StepResult> {
    let total = steps.len();
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            ui::println(&format!("[{}/{total}] {}", index + 1, step.title));
            let error = match run_actions(step, deps) {
                Ok(()) => None,
                Err(err) => {
                    ui::error(&err);
                    ui::warn(&format!("step failed: {}; continuing", step.title));
                    Some(format!("{err:#}"))
                }
            };
            StepResult {
                title: step.title,
                error,
            }
        })
        .collect()
}

/// Run one step's actions in order, stopping the step at the first error.
fn run_actions(step: &Step, deps: &SetupDeps) -> anyhow::Result<()> {
    for action in step.actions {
        match action {
            Action::Install(tool) => {
                let installer = registry::find(tool)?;
                install::run_installer(installer, &deps.install)?;
            }
            Action::GitSetup => (deps.git_setup)()?,
            Action::Run(cmd) => {
                if cmd.sudo {
                    deps.install
                        .run_sudo_step(cmd.what, cmd.program, cmd.args)?;
                } else {
                    deps.install.run_step(cmd.what, cmd.program, cmd.args)?;
                }
            }
        }
    }
    Ok(())
}

/// The end-of-run summary text: one ✓/✗ line per executed step.
pub fn summary(results: &[StepResult]) -> String {
    let mut out = String::from("setup summary:\n");
    for result in results {
        match &result.error {
            None => out.push_str(&format!("  ✓ {}\n", result.title)),
            Some(error) => out.push_str(&format!("  ✗ {} — {error}\n", result.title)),
        }
    }
    out
}

/// Print the summary, optionally write it to a log file, and turn any
/// failed step into the run's error (exit code 1).
pub fn finish(results: &[StepResult], log_path: Option<&Path>) -> anyhow::Result<()> {
    if results.is_empty() {
        ui::println("no steps selected; nothing to do");
        return Ok(());
    }
    let text = summary(results);
    ui::println(text.trim_end());
    if let Some(path) = log_path {
        match std::fs::write(path, &text) {
            Ok(()) => ui::println(&format!("summary written to {}", path.display())),
            Err(err) => ui::warn(&format!(
                "could not write the summary log to {}: {err}",
                path.display()
            )),
        }
    }
    let failed = results.iter().filter(|r| r.error.is_some()).count();
    if failed == 0 {
        ui::success("setup complete");
        Ok(())
    } else {
        Err(anyhow!("{failed} of {} setup steps failed", results.len())).hint(
            "fix the reported problems and re-run `hpds setup`; completed steps are idempotent and will no-op",
        )
    }
}

/// The dev-profile flow: checklist, execute, summarize.
pub fn run_dev(deps: &SetupDeps, yes: bool, interactive: bool) -> anyhow::Result<()> {
    let selected = choose_steps(steps(Profile::Dev), yes, interactive)?;
    finish(&execute(&selected, deps), None)
}

/// The server-profile flow: print the plan, checklist, confirm, execute,
/// summarize into `log_path`. The caller has already verified the host is
/// Linux; nothing runs until the confirmation gate passes (`--yes` skips
/// it, and a non-interactive session without `--yes` refuses).
pub fn run_server(
    deps: &SetupDeps,
    yes: bool,
    interactive: bool,
    log_path: &Path,
) -> anyhow::Result<()> {
    ui::println(plan(Profile::Server).trim_end());
    let selected = choose_steps(steps(Profile::Server), yes, interactive)?;
    if selected.is_empty() {
        return finish(&[], None);
    }
    if !yes {
        let go = ui::confirm(
            &format!(
                "Run these {} steps now? (several need sudo)",
                selected.len()
            ),
            true,
        )?;
        if !go {
            return Err(anyhow!("server setup canceled before any step ran"))
                .hint("re-run `hpds setup --profile server` when you are ready");
        }
    }
    finish(&execute(&selected, deps), Some(log_path))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::install::ReleaseFetcher;
    use crate::install::test_support::{FakeFetcher, FakeRunner, PanicFetcher, probe_fixture};
    use crate::tools::Os;
    use crate::ui::render_error;

    /// `SetupDeps` on `os` whose git-setup seam records itself in the
    /// runner's call log, so step ordering is visible in one place.
    fn deps_recording_git<'a>(
        os: Os,
        yes: bool,
        runner: &'a FakeRunner,
        fetcher: &'a dyn ReleaseFetcher,
        git_setup: &'a dyn Fn() -> anyhow::Result<()>,
    ) -> SetupDeps<'a> {
        SetupDeps {
            install: InstallCtx {
                os,
                yes,
                verbose: false,
                pin: None,
                // Mirrors the CLI wiring: the setup flow's own gate
                // approves the batch before any step executes.
                plan_approved: true,
                sudo_approved: std::cell::Cell::new(false),
                runner,
                fetcher,
            },
            git_setup,
        }
    }

    /// A fake runner on which every dev-profile tool probes as already
    /// installed, so each install step is an idempotent no-op.
    fn runner_with_everything_installed() -> FakeRunner {
        FakeRunner::default()
            .on_path("R")
            .with_output("R --version", &probe_fixture("r.txt"))
            .on_path("quarto")
            .with_output("quarto --version", &probe_fixture("quarto.txt"))
            .with_output(
                "quarto list tools",
                &crate::install::test_support::tool_output_fixture(
                    "tinytex/quarto-list-tools-managed.txt",
                ),
            )
            .on_path("uv")
            .with_output("uv --version", &probe_fixture("uv.txt"))
            .on_path("gh")
            .with_output("gh --version", &probe_fixture("gh.txt"))
            .on_path("rig")
            .with_output("rig --version", &probe_fixture("rig.txt"))
    }

    // --- plans ------------------------------------------------------------

    #[test]
    fn dev_plan_is_stable() {
        insta::assert_snapshot!(plan(Profile::Dev));
    }

    #[test]
    fn server_plan_is_stable_and_linux_shaped_on_any_host() {
        // The server step table never consults the host OS, so this
        // snapshot is the Linux plan wherever the test runs.
        insta::assert_snapshot!(plan(Profile::Server));
    }

    #[test]
    fn every_server_install_action_resolves_in_the_registry() {
        for step in steps(Profile::Server) {
            for action in step.actions {
                if let Action::Install(tool) = action {
                    registry::find(tool).unwrap_or_else(|e| panic!("{tool}: {e}"));
                }
            }
        }
    }

    // --- checklist gating ---------------------------------------------------

    #[test]
    fn yes_takes_every_step_without_prompting() {
        let selected = choose_steps(steps(Profile::Dev), true, false).expect("--yes never asks");
        let titles: Vec<_> = selected.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            vec!["r", "quarto", "uv", "gh", "rig", "tinytex", "git setup"]
        );
    }

    #[test]
    fn non_interactive_without_yes_refuses_with_guidance() {
        let err = choose_steps(steps(Profile::Dev), false, false).expect_err("must refuse");
        let out = render_error(&err, false);
        assert!(out.contains("--yes"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    // --- dev profile end to end ---------------------------------------------

    #[test]
    fn dev_profile_runs_every_step_in_order() {
        let runner = runner_with_everything_installed();
        let git_setup = || {
            runner.calls.borrow_mut().push("hpds git setup".to_string());
            Ok(())
        };
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &git_setup);

        run_dev(&deps, true, false).expect("every step no-ops successfully");

        // Each install step probes its tool in profile order, and the git
        // seam runs last: the full step sequence, seen through the runner.
        assert_eq!(
            *runner.calls.borrow(),
            vec![
                "R --version",
                "quarto --version",
                "uv --version",
                "gh --version",
                "rig --version",
                "quarto list tools",
                "hpds git setup",
            ]
        );
    }

    #[test]
    fn a_failing_step_reports_and_the_rest_still_run() {
        // gh is missing and its installer cannot complete (the fake
        // fetcher "downloads" a binary that never lands on PATH), so the
        // gh step fails; every later step must still run.
        let fetcher = FakeFetcher::default();
        let git_ran = Cell::new(false);
        let git_setup = || {
            git_ran.set(true);
            Ok(())
        };
        let runner = FakeRunner::default()
            .on_path("R")
            .with_output("R --version", &probe_fixture("r.txt"))
            .on_path("quarto")
            .with_output("quarto --version", &probe_fixture("quarto.txt"))
            .with_output(
                "quarto list tools",
                &crate::install::test_support::tool_output_fixture(
                    "tinytex/quarto-list-tools-managed.txt",
                ),
            )
            .on_path("uv")
            .with_output("uv --version", &probe_fixture("uv.txt"))
            .on_path("rig")
            .with_output("rig --version", &probe_fixture("rig.txt"));
        let deps = deps_recording_git(Os::Mac, true, &runner, &fetcher, &git_setup);

        let results = execute(steps(Profile::Dev), &deps);

        let failures: Vec<_> = results
            .iter()
            .filter(|r| r.error.is_some())
            .map(|r| r.title)
            .collect();
        assert_eq!(failures, vec!["gh"], "only the gh step may fail");
        assert!(git_ran.get(), "steps after the failure must still run");
        assert!(
            runner.calls.borrow().iter().any(|c| c == "rig --version"),
            "steps after the failure must still run: {:?}",
            runner.calls.borrow()
        );

        let err = finish(&results, None).expect_err("a failed step fails the run");
        assert!(err.to_string().contains("1 of 7"), "{err}");
    }

    // --- server profile -------------------------------------------------------

    #[test]
    fn server_non_interactive_without_yes_refuses_before_any_step_runs() {
        let runner = FakeRunner::default();
        let git_setup = || panic!("no step may run when confirmation is refused");
        let deps = deps_recording_git(Os::Linux, false, &runner, &PanicFetcher, &git_setup);
        let log = tempfile::tempdir().expect("create temp dir");

        let err = run_server(&deps, false, false, &log.path().join("hpds-setup.log"))
            .expect_err("must refuse without --yes");

        let out = render_error(&err, false);
        assert!(out.contains("--yes"), "{out}");
        assert!(
            runner.calls.borrow().is_empty(),
            "zero commands may run: {:?}",
            runner.calls.borrow()
        );
        assert!(!log.path().join("hpds-setup.log").exists());
    }

    #[test]
    fn server_with_yes_runs_the_command_plans_through_the_runner_and_logs() {
        // Every registry tool probes as installed (no-op installs) so the
        // run exercises exactly the server profile's own command plans.
        let runner = runner_with_everything_installed()
            .on_path("duckdb")
            .with_output("duckdb --version", &probe_fixture("duckdb.txt"))
            .with_output("sudo apt-get update", "")
            .with_output(
                &format!("sudo apt-get {}", APT_SYSTEM_LIBRARIES.join(" ")),
                "",
            )
            .with_output(
                "sudo sh -c echo 'options(repos = c(P3M = \"https://packagemanager.posit.co/cran/__linux__/noble/latest\", CRAN = \"https://cloud.r-project.org\"))' >> /etc/R/Rprofile.site",
                "",
            )
            .with_output("sudo apt-get install -y python3 python3-pip python3-venv", "")
            .with_output(
                "sudo sh -c printf '[global]\\nindex-url = https://packagemanager.posit.co/pypi/latest/simple\\n' > /etc/pip.conf",
                "",
            )
            .with_output(&format!("curl -fsSL -o {RSTUDIO_DEB} {RSTUDIO_URL}"), "")
            .with_output(&format!("sudo gdebi -n {RSTUDIO_DEB}"), "")
            .with_output(
                "sudo sh -c curl -fsSL https://code-server.dev/install.sh | sh",
                "",
            )
            .with_output("code-server --install-extension ms-python.python", "")
            .with_output("code-server --install-extension ms-toolsai.jupyter", "")
            .with_output("code-server --install-extension quarto.quarto", "")
            .with_output("code-server --install-extension charliermarsh.ruff", "")
            .with_output("uv tool install ruff", "")
            .with_output("uv tool install sqlfluff", "")
            .with_output("uv tool update-shell", "");
        let git_setup = || {
            runner.calls.borrow_mut().push("hpds git setup".to_string());
            Ok(())
        };
        let deps = deps_recording_git(Os::Linux, true, &runner, &PanicFetcher, &git_setup);
        let dir = tempfile::tempdir().expect("create temp dir");
        let log_path = dir.path().join("hpds-setup.log");

        run_server(&deps, true, false, &log_path).expect("all steps succeed");

        let calls = runner.calls.borrow();
        // System steps ran as the planned commands, under sudo where
        // declared, all through the runner seam.
        assert_eq!(
            calls.first().map(String::as_str),
            Some("sudo apt-get update")
        );
        assert!(
            calls
                .iter()
                .any(|c| c == "code-server --install-extension charliermarsh.ruff"),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c == "uv tool install sqlfluff"),
            "{calls:?}"
        );
        assert_eq!(calls.last().map(String::as_str), Some("hpds git setup"));

        let log = std::fs::read_to_string(&log_path).expect("summary log must be written");
        for step in steps(Profile::Server) {
            assert!(
                log.contains(step.title),
                "log must mention {}: {log}",
                step.title
            );
        }
        assert!(!log.contains('✗'), "no step failed: {log}");
    }

    // --- summary ----------------------------------------------------------

    #[test]
    fn summary_marks_successes_and_failures() {
        let results = vec![
            StepResult {
                title: "quarto",
                error: None,
            },
            StepResult {
                title: "rig",
                error: Some("boom".to_string()),
            },
        ];
        let text = summary(&results);
        assert!(text.contains("✓ quarto"), "{text}");
        assert!(text.contains("✗ rig — boom"), "{text}");
    }

    #[test]
    fn finish_with_no_selected_steps_is_a_clean_no_op() {
        finish(&[], None).expect("nothing selected is not an error");
    }
}
