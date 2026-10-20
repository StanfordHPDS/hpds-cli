//! The apt prelude: the server profile's first step installs, in one apt
//! transaction, the packages every selected step needs.
//!
//! Each step declares its share ([`AptShare`]). The prelude registers the
//! third-party repositories the selected steps need and refreshes the
//! package lists once while, on threads of their own, it downloads and
//! verifies their package files; then it runs one `apt-get install` of
//! everything. Anything it cannot prepare is left out with a warning, and
//! the owning step installs it later through its own path, where a
//! failure is reported as that step's error.

use std::thread::{Scope, ScopedJoinHandle};

use anyhow::anyhow;

use super::{SetupDeps, Step, server};
use crate::install::installers::rig;
use crate::install::{self, DownloadedPackage};
use crate::ui::{self, HintExt};

/// What a step contributes to the apt prelude when it is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AptShare {
    /// Nothing.
    None,
    /// Packages from the distribution's own repositories.
    Packages(&'static [&'static str]),
    /// Packages from a third-party repository the prelude registers first.
    Repo(AptRepo),
    /// A package file the prelude downloads and verifies.
    LocalDeb(LocalDeb),
}

/// A third-party apt repository a step installs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AptRepo {
    Docker,
    Rig,
}

impl AptRepo {
    /// The packages the owning step installs from this repository.
    fn packages(self) -> &'static [&'static str] {
        match self {
            AptRepo::Docker => server::DOCKER_PACKAGES,
            AptRepo::Rig => &[rig::RIG_APT_PACKAGE],
        }
    }

    /// The plan lines for registering this repository.
    fn plan(self) -> Vec<String> {
        match self {
            AptRepo::Docker => vec![server::DOCKER_REPO_PLAN.to_string()],
            AptRepo::Rig => rig::rig_repo_plan(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            AptRepo::Docker => "Docker",
            AptRepo::Rig => "rig",
        }
    }
}

/// A package file a step installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalDeb {
    RstudioServer,
    CodeServer,
}

impl LocalDeb {
    /// The plan line for downloading and verifying this package.
    fn plan(self) -> &'static str {
        match self {
            LocalDeb::RstudioServer => {
                "download the latest stable RStudio Server amd64 package from Posit and verify its Debian package identity with dpkg-deb"
            }
            LocalDeb::CodeServer => server::CODE_SERVER_PACKAGE_PLAN,
        }
    }

    /// How the merged install line in the plan names this package.
    fn placeholder(self) -> &'static str {
        match self {
            LocalDeb::RstudioServer => "<verified rstudio-server package>",
            LocalDeb::CodeServer => "<verified code-server package>",
        }
    }

    /// When the prelude skips this package.
    fn skipped_when(self) -> &'static str {
        match self {
            LocalDeb::RstudioServer => "unless the installed version is current",
            LocalDeb::CodeServer => "unless code-server is installed",
        }
    }

    fn name(self) -> &'static str {
        match self {
            LocalDeb::RstudioServer => "RStudio Server",
            LocalDeb::CodeServer => "code-server",
        }
    }

    /// The name of the thread that prepares this package.
    fn thread_name(self) -> &'static str {
        match self {
            LocalDeb::RstudioServer => "hpds-prepare-rstudio-server",
            LocalDeb::CodeServer => "hpds-prepare-code-server",
        }
    }
}

/// What the prelude would do for a selection, before probing the host.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PreludePlan {
    /// Distribution packages, deduplicated, in plan order.
    pub(super) base: Vec<&'static str>,
    /// Third-party repositories, with the step each belongs to.
    pub(super) repos: Vec<(AptRepo, &'static str)>,
    /// Package files, with the step each belongs to.
    pub(super) debs: Vec<(LocalDeb, &'static str)>,
}

/// Collect the selected steps' shares, in plan order.
pub(super) fn prelude_plan(selected: &[Step]) -> PreludePlan {
    let mut plan = PreludePlan {
        base: Vec::new(),
        repos: Vec::new(),
        debs: Vec::new(),
    };
    for step in selected {
        match step.apt {
            AptShare::None => {}
            AptShare::Packages(packages) => push_unique(&mut plan.base, packages),
            AptShare::Repo(repo) => {
                if !plan.repos.iter().any(|(known, _)| *known == repo) {
                    plan.repos.push((repo, step.title));
                }
            }
            AptShare::LocalDeb(deb) => {
                if !plan.debs.iter().any(|(known, _)| *known == deb) {
                    plan.debs.push((deb, step.title));
                }
            }
        }
    }
    plan
}

fn push_unique<'a>(list: &mut Vec<&'a str>, items: &[&'a str]) {
    for item in items {
        if !list.contains(item) {
            list.push(item);
        }
    }
}

/// The `apt-get install -y <packages>` arguments.
fn install_args<'a>(packages: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["install", "-y"];
    args.extend_from_slice(packages);
    args
}

