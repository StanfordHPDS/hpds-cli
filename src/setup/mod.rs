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
//! Under `hpds setup --profile server --yes`, steps on the background
//! [`Lane`] (user-space installs that never need sudo or a prompt) run on a
//! worker thread while the rest run on the main thread; each step waits
//! for the steps it `requires` on the other lane, and background output is
//! captured and replayed under the step's header. Every other run executes
//! the steps one at a time, in plan order.
//!
//! [`CommandRunner`]: crate::install::CommandRunner

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, mpsc};

use anyhow::anyhow;

use crate::install::installers::rstudio_server;
use crate::install::{self, InstallCtx};
use crate::ui::{self, HintExt};

mod prelude;
pub(crate) mod server;

use prelude::{AptRepo, AptShare, LocalDeb};

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
    /// Where the step runs when independent work overlaps.
    lane: Lane,
    /// Titles of steps that must have finished (successfully or not)
    /// before this one starts, when they are selected. Each names an
    /// earlier step of the same profile, so plan order alone satisfies
    /// them when steps run one after another.
    requires: &'static [&'static str],
    /// What the step adds to the server profile's apt prelude when it is
    /// selected.
    apt: AptShare,
    actions: &'static [Action],
}

/// Which thread runs a step when `hpds setup --yes` overlaps independent
/// work. Without overlap every step runs in the foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// The main thread, in plan order: everything that uses sudo, apt,
    /// or the terminal.
    Foreground,
    /// One worker thread, in plan order, with its output captured and
    /// replayed by the main thread: user-space installs only.
    Background,
}

/// The server profile's first step, which brings the installed system
/// packages up to date (see [`Action::SystemUpdates`]).
const SYSTEM_UPDATES: &str = "system updates (apt upgrade)";

/// The server profile's second step, which installs every selected step's
/// apt packages in one transaction (see [`Action::AptPrelude`]).
const SYSTEM_LIBRARIES: &str = "system libraries (apt)";

/// One thing a step does.
#[derive(Debug)]
enum Action {
    /// Run the named tool's installer from the `hpds install` registry.
    Install(&'static str),
    /// Run the `hpds git setup` flow (via the seam on [`SetupDeps`]).
    GitSetup,
    /// Run one external command through the runner seam.
    Run(Cmd),
    /// Register the selected steps' apt repositories, download and verify
    /// their package files, then refresh the package lists once and
    /// install every selected step's packages in one `apt-get install`.
    /// If that install fails, retry once with only the distribution
    /// packages; the steps whose packages were dropped install their own.
    AptPrelude,
    /// Install this step's own [`AptShare::Packages`] with apt, unless the
    /// apt prelude already installed them in this run. Names what the
    /// packages are for, such as `Python`.
    StepPackages(&'static str),
    /// Reconcile one typed server-host concern without shell evaluation.
    Server(server::ServerAction),
    /// Refresh the package lists and upgrade the installed packages, then
    /// warn when the host reports that a reboot is pending. hpds never
    /// reboots on its own.
    SystemUpdates,
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
    pub git_setup: &'a (dyn Fn() -> anyhow::Result<()> + Sync),
    pub(crate) installer_lookup: &'a (
            dyn Fn(
        &str,
    ) -> Result<&'static dyn install::Installer, install::registry::RegistryError>
                + Sync
        ),
    pub(crate) server_host: &'a (dyn server::ServerHost + Sync),
}

impl<'a> SetupDeps<'a> {
    /// A copy for another thread: the same seams, with an installer
    /// context that has its own sudo-approval cell.
    fn for_thread(&self) -> SetupDeps<'a> {
        SetupDeps {
            install: self.install.for_thread(),
            git_setup: self.git_setup,
            installer_lookup: self.installer_lookup,
            server_host: self.server_host,
        }
    }
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
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("r")],
    },
    Step {
        title: "quarto",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("quarto")],
    },
    Step {
        title: "uv",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("uv")],
    },
    Step {
        title: "gh",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("gh")],
    },
    Step {
        title: "rig",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("rig")],
    },
    Step {
        title: "tinytex",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("tinytex")],
    },
    Step {
        title: "togi",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("togi")],
    },
    Step {
        title: "git setup",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::GitSetup],
    },
];

/// The Debian/Ubuntu system libraries R and Python packages routinely
/// compile against.
const APT_SYSTEM_LIBRARIES: &[&str] = &[
    "build-essential",
    "ca-certificates",
    "curl",
    "gnupg",
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

/// The system Python the server profile installs.
const PYTHON_PACKAGES: &[&str] = &["python3", "python3-pip", "python3-venv"];

/// The lab-server bundle (Linux only): system libraries, R/Python wired
/// to Posit Package Manager, the IDE servers, and the same toolchain the
/// dev profile installs. Every external command is a static plan run
/// through the runner seam; nothing shells out behind the plan's back.
///
/// The first step brings the installed packages up to date. The second
/// installs every selected step's apt packages in one transaction (each
/// step's `apt` share); the steps that own those packages keep their own
/// install paths, which find the packages installed and do nothing, or
/// install them when the second step could not.
///
/// The background lane holds the steps that touch only the invoking
/// user's files and never use sudo, apt, or a prompt under `--yes`. The
/// requirements are the cross-lane reads: TinyTeX's `tlmgr` needs the
/// full Perl that the system libraries bring in, code-server's settings
/// point at the Quarto launcher, git defaults reports `gh auth status`,
/// and `uv tool install` uses the system Python when it is present. The
/// other background steps download over TLS with hpds's own client or
/// rustup's, so they start right away.
const SERVER_STEPS: &[Step] = &[
    Step {
        title: SYSTEM_UPDATES,
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::SystemUpdates],
    },
    Step {
        title: SYSTEM_LIBRARIES,
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::Packages(APT_SYSTEM_LIBRARIES),
        actions: &[Action::AptPrelude],
    },
    Step {
        title: "Docker Engine",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::Repo(AptRepo::Docker),
        actions: &[Action::Server(server::ServerAction::Docker)],
    },
    Step {
        title: "stable Rust",
        lane: Lane::Background,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Server(server::ServerAction::Rust)],
    },
    Step {
        title: "R + CRAN/PPM repositories",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::Repo(AptRepo::Rig),
        actions: &[
            Action::Install("r"),
            Action::Server(server::ServerAction::RProfile),
        ],
    },
    Step {
        title: "Python + PPM pip configuration",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::Packages(PYTHON_PACKAGES),
        actions: &[
            Action::StepPackages("Python"),
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
        lane: Lane::Background,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("quarto")],
    },
    Step {
        title: "tinytex",
        lane: Lane::Background,
        requires: &[SYSTEM_LIBRARIES],
        apt: AptShare::None,
        actions: &[Action::Install("tinytex")],
    },
    Step {
        title: "RStudio Server",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::LocalDeb(LocalDeb::RstudioServer),
        actions: &[Action::Install("rstudio-server")],
    },
    Step {
        title: "RStudio preferences",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Server(server::ServerAction::RstudioPreferences)],
    },
    Step {
        title: "code-server + extensions",
        lane: Lane::Foreground,
        requires: &["quarto"],
        apt: AptShare::LocalDeb(LocalDeb::CodeServer),
        actions: &[
            Action::Server(server::ServerAction::InstallCodeServer),
            Action::Run(Cmd {
                what: "installing the Python, Jupyter, Quarto, Ruff, and PDF viewer extensions",
                program: "code-server",
                args: &[
                    "--install-extension",
                    "ms-python.python",
                    "--install-extension",
                    "ms-toolsai.jupyter",
                    "--install-extension",
                    "quarto.quarto",
                    "--install-extension",
                    "charliermarsh.ruff",
                    "--install-extension",
                    "tomoki1207.pdf",
                ],
                sudo: false,
            }),
            Action::Server(server::ServerAction::CodeServer),
        ],
    },
    Step {
        title: "gh",
        lane: Lane::Background,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("gh")],
    },
    Step {
        title: "duckdb",
        lane: Lane::Background,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("duckdb")],
    },
    Step {
        title: "uv + ruff + sqlfluff",
        lane: Lane::Background,
        requires: &["Python + PPM pip configuration"],
        apt: AptShare::None,
        actions: &[
            Action::Install("uv"),
            Action::Run(Cmd {
                what: "installing ruff as a uv tool",
                program: "uv",
                args: &["tool", "install", "--upgrade", "ruff"],
                sudo: false,
            }),
            Action::Run(Cmd {
                what: "installing sqlfluff as a uv tool",
                program: "uv",
                args: &["tool", "install", "--upgrade", "sqlfluff"],
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
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::Install("rig")],
    },
    Step {
        title: "git defaults",
        lane: Lane::Foreground,
        requires: &["gh"],
        apt: AptShare::None,
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
    plan_for(profile, steps(profile))
}

/// The numbered plan for `steps`, run under `profile`'s name.
fn plan_for(profile: Profile, steps: &[Step]) -> String {
    let mut out = format!(
        "hpds setup --profile {} runs these {} steps:\n",
        profile.name(),
        steps.len()
    );
    for (index, step) in steps.iter().enumerate() {
        out.push_str(&format!("{:>3}. {}\n", index + 1, step.title));
        for action in step.actions {
            for line in describe(step, action, steps) {
                out.push_str(&format!("       {line}\n"));
            }
        }
    }
    out
}

/// The plan lines for one of `step`'s actions: the commands a user could
/// run themselves. The apt prelude describes what it does for every step
/// of `all`, the whole profile.
fn describe(step: &Step, action: &Action, all: &[Step]) -> Vec<String> {
    match action {
        Action::AptPrelude => prelude::describe(all),
        Action::StepPackages(_) => {
            let packages = match step.apt {
                AptShare::Packages(packages) => packages,
                _ => &[],
            };
            let mut args = vec!["install", "-y"];
            args.extend_from_slice(packages);
            vec![format!(
                "{} (unless {SYSTEM_LIBRARIES} installed them)",
                install::apt_get_command_line(&args)
            )]
        }
        Action::Install("rstudio-server") => vec![
            "hpds install rstudio-server (latest stable)".to_string(),
            rstudio_server::SERVICE_PLAN.to_string(),
        ],
        Action::Install(tool) if matches!(*tool, "uv" | "gh" | "duckdb" | "quarto" | "togi") => {
            vec![format!("hpds install {tool} (latest stable)")]
        }
        Action::Install(tool) => vec![format!("hpds install {tool}")],
        Action::GitSetup => vec!["hpds git setup".to_string()],
        Action::Run(cmd) => {
            let line = format!("{} {}", cmd.program, cmd.args.join(" "));
            vec![if cmd.sudo {
                format!("sudo {line}")
            } else {
                line
            }]
        }
        Action::Server(action) => vec![server::describe(action).to_string()],
        Action::SystemUpdates => vec![
            install::apt_get_command_line(&["update"]),
            install::apt_get_command_line(UPGRADE_ARGS),
            format!(
                "if {} exists afterwards: warn that a reboot is pending (hpds never reboots)",
                server::REBOOT_REQUIRED
            ),
        ],
    }
}

/// The `apt-get upgrade` arguments: keep a locally changed configuration
/// file and take the package's default for any other, so dpkg never
/// prompts.
const UPGRADE_ARGS: &[&str] = &[
    "-o",
    "Dpkg::Options::=--force-confdef",
    "-o",
    "Dpkg::Options::=--force-confold",
    "upgrade",
    "-y",
];

/// The warning raised when the host reports a pending reboot after the
/// system updates.
const REBOOT_PENDING: &str = "a reboot is pending to finish the system updates; run `sudo reboot`";

/// Refresh the package lists and upgrade the installed packages, recording
/// in `run` that the lists are fresh once the refresh succeeds. A failed
/// refresh skips the upgrade. Whether or not the upgrade succeeds, a
/// pending reboot is reported as a warning; hpds never reboots on its own.
fn system_updates(deps: &SetupDeps, run: &Run) -> anyhow::Result<()> {
    deps.install
        .run_apt_step("refreshing apt package lists", &["update"])?;
    run.mark_lists_refreshed();
    let upgraded = deps
        .install
        .run_apt_step("upgrading the installed packages", UPGRADE_ARGS);
    if deps
        .server_host
        .exists(&deps.server_host.reboot_required_path())
    {
        ui::warn(REBOOT_PENDING);
    }
    upgraded.map(|_| ())
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

/// Run every selected step, reporting each failure as it happens and
/// carrying on with the rest. Results come back in plan order, one per
/// selected step.
///
/// With `overlap`, background-lane steps run on a worker thread while the
/// foreground lane runs on this one (see [`execute_overlapped`]). Without
/// it, or when no background step is selected, every step runs here, one
/// at a time, in plan order.
pub fn execute(steps: &[Step], deps: &SetupDeps, overlap: bool) -> Vec<StepResult> {
    execute_rendered(steps, deps, overlap, ui::is_quiet()).0
}

/// [`execute`], also returning the warnings the steps raised in compact
/// mode, deduplicated in the order first seen (see [`Render`]). `quiet`
/// keeps step lines off the terminal (see [`Render::quiet`]).
fn execute_rendered(
    steps: &[Step],
    deps: &SetupDeps,
    overlap: bool,
    quiet: bool,
) -> (Vec<StepResult>, Vec<String>) {
    let run = Run::new(steps);
    let mut render = Render::new(steps.len(), quiet);
    let (foreground, background) = partition(steps);
    let results = if overlap && !background.is_empty() {
        execute_overlapped(&run, &foreground, &background, deps, &mut render)
    } else {
        execute_sequential(&run, deps, &mut render)
    };
    (results, render.next_steps)
}

/// How a run's steps appear on the terminal.
///
/// Outside compact mode (`-v`, or a caller that never entered it) each step
/// prints its `[n/N] title` header and then its output, as it always has.
/// In compact mode the header goes to the log only, a spinner labeled with
/// it marks a running foreground step, and the step's result is one
/// `[n/N] title ✓`/`✗` line, with any error the step reported indented
/// under it. The warnings the step raised are kept for the run's closing
/// next steps.
struct Render {
    compact: bool,
    /// `--quiet`: step lines go to the log only, while a failed step's
    /// error still reaches the terminal.
    quiet: bool,
    total: usize,
    /// Warnings raised so far, deduplicated, in the order first seen.
    next_steps: Vec<String>,
}

/// How far a failed step's error is indented under its step line.
const ERROR_INDENT: &str = "        ";

impl Render {
    fn new(total: usize, quiet: bool) -> Self {
        Render {
            compact: ui::is_compact(),
            quiet,
            total,
            next_steps: Vec::new(),
        }
    }

    /// Introduce a step about to run on this thread. In compact mode the
    /// returned spinner marks it until dropped.
    fn start(&self, index: usize, title: &str) -> Option<ui::Spinner> {
        let header = header(index, self.total, title);
        if self.compact {
            ui::log_detail(&header);
            Some(ui::spinner(header))
        } else {
            ui::println(&header);
            None
        }
    }

    /// Introduce a step whose output is replayed after it ran on the
    /// background lane.
    fn start_background(&self, index: usize, title: &str) {
        let header = background_header(index, self.total, title);
        if self.compact {
            ui::log_detail(&header);
        } else {
            ui::println(&header);
        }
    }

    /// Close a step: in compact mode, its result line, the errors it
    /// reported, and its warnings kept for the next steps. The errors reach
    /// the terminal even under `--quiet`, and stay out of the log, which
    /// already holds them as the step reported them.
    fn end(&mut self, index: usize, result: &StepResult) {
        if !self.compact {
            return;
        }
        let mark = if result.error.is_some() { "✗" } else { "✓" };
        ui::status_line(
            &format!("{} {mark}", header(index, self.total, result.title)),
            self.quiet,
        );
        let continuing = continuing_warning(result.title);
        for report in ui::take_reports() {
            match report.kind {
                ui::ReportKind::Error => {
                    let text = report
                        .message
                        .strip_prefix("error: ")
                        .unwrap_or(&report.message);
                    for line in text.lines() {
                        ui::report(&format!("{ERROR_INDENT}{line}"));
                    }
                }
                ui::ReportKind::Warning => {
                    if report.message != continuing && !self.next_steps.contains(&report.message) {
                        self.next_steps.push(report.message);
                    }
                }
            }
        }
    }
}

/// What every step of one run shares: the selected steps, the steps
/// whose own apt packages the apt prelude has installed, and whether the
/// system updates have refreshed the package lists.
struct Run<'s> {
    selected: &'s [Step],
    covered: Mutex<HashSet<&'static str>>,
    lists_refreshed: AtomicBool,
}

impl<'s> Run<'s> {
    fn new(selected: &'s [Step]) -> Self {
        Run {
            selected,
            covered: Mutex::new(HashSet::new()),
            lists_refreshed: AtomicBool::new(false),
        }
    }

    /// Record that the system updates refreshed the package lists.
    fn mark_lists_refreshed(&self) {
        self.lists_refreshed.store(true, Ordering::SeqCst);
    }

    /// Whether the system updates refreshed the package lists in this run.
    fn lists_refreshed(&self) -> bool {
        self.lists_refreshed.load(Ordering::SeqCst)
    }

    /// Record that the apt prelude installed these steps' packages.
    fn cover(&self, titles: impl IntoIterator<Item = &'static str>) {
        self.covered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(titles);
    }

    /// Whether the apt prelude installed `title`'s packages.
    fn is_covered(&self, title: &str) -> bool {
        self.covered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(title)
    }
}

/// Run the selected steps on this thread, one at a time, in plan order.
fn execute_sequential(run: &Run, deps: &SetupDeps, render: &mut Render) -> Vec<StepResult> {
    run.selected
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let spinner = render.start(index, step.title);
            let result = run_step(step, deps, run);
            if let Some(spinner) = spinner {
                spinner.finish_and_clear();
            }
            render.end(index, &result);
            result
        })
        .collect()
}

/// The `[n/N] title` line that introduces a step's output.
fn header(index: usize, total: usize, title: &str) -> String {
    format!("[{}/{total}] {title}", index + 1)
}

/// The header for a step whose output is replayed after it ran on the
/// background lane.
fn background_header(index: usize, total: usize, title: &str) -> String {
    format!("{} (ran in the background)", header(index, total, title))
}

/// Run one step, reporting its failure (if any) as it happens.
fn run_step(step: &Step, deps: &SetupDeps, run: &Run) -> StepResult {
    let error = match run_actions(step, deps, run) {
        Ok(()) => None,
        Err(err) => {
            ui::error(&err);
            ui::warn(&continuing_warning(step.title));
            Some(format!("{err:#}"))
        }
    };
    StepResult {
        title: step.title,
        error,
    }
}

/// The warning that a step failed and the run goes on. The step's line
/// already says it failed, so compact mode leaves it out of the next steps.
fn continuing_warning(title: &str) -> String {
    format!("step failed: {title}; continuing")
}

/// One lane's steps, in plan order, each with its plan index.
type LaneSteps = Vec<(usize, Step)>;

/// Split selected steps into the foreground and background lanes, each
/// in plan order and carrying its plan index.
fn partition(steps: &[Step]) -> (LaneSteps, LaneSteps) {
    steps
        .iter()
        .copied()
        .enumerate()
        .partition(|(_, step)| step.lane == Lane::Foreground)
}

/// Why a background step has no result, and what to do about it.
const UNREPORTED: &str = "the background worker stopped before this step reported a result";
const UNREPORTED_HINT: &str = "re-run `hpds setup --profile server` to retry it; steps that completed are idempotent and will no-op";

/// One background step's outcome, on its way to the main thread.
struct Finished {
    index: usize,
    result: StepResult,
    transcript: ui::Transcript,
}

/// The foreground steps the background lane may still have to wait for.
/// A step leaves the set when it finishes, whether or not it succeeded.
struct Completed {
    pending: Mutex<HashSet<&'static str>>,
    changed: Condvar,
}

impl Completed {
    fn new(titles: impl IntoIterator<Item = &'static str>) -> Self {
        Completed {
            pending: Mutex::new(titles.into_iter().collect()),
            changed: Condvar::new(),
        }
    }

