//! `hpds setup`: fresh machine bundle.
//!
//! Thin wiring over the engine in `crate::setup`: parse the profile,
//! short-circuit `--plan`, enforce the server profile's Linux-only rule,
//! and hand the profile flow real dependencies (system runner, release
//! fetcher, the `hpds git setup` flow).

use std::io::IsTerminal;
use std::path::Path;

use anyhow::anyhow;
use clap::{Args, ValueEnum};

use crate::install::{CacheFetcher, InstallCtx, SystemRunner};
use crate::setup::{self, Profile, SetupDeps};
use crate::tools::{Os, Platform};
use crate::ui::{self, HintExt};

use super::GlobalArgs;

/// Where the server profile writes its end-of-run summary log. The server
/// profile only ever runs on Linux, so the Unix path is deliberate.
const SERVER_LOG_PATH: &str = "/tmp/hpds-setup.log";

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Setup profile: dev (this machine's toolchain) or server (full lab
    /// server provisioning; Linux only)
    #[arg(long, value_enum, default_value_t = ProfileArg::Dev)]
    pub profile: ProfileArg,

    /// Print the numbered plan for the profile and exit without running
    /// or prompting
    #[arg(long)]
    pub plan: bool,

    /// Skip all prompts: run every step and pre-approve sudo steps
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// `--profile` values. Mirrors [`Profile`] so the engine stays clap-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProfileArg {
    Dev,
    Server,
}

impl From<ProfileArg> for Profile {
    fn from(arg: ProfileArg) -> Self {
        match arg {
            ProfileArg::Dev => Profile::Dev,
            ProfileArg::Server => Profile::Server,
        }
    }
}

pub fn run(args: SetupArgs, global: &GlobalArgs) -> anyhow::Result<()> {
    let profile = Profile::from(args.profile);

    // `--plan` is inspection only: it works on every OS (even for the
    // server profile) and never prompts or executes.
    if args.plan {
        ui::println(setup::plan(profile).trim_end());
        return Ok(());
    }

    let platform = Platform::current()?;
    if profile == Profile::Server && platform.os != Os::Linux {
        return Err(anyhow!(
            "`hpds setup --profile server` provisions a Linux server and cannot run on {}",
            os_name(platform.os)
        ))
        .hint(
            "run `hpds setup --profile dev` to set up this machine, or run this command \
             on the Linux server itself (use --plan here to preview the server steps)",
        );
    }

    let runner = SystemRunner;
    let fetcher = CacheFetcher::new(global.verbose);
    let git_setup = || super::git::run_setup_with_defaults(args.yes);
    let deps = SetupDeps {
        install: InstallCtx {
            os: platform.os,
            yes: args.yes,
            verbose: global.verbose,
            pin: None,
            // The setup flow gates the whole batch itself (checklist or
            // `--yes`) before anything executes, so the per-install
            // confirmation would only re-ask what the user already
            // approved. Sudo steps still confirm individually.
            plan_approved: true,
            sudo_approved: std::cell::Cell::new(false),
            runner: &runner,
            fetcher: &fetcher,
        },
        git_setup: &git_setup,
    };
    let interactive = std::io::stdin().is_terminal();

    match profile {
        Profile::Dev => setup::run_dev(&deps, args.yes, interactive),
        Profile::Server => {
            setup::run_server(&deps, args.yes, interactive, Path::new(SERVER_LOG_PATH))
        }
    }
}

/// Human name for an OS in error messages.
fn os_name(os: Os) -> &'static str {
    match os {
        Os::Mac => "macOS",
        Os::Linux => "Linux",
        Os::Windows => "Windows",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_arg_maps_onto_the_engine_profile() {
        assert_eq!(Profile::from(ProfileArg::Dev), Profile::Dev);
        assert_eq!(Profile::from(ProfileArg::Server), Profile::Server);
    }

    #[test]
    fn os_names_read_like_the_marketing_names() {
        assert_eq!(os_name(Os::Mac), "macOS");
        assert_eq!(os_name(Os::Linux), "Linux");
        assert_eq!(os_name(Os::Windows), "Windows");
    }
}
