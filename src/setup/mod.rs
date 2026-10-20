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
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, mpsc};

use anyhow::anyhow;

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

/// The server profile's first step, which installs every selected step's
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
/// The first step installs every selected step's apt packages in one
/// transaction (each step's `apt` share); the steps that own those
/// packages keep their own install paths, which find the packages
/// installed and do nothing, or install them when the first step could
/// not.
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
                what: "installing the Python, Jupyter, Quarto, and Ruff extensions",
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
    let steps = steps(profile);
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
        Action::Install(tool)
            if matches!(
                *tool,
                "uv" | "gh" | "duckdb" | "quarto" | "togi" | "rstudio-server"
            ) =>
        {
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

/// Run every selected step, reporting each failure as it happens and
/// carrying on with the rest. Results come back in plan order, one per
/// selected step.
///
/// With `overlap`, background-lane steps run on a worker thread while the
/// foreground lane runs on this one (see [`execute_overlapped`]). Without
/// it, or when no background step is selected, every step runs here, one
/// at a time, in plan order.
pub fn execute(steps: &[Step], deps: &SetupDeps, overlap: bool) -> Vec<StepResult> {
    let run = Run::new(steps);
    let (foreground, background) = partition(steps);
    if overlap && !background.is_empty() {
        return execute_overlapped(&run, &foreground, &background, deps);
    }
    execute_sequential(&run, deps)
}

/// What every step of one run shares: the selected steps, and the steps
/// whose own apt packages the apt prelude has installed.
struct Run<'s> {
    selected: &'s [Step],
    covered: Mutex<HashSet<&'static str>>,
}