    /// The set, reclaimed if a panicking thread poisoned the lock: updates
    /// only ever remove titles, so a poisoned set is still accurate.
    fn lock(&self) -> MutexGuard<'_, HashSet<&'static str>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn finish(&self, title: &str) {
        self.lock().remove(title);
        self.changed.notify_all();
    }

    fn finish_all(&self) {
        self.lock().clear();
        self.changed.notify_all();
    }

    /// Block until none of `requires` is a pending foreground step.
    /// Titles that were never pending (unselected steps, or steps on the
    /// caller's own lane, which plan order already settled) do not wait.
    fn wait_for(&self, requires: &[&str]) {
        let pending = self.lock();
        let _settled = self
            .changed
            .wait_while(pending, |pending| {
                requires.iter().any(|title| pending.contains(title))
            })
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// Marks every foreground step finished when dropped, including while the
/// foreground unwinds from a panic, so the background lane never waits on
/// a step that will not run (the scope joins the worker before the panic
/// propagates).
struct ReleaseForeground<'a>(&'a Completed);

impl Drop for ReleaseForeground<'_> {
    fn drop(&mut self) {
        self.0.finish_all();
    }
}

/// Run the foreground lane on this thread and the background lane on one
/// worker thread at the same time.
///
/// The background lane runs its steps in plan order, each inside
/// [`ui::capture`], sending each result and transcript back here. Every
/// step first waits for the selected steps it `requires` on the other
/// lane. This thread replays
/// background transcripts, under their headers, between foreground steps
/// as they arrive, then the rest once its own lane is done. The worker is
/// given this thread's log path (see [`ui::with_log_path`]), so its command
/// failures name the log its replayed lines go to.
///
/// If the worker panics, the steps it never reported fail with a hint to
/// re-run, and every foreground result stands. A step that panics loses
/// the lines it emitted before panicking: its capture is discarded as the
/// panic unwinds.
fn execute_overlapped(
    run: &Run,
    foreground: &[(usize, Step)],
    background: &[(usize, Step)],
    deps: &SetupDeps,
    render: &mut Render,
) -> Vec<StepResult> {
    let total = run.selected.len();
    let completed = Completed::new(foreground.iter().map(|(_, step)| step.title));
    let worker_deps = deps.for_thread();
    let log_path = ui::log_path();
    let (tx, rx) = mpsc::channel::<Finished>();

    let results = std::thread::scope(|scope| {
        let completed = &completed;
        let worker = std::thread::Builder::new()
            .name("hpds-setup-background".to_string())
            .spawn_scoped(scope, move || {
                ui::with_log_path(log_path, || {
                    run_background(background, &worker_deps, run, completed, &tx);
                });
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(err) => {
                ui::warn(&format!(
                    "could not start a background thread ({err}); running every step in order"
                ));
                return None;
            }
        };

        let mut lane = ForegroundLane {
            results: (0..total).map(|_| None).collect(),
            unreported: background.iter().map(|(_, step)| step.title).collect(),
            rx,
            render: &mut *render,
        };
        let release = ReleaseForeground(completed);
        for (index, step) in foreground {
            lane.wait_for(step.requires);
            let spinner = lane.render.start(*index, step.title);
            let result = run_step(step, deps, run);
            if let Some(spinner) = spinner {
                spinner.finish_and_clear();
            }
            lane.render.end(*index, &result);
            if let Some(slot) = lane.results.get_mut(*index) {
                *slot = Some(result);
            }
            completed.finish(step.title);
            lane.replay_arrivals();
        }
        drop(release);
        lane.replay_rest();
        // A panic on the worker has already been reported by the panic
        // hook; what matters here is which steps it never reported, and
        // the loop below fails those.
        let _ = worker.join();

        let mut results = lane.results;
        for (index, step) in background {
            if let Some(slot) = results.get_mut(*index)
                && slot.is_none()
            {
                render.start_background(*index, step.title);
                ui::error(&unreported_error());
                let result = StepResult {
                    title: step.title,
                    error: Some(format!("{UNREPORTED}; {UNREPORTED_HINT}")),
                };
                render.end(*index, &result);
                *slot = Some(result);
            }
        }
        Some(results)
    });

    match results {
        Some(results) => results.into_iter().flatten().collect(),
        None => execute_sequential(run, deps, render),
    }
}

/// The background lane: run each step in plan order once its foreground
/// requirements have finished, capturing its output and sending the
/// outcome to the main thread.
fn run_background(
    steps: &[(usize, Step)],
    deps: &SetupDeps,
    run: &Run,
    completed: &Completed,
    tx: &mpsc::Sender<Finished>,
) {
    for (index, step) in steps {
        completed.wait_for(step.requires);
        let (result, transcript) = ui::capture(|| run_step(step, deps, run));
        let finished = Finished {
            index: *index,
            result,
            transcript,
        };
        if tx.send(finished).is_err() {
            // The main thread stopped listening; nobody would see the rest.
            return;
        }
    }
}

/// The main thread's view of an overlapped run: results so far, and the
/// background steps that have not reported yet.
struct ForegroundLane<'r> {
    results: Vec<Option<StepResult>>,
    unreported: HashSet<&'static str>,
    rx: mpsc::Receiver<Finished>,
    render: &'r mut Render,
}

impl ForegroundLane<'_> {
    /// Record a background step's outcome and replay its output.
    fn record(&mut self, finished: Finished) {
        let title = finished.result.title;
        self.unreported.remove(title);
        self.render.start_background(finished.index, title);
        ui::replay(&finished.transcript);
        self.render.end(finished.index, &finished.result);
        if let Some(slot) = self.results.get_mut(finished.index) {
            *slot = Some(finished.result);
        }
    }

    /// Block until none of `requires` is an unreported background step,
    /// replaying outcomes as they arrive. Stops waiting if the worker has
    /// gone: what it never reported is failed at the end.
    fn wait_for(&mut self, requires: &[&str]) {
        while requires.iter().any(|title| self.unreported.contains(title)) {
            match self.rx.recv() {
                Ok(finished) => self.record(finished),
                Err(mpsc::RecvError) => return,
            }
        }
    }

    /// Replay every outcome that has already arrived, without blocking.
    fn replay_arrivals(&mut self) {
        while let Ok(finished) = self.rx.try_recv() {
            self.record(finished);
        }
    }

    /// Replay outcomes until the worker is done.
    fn replay_rest(&mut self) {
        while let Ok(finished) = self.rx.recv() {
            self.record(finished);
        }
    }
}

/// The error shown for a background step that never reported.
fn unreported_error() -> anyhow::Error {
    Err::<(), _>(anyhow!(UNREPORTED))
        .hint(UNREPORTED_HINT)
        .err()
        .unwrap_or_else(|| anyhow!(UNREPORTED))
}

/// Run one step's actions in order, stopping the step at the first error.
fn run_actions(step: &Step, deps: &SetupDeps, run: &Run) -> anyhow::Result<()> {
    for action in step.actions {
        match action {
            Action::Install(tool) => {
                let installer = (deps.installer_lookup)(tool)?;
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
            Action::AptPrelude => {
                let covered = prelude::run(run.selected, deps, run.lists_refreshed())?;
                run.cover(covered);
            }
            Action::StepPackages(name) => {
                let what = format!("installing {name}");
                if let AptShare::Packages(packages) = step.apt {
                    if run.is_covered(step.title) {
                        ui::println(&format!(
                            "{name}'s packages were installed with the system libraries"
                        ));
                    } else {
                        let mut args = vec!["install", "-y"];
                        args.extend_from_slice(packages);
                        deps.install.run_apt_step(&what, &args)?;
                    }
                }
            }
            Action::Server(action) => {
                server::run(action, deps.server_host, deps.install.runner)?;
            }
            Action::SystemUpdates => system_updates(deps, run)?,
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
            Some(error) => out.push_str(&format!("  ✗ {}: {error}\n", result.title)),
        }
    }
    out
}

/// End the run: the summary block, then turn any failed step into the
/// run's error (exit code 1), whose hint names `profile`'s command.
pub fn finish(profile: Profile, results: &[StepResult]) -> anyhow::Result<()> {
    if results.is_empty() {
        ui::println("no steps selected; nothing to do");
        return Ok(());
    }
    ui::println(summary(results).trim_end());
    if failed_count(results) == 0 {
        ui::success("setup complete");
    }
    outcome(profile, results)
}

/// End a compact run: the summary block (which compact mode sends to the
/// log only), then on the terminal the one-line tally and `next_steps`,
/// the warnings the steps raised. `quiet` keeps the tally off the terminal;
/// the next steps reach it regardless and stay out of the log, which holds
/// each warning as it was raised. The outcome is the same as [`finish`]'s.
fn finish_compact(
    profile: Profile,
    results: &[StepResult],
    next_steps: &[String],
    quiet: bool,
) -> anyhow::Result<()> {
    if results.is_empty() {
        return finish(profile, results);
    }
    ui::println(summary(results).trim_end());
    let succeeded = results.len() - failed_count(results);
    ui::status_line(
        &tally(succeeded, results.len(), ui::log_path().as_deref()),
        quiet,
    );
    if !next_steps.is_empty() {
        ui::report("Next steps:");
        for step in next_steps {
            ui::report(&format!("  - {step}"));
        }
    }
    outcome(profile, results)
}

/// How many steps failed.
fn failed_count(results: &[StepResult]) -> usize {
    results.iter().filter(|r| r.error.is_some()).count()
}

/// Turn any failed step into the run's error (exit code 1).
fn outcome(profile: Profile, results: &[StepResult]) -> anyhow::Result<()> {
    let failed = failed_count(results);
    if failed == 0 {
        Ok(())
    } else {
        Err(anyhow!("{failed} of {} setup steps failed", results.len())).hint(format!(
            "fix the reported problems and re-run `{}`; completed steps are idempotent and will no-op",
            rerun_command(profile)
        ))
    }
}

/// The command that re-runs `profile`'s setup.
fn rerun_command(profile: Profile) -> &'static str {
    match profile {
        Profile::Dev => "hpds setup",
        Profile::Server => "hpds setup --profile server",
    }
}

/// The closing line of a compact run: how many of `total` steps succeeded,
/// and where the log is, if there is one.
fn tally(succeeded: usize, total: usize, log: Option<&Path>) -> String {
    let steps = if total == 1 { "step" } else { "steps" };
    let count = if succeeded == total {
        format!("All {total} {steps} succeeded.")
    } else {
        format!("{succeeded} of {total} {steps} succeeded.")
    };
    match log {
        Some(path) => format!("{count} Details: {}", path.display()),
        None => count,
    }
}

/// The first line a run shows: the profile, how many steps will run, and
/// where the log is, if there is one.
fn opening_line(profile: Profile, steps: usize, log: Option<&Path>) -> String {
    let noun = if steps == 1 { "step" } else { "steps" };
    let log = match log {
        Some(path) => format!("Full log: {}", path.display()),
        None => "No log file.".to_string(),
    };
    format!(
        "hpds setup --profile {}: {steps} {noun}. {log}",
        profile.name()
    )
}

/// What a run's log header records besides the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    /// The hpds version.
    pub version: String,
    /// When the run started, as [`utc_timestamp`] formats it.
    pub started: String,
    /// The operating system and CPU architecture.
    pub os: String,
}

impl RunInfo {
    /// This process: its version, the current time, and the host. A clock
    /// set before 1970 gives `unknown time`.
    pub fn current() -> Self {
        let started = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(elapsed) => utc_timestamp(elapsed.as_secs()),
            Err(_) => "unknown time".to_string(),
        };
        RunInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            started,
            os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }
}