/// The plan lines for the prelude over `all`, the whole profile: what it
/// does when every step is selected. Unselected steps' shares are left
/// out at run time, as are repositories and packages already installed.
pub(super) fn describe(all: &[Step]) -> Vec<String> {
    let plan = prelude_plan(all);
    let mut lines = Vec::new();
    for (repo, title) in &plan.repos {
        for line in repo.plan() {
            lines.push(format!("for {title}, unless installed: {line}"));
        }
    }
    for (deb, title) in &plan.debs {
        lines.push(format!(
            "for {title}, {}: {}",
            deb.skipped_when(),
            deb.plan()
        ));
    }
    lines.push(install::apt_get_command_line(&["update"]));
    if !plan.repos.is_empty() {
        lines.push(
            "if the refresh fails: remove the apt sources this run added and refresh again (their steps then install their own packages)"
                .to_string(),
        );
    }
    let mut merged = plan.base.clone();
    for (repo, _) in &plan.repos {
        push_unique(&mut merged, repo.packages());
    }
    for (deb, _) in &plan.debs {
        merged.push(deb.placeholder());
    }
    lines.push(install::apt_get_command_line(&install_args(&merged)));
    if merged != plan.base {
        lines.push(format!(
            "if that install fails: {} (the other steps then install their own packages)",
            install::apt_get_command_line(&install_args(&plan.base))
        ));
    }
    lines
}

/// Run the prelude for `selected`, returning the titles of the steps
/// whose [`AptShare::Packages`] it installed. Fails only when the package
/// lists cannot be refreshed even without this run's repositories, or
/// when the install of the distribution packages alone fails.
pub(super) fn run(selected: &[Step], deps: &SetupDeps) -> anyhow::Result<Vec<&'static str>> {
    let plan = prelude_plan(selected);
    let base_titles: Vec<&'static str> = selected
        .iter()
        .filter(|step| matches!(step.apt, AptShare::Packages(_)))
        .map(|step| step.title)
        .collect();

    // Each package file is prepared on a thread of its own while this
    // thread registers the repositories and refreshes the package lists.
    // The files stay on disk until this function returns, after apt has
    // installed them.
    let (refreshed, registered, packages) = std::thread::scope(|scope| {
        let preparations: Vec<Preparation> = plan
            .debs
            .iter()
            .map(|(deb, title)| Preparation::start(scope, *deb, title, deps))
            .collect();
        let mut registered = register_missing(&plan, deps);
        let refreshed = refresh(deps, &mut registered);
        let packages: Vec<(DownloadedPackage, String)> = preparations
            .into_iter()
            .filter_map(|preparation| preparation.finish(deps))
            .collect();
        (refreshed, registered, packages)
    });
    refreshed?;
    let mut extra: Vec<&'static str> = Vec::new();
    for registered in &registered {
        push_unique(&mut extra, registered.repo.packages());
    }

    let mut merged = plan.base.clone();
    push_unique(&mut merged, &extra);
    merged.extend(packages.iter().map(|(_, path)| path.as_str()));
    if merged.is_empty() {
        return Ok(base_titles);
    }
    let first = match deps.install.run_apt_step(
        "installing the system libraries and the selected steps' packages",
        &install_args(&merged),
    ) {
        Ok(_) => return Ok(base_titles),
        Err(err) => err,
    };
    if merged == plan.base || plan.base.is_empty() {
        return Err(first);
    }
    ui::warn(&format!(
        "the combined apt install failed ({first:#}); retrying with the distribution packages only, and the other steps will install their own"
    ));
    match deps.install.run_apt_step(
        "installing the distribution packages",
        &install_args(&plan.base),
    ) {
        Ok(_) => Ok(base_titles),
        Err(second) => Err(anyhow!(
            "{second:#}; the combined install had failed first: {first:#}"
        ))
        .hint("fix the apt error above and rerun server setup; completed steps are idempotent"),
    }
}

/// Register the repositories in `plan` whose packages still need
/// installing. One that cannot be registered is left out with a warning.
fn register_missing(plan: &PreludePlan, deps: &SetupDeps) -> Vec<Registered> {
    let mut registered = Vec::new();
    for (repo, title) in &plan.repos {
        if !repo_missing(*repo, deps) {
            continue;
        }
        match register(*repo, deps) {
            Ok(created) => registered.push(Registered {
                repo: *repo,
                created,
            }),
            Err(err) => ui::warn(&format!(
                "could not register the {} apt repository ({err:#}); {title} will install it itself",
                repo.name()
            )),
        }
    }
    registered
}

/// A verified package file and the path `apt-get` installs it from, or
/// `None` when the package is current.
type Prepared = anyhow::Result<Option<(DownloadedPackage, String)>>;

/// One package file being prepared.
struct Preparation<'scope> {
    deb: LocalDeb,
    title: &'static str,
    /// The thread preparing it, with its output captured; `None` when no
    /// thread could be started, and the package is prepared on this one
    /// when it is collected.
    thread: Option<ScopedJoinHandle<'scope, (Prepared, ui::Transcript)>>,
}