impl<'s> Run<'s> {
    fn new(selected: &'s [Step]) -> Self {
        Run {
            selected,
            covered: Mutex::new(HashSet::new()),
        }
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
fn execute_sequential(run: &Run, deps: &SetupDeps) -> Vec<StepResult> {
    let total = run.selected.len();
    run.selected
        .iter()
        .enumerate()
        .map(|(index, step)| {
            ui::println(&header(index, total, step.title));
            run_step(step, deps, run)
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
            ui::warn(&format!("step failed: {}; continuing", step.title));
            Some(format!("{err:#}"))
        }
    };
    StepResult {
        title: step.title,
        error,
    }
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
const UNREPORTED_HINT: &str =
    "re-run `hpds setup` to retry it; steps that completed are idempotent and will no-op";

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
/// as they arrive, then the rest once its own lane is done.
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
) -> Vec<StepResult> {
    let total = run.selected.len();
    let completed = Completed::new(foreground.iter().map(|(_, step)| step.title));
    let worker_deps = deps.for_thread();
    let (tx, rx) = mpsc::channel::<Finished>();

    let results = std::thread::scope(|scope| {
        let completed = &completed;
        let worker = std::thread::Builder::new()
            .name("hpds-setup-background".to_string())
            .spawn_scoped(scope, move || {
                run_background(background, &worker_deps, run, completed, &tx);
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
            total,
            results: (0..total).map(|_| None).collect(),
            unreported: background.iter().map(|(_, step)| step.title).collect(),
            rx,
        };
        let release = ReleaseForeground(completed);
        for (index, step) in foreground {
            lane.wait_for(step.requires);
            ui::println(&header(*index, total, step.title));
            let result = run_step(step, deps, run);
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
                ui::println(&background_header(*index, total, step.title));
                ui::error(&unreported_error());
                *slot = Some(StepResult {
                    title: step.title,
                    error: Some(format!("{UNREPORTED}; {UNREPORTED_HINT}")),
                });
            }
        }
        Some(results)
    });

    match results {
        Some(results) => results.into_iter().flatten().collect(),
        None => execute_sequential(run, deps),
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
struct ForegroundLane {
    total: usize,
    results: Vec<Option<StepResult>>,
    unreported: HashSet<&'static str>,
    rx: mpsc::Receiver<Finished>,
}

impl ForegroundLane {
    /// Record a background step's outcome and replay its output.
    fn record(&mut self, finished: Finished) {
        let title = finished.result.title;
        self.unreported.remove(title);
        ui::println(&background_header(finished.index, self.total, title));
        ui::replay(&finished.transcript);
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
                let covered = prelude::run(run.selected, deps)?;
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
    finish(&execute(&selected, deps, false), None)
}

/// The server-profile flow: print the plan, checklist, confirm, execute,
/// summarize into `log_path`. The caller has already verified the host is
/// Linux; nothing runs until the confirmation gate passes (`--yes` skips
/// it, and a non-interactive session without `--yes` refuses). Under
/// `--yes` the background lane overlaps the foreground; an interactive
/// run keeps every step on the main thread, where it may prompt.
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
    finish(&execute(&selected, deps, yes), Some(log_path))
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
    use crate::install::{CommandOutput, CommandRunner, ReleaseFetcher};
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
    }
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
                ]
                .iter()
                .all(|id| line.contains(id))
            }),
            "one plan line installs all four extensions:\n{text}"
        );
        let lowercase = text.to_ascii_lowercase();
        for omitted in [
            "miniconda",
            "conda",
            "apt upgrade",
            "dist-upgrade",
            "reboot",
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

        run_dev(&deps, true, false).expect("every step no-ops successfully");

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

        let err = finish(&results, None).expect_err("a failed step fails the run");
        assert!(err.to_string().contains("1 of 8"), "{err}");
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
            runner.calls().is_empty(),
            "zero commands may run: {:?}",
            runner.calls()
        );
        assert!(!log.path().join("hpds-setup.log").exists());
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
            self.existing.iter().any(|file| Path::new(file) == path) || NoServerHost.exists(path)
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

        finish(
            &execute(steps(Profile::Server), &deps, false),
            Some(&log_path),
        )
        .expect("all steps succeed");

        let calls = sim.calls();
        // System steps ran as the planned commands, under sudo where
        // declared, all through the runner seam.
        assert_eq!(
            sim.apt_calls().first(),
            Some(&install::apt_get_command_line(&["update"]))
        );
        // All four extensions install in one code-server invocation, before
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
            "code-server --install-extension ms-python.python --install-extension ms-toolsai.jupyter --install-extension quarto.quarto --install-extension charliermarsh.ruff",
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

        let only_libraries = prelude::prelude_plan(&steps(Profile::Server)[..1]);
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
        let (results, rstudio) =
            run_sim(&sim, &host, SimRstudio::default(), steps(Profile::Server));

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
        let (results, rstudio) = run_sim(
            &sim,
            &host,
            SimRstudio::default(),
            &steps(Profile::Server)[..1],
        );
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
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), steps(Profile::Server));

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
        let (results, rstudio) =
            run_sim(&sim, &SimHost::default(), rstudio, steps(Profile::Server));

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
            steps(Profile::Server),
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
            steps(Profile::Server),
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
            steps(Profile::Server),
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
            steps(Profile::Server),
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
            steps(Profile::Server),
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
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), steps(Profile::Server));

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
        let (results, _) = run_sim(&sim, &SimHost::default(), rstudio, steps(Profile::Server));

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
            ui::capture(|| run_sim(&sim, &host, SimRstudio::default(), steps(Profile::Server)));
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

    #[test]
    fn a_rerun_downloads_nothing_already_installed() {
        let sim = Sim::provisioned();
        let host = SimHost::default();
        let (results, rstudio) =
            run_sim(&sim, &host, SimRstudio::default(), steps(Profile::Server));

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
        let (results, _) = run_sim(&sim, &host, rstudio, steps(Profile::Server));

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
        let (results, _, lines) = run_sim_captured(&sim, &host, rstudio, steps(Profile::Server));

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
    fn a_panicking_package_download_drops_only_that_package() {
        let sim = Sim::default();
        let rstudio = SimRstudio {
            first_download_panics: AtomicBool::new(true),
            ..SimRstudio::default()
        };
        let (results, rstudio, lines) =
            run_sim_captured(&sim, &SimHost::default(), rstudio, steps(Profile::Server));

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
        let (results, _) = run_sim(&sim, &host, SimRstudio::default(), steps(Profile::Server));

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
            steps(Profile::Server),
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
        let (results, rstudio) =
            run_sim(&sim, &SimHost::default(), rstudio, steps(Profile::Server));

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
        let (results, rstudio) =
            run_sim(&sim, &SimHost::default(), rstudio, steps(Profile::Server));

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
        let step_one: Vec<&str> = text
            .lines()
            .skip_while(|line| !line.ends_with("1. system libraries (apt)"))
            .skip(1)
            .take_while(|line| !line.ends_with("2. Docker Engine"))
            .collect();
        let merged = step_one
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
            step_one
                .iter()
                .filter(|line| line.ends_with("apt-get -o DPkg::Lock::Timeout=600 update"))
                .count(),
            1,
            "{step_one:#?}"
        );
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
        assert_eq!(steps(Profile::Server)[0].title, SYSTEM_LIBRARIES);
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
        assert_eq!(indices(&foreground), [0, 1, 3, 4, 7, 8, 9, 13, 14]);
        assert_eq!(indices(&background), [2, 5, 6, 10, 11, 12]);
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
            assert!(error.contains("re-run `hpds setup`"), "{error}");
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
                .any(|line| line.contains("hint: re-run `hpds setup`")),
            "{:?}",
            outcome.output
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
        finish(&[], None).expect("nothing selected is not an error");
    }
}