/// `secs` seconds after the Unix epoch, as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// The date comes from the day count by the proleptic Gregorian
/// civil-from-days conversion, in 400-year eras of 146,097 days counted
/// from 0000-03-01, so leap days fall at the end of each era-year.
pub fn utc_timestamp(secs: u64) -> String {
    let days = secs / 86_400;
    let of_day = secs % 86_400;
    let (hour, minute, second) = (of_day / 3600, of_day % 3600 / 60, of_day % 60);

    // Days since 0000-03-01.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // Months counted from March.
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Where a run's log goes, and what its header records.
#[derive(Debug, Clone)]
pub struct SetupLog {
    /// The log file, appended to. `None` when no path could be resolved
    /// (`HOME` is unset); the run then continues without a log.
    pub path: Option<PathBuf>,
    pub info: RunInfo,
}

/// The dev-profile flow: checklist, execute, summarize.
pub fn run_dev(
    deps: &SetupDeps,
    yes: bool,
    interactive: bool,
    log: &SetupLog,
) -> anyhow::Result<()> {
    run_profile(
        Profile::Dev,
        steps(Profile::Dev),
        deps,
        yes,
        interactive,
        log,
    )
}

/// The server-profile flow: plan, checklist, confirm, execute, summarize.
/// The caller has already verified the host is Linux; nothing runs until
/// the confirmation gate passes (`--yes` skips it, and a non-interactive
/// session without `--yes` refuses). Under `--yes` the background lane
/// overlaps the foreground; an interactive run keeps every step on the
/// main thread, where it may prompt.
pub fn run_server(
    deps: &SetupDeps,
    yes: bool,
    interactive: bool,
    log: &SetupLog,
) -> anyhow::Result<()> {
    run_profile(
        Profile::Server,
        steps(Profile::Server),
        deps,
        yes,
        interactive,
        log,
    )
}

/// Run `all` (the profile's step table) under `profile`'s flow.
///
/// The log at `log.path` is opened (appending) and begins with a run
/// header; when it cannot be opened, a warning says so and the run
/// continues without one. Unless `-v` is on, the terminal is compact: an
/// opening line naming the log, one `[n/N] title ✓`/`✗` line per step
/// (with a failed step's error indented under it), a tally, and the
/// deduplicated warnings as next steps. Everything, at full verbosity,
/// goes to the log. The server plan goes to the log only under a compact
/// `--yes` run, and to the terminal as well under `-v` or before the
/// checklist of an interactive run. `--quiet` keeps the opening line, the
/// step lines, and the tally off the terminal, never a failed step's error
/// or the next steps.
fn run_profile(
    profile: Profile,
    all: &[Step],
    deps: &SetupDeps,
    yes: bool,
    interactive: bool,
    log: &SetupLog,
) -> anyhow::Result<()> {
    run_profile_with(profile, all, deps, yes, interactive, log, ui::is_quiet())
}

/// [`run_profile`] with `--quiet` given as `quiet` rather than read from
/// the process-wide setting.
fn run_profile_with(
    profile: Profile,
    all: &[Step],
    deps: &SetupDeps,
    yes: bool,
    interactive: bool,
    log: &SetupLog,
    quiet: bool,
) -> anyhow::Result<()> {
    let open = OpenLog::open(log.path.as_deref());
    write_run_header(profile, &log.info);
    let server = profile == Profile::Server;
    if server {
        let plan = plan_for(profile, all);
        if yes && !deps.install.verbose {
            ui::log_detail(plan.trim_end());
        } else {
            ui::println(plan.trim_end());
        }
    }
    let selected = choose_steps(all, yes, interactive)?;
    if selected.is_empty() {
        return finish(profile, &[]);
    }
    if server && !yes {
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
    ui::status_line(&opening_line(profile, selected.len(), open.path()), quiet);
    let overlap = server && yes;
    if deps.install.verbose {
        return finish(profile, &execute(&selected, deps, overlap));
    }
    let compact = ui::compact();
    let (results, next_steps) = execute_rendered(&selected, deps, overlap, quiet);
    let outcome = finish_compact(profile, &results, &next_steps, quiet);
    // Compact mode ends before the caller renders the run's error.
    drop(compact);
    outcome
}

/// The calling thread's setup log while it lives: opened (appending) on
/// creation, closed on drop, so every return path closes it.
struct OpenLog {
    /// The open log, or `None` when the run has no log.
    path: Option<PathBuf>,
}

impl OpenLog {
    /// Open the log at `path`. When there is no path, or the file cannot be
    /// opened, warn and carry on without a log.
    fn open(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            ui::warn(
                "HOME is not set, so setup has nowhere to keep its log; continuing without a log",
            );
            return OpenLog { path: None };
        };
        match ui::open_log(path) {
            Ok(()) => OpenLog {
                path: Some(path.to_path_buf()),
            },
            Err(err) => {
                ui::warn(&format!(
                    "could not open the setup log {}: {err}; continuing without a log",
                    path.display()
                ));
                OpenLog { path: None }
            }
        }
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Drop for OpenLog {
    fn drop(&mut self) {
        if self.path.is_some() {
            ui::close_log();
        }
    }
}

/// Begin a run's section of the log: the profile, then what `info` records.
fn write_run_header(profile: Profile, info: &RunInfo) {
    ui::log_detail(&format!("=== hpds setup --profile {} ===", profile.name()));
    ui::log_detail(&format!("hpds version: {}", info.version));
    ui::log_detail(&format!("started: {}", info.started));
    ui::log_detail(&format!("os: {}", info.os));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;

    use super::*;
    use crate::install::DownloadedPackage;
    use crate::install::installers::rstudio_server::{
        RstudioBackend, RstudioRelease, RstudioServer,
    };
    use crate::install::registry;
    use crate::install::test_support::{FakeFetcher, FakeRunner, PanicFetcher, probe_fixture};
    use crate::install::{CommandOutput, CommandRunner, LoggingRunner, ReleaseFetcher};
    use crate::tools::Os;
    use crate::ui::render_error;

    struct NoServerHost;
    impl server::ServerHost for NoServerHost {
        fn current_user(&self) -> anyhow::Result<String> {
            Ok("analyst".to_string())
        }
        fn home_dir(&self, _user: &str) -> anyhow::Result<std::path::PathBuf> {
            Ok("/home/analyst".into())
        }
        fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
            if path == Path::new("/etc/os-release") {
                Ok(Some(b"ID=ubuntu\nVERSION_CODENAME=noble\n".to_vec()))
            } else {
                Ok(None)
            }
        }
        fn exists(&self, path: &Path) -> bool {
            path == Path::new("/home/analyst/.local/bin/quarto")
        }
        fn write_atomic(&self, _path: &Path, _bytes: &[u8], _mode: u32) -> anyhow::Result<()> {
            Ok(())
        }
        fn acquire_verified_rustup_init(&self) -> anyhow::Result<std::path::PathBuf> {
            Ok("/verified-download/rustup-init".into())
        }
        fn acquire_docker_key(
            &self,
            _runner: &dyn crate::install::CommandRunner,
        ) -> anyhow::Result<Vec<u8>> {
            Ok(b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nfixture\n-----END PGP PUBLIC KEY BLOCK-----\n".to_vec())
        }
        fn acquire_code_server_package(&self) -> anyhow::Result<server::CodeServerPackage> {
            Err(anyhow!("this host downloads nothing"))
        }
        fn reboot_required_path(&self) -> std::path::PathBuf {
            // Never the real marker, so a pending reboot on the machine
            // running the tests cannot leak into them.
            NO_REBOOT_MARKER.into()
        }
    }
    /// A reboot marker path that exists on no machine.
    const NO_REBOOT_MARKER: &str = "/nonexistent/hpds-test/reboot-required";
    static NO_SERVER_HOST: NoServerHost = NoServerHost;

    /// `SetupDeps` on `os` whose git-setup seam records itself in the
    /// runner's call log, so step ordering is visible in one place.
    fn deps_recording_git<'a>(
        os: Os,
        yes: bool,
        runner: &'a (dyn CommandRunner + Sync),
        fetcher: &'a (dyn ReleaseFetcher + Sync),
        git_setup: &'a (dyn Fn() -> anyhow::Result<()> + Sync),
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
            installer_lookup: &registry::find,
            server_host: &NO_SERVER_HOST,
        }
    }

    /// The run header the log tests write: fixed, so logs are stable.
    fn test_info() -> RunInfo {
        RunInfo {
            version: "9.9.9".to_string(),
            started: "2026-11-03 21:15:00 UTC".to_string(),
            os: "linux x86_64".to_string(),
        }
    }

    /// A [`SetupLog`] at `path` with the fixed test header.
    fn test_log(path: Option<std::path::PathBuf>) -> SetupLog {
        SetupLog {
            path,
            info: test_info(),
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
            .on_path("togi")
            .with_output("togi --version", &probe_fixture("togi.txt"))
    }

    // --- thread safety -----------------------------------------------------

    /// Compiles only while the setup dependencies can move to another
    /// thread, which overlapping steps on a worker thread relies on.
    #[test]
    fn setup_dependencies_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SetupDeps<'static>>();
        assert_send::<InstallCtx<'static>>();
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

    #[test]
    fn server_uses_the_registered_rstudio_server_installer() {
        let step = steps(Profile::Server)
            .iter()
            .find(|step| step.title == "RStudio Server")
            .expect("RStudio Server step");
        assert!(matches!(step.actions, [Action::Install("rstudio-server")]));
    }

    #[test]
    fn server_profile_includes_complete_machine_provisioning() {
        let text = plan(Profile::Server);
        for required in [
            "Docker Engine",
            "download.docker.com/linux/ubuntu",
            "docker-ce",
            "docker-ce-cli",
            "containerd.io",
            "docker-buildx-plugin",
            "docker-compose-plugin",
            "docker group",
            "stable Rust",
            "rustup",
            "RStudio preferences",
            "insert_native_pipe_operator=true",
            "save_workspace=never",
            "load_workspace=never",
            "rainbow_parentheses=true",
            "rainbow_fenced_divs=true",
            "code-server@<user>",
            "enable --now",
            "quarto.path",
            "chat.disableAIFeatures=true",
            "telemetry.telemetryLevel=off",
            ".local/bin/quarto",
        ] {
            assert!(text.contains(required), "missing {required}:\n{text}");
        }
        assert!(
            text.lines().any(|line| {
                [
                    "ms-python.python",
                    "ms-toolsai.jupyter",
                    "quarto.quarto",
                    "charliermarsh.ruff",
                    "tomoki1207.pdf",
                ]
                .iter()
                .all(|id| line.contains(id))
            }),
            "one plan line installs all five extensions:\n{text}"
        );
        let lowercase = text.to_ascii_lowercase();
        for omitted in [
            "miniconda",
            "conda",
            "dist-upgrade",
            "full-upgrade",
            "sudo reboot",
            "systemctl reboot",
            "shutdown",
        ] {
            assert!(
                !lowercase.contains(omitted),
                "unexpected {omitted}:\n{text}"
            );
        }
    }

    // --- checklist gating ---------------------------------------------------

    #[test]
    fn yes_takes_every_step_without_prompting() {
        let selected = choose_steps(steps(Profile::Dev), true, false).expect("--yes never asks");
        let titles: Vec<_> = selected.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            vec![
                "r",
                "quarto",
                "uv",
                "gh",
                "rig",
                "tinytex",
                "togi",
                "git setup"
            ]
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
        let fetcher = FakeFetcher::default().with_latest("0.1.0");
        let git_setup = || {
            runner.record("hpds git setup");
            Ok(())
        };
        let deps = deps_recording_git(Os::Mac, true, &runner, &fetcher, &git_setup);
        let dir = tempfile::tempdir().expect("create temp dir");
        let log = test_log(Some(dir.path().join("hpds-setup.log")));

        let (result, _) = ui::tap_terminal(|| run_dev(&deps, true, false, &log));
        result.expect("every step no-ops successfully");

        // Each install step probes its tool in profile order, and the git
        // seam runs last: the full step sequence, seen through the runner.
        assert_eq!(
            runner.calls(),
            vec![
                "R --version",
                "quarto --version",
                "uv --version",
                "gh --version",
                "rig --version",
                "quarto list tools",
                "togi --version",
                "hpds git setup",
            ]
        );
        assert_eq!(
            fetcher.latest_calls(),
            vec!["quarto", "uv", "gh", "togi"],
            "setup resolves each dynamic tool once during execution"
        );
    }

    #[test]
    fn a_failing_step_reports_and_the_rest_still_run() {
        // gh is missing and its installer cannot complete (the fake
        // fetcher "downloads" a binary that never lands on PATH), so the
        // gh step fails; every later step must still run.
        let fetcher = FakeFetcher::default().with_latest("0.1.0");
        let git_ran = AtomicBool::new(false);
        let git_setup = || {
            git_ran.store(true, Ordering::SeqCst);
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
            .with_output("rig --version", &probe_fixture("rig.txt"))
            .on_path("togi")
            .with_output("togi --version", &probe_fixture("togi.txt"));
        let deps = deps_recording_git(Os::Mac, true, &runner, &fetcher, &git_setup);

        let results = execute(steps(Profile::Dev), &deps, false);

        let failures: Vec<_> = results
            .iter()
            .filter(|r| r.error.is_some())
            .map(|r| r.title)
            .collect();
        assert_eq!(failures, vec!["gh"], "only the gh step may fail");
        assert!(
            git_ran.load(Ordering::SeqCst),
            "steps after the failure must still run"
        );
        assert!(
            runner.calls().iter().any(|c| c == "rig --version"),
            "steps after the failure must still run: {:?}",
            runner.calls()
        );

        let err = finish(Profile::Dev, &results).expect_err("a failed step fails the run");
        assert!(err.to_string().contains("1 of 8"), "{err}");
    }

    // --- server profile -------------------------------------------------------

    #[test]
    fn server_non_interactive_without_yes_refuses_before_any_step_runs() {
        let runner = FakeRunner::default();
        let git_setup = || panic!("no step may run when confirmation is refused");
        let deps = deps_recording_git(Os::Linux, false, &runner, &PanicFetcher, &git_setup);
        let dir = tempfile::tempdir().expect("create temp dir");
        let log_path = dir.path().join("hpds-setup.log");

        let (result, _) =
            ui::tap_terminal(|| run_server(&deps, false, false, &test_log(Some(log_path.clone()))));
        let err = result.expect_err("must refuse without --yes");

        let out = render_error(&err, false);
        assert!(out.contains("--yes"), "{out}");
        assert!(
            runner.calls().is_empty(),
            "zero commands may run: {:?}",
            runner.calls()
        );
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(!log.contains("setup summary:"), "{log}");
    }

    // --- a simulated server -------------------------------------------------

    const RSTUDIO_LATEST: &str = "2026.09.0+174";
    const RSTUDIO_DEB: &str = "/tmp/hpds-rstudio-server-sim.deb";
    const CODE_SERVER_DEB: &str = "/tmp/hpds-code-server-sim.deb";
    const RIG_KEY: &str =
        "sudo curl -L https://rig.r-pkg.org/deb/rig.gpg -o /etc/apt/trusted.gpg.d/rig.gpg";
    const DOCKER_SOURCES: &str = "/etc/apt/sources.list.d/docker.sources";
    const RIG_SOURCES: &str = "/etc/apt/sources.list.d/rig.list";
    const RIG_ADD: &str =
        "sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 rig add release";
    const RSTUDIO_IS_ACTIVE: &str = "systemctl is-active rstudio-server";
    const RSTUDIO_RESET: &str = "sudo systemctl reset-failed rstudio-server";
    const RSTUDIO_RESTART: &str = "sudo systemctl restart rstudio-server";

    /// A fresh Ubuntu server, simulated through the runner seam: the user
    /// tools the dev profile manages probe as installed, every command
    /// succeeds unless marked failing, and the server packages (Docker,
    /// rig, R, RStudio Server, code-server) become installed when the
    /// command that installs them succeeds, so later probes see them.
    #[derive(Default)]
    struct Sim {
        calls: Mutex<Vec<String>>,
        installed: Mutex<HashSet<&'static str>>,
        failing: Vec<(String, &'static str)>,
        /// Third-party apt sources (`docker`, `rig`) that make
        /// `apt-get update` fail while they are registered.
        broken_sources: Vec<&'static str>,
        /// The third-party apt sources currently registered.
        sources: Mutex<HashSet<&'static str>>,
        /// Where the first `apt-get update` meets the package downloads.
        meet: Option<Arc<Rendezvous>>,
        /// Each package file an `apt-get install` named, and whether it
        /// was on disk when the install ran.
        package_files: Mutex<Vec<(String, bool)>>,
        /// Whether systemd reports `rstudio-server` active. A restart
        /// starts it only once R is installed, as `rserver` needs R.
        rstudio_active: Mutex<bool>,
        /// A restart reports success but the service does not stay up.
        rstudio_never_starts: bool,
    }

    impl Sim {
        /// A server where the server packages are already installed.
        fn provisioned() -> Self {
            let sim = Sim::default();
            sim.installed.lock().unwrap().extend([
                "docker",
                "rig",
                "R",
                "rstudio-server",
                "code-server",
            ]);
            sim
        }

        /// `apt-get update` fails while the `source` repository
        /// (`docker` or `rig`) is registered.
        fn broken_source(mut self, source: &'static str) -> Self {
            self.broken_sources.push(source);
            self
        }

        /// The `source` repository was registered before this run.
        fn with_source(self, source: &'static str) -> Self {
            self.sources.lock().unwrap().insert(source);
            self
        }

        /// What `command` does to the registered sources.
        fn track_sources(&self, command: &str) {
            let mut sources = self.sources.lock().unwrap();
            match command {
                "sudo install -m 0755 -d /etc/apt/keyrings" => {
                    sources.insert("docker");
                }
                c if c.starts_with("sudo sh -c echo 'deb http://rig.r-pkg.org/deb") => {
                    sources.insert("rig");
                }
                c if c == format!("sudo rm -f {DOCKER_SOURCES}") => {
                    sources.remove("docker");
                }
                c if c == format!("sudo rm -f {RIG_SOURCES}") => {
                    sources.remove("rig");
                }
                _ => {}
            }
        }

        /// `command` fails with `stderr` and installs nothing.
        fn failing(mut self, command: &str, stderr: &'static str) -> Self {
            self.failing.push((command.to_string(), stderr));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn package_files(&self) -> Vec<(String, bool)> {
            self.package_files.lock().unwrap().clone()
        }

        fn record(&self, call: &str) {
            self.calls.lock().unwrap().push(call.to_string());
        }

        fn has(&self, package: &str) -> bool {
            self.installed.lock().unwrap().contains(package)
        }

        /// The apt-get calls, in order.
        fn apt_calls(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|call| call.contains("apt-get"))
                .collect()
        }

        fn ok(stdout: &str) -> CommandOutput {
            CommandOutput {
                success: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        }

        /// What a successful `command` installs.
        fn installs(command: &str) -> Vec<&'static str> {
            let mut installed = Vec::new();
            if command == RIG_ADD {
                installed.push("R");
            }
            if command.starts_with(&install::apt_get_command_line(&["install"])) {
                for word in command.split_whitespace() {
                    match word {
                        "docker-ce" => installed.push("docker"),
                        "r-rig" => installed.push("rig"),
                        // The fixed download path, or a private temporary
                        // file named after it.
                        w if w.contains("hpds-rstudio-server-sim") && w.ends_with(".deb") => {
                            installed.push("rstudio-server")
                        }
                        CODE_SERVER_DEB => installed.push("code-server"),
                        _ => {}
                    }
                }
            }
            installed
        }
    }

    impl CommandRunner for Sim {
        fn which(&self, program: &str) -> Option<std::path::PathBuf> {
            let present = match program {
                "quarto" | "uv" | "gh" | "duckdb" | "togi" | "apt-get" => true,
                "R" | "rig" | "code-server" | "rstudio-server" => self.has(program),
                _ => false,
            };
            present.then(|| std::path::PathBuf::from("/usr/bin").join(program))
        }

        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
            let command = format!("{program} {}", args.join(" "));
            self.record(&command);
            if command == install::apt_get_command_line(&["update"])
                && let Some(meet) = &self.meet
            {
                meet.arrive(REFRESH);
            }
            if command.starts_with(&install::apt_get_command_line(&["install"])) {
                self.package_files.lock().unwrap().extend(
                    args.iter()
                        .filter(|arg| arg.ends_with(".deb"))
                        .map(|arg| (arg.to_string(), Path::new(arg).exists())),
                );
            }
            if command == install::apt_get_command_line(&["update"])
                && let Some(broken) = self
                    .broken_sources
                    .iter()
                    .find(|source| self.sources.lock().unwrap().contains(*source))
            {
                return Ok(CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("E: The {broken} repository is not signed."),
                });
            }
            self.track_sources(&command);
            if let Some((_, stderr)) = self.failing.iter().find(|(c, _)| *c == command) {
                return Ok(CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                });
            }
            let output = match command.as_str() {
                RSTUDIO_IS_ACTIVE => {
                    if *self.rstudio_active.lock().unwrap() {
                        Sim::ok("active\n")
                    } else {
                        CommandOutput {
                            success: false,
                            stdout: "failed\n".to_string(),
                            stderr: String::new(),
                        }
                    }
                }
                RSTUDIO_RESTART if !self.has("R") => CommandOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "Job for rstudio-server.service failed because the control process exited with error code.".to_string(),
                },
                RSTUDIO_RESTART => {
                    if !self.rstudio_never_starts {
                        *self.rstudio_active.lock().unwrap() = true;
                    }
                    Sim::ok("")
                }
                c if c.starts_with("dpkg-query -W") => {
                    if self.has("docker") {
                        Sim::ok(&"install ok installed\n".repeat(5))
                    } else {
                        CommandOutput {
                            success: false,
                            stdout: String::new(),
                            stderr: "dpkg-query: no packages found".to_string(),
                        }
                    }
                }
                c if c.starts_with("dpkg-deb --field") && c.contains(CODE_SERVER_DEB) => {
                    Sim::ok("Package: code-server\nArchitecture: amd64\nVersion: 4.141.0\n")
                }
                c if c == format!("test -e {RIG_SOURCES}") => CommandOutput {
                    success: self.sources.lock().unwrap().contains("rig"),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                "dpkg --print-architecture" => Sim::ok("amd64\n"),
                "id -nG analyst" => Sim::ok("analyst sudo"),
                "R --version" => Sim::ok(&probe_fixture("r.txt")),
                "rig --version" => Sim::ok(&probe_fixture("rig.txt")),
                "quarto --version" => Sim::ok(&probe_fixture("quarto.txt")),
                "uv --version" => Sim::ok(&probe_fixture("uv.txt")),
                "gh --version" => Sim::ok(&probe_fixture("gh.txt")),
                "duckdb --version" => Sim::ok(&probe_fixture("duckdb.txt")),
                "togi --version" => Sim::ok(&probe_fixture("togi.txt")),
                "quarto list tools" => Sim::ok(&crate::install::test_support::tool_output_fixture(
                    "tinytex/quarto-list-tools-managed.txt",
                )),
                _ => Sim::ok(""),
            };
            self.installed
                .lock()
                .unwrap()
                .extend(Sim::installs(&command));
            Ok(output)
        }
    }

    /// The simulated server's host side: an Ubuntu noble machine with the
    /// managed Quarto launcher, counting the downloads it is asked for.
    #[derive(Default)]
    struct SimHost {
        /// Files that existed before this run.
        existing: Vec<&'static str>,
        docker_key_fails: bool,
        docker_keys: Mutex<usize>,
        code_server_fails: bool,
        /// Print a line when the code-server download starts.
        announce: bool,
        /// Where the code-server download meets the other downloads and
        /// the refresh.
        meet: Option<Arc<Rendezvous>>,
        code_server_downloads: Mutex<usize>,
        /// The name of the thread each code-server download ran on.
        code_server_threads: Mutex<Vec<Option<String>>>,
        writes: Mutex<Vec<std::path::PathBuf>>,
        /// The reboot marker to check, when not [`NO_REBOOT_MARKER`]: a
        /// real file in the tests that want a pending reboot.
        reboot_required: Option<std::path::PathBuf>,
    }

    impl SimHost {
        fn docker_keys(&self) -> usize {
            *self.docker_keys.lock().unwrap()
        }

        fn code_server_downloads(&self) -> usize {
            *self.code_server_downloads.lock().unwrap()
        }

        fn code_server_threads(&self) -> Vec<Option<String>> {
            self.code_server_threads.lock().unwrap().clone()
        }

        fn writes(&self) -> Vec<std::path::PathBuf> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl server::ServerHost for SimHost {
        fn current_user(&self) -> anyhow::Result<String> {
            NoServerHost.current_user()
        }
        fn home_dir(&self, user: &str) -> anyhow::Result<std::path::PathBuf> {
            NoServerHost.home_dir(user)
        }
        fn read(&self, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
            if self.existing.iter().any(|file| Path::new(file) == path) {
                return Ok(Some(b"# written before this run\n".to_vec()));
            }
            NoServerHost.read(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.existing.iter().any(|file| Path::new(file) == path)
                || NoServerHost.exists(path)
                || (self.reboot_required.as_deref() == Some(path) && path.exists())
        }
        fn write_atomic(&self, path: &Path, _bytes: &[u8], _mode: u32) -> anyhow::Result<()> {
            self.writes.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
        fn acquire_verified_rustup_init(&self) -> anyhow::Result<std::path::PathBuf> {
            NoServerHost.acquire_verified_rustup_init()
        }
        fn acquire_docker_key(
            &self,
            runner: &dyn crate::install::CommandRunner,
        ) -> anyhow::Result<Vec<u8>> {
            *self.docker_keys.lock().unwrap() += 1;
            if self.docker_key_fails {
                Err(anyhow!("Docker key download interrupted"))
            } else {
                NoServerHost.acquire_docker_key(runner)
            }
        }
        fn acquire_code_server_package(&self) -> anyhow::Result<server::CodeServerPackage> {
            *self.code_server_downloads.lock().unwrap() += 1;
            self.code_server_threads
                .lock()
                .unwrap()
                .push(current_thread_name());
            if self.announce {
                ui::println("downloading code-server (simulated)");
            }
            if let Some(meet) = &self.meet {
                meet.arrive(CODE_SERVER_DOWNLOAD);
            }
            if self.code_server_fails {
                return Err(anyhow!("code-server download interrupted"));
            }
            Ok(server::CodeServerPackage {
                package: DownloadedPackage::at(CODE_SERVER_DEB.into()),
                version: "4.141.0".to_string(),
            })
        }
        fn reboot_required_path(&self) -> std::path::PathBuf {
            self.reboot_required
                .clone()
                .unwrap_or_else(|| NoServerHost.reboot_required_path())
        }
    }

    /// RStudio Server against the simulated server: installed when
    /// `rstudio-server` is on its PATH, downloaded to a fixed path, and
    /// installed with the shared apt policy, like the real backend.
    #[derive(Default)]
    struct SimRstudio {
        download_fails: bool,
        /// The first download panics instead of returning.
        first_download_panics: AtomicBool,
        /// Where the download lands, when not [`RSTUDIO_DEB`].
        download_path: Option<std::path::PathBuf>,
        /// Download into a real private temporary file instead.
        temporary: bool,
        /// Print a line when a download starts.
        announce: bool,
        /// How long a download takes.
        delay: Option<Duration>,
        /// Where the download meets the other download and the refresh.
        meet: Option<Arc<Rendezvous>>,
        downloads: Mutex<usize>,
        /// The temporary files the downloads created.
        created: Mutex<Vec<std::path::PathBuf>>,
        /// The name of the thread each release lookup ran on.
        lookup_threads: Mutex<Vec<Option<String>>>,
        /// Whether systemd runs the simulated host.
        systemd: bool,
    }

    impl SimRstudio {
        fn downloads(&self) -> usize {
            *self.downloads.lock().unwrap()
        }

        fn created(&self) -> Vec<std::path::PathBuf> {
            self.created.lock().unwrap().clone()
        }

        fn lookup_threads(&self) -> Vec<Option<String>> {
            self.lookup_threads.lock().unwrap().clone()
        }
    }

    fn current_thread_name() -> Option<String> {
        std::thread::current().name().map(str::to_string)
    }

    const RSTUDIO_DOWNLOAD: &str = "RStudio Server download";
    const CODE_SERVER_DOWNLOAD: &str = "code-server download";
    const REFRESH: &str = "apt-get update";

    /// A meeting point for the two package downloads and the first
    /// package-list refresh. Each party waits, bounded, until all three
    /// have arrived, and records whether they did: three parties that
    /// all met were running at the same time.
    struct Rendezvous {
        arrived: Mutex<Vec<&'static str>>,
        changed: Condvar,
        met: Mutex<Vec<(&'static str, bool)>>,
        timeout: Duration,
    }

    impl Rendezvous {
        const PARTIES: [&'static str; 3] = [RSTUDIO_DOWNLOAD, CODE_SERVER_DOWNLOAD, REFRESH];

        fn new(timeout: Duration) -> Arc<Self> {
            Arc::new(Rendezvous {
                arrived: Mutex::new(Vec::new()),
                changed: Condvar::new(),
                met: Mutex::new(Vec::new()),
                timeout,
            })
        }

        /// `party` arrives and waits for the others. A party that has
        /// already arrived once does not wait again.
        fn arrive(&self, party: &'static str) {
            let mut arrived = self.arrived.lock().unwrap();
            if arrived.contains(&party) {
                return;
            }
            arrived.push(party);
            self.changed.notify_all();
            let (arrived, wait) = self
                .changed
                .wait_timeout_while(arrived, self.timeout, |arrived| {
                    !Self::PARTIES.iter().all(|other| arrived.contains(other))
                })
                .unwrap();
            drop(arrived);
            self.met.lock().unwrap().push((party, !wait.timed_out()));
        }

        /// Each party and whether it met the others, in party order.
        fn met(&self) -> Vec<(&'static str, bool)> {
            let mut met = self.met.lock().unwrap().clone();
            met.sort_by_key(|(party, _)| Self::PARTIES.iter().position(|known| known == party));
            met
        }
    }

    impl RstudioBackend for SimRstudio {
        fn detect(&self, ctx: &InstallCtx<'_>) -> Option<String> {
            ctx.runner
                .which("rstudio-server")
                .map(|_| RSTUDIO_LATEST.to_string())
        }
        fn latest(&self) -> anyhow::Result<RstudioRelease> {
            self.lookup_threads
                .lock()
                .unwrap()
                .push(current_thread_name());
            Ok(RstudioRelease {
                version: RSTUDIO_LATEST.to_string(),
                filename: "rstudio-server-2026.09.0-174-amd64.deb".to_string(),
                url: "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb".to_string(),
            })
        }
        fn exact(&self, _version: &str) -> anyhow::Result<RstudioRelease> {
            unreachable!("setup never pins RStudio Server")
        }
        fn download(&self, _release: &RstudioRelease) -> anyhow::Result<DownloadedPackage> {
            *self.downloads.lock().unwrap() += 1;
            if self.announce {
                ui::println("downloading RStudio Server (simulated)");
            }
            if let Some(meet) = &self.meet {
                meet.arrive(RSTUDIO_DOWNLOAD);
            }
            if let Some(delay) = self.delay {
                std::thread::sleep(delay);
            }
            if self.first_download_panics.swap(false, Ordering::SeqCst) {
                panic!("the simulated RStudio Server download blew up");
            }
            if self.download_fails {
                return Err(anyhow!("RStudio Server download interrupted"));
            }
            if self.temporary {
                let file = tempfile::Builder::new()
                    .prefix("hpds-rstudio-server-sim-")
                    .suffix(".deb")
                    .tempfile()
                    .unwrap();
                self.created.lock().unwrap().push(file.path().to_path_buf());
                return Ok(DownloadedPackage::temporary(file));
            }
            Ok(DownloadedPackage::at(
                self.download_path
                    .clone()
                    .unwrap_or_else(|| RSTUDIO_DEB.into()),
            ))
        }
        fn inspect(&self, _ctx: &InstallCtx<'_>, _package: &Path) -> anyhow::Result<String> {
            Ok(format!(
                "Package: rstudio-server\nArchitecture: amd64\nVersion: {RSTUDIO_LATEST}\n"
            ))
        }
        fn install(&self, ctx: &InstallCtx<'_>, package: &Path) -> anyhow::Result<()> {
            let path = package
                .to_str()
                .ok_or_else(|| anyhow!("the RStudio Server package path is not UTF-8"))?;
            ctx.run_apt_step("installing RStudio Server", &["install", "-y", path])?;
            Ok(())
        }
        fn systemd_running(&self) -> bool {
            self.systemd
        }
    }

    /// How setup finds an installer by name.
    type InstallerLookup =
        dyn Fn(&str) -> Result<&'static dyn install::Installer, registry::RegistryError> + Sync;

    /// The registry, except that RStudio Server is backed by `rstudio`.
    fn sim_lookup(rstudio: SimRstudio) -> (&'static SimRstudio, &'static InstallerLookup) {
        let backend: &'static SimRstudio = Box::leak(Box::new(rstudio));
        let installer: &'static RstudioServer<&'static SimRstudio> =
            Box::leak(Box::new(RstudioServer::with_backend(backend)));
        let lookup: &'static InstallerLookup = Box::leak(Box::new(move |tool: &str| {
            if tool == "rstudio-server" {
                Ok(installer as &'static dyn install::Installer)
            } else {
                registry::find(tool)
            }
        }));
        (backend, lookup)
    }

    /// The steps of the server profile except those titled in `without`.
    fn server_steps_without(without: &[&str]) -> Vec<Step> {
        steps(Profile::Server)
            .iter()
            .filter(|step| !without.contains(&step.title))
            .copied()
            .collect()
    }

    /// The server profile without its system-updates step, so the apt
    /// prelude makes the run's first package-list refresh, as it does
    /// whenever that step is not selected.
    fn prelude_steps() -> Vec<Step> {
        server_steps_without(&[SYSTEM_UPDATES])
    }

    /// Run `selected` against the simulated server, one step at a time,
    /// with the git flow recorded in the simulation's call log.
    fn run_sim(
        sim: &Sim,
        host: &SimHost,
        rstudio: SimRstudio,
        selected: &[Step],
    ) -> (Vec<StepResult>, &'static SimRstudio) {
        let git_setup = || {
            sim.record("hpds git setup");
            Ok(())
        };
        let fetcher = FakeFetcher::default().with_latest("0.1.0");
        let (backend, lookup) = sim_lookup(rstudio);
        let mut deps = deps_recording_git(Os::Linux, true, sim, &fetcher, &git_setup);
        deps.installer_lookup = lookup;
        deps.server_host = host;
        (execute(selected, &deps, false), backend)
    }

    fn failures(results: &[StepResult]) -> Vec<(&'static str, String)> {
        results
            .iter()
            .filter_map(|result| result.error.clone().map(|error| (result.title, error)))
            .collect()
    }

    /// The `apt-get install -y` line for `packages`.
    fn apt_install(packages: &[&str]) -> String {
        let mut args = vec!["install", "-y"];
        args.extend_from_slice(packages);
        install::apt_get_command_line(&args)
    }

    /// Every package the full server profile installs in step 1, in order.
    fn merged_packages() -> Vec<&'static str> {
        let mut packages = APT_SYSTEM_LIBRARIES.to_vec();
        packages.extend_from_slice(PYTHON_PACKAGES);
        packages.extend_from_slice(server::DOCKER_PACKAGES);
        packages.push("r-rig");
        packages.push(RSTUDIO_DEB);
        packages.push(CODE_SERVER_DEB);
        packages
    }

    /// Step 1's distribution packages: the system libraries and Python.
    fn base_packages() -> Vec<&'static str> {
        let mut packages = APT_SYSTEM_LIBRARIES.to_vec();
        packages.extend_from_slice(PYTHON_PACKAGES);
        packages
    }

    #[test]
    fn server_with_yes_runs_the_command_plans_through_the_runner_and_logs() {
        let sim = Sim::default();
        let host = SimHost::default();
        let git_setup = || {
            sim.record("hpds git setup");
            Ok(())
        };
        let fetcher = FakeFetcher::default().with_latest("0.1.0");
        let (_, lookup) = sim_lookup(SimRstudio::default());
        let mut deps = deps_recording_git(Os::Linux, true, &sim, &fetcher, &git_setup);
        deps.installer_lookup = lookup;
        deps.server_host = &host;
        let dir = tempfile::tempdir().expect("create temp dir");
        let log_path = dir.path().join("hpds-setup.log");

        ui::open_log(&log_path).expect("open the log");
        let (result, _) = ui::tap_terminal(|| {
            finish(
                Profile::Server,
                &execute(steps(Profile::Server), &deps, false),
            )
        });
        ui::close_log();
        result.expect("all steps succeed");

        let calls = sim.calls();
        // System steps ran as the planned commands, under sudo where
        // declared, all through the runner seam.
        assert_eq!(
            sim.apt_calls().first(),
            Some(&install::apt_get_command_line(&["update"]))
        );
        // All five extensions install in one code-server invocation, before
        // the service is enabled.
        let extension_calls: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| c.starts_with("code-server --install-extension"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(extension_calls.len(), 1, "{calls:?}");
        assert_eq!(
            calls[extension_calls[0]],
            "code-server --install-extension ms-python.python --install-extension ms-toolsai.jupyter --install-extension quarto.quarto --install-extension charliermarsh.ruff --install-extension tomoki1207.pdf",
            "{calls:?}"
        );
        let enable = calls
            .iter()
            .position(|c| c == "sudo systemctl enable --now code-server@analyst")
            .expect("code-server service enabled");
        assert!(extension_calls[0] < enable, "{calls:?}");
        assert!(
            calls
                .iter()
                .any(|c| c == "uv tool install --upgrade sqlfluff"),
            "{calls:?}"
        );
        assert_eq!(calls.last().map(String::as_str), Some("hpds git setup"));

        let log = std::fs::read_to_string(&log_path).expect("the summary must reach the log");
        for step in steps(Profile::Server) {
            assert!(
                log.contains(step.title),
                "log must mention {}: {log}",
                step.title
            );
        }
        assert!(!log.contains('✗'), "no step failed: {log}");
    }

    // --- the apt prelude ------------------------------------------------------

    #[test]
    fn prelude_plan_takes_every_selected_steps_share() {
        let plan = prelude::prelude_plan(steps(Profile::Server));
        assert_eq!(plan.base, base_packages());
        assert_eq!(
            plan.repos,
            [
                (AptRepo::Docker, "Docker Engine"),
                (AptRepo::Rig, "R + CRAN/PPM repositories")
            ]
        );
        assert_eq!(
            plan.debs,
            [
                (LocalDeb::RstudioServer, "RStudio Server"),
                (LocalDeb::CodeServer, "code-server + extensions")
            ]
        );
    }

    #[test]
    fn prelude_plan_leaves_out_deselected_steps() {
        type LeftOut = fn(&prelude::PreludePlan) -> bool;
        let cases: [(&str, LeftOut); 5] = [
            ("Docker Engine", |plan| {
                !plan.repos.iter().any(|(repo, _)| *repo == AptRepo::Docker)
            }),
            ("R + CRAN/PPM repositories", |plan| {
                !plan.repos.iter().any(|(repo, _)| *repo == AptRepo::Rig)
            }),
            ("Python + PPM pip configuration", |plan| {
                plan.base == APT_SYSTEM_LIBRARIES
            }),
            ("RStudio Server", |plan| {
                !plan
                    .debs
                    .iter()
                    .any(|(deb, _)| *deb == LocalDeb::RstudioServer)
            }),
            ("code-server + extensions", |plan| {
                !plan
                    .debs
                    .iter()
                    .any(|(deb, _)| *deb == LocalDeb::CodeServer)
            }),
        ];
        for (title, left_out) in cases {
            let plan = prelude::prelude_plan(&server_steps_without(&[title]));
            assert!(left_out(&plan), "{title}: {plan:?}");
        }

        let only_libraries = prelude::prelude_plan(&prelude_steps()[..1]);
        assert_eq!(only_libraries.base, APT_SYSTEM_LIBRARIES);
        assert!(only_libraries.repos.is_empty() && only_libraries.debs.is_empty());
        assert_eq!(
            prelude::prelude_plan(&[]),
            prelude::PreludePlan {
                base: vec![],
                repos: vec![],
                debs: vec![]
            }
        );
    }

    #[test]
    fn a_fresh_server_installs_everything_in_one_update_and_one_install() {
        let sim = Sim::default();
        let host = SimHost::default();
        let (results, rstudio) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(
            sim.apt_calls(),
            [
                install::apt_get_command_line(&["update"]),
                apt_install(&merged_packages())
            ],
            "later steps run no apt for packages step 1 installed"
        );
        assert_eq!(host.docker_keys(), 1);
        assert_eq!(host.code_server_downloads(), 1);
        assert_eq!(rstudio.downloads(), 1);
        let calls = sim.calls();
        let install = calls
            .iter()
            .position(|call| *call == apt_install(&merged_packages()))
            .unwrap();
        let identity = calls
            .iter()
            .position(|call| call.starts_with("dpkg-deb --field") && call.contains(CODE_SERVER_DEB))
            .expect("the code-server package identity is verified");
        assert!(identity < install, "{calls:#?}");
        let rig_add = calls
            .iter()
            .position(|call| call == RIG_ADD)
            .expect("R added");
        assert!(install < rig_add, "{calls:#?}");
        assert!(
            calls
                .iter()
                .any(|call| call == "sudo usermod -aG docker analyst")
        );
        assert!(
            calls.iter().all(|call| !call.contains("install.sh")),
            "{calls:#?}"
        );
    }

    #[test]
    fn deselected_docker_fetches_no_key_writes_no_repo_and_installs_no_packages() {
        let sim = Sim::default();
        let host = SimHost::default();
        let selected = server_steps_without(&["Docker Engine"]);
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), &selected);

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(host.docker_keys(), 0);
        assert!(
            !host
                .writes()
                .iter()
                .any(|path| path.to_string_lossy().contains("docker")),
            "{:?}",
            host.writes()
        );
        assert!(
            sim.apt_calls()
                .iter()
                .all(|call| !call.contains("docker-ce"))
        );
    }

    #[test]
    fn deselected_r_registers_no_rig_repo_and_installs_no_r_rig() {
        let sim = Sim::default();
        let selected = server_steps_without(&["R + CRAN/PPM repositories", "rig"]);
        let (results, _) = run_sim(&sim, &SimHost::default(), SimRstudio::default(), &selected);

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert!(sim.calls().iter().all(|call| call != RIG_KEY));
        assert!(sim.apt_calls().iter().all(|call| !call.contains("r-rig")));
    }

    #[test]
    fn deselected_python_leaves_python_out_and_python_installs_itself_without_step_one() {
        let sim = Sim::default();
        let selected = server_steps_without(&["Python + PPM pip configuration"]);
        run_sim(&sim, &SimHost::default(), SimRstudio::default(), &selected);
        assert!(sim.apt_calls().iter().all(|call| !call.contains("python3")));

        let sim = Sim::default();
        let selected = server_steps_without(&[SYSTEM_LIBRARIES]);
        let (results, _) = run_sim(&sim, &SimHost::default(), SimRstudio::default(), &selected);
        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert!(
            sim.apt_calls().contains(&apt_install(PYTHON_PACKAGES)),
            "{:#?}",
            sim.apt_calls()
        );
    }

    #[test]
    fn deselected_ide_servers_download_nothing() {
        let sim = Sim::default();
        let host = SimHost::default();
        let selected = server_steps_without(&["RStudio Server", "code-server + extensions"]);
        let (results, rstudio) = run_sim(&sim, &host, SimRstudio::default(), &selected);

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(rstudio.downloads(), 0);
        assert_eq!(host.code_server_downloads(), 0);
        assert!(sim.apt_calls().iter().all(|call| !call.contains(".deb")));
    }

    #[test]
    fn step_one_alone_installs_only_the_system_libraries() {
        let sim = Sim::default();
        let host = SimHost::default();
        let (results, rstudio) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps()[..1]);
        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(
            sim.apt_calls(),
            [
                install::apt_get_command_line(&["update"]),
                apt_install(APT_SYSTEM_LIBRARIES)
            ]
        );
        assert_eq!((host.docker_keys(), host.code_server_downloads()), (0, 0));
        assert_eq!(rstudio.downloads(), 0);
    }

    #[test]
    fn a_docker_key_failure_leaves_step_one_passing_and_fails_docker_itself() {
        let sim = Sim::default();
        let host = SimHost {
            docker_key_fails: true,
            ..SimHost::default()
        };
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps());

        let failed = failures(&results);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, "Docker Engine");
        assert!(
            failed[0].1.contains("Docker key download interrupted"),
            "{failed:?}"
        );
        assert_eq!(host.docker_keys(), 2, "step 1 tried, then Docker retried");
        let mut without_docker = base_packages();
        without_docker.extend(["r-rig", RSTUDIO_DEB, CODE_SERVER_DEB]);
        assert!(sim.apt_calls().contains(&apt_install(&without_docker)));
    }

    #[test]
    fn an_rstudio_download_failure_is_dropped_and_rstudio_reports_its_own_error() {
        let sim = Sim::default();
        let rstudio = SimRstudio {
            download_fails: true,
            ..SimRstudio::default()
        };
        let (results, rstudio) = run_sim(&sim, &SimHost::default(), rstudio, &prelude_steps());

        let failed = failures(&results);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, "RStudio Server");
        assert!(failed[0].1.contains("download interrupted"), "{failed:?}");
        assert_eq!(rstudio.downloads(), 2, "step 1 tried, then RStudio retried");
        assert!(
            sim.apt_calls()
                .iter()
                .all(|call| !call.contains(RSTUDIO_DEB))
        );
    }

    #[test]
    fn a_failed_merged_install_retries_the_base_list_and_steps_install_themselves() {
        let sim = Sim::default().failing(&apt_install(&merged_packages()), "E: held packages");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &prelude_steps(),
        );

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let apt = sim.apt_calls();
        assert_eq!(apt[1], apt_install(&merged_packages()));
        assert_eq!(apt[2], apt_install(&base_packages()));
        for own in [
            apt_install(server::DOCKER_PACKAGES),
            apt_install(&["r-rig"]),
            apt_install(&[RSTUDIO_DEB]),
            apt_install(&[CODE_SERVER_DEB]),
        ] {
            assert!(apt.contains(&own), "missing {own}: {apt:#?}");
        }
        assert!(
            !apt.contains(&apt_install(PYTHON_PACKAGES)),
            "the retry covered Python: {apt:#?}"
        );
    }

    #[test]
    fn step_one_fails_only_when_the_retry_fails_too() {
        let sim = Sim::default()
            .failing(&apt_install(&merged_packages()), "E: held packages")
            .failing(&apt_install(&base_packages()), "E: disk full");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &prelude_steps(),
        );

        let step_one = results[0].error.as_deref().expect("step 1 fails");
        assert!(step_one.contains("E: disk full"), "{step_one}");
        assert!(step_one.contains("E: held packages"), "{step_one}");
    }

    #[test]
    fn an_unreachable_docker_repo_is_unwound_and_docker_fails_on_its_own() {
        let sim = Sim::default().broken_source("docker");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &prelude_steps(),
        );

        // Both of this run's sources come out, since apt does not say which
        // one broke. R then installs rig itself, and Docker fails on its
        // own, removing its source again so later refreshes still work.
        let failed = failures(&results);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, "Docker Engine");
        assert!(
            failed[0].1.contains("docker repository is not signed"),
            "{failed:?}"
        );
        let calls = sim.calls();
        let removal = format!("sudo rm -f {DOCKER_SOURCES}");
        assert_eq!(
            calls.iter().filter(|call| **call == removal).count(),
            2,
            "{calls:#?}"
        );
        assert!(
            calls.contains(&format!("sudo rm -f {RIG_SOURCES}")),
            "{calls:#?}"
        );
        let mut without_repos = base_packages();
        without_repos.extend([RSTUDIO_DEB, CODE_SERVER_DEB]);
        let apt = sim.apt_calls();
        assert_eq!(
            apt[..3],
            [
                install::apt_get_command_line(&["update"]),
                install::apt_get_command_line(&["update"]),
                apt_install(&without_repos)
            ],
            "{apt:#?}"
        );
        assert!(apt.contains(&apt_install(&["r-rig"])), "{apt:#?}");
    }

    #[test]
    fn an_unreachable_rig_repo_is_unwound_and_r_fails_on_its_own() {
        let sim = Sim::default().broken_source("rig");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &prelude_steps(),
        );

        let failed: Vec<_> = failures(&results)
            .into_iter()
            .map(|(title, _)| title)
            .collect();
        assert_eq!(failed, ["R + CRAN/PPM repositories", "rig"]);
        assert!(
            failures(&results)[0]
                .1
                .contains("rig repository is not signed"),
            "{:?}",
            failures(&results)
        );
        assert!(sim.calls().contains(&format!("sudo rm -f {RIG_SOURCES}")));
        let mut without_repos = base_packages();
        without_repos.extend([RSTUDIO_DEB, CODE_SERVER_DEB]);
        let apt = sim.apt_calls();
        assert!(apt.contains(&apt_install(&without_repos)), "{apt:#?}");
        assert!(
            apt.contains(&apt_install(server::DOCKER_PACKAGES)),
            "Docker installs itself: {apt:#?}"
        );
        assert_eq!(
            sim.calls()
                .iter()
                .filter(|call| **call == format!("sudo rm -f {RIG_SOURCES}"))
                .count(),
            3,
            "step 1, R's rig install, and the rig step each remove the source they added"
        );
    }

    #[test]
    fn step_one_fails_when_update_fails_even_after_unwinding() {
        let sim = Sim::default().broken_source("docker").failing(
            &install::apt_get_command_line(&["update"]),
            "E: mirror down",
        );
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &prelude_steps(),
        );

        let step_one = results[0].error.as_deref().expect("step 1 fails");
        assert!(step_one.contains("E: mirror down"), "{step_one}");
        assert!(
            sim.calls()
                .contains(&format!("sudo rm -f {DOCKER_SOURCES}"))
        );
    }

    #[test]
    fn a_source_file_from_before_this_run_is_never_removed() {
        let sim = Sim::default().broken_source("docker").with_source("docker");
        let host = SimHost {
            existing: vec![DOCKER_SOURCES],
            ..SimHost::default()
        };
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps());

        assert!(
            results[0].error.is_some(),
            "the broken source still breaks update"
        );
        let removal = format!("sudo rm -f {DOCKER_SOURCES}");
        assert!(
            sim.calls().iter().all(|call| *call != removal),
            "{:#?}",
            sim.calls()
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unusable_package_path_drops_only_that_package() {
        use std::os::unix::ffi::OsStrExt;

        let sim = Sim::default();
        let rstudio = SimRstudio {
            download_path: Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
                b"/tmp/hpds-rstudio-\xff.deb",
            ))),
            ..SimRstudio::default()
        };
        let (results, _) = run_sim(&sim, &SimHost::default(), rstudio, &prelude_steps());

        let failed: Vec<_> = failures(&results)
            .into_iter()
            .map(|(title, _)| title)
            .collect();
        assert_eq!(failed, ["RStudio Server"]);
        let mut without_rstudio = base_packages();
        without_rstudio.extend_from_slice(server::DOCKER_PACKAGES);
        without_rstudio.extend(["r-rig", CODE_SERVER_DEB]);
        assert!(sim.apt_calls().contains(&apt_install(&without_rstudio)));
    }

    #[test]
    fn python_says_plainly_when_step_one_installed_its_packages() {
        let sim = Sim::default();
        let host = SimHost::default();
        let (_, transcript) =
            ui::capture(|| run_sim(&sim, &host, SimRstudio::default(), &prelude_steps()));
        let lines: Vec<_> = transcript
            .lines()
            .iter()
            .map(|(_, line)| line.clone())
            .collect();
        assert!(
            lines
                .iter()
                .any(|line| line == "Python's packages were installed with the system libraries"),
            "{lines:#?}"
        );
    }

    /// RStudio Server on a simulated host booted with systemd.
    fn systemd_rstudio() -> SimRstudio {
        SimRstudio {
            systemd: true,
            ..SimRstudio::default()
        }
    }

    #[test]
    fn rstudio_step_restarts_a_service_that_failed_before_r_was_installed() {
        // Step 1 installs the RStudio Server package before the R step
        // runs, so its postinst start fails; the RStudio Server step
        // restarts it once R is installed.
        let sim = Sim::default();
        let host = SimHost::default();
        let (results, _) = run_sim(&sim, &host, systemd_rstudio(), &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let calls = sim.calls();
        let position = |wanted: &str| {
            calls
                .iter()
                .position(|call| call == wanted)
                .unwrap_or_else(|| panic!("{wanted} never ran: {calls:#?}"))
        };
        let rig_add = position(RIG_ADD);
        let check = position(RSTUDIO_IS_ACTIVE);
        let reset = position(RSTUDIO_RESET);
        let restart = position(RSTUDIO_RESTART);
        assert!(rig_add < check, "{calls:#?}");
        assert!(check < reset, "{calls:#?}");
        assert!(reset < restart, "{calls:#?}");
        assert!(*sim.rstudio_active.lock().unwrap());
    }

    #[test]
    fn rstudio_step_leaves_an_active_service_alone() {
        let sim = Sim::provisioned();
        *sim.rstudio_active.lock().unwrap() = true;
        let host = SimHost::default();
        let (results, _) = run_sim(&sim, &host, systemd_rstudio(), &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let calls = sim.calls();
        assert!(
            calls.iter().any(|call| call == RSTUDIO_IS_ACTIVE),
            "{calls:#?}"
        );
        assert!(
            calls
                .iter()
                .all(|call| call != RSTUDIO_RESET && call != RSTUDIO_RESTART),
            "{calls:#?}"
        );
    }

    /// The RStudio Server step's error, and the run's output lines.
    fn rstudio_failure(sim: &Sim, selected: &[Step]) -> (String, Vec<String>) {
        let host = SimHost::default();
        let (results, _, lines) = run_sim_captured(sim, &host, systemd_rstudio(), selected);
        let failed = failures(&results);
        let error = failed
            .iter()
            .find(|(title, _)| *title == "RStudio Server")
            .map(|(_, error)| error.clone())
            .unwrap_or_else(|| panic!("RStudio Server did not fail: {failed:?}"));
        (error, lines)
    }

    #[test]
    fn rstudio_step_fails_with_a_hint_when_the_service_cannot_start_without_r() {
        let sim = Sim::default();
        let (error, lines) =
            rstudio_failure(&sim, &server_steps_without(&["R + CRAN/PPM repositories"]));

        assert!(error.contains("rstudio-server"), "{error}");
        assert!(
            lines.iter().any(|line| line.contains("hpds install r")
                && line.contains("sudo systemctl restart rstudio-server")),
            "{lines:#?}"
        );
        let calls = sim.calls();
        assert!(
            calls.iter().any(|call| call == RSTUDIO_RESTART),
            "{calls:#?}"
        );
    }

    #[test]
    fn rstudio_step_fails_with_a_hint_when_a_restart_leaves_the_service_inactive() {
        let sim = Sim {
            rstudio_never_starts: true,
            ..Sim::default()
        };
        let (error, lines) = rstudio_failure(&sim, &prelude_steps());

        assert!(error.contains("rstudio-server"), "{error}");
        assert!(
            lines.iter().any(|line| line.contains("hpds install r")
                && line.contains("sudo systemctl restart rstudio-server")),
            "{lines:#?}"
        );
        let calls = sim.calls();
        let restart = calls
            .iter()
            .position(|call| call == RSTUDIO_RESTART)
            .unwrap_or_else(|| panic!("no restart: {calls:#?}"));
        assert!(
            calls[restart..]
                .iter()
                .any(|call| call == RSTUDIO_IS_ACTIVE),
            "the service is checked again after the restart: {calls:#?}"
        );
    }

    #[test]
    fn rstudio_step_without_systemd_installs_and_passes_as_before() {
        let sim = Sim::default();
        let host = SimHost::default();
        let (results, rstudio) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(rstudio.downloads(), 1);
        let calls = sim.calls();
        assert!(
            calls
                .iter()
                .all(|call| !(call.contains("systemctl") && call.contains("rstudio-server"))),
            "{calls:#?}"
        );
    }

    #[test]
    fn rstudio_server_runs_after_r_in_the_foreground() {
        // RStudio Server's service needs R, and both steps are on the
        // foreground lane, so plan order alone puts R first.
        let table = steps(Profile::Server);
        let position = |title: &str| {
            table
                .iter()
                .position(|step| step.title == title)
                .unwrap_or_else(|| panic!("no step {title}"))
        };
        let r = position("R + CRAN/PPM repositories");
        let rstudio = position("RStudio Server");
        assert!(r < rstudio);
        assert_eq!(table[r].lane, Lane::Foreground);
        assert_eq!(table[rstudio].lane, Lane::Foreground);
    }

    #[test]
    fn a_rerun_downloads_nothing_already_installed() {
        let sim = Sim::provisioned();
        let host = SimHost::default();
        let (results, rstudio) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(rstudio.downloads(), 0);
        assert_eq!(host.code_server_downloads(), 0);
        assert_eq!(host.docker_keys(), 0);
        assert!(sim.calls().iter().all(|call| call != RIG_KEY));
        assert_eq!(
            sim.apt_calls(),
            [
                install::apt_get_command_line(&["update"]),
                apt_install(&base_packages())
            ]
        );
    }

    /// [`run_sim`] with the run's output captured, as plain lines.
    fn run_sim_captured(
        sim: &Sim,
        host: &SimHost,
        rstudio: SimRstudio,
        selected: &[Step],
    ) -> (Vec<StepResult>, &'static SimRstudio, Vec<String>) {
        let ((results, rstudio), transcript) =
            ui::capture(|| run_sim(sim, host, rstudio, selected));
        let lines = transcript
            .lines()
            .iter()
            .map(|(_, line)| line.clone())
            .collect();
        (results, rstudio, lines)
    }

    /// The position of the only line in `lines` that starts with `prefix`.
    fn only_line(lines: &[String], prefix: &str) -> usize {
        let found: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with(prefix))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(found.len(), 1, "{prefix}: {lines:#?}");
        found[0]
    }

    #[test]
    fn step_one_downloads_both_packages_while_it_refreshes_the_package_lists() {
        let meet = Rendezvous::new(Duration::from_secs(10));
        let sim = Sim {
            meet: Some(Arc::clone(&meet)),
            ..Sim::default()
        };
        let host = SimHost {
            meet: Some(Arc::clone(&meet)),
            ..SimHost::default()
        };
        let rstudio = SimRstudio {
            meet: Some(Arc::clone(&meet)),
            ..SimRstudio::default()
        };
        let (results, _) = run_sim(&sim, &host, rstudio, &prelude_steps());

        assert_eq!(
            meet.met(),
            [
                (RSTUDIO_DOWNLOAD, true),
                (CODE_SERVER_DOWNLOAD, true),
                (REFRESH, true)
            ],
            "the downloads and the refresh must all be under way at once"
        );
        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(
            sim.apt_calls(),
            [
                install::apt_get_command_line(&["update"]),
                apt_install(&merged_packages())
            ]
        );
    }

    #[test]
    fn package_download_output_follows_step_ones_own_lines_in_plan_order() {
        let sim = Sim::default();
        let host = SimHost {
            announce: true,
            ..SimHost::default()
        };
        // RStudio Server finishes last, yet its lines still come first.
        let rstudio = SimRstudio {
            announce: true,
            delay: Some(Duration::from_millis(300)),
            ..SimRstudio::default()
        };
        let (results, _, lines) = run_sim_captured(&sim, &host, rstudio, &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let refresh = only_line(&lines, "refreshing apt package lists");
        let rstudio = only_line(&lines, "downloading RStudio Server (simulated)");
        let code_server = only_line(&lines, "downloading code-server (simulated)");
        let install = only_line(
            &lines,
            "installing the system libraries and the selected steps' packages",
        );
        assert!(
            refresh < rstudio && rstudio < code_server && code_server < install,
            "{lines:#?}"
        );
    }

    #[test]
    fn step_one_says_which_downloads_it_is_still_waiting_for() {
        let sim = Sim::default();
        let rstudio = SimRstudio {
            delay: Some(Duration::from_millis(300)),
            ..SimRstudio::default()
        };
        let (results, _, lines) =
            run_sim_captured(&sim, &SimHost::default(), rstudio, &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let refresh = only_line(&lines, "refreshing apt package lists");
        // code-server has no delay, so it may or may not have finished.
        let waiting = only_line(&lines, "waiting for the RStudio Server ");
        let install = only_line(
            &lines,
            "installing the system libraries and the selected steps' packages",
        );
        assert!(lines[waiting].ends_with(" to finish"), "{}", lines[waiting]);
        assert!(refresh < waiting && waiting < install, "{lines:#?}");
    }

    #[test]
    fn a_panicking_package_download_drops_only_that_package() {
        let sim = Sim::default();
        let rstudio = SimRstudio {
            first_download_panics: AtomicBool::new(true),
            ..SimRstudio::default()
        };
        let (results, rstudio, lines) =
            run_sim_captured(&sim, &SimHost::default(), rstudio, &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(rstudio.downloads(), 2, "step 1 tried, then RStudio retried");
        let mut without_rstudio = base_packages();
        without_rstudio.extend_from_slice(server::DOCKER_PACKAGES);
        without_rstudio.extend(["r-rig", CODE_SERVER_DEB]);
        let apt = sim.apt_calls();
        assert_eq!(apt[1], apt_install(&without_rstudio), "{apt:#?}");
        assert!(apt.contains(&apt_install(&[RSTUDIO_DEB])), "{apt:#?}");
        assert!(
            lines.iter().any(
                |line| line.contains("could not prepare the RStudio Server package")
                    && line.contains("RStudio Server will install it itself")
            ),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_failed_code_server_download_drops_only_code_server() {
        let sim = Sim::default();
        let host = SimHost {
            code_server_fails: true,
            ..SimHost::default()
        };
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), &prelude_steps());

        let failed = failures(&results);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, "code-server + extensions");
        assert!(failed[0].1.contains("download interrupted"), "{failed:?}");
        let mut without_code_server = base_packages();
        without_code_server.extend_from_slice(server::DOCKER_PACKAGES);
        without_code_server.extend(["r-rig", RSTUDIO_DEB]);
        assert_eq!(sim.apt_calls()[1], apt_install(&without_code_server));
    }

    #[test]
    fn packages_are_prepared_on_their_own_threads_only_for_selected_steps() {
        let preparing = |name: &Option<String>| {
            name.as_deref()
                .is_some_and(|name| name.starts_with("hpds-prepare-"))
        };

        let host = SimHost::default();
        let (_, rstudio) = run_sim(
            &Sim::default(),
            &host,
            SimRstudio::default(),
            &prelude_steps(),
        );
        assert_eq!(
            host.code_server_threads(),
            [Some("hpds-prepare-code-server".to_string())]
        );
        assert_eq!(
            rstudio.lookup_threads().first(),
            Some(&Some("hpds-prepare-rstudio-server".to_string()))
        );

        let host = SimHost::default();
        let (_, rstudio) = run_sim(
            &Sim::default(),
            &host,
            SimRstudio::default(),
            &server_steps_without(&["RStudio Server"]),
        );
        assert_eq!(
            host.code_server_threads(),
            [Some("hpds-prepare-code-server".to_string())]
        );
        assert!(rstudio.lookup_threads().is_empty());

        let host = SimHost::default();
        let (_, rstudio) = run_sim(
            &Sim::default(),
            &host,
            SimRstudio::default(),
            &server_steps_without(&["RStudio Server", "code-server + extensions"]),
        );
        assert!(host.code_server_threads().is_empty());
        assert!(!rstudio.lookup_threads().iter().any(preparing));
        assert_eq!(rstudio.downloads(), 0);
    }

    #[test]
    fn prepared_package_files_last_through_the_install_and_are_removed_after() {
        let sim = Sim::default();
        let rstudio = SimRstudio {
            temporary: true,
            ..SimRstudio::default()
        };
        let (results, rstudio) = run_sim(&sim, &SimHost::default(), rstudio, &prelude_steps());

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let created = rstudio.created();
        assert!(!created.is_empty());
        let merged = sim.apt_calls()[1].clone();
        let prepared = created[0].to_str().unwrap().to_string();
        assert!(merged.contains(&prepared), "{merged}");
        assert!(
            sim.package_files().contains(&(prepared, true)),
            "{:?}",
            sim.package_files()
        );
        for path in created {
            assert!(!path.exists(), "{} was left behind", path.display());
        }
    }

    #[test]
    fn prepared_package_files_are_removed_when_the_refresh_fails() {
        let sim = Sim::default().failing(
            &install::apt_get_command_line(&["update"]),
            "E: mirror down",
        );
        let rstudio = SimRstudio {
            temporary: true,
            ..SimRstudio::default()
        };
        let (results, rstudio) = run_sim(&sim, &SimHost::default(), rstudio, &prelude_steps());

        assert!(results[0].error.is_some(), "step 1 fails");
        let created = rstudio.created();
        assert!(!created.is_empty());
        for path in created {
            assert!(!path.exists(), "{} was left behind", path.display());
        }
    }

    #[test]
    fn the_plan_describes_the_whole_prelude_and_no_install_script() {
        let text = plan(Profile::Server);
        assert!(!text.contains("install.sh"), "{text}");
        assert!(!text.contains("code-server.dev"), "{text}");
        let prelude_lines: Vec<&str> = text
            .lines()
            .skip_while(|line| !line.ends_with("2. system libraries (apt)"))
            .skip(1)
            .take_while(|line| !line.ends_with("3. Docker Engine"))
            .collect();
        let merged = prelude_lines
            .iter()
            .find(|line| line.contains("install -y build-essential"))
            .expect("merged install line");
        for package in [
            "python3-venv",
            "docker-ce",
            "r-rig",
            "rstudio-server",
            "code-server",
        ] {
            assert!(merged.contains(package), "{package}: {merged}");
        }
        assert_eq!(
            prelude_lines
                .iter()
                .filter(|line| line.contains("apt-get -o DPkg::Lock::Timeout=600 update"))
                .count(),
            1,
            "{prelude_lines:#?}"
        );
    }

    // --- system updates -------------------------------------------------------

    /// The package-list refresh, exactly as the system-updates step and the
    /// apt prelude run it.
    const REFRESH_LINE: &str = "sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 \
                                apt-get -o DPkg::Lock::Timeout=600 update";

    /// The upgrade, exactly as the system-updates step runs it: keep a
    /// locally changed configuration file and take the package's default
    /// for any other, so dpkg never prompts.
    const UPGRADE_LINE: &str = "sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 \
                                apt-get -o DPkg::Lock::Timeout=600 \
                                -o Dpkg::Options::=--force-confdef \
                                -o Dpkg::Options::=--force-confold upgrade -y";

    /// The warning a pending reboot raises after the upgrade.
    const REBOOT_PENDING: &str =
        "a reboot is pending to finish the system updates; run `sudo reboot`";

    /// The server steps titled in `titles`, in plan order.
    fn server_steps_titled(titles: &[&str]) -> Vec<Step> {
        let selected: Vec<Step> = steps(Profile::Server)
            .iter()
            .filter(|step| titles.contains(&step.title))
            .copied()
            .collect();
        assert_eq!(selected.len(), titles.len(), "unknown title in {titles:?}");
        selected
    }

    /// Every call that would restart or power off the host.
    fn reboot_calls(sim: &Sim) -> Vec<String> {
        sim.calls()
            .into_iter()
            .filter(|call| {
                call.split_whitespace()
                    .any(|word| ["reboot", "shutdown", "poweroff"].contains(&word))
            })
            .collect()
    }

    #[test]
    fn system_updates_is_the_first_server_step_and_runs_in_the_foreground() {
        let step = steps(Profile::Server)[0];
        assert_eq!(step.title, "system updates (apt upgrade)");
        assert_eq!(step.title, SYSTEM_UPDATES);
        assert_eq!(step.lane, Lane::Foreground);
        assert!(step.requires.is_empty(), "{:?}", step.requires);
        assert_eq!(step.apt, AptShare::None);
        assert!(matches!(step.actions, [Action::SystemUpdates]));
        assert_eq!(steps(Profile::Server).len(), 16);
        assert!(
            steps(Profile::Dev)
                .iter()
                .all(|step| step.title != SYSTEM_UPDATES
                    && !step
                        .actions
                        .iter()
                        .any(|action| matches!(action, Action::SystemUpdates))),
            "the dev profile never upgrades the system"
        );
    }

    #[test]
    fn the_server_plan_lists_the_system_update_commands_first() {
        let text = plan(Profile::Server);
        let expected = format!(
            "hpds setup --profile server runs these 16 steps:\n\
             \x20 1. system updates (apt upgrade)\n\
             \x20      {REFRESH_LINE}\n\
             \x20      {UPGRADE_LINE}\n\
             \x20      if /var/run/reboot-required exists afterwards: warn that a reboot is pending (hpds never reboots)\n\
             \x20 2. system libraries (apt)\n"
        );
        assert!(text.starts_with(&expected), "{text}");
    }

    #[test]
    fn system_updates_refresh_then_upgrade_through_the_apt_policy() {
        let sim = Sim::default();
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &server_steps_titled(&[SYSTEM_UPDATES]),
        );

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(
            install::apt_get_command_line(&[
                "-o",
                "Dpkg::Options::=--force-confdef",
                "-o",
                "Dpkg::Options::=--force-confold",
                "upgrade",
                "-y"
            ]),
            UPGRADE_LINE
        );
        assert_eq!(sim.calls(), [REFRESH_LINE, UPGRADE_LINE]);
        assert_eq!(reboot_calls(&sim), Vec::<String>::new());
    }

    #[test]
    fn the_prelude_does_not_refresh_again_after_system_updates_refreshed() {
        let sim = Sim::default();
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &server_steps_titled(&[SYSTEM_UPDATES, SYSTEM_LIBRARIES]),
        );

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert_eq!(
            sim.apt_calls(),
            [
                REFRESH_LINE.to_string(),
                UPGRADE_LINE.to_string(),
                apt_install(APT_SYSTEM_LIBRARIES)
            ],
            "the lists system updates refreshed are current for the prelude"
        );
    }

    #[test]
    fn a_rerun_of_the_whole_server_profile_refreshes_the_lists_once() {
        // Every repository is registered and every package installed, so
        // the prelude adds no apt source and needs no refresh of its own.
        let sim = Sim::provisioned();
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            steps(Profile::Server),
        );

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let apt = sim.apt_calls();
        assert_eq!(
            apt.iter().filter(|call| *call == REFRESH_LINE).count(),
            1,
            "{apt:#?}"
        );
        assert_eq!(apt[..2], [REFRESH_LINE, UPGRADE_LINE], "{apt:#?}");
        assert_eq!(reboot_calls(&sim), Vec::<String>::new());
    }

    #[test]
    fn the_prelude_refreshes_again_when_it_adds_apt_sources_after_system_updates() {
        // A fresh server: the prelude registers the Docker and rig
        // repositories, whose package lists the first refresh never saw.
        let sim = Sim::default();
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            steps(Profile::Server),
        );

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let apt = sim.apt_calls();
        assert_eq!(
            apt,
            [
                REFRESH_LINE.to_string(),
                UPGRADE_LINE.to_string(),
                REFRESH_LINE.to_string(),
                apt_install(&merged_packages())
            ],
            "{apt:#?}"
        );
        let calls = sim.calls();
        let registered = calls
            .iter()
            .position(|call| call == "sudo install -m 0755 -d /etc/apt/keyrings")
            .unwrap_or_else(|| panic!("the Docker repository is registered: {calls:#?}"));
        let second_refresh = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| *call == REFRESH_LINE)
            .map(|(index, _)| index)
            .nth(1)
            .unwrap_or_else(|| panic!("a second refresh: {calls:#?}"));
        assert!(registered < second_refresh, "{calls:#?}");
    }

    #[test]
    fn without_system_updates_the_prelude_refreshes_as_before() {
        let sim = Sim::provisioned();
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &prelude_steps(),
        );

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let apt = sim.apt_calls();
        assert_eq!(apt.first().map(String::as_str), Some(REFRESH_LINE));
        assert_eq!(
            apt.iter().filter(|call| *call == REFRESH_LINE).count(),
            1,
            "{apt:#?}"
        );
        assert!(!apt.iter().any(|call| call.contains("upgrade")), "{apt:#?}");
    }

    #[test]
    fn a_failed_upgrade_fails_only_its_own_step() {
        let sim = Sim::default().failing(UPGRADE_LINE, "E: dpkg was interrupted");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &server_steps_titled(&[SYSTEM_UPDATES, SYSTEM_LIBRARIES]),
        );

        let failed = failures(&results);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].0, SYSTEM_UPDATES);
        assert!(
            failed[0].1.contains("E: dpkg was interrupted"),
            "{failed:?}"
        );
        assert_eq!(
            sim.apt_calls(),
            [
                REFRESH_LINE.to_string(),
                UPGRADE_LINE.to_string(),
                apt_install(APT_SYSTEM_LIBRARIES)
            ],
            "the refresh succeeded, so the prelude adds no refresh of its own"
        );
    }

    #[test]
    fn a_failed_refresh_skips_the_upgrade_and_leaves_the_prelude_its_own_refresh() {
        let sim = Sim::default().failing(REFRESH_LINE, "E: mirror down");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            &server_steps_titled(&[SYSTEM_UPDATES, SYSTEM_LIBRARIES]),
        );

        let failed = failures(&results);
        assert!(
            failed
                .iter()
                .any(|(title, error)| *title == SYSTEM_UPDATES && error.contains("E: mirror down")),
            "{failed:?}"
        );
        let apt = sim.apt_calls();
        assert!(!apt.iter().any(|call| call == UPGRADE_LINE), "{apt:#?}");
        assert_eq!(
            apt.iter().filter(|call| *call == REFRESH_LINE).count(),
            2,
            "the prelude still tries its own refresh: {apt:#?}"
        );
    }

    /// Run the system-updates step alone against a host whose reboot
    /// marker is `marker`, returning the step's results, the simulation,
    /// and what the run printed.
    fn run_system_updates(marker: std::path::PathBuf) -> (Vec<StepResult>, Sim, Vec<String>) {
        let sim = Sim::default();
        let host = SimHost {
            reboot_required: Some(marker),
            ..SimHost::default()
        };
        let ((results, _), transcript) = ui::capture(|| {
            run_sim(
                &sim,
                &host,
                SimRstudio::default(),
                &server_steps_titled(&[SYSTEM_UPDATES]),
            )
        });
        let lines = transcript
            .lines()
            .iter()
            .map(|(_, line)| line.clone())
            .collect();
        (results, sim, lines)
    }

    #[test]
    fn a_pending_reboot_is_reported_after_the_upgrade_and_never_performed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("reboot-required");
        std::fs::write(&marker, "*** System restart required ***\n").unwrap();

        let (results, sim, lines) = run_system_updates(marker);

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let warnings: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains(REBOOT_PENDING))
            .collect();
        assert_eq!(warnings.len(), 1, "{lines:#?}");
        assert!(warnings[0].starts_with("warning: "), "{lines:#?}");
        assert_eq!(sim.calls(), [REFRESH_LINE, UPGRADE_LINE]);
        assert_eq!(reboot_calls(&sim), Vec::<String>::new());
    }

    #[test]
    fn no_reboot_marker_means_no_reboot_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (results, sim, lines) = run_system_updates(dir.path().join("reboot-required"));

        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        assert!(
            !lines.iter().any(|line| line.contains("reboot")),
            "{lines:#?}"
        );
        assert_eq!(reboot_calls(&sim), Vec::<String>::new());
    }

    #[test]
    fn a_pending_reboot_reaches_the_compact_next_steps() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("reboot-required");
        std::fs::write(&marker, "*** System restart required ***\n").unwrap();
        let sim = Sim::default();
        let host = SimHost {
            reboot_required: Some(marker),
            ..SimHost::default()
        };
        let git_setup = || Ok(());
        let fetcher = FakeFetcher::default();
        let runner = LoggingRunner::new(&sim);
        let mut deps = deps_recording_git(Os::Linux, true, &runner, &fetcher, &git_setup);
        deps.server_host = &host;
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(
            Profile::Server,
            &server_steps_titled(&[SYSTEM_UPDATES]),
            &deps,
            true,
            &log_path,
        );

        assert!(run.result.is_ok(), "{:?}", run.result);
        assert!(
            run.terminal.ends_with(&format!(
                "[1/1] system updates (apt upgrade) ✓\n\
                 All 1 step succeeded. Details: <LOG>\n\
                 Next steps:\n\
                 \x20 - {REBOOT_PENDING}"
            )),
            "{}",
            run.terminal
        );
        assert_eq!(reboot_calls(&sim), Vec::<String>::new());
    }

    // --- lanes and requirements ---------------------------------------------

    const SERVER_BACKGROUND: [&str; 6] = [
        "stable Rust",
        "quarto",
        "tinytex",
        "gh",
        "duckdb",
        "uv + ruff + sqlfluff",
    ];

    #[test]
    fn server_lanes_and_requirements_match_the_dependency_analysis() {
        let background: Vec<_> = steps(Profile::Server)
            .iter()
            .filter(|step| step.lane == Lane::Background)
            .map(|step| step.title)
            .collect();
        assert_eq!(background, SERVER_BACKGROUND);
        let requirements: Vec<(&str, &[&str])> = steps(Profile::Server)
            .iter()
            .filter(|step| !step.requires.is_empty())
            .map(|step| (step.title, step.requires))
            .collect();
        assert_eq!(
            requirements,
            [
                ("tinytex", &[SYSTEM_LIBRARIES][..]),
                ("code-server + extensions", &["quarto"][..]),
                (
                    "uv + ruff + sqlfluff",
                    &["Python + PPM pip configuration"][..]
                ),
                ("git defaults", &["gh"][..]),
            ]
        );
    }

    #[test]
    fn dev_profile_runs_everything_in_the_foreground() {
        for step in steps(Profile::Dev) {
            assert_eq!(step.lane, Lane::Foreground, "{}", step.title);
        }
    }

    #[test]
    fn every_requirement_names_an_earlier_step_of_the_same_profile() {
        // Pointing only backwards makes the requirement graph acyclic, and
        // means plain plan order satisfies it when steps run one at a time.
        for profile in [Profile::Dev, Profile::Server] {
            let table = steps(profile);
            for (index, step) in table.iter().enumerate() {
                for required in step.requires {
                    let position = table
                        .iter()
                        .position(|other| other.title == *required)
                        .unwrap_or_else(|| panic!("{}: unknown step {required}", step.title));
                    assert!(
                        position < index,
                        "{} requires the later step {required}",
                        step.title
                    );
                }
            }
        }
    }

    #[test]
    fn only_background_steps_that_need_system_packages_wait_for_step_one() {
        // rustup-init, the Quarto, gh, DuckDB, and uv downloads need only
        // TLS, which the cloud image has; TinyTeX's tlmgr needs the full
        // Perl that the system libraries bring in.
        let waiting: Vec<_> = steps(Profile::Server)
            .iter()
            .filter(|step| step.lane == Lane::Background)
            .filter(|step| step.requires.contains(&SYSTEM_LIBRARIES))
            .map(|step| step.title)
            .collect();
        assert_eq!(waiting, ["tinytex"]);
        assert_eq!(steps(Profile::Server)[0].title, SYSTEM_UPDATES);
        assert_eq!(steps(Profile::Server)[1].title, SYSTEM_LIBRARIES);
    }

    #[test]
    fn background_steps_never_use_sudo_apt_or_the_git_flow() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = crate::install::test_support::ctx_on(Os::Linux, &runner, &fetcher);
        for profile in [Profile::Dev, Profile::Server] {
            for step in steps(profile)
                .iter()
                .filter(|step| step.lane == Lane::Background)
            {
                for action in step.actions {
                    match action {
                        Action::Install(tool) => {
                            assert!(
                                ["quarto", "tinytex", "gh", "duckdb", "uv"].contains(tool),
                                "{}: {tool} is not a user-space installer",
                                step.title
                            );
                            let installer = registry::find(tool).unwrap();
                            for line in installer.plan(&ctx) {
                                for word in ["sudo", "apt", "dpkg"] {
                                    assert!(!line.contains(word), "{tool}: {line}");
                                }
                            }
                        }
                        Action::Run(cmd) => {
                            assert!(!cmd.sudo, "{}: {}", step.title, cmd.what);
                            assert!(
                                !["apt-get", "apt", "dpkg"].contains(&cmd.program),
                                "{}: {}",
                                step.title,
                                cmd.what
                            );
                        }
                        Action::AptPrelude | Action::StepPackages(_) => {
                            panic!("{}: uses apt", step.title)
                        }
                        Action::Server(action) => assert!(
                            matches!(action, server::ServerAction::Rust),
                            "{}: {action:?} is not user-space",
                            step.title
                        ),
                        Action::GitSetup => panic!("{}: git setup is foreground", step.title),
                        Action::SystemUpdates => {
                            panic!("{}: system updates use apt", step.title)
                        }
                    }
                }
            }
        }
    }

    /// The prefix every planned or executed apt-get call must carry: a
    /// dpkg lock wait and the noninteractive environment, passed through
    /// `env` because sudo drops the caller's environment.
    const APT_POLICY: &str = "sudo env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 \
                              apt-get -o DPkg::Lock::Timeout=600 ";

    /// Every apt-get mention in `line` is the policy-carrying invocation.
    fn assert_apt_policy(context: &str, line: &str) {
        let mentions = line.matches("apt-get").count();
        assert_eq!(
            line.matches(APT_POLICY).count(),
            mentions,
            "{context}: apt-get without the lock wait and environment: {line}"
        );
        assert!(!line.contains("gdebi"), "{context}: {line}");
    }

    #[test]
    fn every_planned_apt_call_waits_for_the_lock_and_runs_noninteractive() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = crate::install::test_support::ctx_on(Os::Linux, &runner, &fetcher);
        let mut apt_lines = 0;
        for profile in [Profile::Dev, Profile::Server] {
            for step in steps(profile) {
                for action in step.actions {
                    let lines = match action {
                        Action::Install(tool) => registry::find(tool).unwrap().plan(&ctx),
                        Action::Run(cmd) => {
                            assert!(
                                !["apt-get", "apt", "dpkg", "gdebi"].contains(&cmd.program)
                                    && !cmd.args.iter().any(|arg| arg.contains("apt-get")),
                                "{}: `{}` must go through the apt policy",
                                step.title,
                                cmd.what
                            );
                            describe(step, action, steps(profile))
                        }
                        _ => describe(step, action, steps(profile)),
                    };
                    for line in lines {
                        apt_lines += line.matches("apt-get").count();
                        assert_apt_policy(step.title, &line);
                    }
                }
            }
        }
        assert!(apt_lines >= 6, "the walk found only {apt_lines} apt calls");
        assert!(!APT_SYSTEM_LIBRARIES.contains(&"gdebi-core"));
    }

    #[test]
    fn every_executed_apt_call_waits_for_the_lock_and_runs_noninteractive() {
        // Step 1 merges everything, then its install fails, so every
        // step's own apt path runs too.
        let sim = Sim::default().failing(&apt_install(&merged_packages()), "E: broken");
        let (results, _) = run_sim(
            &sim,
            &SimHost::default(),
            SimRstudio::default(),
            steps(Profile::Server),
        );
        assert!(failures(&results).is_empty(), "{:?}", failures(&results));
        let apt = sim.apt_calls();
        assert!(apt.len() >= 8, "{apt:#?}");
        for call in apt {
            assert!(call.starts_with(APT_POLICY), "{call}");
        }
    }

    #[test]
    fn partition_splits_lanes_in_plan_order_with_plan_indices() {
        let (foreground, background) = partition(steps(Profile::Server));
        let indices = |lane: &[(usize, Step)]| lane.iter().map(|(i, _)| *i).collect::<Vec<_>>();
        assert_eq!(indices(&foreground), [0, 1, 2, 4, 5, 8, 9, 10, 14, 15]);
        assert_eq!(indices(&background), [3, 6, 7, 11, 12, 13]);
        for (index, step) in foreground.iter().chain(&background) {
            assert_eq!(step.title, steps(Profile::Server)[*index].title);
        }
        let titles: Vec<_> = background.iter().map(|(_, step)| step.title).collect();
        assert_eq!(titles, SERVER_BACKGROUND);
    }

    // --- overlapped execution -----------------------------------------------

    /// One non-sudo `fake <arg>` command, announced as `<arg>`.
    macro_rules! fake {
        ($arg:literal) => {
            Action::Run(Cmd {
                what: $arg,
                program: "fake",
                args: &[$arg],
                sudo: false,
            })
        };
    }

    /// A runner for overlap tests. Every command succeeds and is logged
    /// when it finishes. A gated command first waits until another
    /// command has finished; the wait is bounded, and a gate that times
    /// out fails the command instead of hanging the test.
    struct GateRunner {
        gates: Vec<(&'static str, &'static str)>,
        delays: Vec<(&'static str, Duration)>,
        panics_on: Option<&'static str>,
        timeout: Duration,
        finished: Mutex<Vec<String>>,
        changed: Condvar,
    }

    impl GateRunner {
        fn new(timeout: Duration) -> Self {
            GateRunner {
                gates: Vec::new(),
                delays: Vec::new(),
                panics_on: None,
                timeout,
                finished: Mutex::new(Vec::new()),
                changed: Condvar::new(),
            }
        }

        /// `command` waits until `after` has finished.
        fn gate(mut self, command: &'static str, after: &'static str) -> Self {
            self.gates.push((command, after));
            self
        }

        /// `command` takes `delay` to run.
        fn delay(mut self, command: &'static str, delay: Duration) -> Self {
            self.delays.push((command, delay));
            self
        }

        /// `command` panics instead of running.
        fn panics_on(mut self, command: &'static str) -> Self {
            self.panics_on = Some(command);
            self
        }

        fn finished(&self) -> Vec<String> {
            self.finished.lock().unwrap().clone()
        }

        fn finish(&self, entry: String) {
            self.finished.lock().unwrap().push(entry);
            self.changed.notify_all();
        }
    }

    impl CommandRunner for GateRunner {
        fn which(&self, _program: &str) -> Option<std::path::PathBuf> {
            None
        }

        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
            let key = format!("{program} {}", args.join(" "));
            if self.panics_on == Some(key.as_str()) {
                panic!("{key} blew up");
            }
            if let Some((_, after)) = self.gates.iter().find(|(command, _)| *command == key) {
                let finished = self.finished.lock().unwrap();
                let (finished, wait) = self
                    .changed
                    .wait_timeout_while(finished, self.timeout, |done| {
                        !done.iter().any(|entry| entry == after)
                    })
                    .unwrap();
                drop(finished);
                if wait.timed_out() {
                    self.finish(format!("{key} (timed out)"));
                    return Ok(CommandOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: format!("gave up waiting for {after}"),
                    });
                }
            }
            if let Some((_, delay)) = self.delays.iter().find(|(command, _)| *command == key) {
                std::thread::sleep(*delay);
            }
            self.finish(key);
            Ok(CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// What a bounded run hands back: each step's title and error, the
    /// runner's log, and the lines the main thread emitted.
    struct Outcome {
        results: Vec<(&'static str, Option<String>)>,
        log: Vec<String>,
        output: Vec<String>,
    }

    impl Outcome {
        fn failed(&self) -> Vec<&'static str> {
            self.results
                .iter()
                .filter(|(_, error)| error.is_some())
                .map(|(title, _)| *title)
                .collect()
        }

        fn position(&self, entry: &str) -> usize {
            self.log
                .iter()
                .position(|done| done == entry)
                .unwrap_or_else(|| panic!("{entry} never ran: {:?}", self.log))
        }

        fn lines_starting(&self, prefix: &str) -> Vec<&str> {
            self.output
                .iter()
                .filter(|line| line.starts_with(prefix))
                .map(String::as_str)
                .collect()
        }
    }

    /// Run `table` on its own thread with `runner`, capturing the main
    /// thread's output. A run that has not finished within 30 seconds
    /// fails the test rather than hanging it.
    fn run_bounded(table: &'static [Step], runner: GateRunner, overlap: bool) -> Outcome {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let git_setup = || Ok(());
            let deps = deps_recording_git(Os::Linux, true, &runner, &PanicFetcher, &git_setup);
            let (results, transcript) = ui::capture(|| execute(table, &deps, overlap));
            let _ = tx.send(Outcome {
                results: results.into_iter().map(|r| (r.title, r.error)).collect(),
                log: runner.finished(),
                output: transcript
                    .lines()
                    .iter()
                    .map(|(_, line)| line.clone())
                    .collect(),
            });
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("setup execution hung"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("setup execution panicked"),
        }
    }

    /// A background step that can only finish once a later foreground
    /// step has run: run one step at a time, it times out.
    const OVERLAP_STEPS: &[Step] = &[
        Step {
            title: "foreground first",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("one")],
        },
        Step {
            title: "background waits",
            lane: Lane::Background,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("wait")],
        },
        Step {
            title: "foreground release",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("release")],
        },
    ];

    #[test]
    fn overlap_runs_a_background_step_while_foreground_steps_run() {
        let runner = GateRunner::new(Duration::from_secs(10)).gate("fake wait", "fake release");
        let outcome = run_bounded(OVERLAP_STEPS, runner, true);

        assert_eq!(outcome.failed(), Vec::<&str>::new(), "{:?}", outcome.log);
        assert!(
            outcome.position("fake release") < outcome.position("fake wait"),
            "{:?}",
            outcome.log
        );
    }

    #[test]
    fn overlap_reports_results_and_headers_in_plan_order() {
        let runner = GateRunner::new(Duration::from_secs(10)).gate("fake wait", "fake release");
        let outcome = run_bounded(OVERLAP_STEPS, runner, true);

        let titles: Vec<_> = outcome.results.iter().map(|(title, _)| *title).collect();
        assert_eq!(
            titles,
            ["foreground first", "background waits", "foreground release"]
        );
        assert_eq!(outcome.lines_starting("[1/3]"), ["[1/3] foreground first"]);
        assert_eq!(
            outcome.lines_starting("[2/3]"),
            ["[2/3] background waits (ran in the background)"]
        );
        assert_eq!(
            outcome.lines_starting("[3/3]"),
            ["[3/3] foreground release"]
        );
        // The background step's own lines follow its header.
        let header = outcome
            .output
            .iter()
            .position(|line| line.starts_with("[2/3]"))
            .unwrap();
        assert_eq!(
            outcome.output.get(header + 1).map(String::as_str),
            Some("wait")
        );
    }

    #[test]
    fn without_overlap_steps_run_one_at_a_time_in_plan_order() {
        let runner = GateRunner::new(Duration::from_millis(200)).gate("fake wait", "fake release");
        let outcome = run_bounded(OVERLAP_STEPS, runner, false);

        assert_eq!(
            outcome.log,
            ["fake one", "fake wait (timed out)", "fake release"]
        );
        assert_eq!(outcome.failed(), ["background waits"]);
        assert_eq!(outcome.lines_starting("[2/3]"), ["[2/3] background waits"]);
    }

    /// Step 1 can only finish once a background step without
    /// requirements has run: if the background lane waited for step 1, the
    /// gated command would time out.
    const UNGATED_STEPS: &[Step] = &[
        Step {
            title: SYSTEM_LIBRARIES,
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("apt")],
        },
        Step {
            title: "background download",
            lane: Lane::Background,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("download")],
        },
    ];

    #[test]
    fn download_only_background_steps_start_before_the_system_libraries_finish() {
        let runner = GateRunner::new(Duration::from_secs(10)).gate("fake apt", "fake download");
        let outcome = run_bounded(UNGATED_STEPS, runner, true);

        assert_eq!(outcome.failed(), Vec::<&str>::new(), "{:?}", outcome.log);
        assert_eq!(outcome.log, ["fake download", "fake apt"]);
    }

    const SYSTEM_PACKAGE_STEPS: &[Step] = &[
        Step {
            title: SYSTEM_LIBRARIES,
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("apt")],
        },
        Step {
            title: "background needs system packages",
            lane: Lane::Background,
            requires: &[SYSTEM_LIBRARIES],
            apt: AptShare::None,
            actions: &[fake!("tlmgr")],
        },
    ];

    #[test]
    fn background_steps_that_need_system_packages_wait_for_them() {
        let runner =
            GateRunner::new(Duration::from_secs(10)).delay("fake apt", Duration::from_millis(300));
        let outcome = run_bounded(SYSTEM_PACKAGE_STEPS, runner, true);

        assert_eq!(outcome.log, ["fake apt", "fake tlmgr"]);
    }

    const REQUIRING_STEPS: &[Step] = &[
        Step {
            title: "foreground slow",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("slow")],
        },
        Step {
            title: "background needs foreground",
            lane: Lane::Background,
            requires: &["foreground slow"],
            apt: AptShare::None,
            actions: &[fake!("needs-slow")],
        },
        Step {
            title: "background slow",
            lane: Lane::Background,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("bg-slow")],
        },
        Step {
            title: "foreground needs background",
            lane: Lane::Foreground,
            requires: &["background slow"],
            apt: AptShare::None,
            actions: &[fake!("needs-bg")],
        },
    ];

    #[test]
    fn steps_wait_for_their_requirements_across_lanes() {
        let runner = GateRunner::new(Duration::from_secs(10))
            .delay("fake slow", Duration::from_millis(300))
            .delay("fake bg-slow", Duration::from_millis(300));
        let outcome = run_bounded(REQUIRING_STEPS, runner, true);

        assert_eq!(outcome.failed(), Vec::<&str>::new());
        assert!(outcome.position("fake slow") < outcome.position("fake needs-slow"));
        assert!(outcome.position("fake bg-slow") < outcome.position("fake needs-bg"));
    }

    const PANICKING_STEPS: &[Step] = &[
        Step {
            title: "foreground first",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("one")],
        },
        Step {
            title: "background explodes",
            lane: Lane::Background,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("boom")],
        },
        Step {
            title: "background never runs",
            lane: Lane::Background,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("two")],
        },
        Step {
            title: "foreground needs background",
            lane: Lane::Foreground,
            requires: &["background never runs"],
            apt: AptShare::None,
            actions: &[fake!("three")],
        },
    ];

    #[test]
    fn a_background_panic_fails_unreported_steps_and_keeps_foreground_results() {
        let runner = GateRunner::new(Duration::from_secs(10)).panics_on("fake boom");
        let outcome = run_bounded(PANICKING_STEPS, runner, true);

        let titles: Vec<_> = outcome.results.iter().map(|(title, _)| *title).collect();
        assert_eq!(
            titles,
            [
                "foreground first",
                "background explodes",
                "background never runs",
                "foreground needs background"
            ]
        );
        assert_eq!(
            outcome.failed(),
            ["background explodes", "background never runs"]
        );
        for (_, error) in &outcome.results[1..3] {
            let error = error.as_deref().unwrap();
            assert!(
                error.contains("re-run `hpds setup --profile server`"),
                "{error}"
            );
        }
        assert_eq!(outcome.log, ["fake one", "fake three"]);
        assert_eq!(
            outcome.lines_starting("[2/4]"),
            ["[2/4] background explodes (ran in the background)"]
        );
        assert_eq!(
            outcome.lines_starting("[3/4]"),
            ["[3/4] background never runs (ran in the background)"]
        );
        assert!(
            outcome
                .output
                .iter()
                .any(|line| line.contains("hint: re-run `hpds setup --profile server`")),
            "{:?}",
            outcome.output
        );
    }

    /// A background step whose command fails: its gate never opens.
    const FAILING_BACKGROUND_STEPS: &[Step] = &[
        Step {
            title: "foreground first",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("one")],
        },
        Step {
            title: "background fails",
            lane: Lane::Background,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("stuck")],
        },
    ];

    #[test]
    fn a_failing_background_step_names_the_log_of_the_main_thread() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hpds-setup.log");
        ui::open_log(&log).unwrap();
        let runner =
            GateRunner::new(Duration::from_millis(20)).gate("fake stuck", "fake never runs");
        let git_setup = || Ok(());
        let deps = deps_recording_git(Os::Linux, true, &runner, &PanicFetcher, &git_setup);
        let (results, _) = ui::capture(|| execute(FAILING_BACKGROUND_STEPS, &deps, true));
        ui::close_log();

        let failed: Vec<_> = results
            .iter()
            .filter_map(|result| result.error.as_deref().map(|error| (result.title, error)))
            .collect();
        assert_eq!(failed.len(), 1, "{failed:?}");
        let (title, error) = failed[0];
        assert_eq!(title, "background fails");
        assert!(
            error.contains("gave up waiting for fake never runs"),
            "{error}"
        );
        assert!(
            error.contains(&format!("full output: {}", log.display())),
            "{error}"
        );
    }

    // --- overlapped server profile ------------------------------------------

    /// Wraps the server fakes: the command starting with `slow` takes a
    /// moment, and every command is logged when it finishes.
    struct SlowRunner {
        inner: Sim,
        slow: &'static str,
    }

    impl CommandRunner for SlowRunner {
        fn which(&self, program: &str) -> Option<std::path::PathBuf> {
            self.inner.which(program)
        }

        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
            if format!("{program} {}", args.join(" ")).starts_with(self.slow) {
                std::thread::sleep(Duration::from_millis(300));
            }
            self.inner.run(program, args)
        }
    }

    /// Run the whole server profile overlapped, with `slow` delayed, on
    /// its own thread; a run that has not finished within 30 seconds
    /// fails the test rather than hanging it.
    fn run_server_bounded(slow: &'static str) -> Outcome {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let runner = SlowRunner {
                inner: Sim::default(),
                slow,
            };
            let host = SimHost::default();
            let git_setup = || {
                runner.inner.record("hpds git setup");
                Ok(())
            };
            let fetcher = FakeFetcher::default().with_latest("0.1.0");
            let (_, lookup) = sim_lookup(SimRstudio::default());
            let mut deps = deps_recording_git(Os::Linux, true, &runner, &fetcher, &git_setup);
            deps.installer_lookup = lookup;
            deps.server_host = &host;
            let (results, transcript) =
                ui::capture(|| execute(steps(Profile::Server), &deps, true));
            let _ = tx.send(Outcome {
                results: results.into_iter().map(|r| (r.title, r.error)).collect(),
                log: runner.inner.calls(),
                output: transcript
                    .lines()
                    .iter()
                    .map(|(_, line)| line.clone())
                    .collect(),
            });
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("server setup hung"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("server setup panicked"),
        }
    }

    #[test]
    fn overlapped_server_reports_every_step_once_in_plan_order() {
        let outcome = run_server_bounded("no command starts with this");

        assert_eq!(
            outcome.failed(),
            Vec::<&str>::new(),
            "{:?}",
            outcome.results
        );
        let table = steps(Profile::Server);
        let titles: Vec<_> = outcome.results.iter().map(|(title, _)| *title).collect();
        let planned: Vec<_> = table.iter().map(|step| step.title).collect();
        assert_eq!(titles, planned);
        for (index, step) in table.iter().enumerate() {
            let header = format!("[{}/{}] {}", index + 1, table.len(), step.title);
            let expected = match step.lane {
                Lane::Foreground => header.clone(),
                Lane::Background => format!("{header} (ran in the background)"),
            };
            assert_eq!(
                outcome.lines_starting(&format!("[{}/{}]", index + 1, table.len())),
                [expected.as_str()],
                "{:?}",
                outcome.output
            );
        }
    }

    #[test]
    fn overlapped_server_enables_code_server_only_after_quarto() {
        let outcome = run_server_bounded("quarto --version");
        assert!(
            outcome.position("quarto --version")
                < outcome.position("sudo systemctl enable --now code-server@analyst"),
            "{:?}",
            outcome.log
        );
    }

    #[test]
    fn overlapped_server_installs_uv_tools_only_after_python() {
        let outcome = run_server_bounded("sudo sh -c printf");
        let pip = outcome
            .log
            .iter()
            .position(|call| call.starts_with("sudo sh -c printf"))
            .expect("pip configured");
        assert!(pip < outcome.position("uv --version"), "{:?}", outcome.log);
    }

    #[test]
    fn overlapped_server_runs_git_defaults_only_after_gh() {
        let outcome = run_server_bounded("gh --version");
        assert!(
            outcome.position("gh --version") < outcome.position("hpds git setup"),
            "{:?}",
            outcome.log
        );
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
        assert!(text.contains("✗ rig: boom"), "{text}");
    }

    const GIT_STEP: Step = Step {
        title: "git defaults",
        lane: Lane::Foreground,
        requires: &[],
        apt: AptShare::None,
        actions: &[Action::GitSetup],
    };

    fn git_step_error(git_setup: &(dyn Fn() -> anyhow::Result<()> + Sync)) -> Option<String> {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let deps = deps_recording_git(Os::Linux, true, &runner, &fetcher, git_setup);
        run_step(&GIT_STEP, &deps, &Run::new(&[GIT_STEP])).error
    }

    #[test]
    fn summary_shows_a_hinted_error_message_once() {
        let message = "user.name is not set, and --yes suppresses the prompt for it";
        let error = git_step_error(&|| {
            Err(anyhow!(
                "user.name is not set, and --yes suppresses the prompt for it"
            ))
            .hint("set it with `git config --global user.name`")
        });
        assert_eq!(error.as_deref(), Some(message));
        let text = summary(&[StepResult {
            title: "git defaults",
            error,
        }]);
        assert_eq!(text.matches(message).count(), 1, "{text}");
    }

    #[test]
    fn summary_keeps_the_context_chain_of_a_plain_error() {
        let error = git_step_error(&|| {
            Err(anyhow!("permission denied").context("could not write ~/.gitconfig"))
        });
        assert_eq!(
            error.as_deref(),
            Some("could not write ~/.gitconfig: permission denied")
        );
    }

    #[test]
    fn finish_with_no_selected_steps_is_a_clean_no_op() {
        finish(Profile::Dev, &[]).expect("nothing selected is not an error");
    }

    // --- compact rendering and the setup log ----------------------------

    /// `text` with ANSI color codes removed: the color choice is
    /// process-wide, and another test may change it concurrently.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// `text` with the log path shown as `<LOG>`.
    fn redact(text: &str, log: &Path) -> String {
        text.replace(&log.display().to_string(), "<LOG>")
    }

    /// What the terminal shows, one written line after another, without
    /// color and with the log path redacted.
    fn screen(written: &[(ui::Stream, String)], log: &Path) -> String {
        let lines: Vec<String> = written
            .iter()
            .map(|(_, line)| redact(&strip_ansi(line), log))
            .collect();
        lines.join("\n")
    }

    /// The lines written to one stream, as [`screen`] shows them.
    fn stream_lines(
        written: &[(ui::Stream, String)],
        stream: ui::Stream,
        log: &Path,
    ) -> Vec<String> {
        written
            .iter()
            .filter(|(s, _)| *s == stream)
            .map(|(_, line)| redact(&strip_ansi(line), log))
            .collect()
    }

    /// The log at `path`, redacted; empty when there is none.
    fn read_redacted(path: &Path) -> String {
        redact(&std::fs::read_to_string(path).unwrap_or_default(), path)
    }

    const USER_NAME_WARNING: &str =
        "user.name is not set; run `git config --global user.name <VALUE>` or `hpds git setup`";
    const GH_WARNING: &str = "gh is installed but not logged in to GitHub; run `gh auth login`";

    /// A git-setup seam that succeeds with the two warnings a fresh
    /// account gets.
    fn warning_git_setup() -> anyhow::Result<()> {
        ui::warn(USER_NAME_WARNING);
        ui::warn(GH_WARNING);
        Ok(())
    }

    /// Two command steps and two git steps; both git steps warn the same
    /// two warnings.
    const COMPACT_STEPS: &[Step] = &[
        Step {
            title: "first",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("one")],
        },
        Step {
            title: "git defaults",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[Action::GitSetup],
        },
        Step {
            title: "second",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("two")],
        },
        Step {
            title: "git again",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[Action::GitSetup],
        },
    ];

    /// The runner [`COMPACT_STEPS`] runs against: both commands succeed
    /// with one line of output.
    fn compact_runner() -> FakeRunner {
        FakeRunner::default()
            .with_output("fake one", "one done\n")
            .with_output("fake two", "two done\n")
    }

    /// What one run of a profile flow left behind.
    struct Logged {
        result: anyhow::Result<()>,
        written: Vec<(ui::Stream, String)>,
        /// [`screen`] of `written`.
        terminal: String,
        /// The log, redacted.
        log: String,
    }

    /// Run `table` through `profile`'s flow with the terminal tapped and
    /// the log at `log_path`, non-interactively.
    fn run_logged(
        profile: Profile,
        table: &[Step],
        deps: &SetupDeps,
        yes: bool,
        log_path: &Path,
    ) -> Logged {
        run_logged_with(profile, table, deps, yes, log_path, false)
    }

    /// [`run_logged`] with `--quiet` given as `quiet`.
    fn run_logged_with(
        profile: Profile,
        table: &[Step],
        deps: &SetupDeps,
        yes: bool,
        log_path: &Path,
        quiet: bool,
    ) -> Logged {
        let log = test_log(Some(log_path.to_path_buf()));
        let (result, written) =
            ui::tap_terminal(|| run_profile_with(profile, table, deps, yes, false, &log, quiet));
        Logged {
            result,
            terminal: screen(&written, log_path),
            written,
            log: read_redacted(log_path),
        }
    }

    #[test]
    fn compact_dev_run_shows_one_line_per_step_a_tally_and_next_steps() {
        let fake = compact_runner();
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &warning_git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(Profile::Dev, COMPACT_STEPS, &deps, true, &log_path);

        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(
            stream_lines(&run.written, ui::Stream::Stderr, &log_path),
            Vec::<String>::new(),
            "warnings are collected, not printed inline"
        );
        insta::assert_snapshot!("compact_dev_terminal", run.terminal);
        insta::assert_snapshot!("compact_dev_log", run.log);
        assert!(
            ui::take_reports().is_empty(),
            "the run drains the warnings it collected"
        );
    }

    /// A git-setup seam that fails with a hint.
    fn failing_git_setup() -> anyhow::Result<()> {
        Err(anyhow!(
            "user.name is not set, and --yes suppresses the prompt for it"
        ))
        .hint("set it with `git config --global user.name`")
    }

    /// One passing step, then two that fail.
    const FAILING_COMPACT_STEPS: &[Step] = &[
        Step {
            title: "first",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("one")],
        },
        Step {
            title: "git defaults",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[Action::GitSetup],
        },
        Step {
            title: "second",
            lane: Lane::Foreground,
            requires: &[],
            apt: AptShare::None,
            actions: &[fake!("two")],
        },
    ];

    #[test]
    fn compact_failures_show_their_error_indented_under_the_step_line() {
        let fake = FakeRunner::default()
            .with_output("fake one", "one done\n")
            .with_failure("fake two", "line a\nline b\n");
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &failing_git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(Profile::Dev, FAILING_COMPACT_STEPS, &deps, true, &log_path);

        insta::assert_snapshot!("compact_failure_terminal", run.terminal);
        assert_eq!(ui::log_path(), None, "a failed run closes its log");
        let err = run.result.expect_err("two failed steps fail the run");
        assert_eq!(
            render_error(&err, false),
            "error: 2 of 3 setup steps failed\n\
             hint: fix the reported problems and re-run `hpds setup`; completed steps are \
             idempotent and will no-op"
        );
        // The full detail is in the log, as today's output reads.
        for expected in [
            "$ fake two\nstatus: failed\nstderr:\n  line a\n  line b\n",
            "error: user.name is not set, and --yes suppresses the prompt for it\n\
             hint: set it with `git config --global user.name`\n",
            "setup summary:\n  ✓ first\n  ✗ git defaults: user.name is not set",
        ] {
            assert!(
                run.log.contains(expected),
                "missing {expected:?}:\n{}",
                run.log
            );
        }
        // Compact mode is over: the caller's rendering of the final error
        // reaches the terminal, and nothing is left collected.
        let ((), written) = ui::tap_terminal(|| ui::error(&err));
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(written[0].0, ui::Stream::Stderr);
        assert!(
            written[0].1.contains("2 of 3 setup steps failed"),
            "{written:?}"
        );
        assert!(ui::take_reports().is_empty());
    }

    /// A git-setup seam that raises the two warnings a fresh account gets
    /// and then fails like [`failing_git_setup`].
    fn warning_then_failing_git_setup() -> anyhow::Result<()> {
        warning_git_setup()?;
        failing_git_setup()
    }

    #[test]
    fn quiet_compact_failures_still_show_their_errors_and_next_steps() {
        let fake = FakeRunner::default()
            .with_output("fake one", "one done\n")
            .with_failure("fake two", "line a\nline b\n");
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(
            Os::Mac,
            true,
            &runner,
            &PanicFetcher,
            &warning_then_failing_git_setup,
        );
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged_with(
            Profile::Dev,
            FAILING_COMPACT_STEPS,
            &deps,
            true,
            &log_path,
            true,
        );

        assert!(run.result.is_err());
        assert_eq!(
            stream_lines(&run.written, ui::Stream::Stdout, &log_path),
            [
                "        user.name is not set, and --yes suppresses the prompt for it",
                "        hint: set it with `git config --global user.name`",
                "        `fake` failed: line a",
                "        line b",
                "        full output: <LOG>",
                "        hint: fix the reported problem and re-run setup",
                "Next steps:",
                &format!("  - {USER_NAME_WARNING}"),
                &format!("  - {GH_WARNING}"),
            ],
            "quiet hides the opening line, step lines, and tally, never errors"
        );
        assert_eq!(
            stream_lines(&run.written, ui::Stream::Stderr, &log_path),
            Vec::<String>::new()
        );
        // The log has every quiet line, and each error once, as the step
        // reported it.
        for expected in [
            "hpds setup --profile dev: 3 steps. Full log: <LOG>\n",
            "[1/3] first ✓\n",
            "[2/3] git defaults ✗\n",
            "[3/3] second ✗\n",
            "1 of 3 steps succeeded. Details: <LOG>\n",
        ] {
            assert!(
                run.log.contains(expected),
                "missing {expected:?}:\n{}",
                run.log
            );
        }
        for once in [
            "error: user.name is not set, and --yes suppresses the prompt for it\n",
            "hint: set it with `git config --global user.name`\n",
            "error: `fake` failed: line a\n",
            &format!("warning: {USER_NAME_WARNING}\n"),
            &format!("warning: {GH_WARNING}\n"),
        ] {
            assert_eq!(run.log.matches(once).count(), 1, "{once:?}:\n{}", run.log);
        }
        assert!(!run.log.contains("Next steps:"), "{}", run.log);
        assert!(!run.log.contains("        "), "{}", run.log);
        assert!(ui::take_reports().is_empty());
    }

    #[test]
    fn compact_background_steps_print_their_line_when_their_result_arrives() {
        // The background step can only finish after the last foreground
        // step, so its line comes last.
        let runner = GateRunner::new(Duration::from_secs(10)).gate("fake wait", "fake release");
        let git_setup = || Ok(());
        let deps = deps_recording_git(Os::Linux, true, &runner, &PanicFetcher, &git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(Profile::Server, OVERLAP_STEPS, &deps, true, &log_path);

        assert!(run.result.is_ok(), "{:?}", run.result);
        assert!(
            !run.terminal.contains("runs these"),
            "under --yes the plan goes to the log only:\n{}",
            run.terminal
        );
        insta::assert_snapshot!("compact_background_terminal", run.terminal);
        insta::assert_snapshot!("compact_background_log", run.log);
    }

    #[test]
    fn compact_background_failure_shows_its_error_under_its_line() {
        let runner =
            GateRunner::new(Duration::from_millis(20)).gate("fake stuck", "fake never runs");
        let git_setup = || Ok(());
        let deps = deps_recording_git(Os::Linux, true, &runner, &PanicFetcher, &git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(
            Profile::Server,
            FAILING_BACKGROUND_STEPS,
            &deps,
            true,
            &log_path,
        );

        assert!(run.result.is_err());
        insta::assert_snapshot!("compact_background_failure_terminal", run.terminal);
    }

    #[test]
    fn interactive_server_shows_the_plan_on_the_terminal_before_the_checklist() {
        // Prompts must fail here rather than wait on a terminal a test
        // run may have attached; the plan has to be on screen by then.
        ui::set_non_interactive(true);
        let runner = GateRunner::new(Duration::from_secs(10));
        let git_setup = || panic!("no step may run before the checklist is answered");
        let deps = deps_recording_git(Os::Linux, false, &runner, &PanicFetcher, &git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");
        let log = test_log(Some(log_path.clone()));

        let (result, written) = ui::tap_terminal(|| {
            run_profile(Profile::Server, OVERLAP_STEPS, &deps, false, true, &log)
        });

        let err = result.expect_err("the checklist cannot be shown in a test");
        assert!(render_error(&err, false).contains("cannot ask"), "{err:#}");
        assert_eq!(ui::log_path(), None, "a refused run closes its log");
        assert!(runner.finished().is_empty(), "{:?}", runner.finished());
        let stdout = stream_lines(&written, ui::Stream::Stdout, &log_path).join("\n");
        assert_eq!(
            stdout,
            plan_for(Profile::Server, OVERLAP_STEPS).trim_end(),
            "the plan, and only the plan, is on the terminal before the checklist"
        );
        assert!(
            read_redacted(&log_path).contains("hpds setup --profile server runs these 3 steps:"),
            "the log receives the plan as well"
        );
    }

    #[test]
    fn verbose_run_prints_todays_full_output_and_still_logs_everything() {
        let fake = compact_runner();
        let runner = LoggingRunner::new(&fake);
        let mut deps =
            deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &warning_git_setup);
        deps.install.verbose = true;
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(Profile::Dev, COMPACT_STEPS, &deps, true, &log_path);

        assert!(run.result.is_ok(), "{:?}", run.result);
        insta::assert_snapshot!("verbose_dev_terminal", run.terminal);
        assert!(
            run.log.starts_with(
                "=== hpds setup --profile dev ===\n\
                 hpds version: 9.9.9\n\
                 started: 2026-11-03 21:15:00 UTC\n\
                 os: linux x86_64\n"
            ),
            "{}",
            run.log
        );
        for expected in [
            "[1/4] first\none\n$ fake one\nstatus: success\nstdout:\n  one done\n",
            &format!("[2/4] git defaults\nwarning: {USER_NAME_WARNING}\nwarning: {GH_WARNING}\n"),
            "setup summary:\n  ✓ first\n  ✓ git defaults\n  ✓ second\n  ✓ git again\n\
             ✓ setup complete\n",
        ] {
            assert!(
                run.log.contains(expected),
                "missing {expected:?}:\n{}",
                run.log
            );
        }
        assert!(ui::take_reports().is_empty());
    }

    #[test]
    fn verbose_server_run_with_yes_prints_the_plan_to_the_terminal_and_the_log() {
        let runner = GateRunner::new(Duration::from_secs(10)).gate("fake wait", "fake release");
        let git_setup = || Ok(());
        let mut deps = deps_recording_git(Os::Linux, true, &runner, &PanicFetcher, &git_setup);
        deps.install.verbose = true;
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(Profile::Server, OVERLAP_STEPS, &deps, true, &log_path);

        assert!(run.result.is_ok(), "{:?}", run.result);
        let plan = plan_for(Profile::Server, OVERLAP_STEPS);
        let plan = plan.trim_end();
        assert!(
            run.terminal.starts_with(plan),
            "-v shows the plan first, even with --yes:\n{}",
            run.terminal
        );
        assert_eq!(run.log.matches(plan).count(), 1, "{}", run.log);
    }

    #[test]
    fn each_run_appends_its_own_header_and_keeps_what_the_log_held() {
        let fake = compact_runner();
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &warning_git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");
        std::fs::write(&log_path, "written by the bootstrap script\n").unwrap();

        let first = run_logged(Profile::Dev, COMPACT_STEPS, &deps, true, &log_path);
        assert!(first.result.is_ok(), "{:?}", first.result);
        assert_eq!(ui::log_path(), None, "the run closes its log");
        let second_log = SetupLog {
            path: Some(log_path.clone()),
            info: RunInfo {
                started: "2026-11-04 09:00:00 UTC".to_string(),
                ..test_info()
            },
        };
        let (second, _) = ui::tap_terminal(|| {
            run_profile(Profile::Dev, COMPACT_STEPS, &deps, true, false, &second_log)
        });
        assert!(second.is_ok(), "{second:?}");
        assert_eq!(ui::log_path(), None, "the run closes its log");

        let log = read_redacted(&log_path);
        let first_header = "=== hpds setup --profile dev ===\n\
                            hpds version: 9.9.9\n\
                            started: 2026-11-03 21:15:00 UTC\n\
                            os: linux x86_64\n";
        let second_header = "=== hpds setup --profile dev ===\n\
                             hpds version: 9.9.9\n\
                             started: 2026-11-04 09:00:00 UTC\n\
                             os: linux x86_64\n";
        assert!(
            log.starts_with(&format!("written by the bootstrap script\n{first_header}")),
            "{log}"
        );
        let second_at = log.find(second_header).unwrap_or_else(|| panic!("{log}"));
        assert_eq!(log.matches("setup summary:").count(), 2, "{log}");
        assert!(
            log[..second_at].contains("setup summary:"),
            "the first run's section is complete before the second begins:\n{log}"
        );
    }

    /// The stdout lines of a successful [`COMPACT_STEPS`] run without a
    /// log.
    fn compact_lines_without_a_log() -> Vec<String> {
        [
            "hpds setup --profile dev: 4 steps. No log file.",
            "[1/4] first ✓",
            "[2/4] git defaults ✓",
            "[3/4] second ✓",
            "[4/4] git again ✓",
            "All 4 steps succeeded.",
            "Next steps:",
            &format!("  - {USER_NAME_WARNING}"),
            &format!("  - {GH_WARNING}"),
        ]
        .iter()
        .map(|line| line.to_string())
        .collect()
    }

    #[test]
    fn without_home_the_run_warns_and_continues_without_a_log() {
        let fake = compact_runner();
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &warning_git_setup);
        let log = test_log(None);

        let (result, written) =
            ui::tap_terminal(|| run_profile(Profile::Dev, COMPACT_STEPS, &deps, true, false, &log));

        assert!(result.is_ok(), "{result:?}");
        let nowhere = Path::new("/no-log");
        let stderr = stream_lines(&written, ui::Stream::Stderr, nowhere);
        assert_eq!(stderr.len(), 1, "{stderr:?}");
        assert!(stderr[0].starts_with("warning: "), "{stderr:?}");
        assert!(stderr[0].contains("HOME"), "{stderr:?}");
        assert!(stderr[0].contains("without a log"), "{stderr:?}");
        assert_eq!(
            stream_lines(&written, ui::Stream::Stdout, nowhere),
            compact_lines_without_a_log()
        );
    }

    #[test]
    fn a_log_that_cannot_be_opened_is_a_warning_and_the_run_continues() {
        let fake = compact_runner();
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &warning_git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("missing-dir").join("hpds-setup.log");
        let log = test_log(Some(log_path.clone()));

        let (result, written) =
            ui::tap_terminal(|| run_profile(Profile::Dev, COMPACT_STEPS, &deps, true, false, &log));

        assert!(result.is_ok(), "{result:?}");
        let unredacted = Path::new("/no-log");
        let stderr = stream_lines(&written, ui::Stream::Stderr, unredacted);
        assert_eq!(stderr.len(), 1, "{stderr:?}");
        assert!(stderr[0].starts_with("warning: "), "{stderr:?}");
        assert!(
            stderr[0].contains(&log_path.display().to_string()),
            "the warning names the log it could not open: {stderr:?}"
        );
        assert!(stderr[0].contains("without a log"), "{stderr:?}");
        assert_eq!(
            stream_lines(&written, ui::Stream::Stdout, unredacted),
            compact_lines_without_a_log()
        );
        assert!(!log_path.exists());
    }

    #[test]
    fn a_running_foreground_step_is_the_active_spinner_so_prompts_suspend_it() {
        let seen = Mutex::new(Vec::new());
        let git_setup = || {
            seen.lock().unwrap().push(ui::active_spinner_message());
            Ok(())
        };
        let fake = compact_runner();
        let runner = LoggingRunner::new(&fake);
        let deps = deps_recording_git(Os::Mac, true, &runner, &PanicFetcher, &git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");

        let run = run_logged(Profile::Dev, COMPACT_STEPS, &deps, true, &log_path);

        assert!(run.result.is_ok(), "{:?}", run.result);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                Some("[2/4] git defaults".to_string()),
                Some("[4/4] git again".to_string())
            ]
        );
        assert_eq!(
            ui::active_spinner_message(),
            None,
            "no spinner outlives its step"
        );
    }

    /// The terminal lines after the tally must be a `Next steps:` list.
    fn assert_next_steps(rest: &[String]) {
        if let Some((first, items)) = rest.split_first() {
            assert_eq!(first, "Next steps:", "{rest:#?}");
            assert!(!items.is_empty(), "{rest:#?}");
            for item in items {
                assert!(item.starts_with("  - "), "{rest:#?}");
            }
        }
    }

    #[test]
    fn compact_dev_profile_shows_one_line_per_step_in_plan_order() {
        let runner = runner_with_everything_installed();
        let fetcher = FakeFetcher::default().with_latest("0.1.0");
        let git_setup = || Ok(());
        let deps = deps_recording_git(Os::Mac, true, &runner, &fetcher, &git_setup);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("hpds-setup.log");
        let log = test_log(Some(log_path.clone()));

        let (result, written) = ui::tap_terminal(|| run_dev(&deps, true, false, &log));

        assert!(result.is_ok(), "{result:?}");
        let table = steps(Profile::Dev);
        let total = table.len();
        let lines = stream_lines(&written, ui::Stream::Stdout, &log_path);
        let mut expected = vec![format!(
            "hpds setup --profile dev: {total} steps. Full log: <LOG>"
        )];
        for (index, step) in table.iter().enumerate() {
            expected.push(format!("[{}/{total}] {} ✓", index + 1, step.title));
        }
        expected.push(format!("All {total} steps succeeded. Details: <LOG>"));
        assert!(lines.len() >= expected.len(), "{lines:#?}");
        assert_eq!(lines[..expected.len()], expected[..], "{lines:#?}");
        assert_next_steps(&lines[expected.len()..]);
        assert_eq!(
            stream_lines(&written, ui::Stream::Stderr, &log_path),
            Vec::<String>::new()
        );
    }

    #[test]
    fn compact_overlapped_server_shows_every_step_once_and_logs_the_plan() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let sim = Sim::default();
            let host = SimHost::default();
            let git_setup = || {
                sim.record("hpds git setup");
                Ok(())
            };
            let fetcher = FakeFetcher::default().with_latest("0.1.0");
            let (_, lookup) = sim_lookup(SimRstudio::default());
            let mut deps = deps_recording_git(Os::Linux, true, &sim, &fetcher, &git_setup);
            deps.installer_lookup = lookup;
            deps.server_host = &host;
            let dir = tempfile::tempdir().unwrap();
            let log_path = dir.path().join("hpds-setup.log");
            let log = test_log(Some(log_path.clone()));
            let (result, written) = ui::tap_terminal(|| run_server(&deps, true, false, &log));
            let _ = tx.send((
                result.map_err(|err| format!("{err:#}")),
                stream_lines(&written, ui::Stream::Stdout, &log_path),
                read_redacted(&log_path),
            ));
        });
        let (result, lines, log) = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("server setup hung"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("server setup panicked"),
        };

        assert!(result.is_ok(), "{result:?}");
        let table = steps(Profile::Server);
        let total = table.len();
        assert_eq!(
            lines.first().map(String::as_str),
            Some(format!("hpds setup --profile server: {total} steps. Full log: <LOG>").as_str()),
            "{lines:#?}"
        );
        let step_lines: Vec<&String> = lines.iter().filter(|line| line.starts_with('[')).collect();
        assert_eq!(step_lines.len(), total, "{lines:#?}");
        for (index, step) in table.iter().enumerate() {
            let expected = format!("[{}/{total}] {} ✓", index + 1, step.title);
            assert_eq!(
                lines.iter().filter(|line| **line == expected).count(),
                1,
                "{expected}: {lines:#?}"
            );
        }
        let foreground: Vec<String> = lines
            .iter()
            .filter(|line| {
                table.iter().enumerate().any(|(index, step)| {
                    step.lane == Lane::Foreground
                        && **line == format!("[{}/{total}] {} ✓", index + 1, step.title)
                })
            })
            .cloned()
            .collect();
        let planned_foreground: Vec<String> = table
            .iter()
            .enumerate()
            .filter(|(_, step)| step.lane == Lane::Foreground)
            .map(|(index, step)| format!("[{}/{total}] {} ✓", index + 1, step.title))
            .collect();
        assert_eq!(
            foreground, planned_foreground,
            "foreground lines follow plan order"
        );
        let tally = lines
            .iter()
            .position(|line| line.starts_with("All "))
            .unwrap_or_else(|| panic!("no tally: {lines:#?}"));
        assert_eq!(
            lines[tally],
            format!("All {total} steps succeeded. Details: <LOG>")
        );
        assert_eq!(tally, total + 1, "{lines:#?}");
        assert_next_steps(&lines[tally + 1..]);
        assert!(
            !lines.iter().any(|line| line.contains("runs these")),
            "{lines:#?}"
        );

        assert!(
            log.contains(&plan(Profile::Server).trim_end().to_string()),
            "the log holds the plan:\n{log}"
        );
        for (index, step) in table.iter().enumerate() {
            if step.lane == Lane::Background {
                let header = format!(
                    "[{}/{total}] {} (ran in the background)",
                    index + 1,
                    step.title
                );
                assert!(log.contains(&header), "missing {header}:\n{log}");
            }
        }
        assert!(log.contains("setup summary:\n"), "{log}");
    }

    #[test]
    fn the_tally_counts_one_step_in_the_singular() {
        assert_eq!(tally(1, 1, None), "All 1 step succeeded.");
        assert_eq!(tally(0, 1, None), "0 of 1 step succeeded.");
        assert_eq!(
            tally(1, 2, Some(Path::new("/home/nero/hpds-setup.log"))),
            "1 of 2 steps succeeded. Details: /home/nero/hpds-setup.log"
        );
        assert_eq!(tally(2, 2, None), "All 2 steps succeeded.");
    }

    // --- the log's run header -----------------------------------------------

    #[test]
    fn utc_timestamps_read_as_calendar_dates_in_utc() {
        assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_timestamp(1_793_740_500), "2026-11-03 21:15:00 UTC");
        assert_eq!(utc_timestamp(1_709_251_199), "2024-02-29 23:59:59 UTC");
        assert_eq!(utc_timestamp(951_868_800), "2000-03-01 00:00:00 UTC");
    }

    #[test]
    fn the_current_run_reports_this_build_and_host() {
        let info = RunInfo::current();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.os.contains(std::env::consts::OS), "{info:?}");
        assert!(info.started.ends_with(" UTC"), "{info:?}");
        assert_eq!(
            info.started.len(),
            "2026-11-03 21:15:00 UTC".len(),
            "{info:?}"
        );
    }
}