impl<'scope> Preparation<'scope> {
    /// Start preparing `deb` on a thread of its own. The preparation only
    /// probes, downloads, and runs `dpkg-deb`: it never asks for sudo or
    /// prompts, so capturing its output costs it nothing.
    fn start<'a: 'scope>(
        scope: &'scope Scope<'scope, '_>,
        deb: LocalDeb,
        title: &'static str,
        deps: &SetupDeps<'a>,
    ) -> Self {
        let thread_deps = deps.for_thread();
        let thread = std::thread::Builder::new()
            .name(deb.thread_name().to_string())
            .spawn_scoped(scope, move || {
                ui::capture(|| prepare_file(deb, &thread_deps))
            })
            .ok();
        Preparation { deb, title, thread }
    }

    /// Wait for the package and replay what its preparation printed. A
    /// package that could not be prepared, including one whose thread
    /// panicked, is left out with a warning.
    fn finish(self, deps: &SetupDeps) -> Option<(DownloadedPackage, String)> {
        let prepared = match self.thread {
            Some(thread) => match thread.join() {
                Ok((prepared, transcript)) => {
                    ui::replay(&transcript);
                    prepared
                }
                // The panic hook has already reported the panic itself.
                Err(_) => Err(anyhow!("its preparation stopped unexpectedly")),
            },
            None => prepare_file(self.deb, deps),
        };
        match prepared {
            Ok(found) => found,
            Err(err) => {
                ui::warn(&format!(
                    "could not prepare the {} package ({err:#}); {} will install it itself",
                    self.deb.name(),
                    self.title
                ));
                None
            }
        }
    }
}

/// Prepare `deb` and resolve the path `apt-get` installs it from.
fn prepare_file(deb: LocalDeb, deps: &SetupDeps) -> Prepared {
    let Some(package) = prepare(deb, deps)? else {
        return Ok(None);
    };
    let path = server::absolute_package_path(&package)?;
    Ok(Some((package, path)))
}

/// A repository the prelude registered in this run.
struct Registered {
    repo: AptRepo,
    /// The source file this run created, if it did not exist before.
    created: Option<&'static str>,
}

/// Refresh the package lists. If that fails while this run's own
/// repository sources are in place, remove those sources, leave their
/// repositories out of `registered`, and refresh once more: one
/// unreachable third-party repository must not stop the distribution
/// packages from installing. The owning steps register their repositories
/// again later and report their own errors.
fn refresh(deps: &SetupDeps, registered: &mut Vec<Registered>) -> anyhow::Result<()> {
    let first = match deps
        .install
        .run_apt_step("refreshing apt package lists", &["update"])
    {
        Ok(_) => return Ok(()),
        Err(err) => err,
    };
    if !registered.iter().any(|repo| repo.created.is_some()) {
        return Err(first);
    }
    let mut unwound = Vec::new();
    registered.retain(|registered| {
        let Some(path) = registered.created else {
            return true;
        };
        let what = format!(
            "removing the {} apt source this run added",
            registered.repo.name()
        );
        if let Err(err) = deps.install.run_sudo_step(&what, "rm", &["-f", path]) {
            ui::warn(&format!("could not remove `{path}`: {err:#}"));
        }
        unwound.push(registered.repo.name());
        false
    });
    ui::warn(&format!(
        "refreshing the apt package lists failed ({first:#}); retrying without the {} apt {}, whose steps will install their own packages",
        unwound.join(" and "),
        if unwound.len() == 1 {
            "repository"
        } else {
            "repositories"
        }
    ));
    deps.install
        .run_apt_step("refreshing apt package lists again", &["update"])
        .map(|_| ())
        .map_err(|second| anyhow!("{second:#}; the first refresh had failed too: {first:#}"))
        .hint("fix the apt error above and rerun server setup; completed steps are idempotent")
}

/// Whether the packages from `repo` still need installing.
fn repo_missing(repo: AptRepo, deps: &SetupDeps) -> bool {
    match repo {
        AptRepo::Docker => server::docker_missing(deps.install.runner),
        AptRepo::Rig => (deps.installer_lookup)("rig")
            .map(|installer| installer.detect(&deps.install).is_none())
            .unwrap_or(true),
    }
}

/// Register `repo`, returning the source file this call created, if it
/// did not exist before.
fn register(repo: AptRepo, deps: &SetupDeps) -> anyhow::Result<Option<&'static str>> {
    match repo {
        AptRepo::Docker => {
            let created = server::register_docker_repo(deps.server_host, deps.install.runner)?;
            Ok(created.then_some(server::DOCKER_SOURCES))
        }
        AptRepo::Rig => {
            let existed = deps
                .server_host
                .exists(std::path::Path::new(rig::RIG_SOURCES));
            rig::register_rig_repo(&deps.install)?;
            Ok((!existed).then_some(rig::RIG_SOURCES))
        }
    }
}

/// The verified package file for `deb`, or `None` when it is current.
fn prepare(deb: LocalDeb, deps: &SetupDeps) -> anyhow::Result<Option<DownloadedPackage>> {
    match deb {
        LocalDeb::RstudioServer => {
            (deps.installer_lookup)("rstudio-server")?.prepare_package(&deps.install)
        }
        LocalDeb::CodeServer => server::prepare_code_server(deps.server_host, deps.install.runner),
    }
}
