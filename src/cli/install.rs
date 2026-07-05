//! `hpds install` — install external software per platform.
//!
//! Thin wiring: look the tool up in the installer registry, build the
//! [`InstallCtx`] from the flags and this machine's platform, and hand off
//! to the framework's shared flow.

use clap::Args;

use crate::install::{self, CacheFetcher, InstallCtx, SystemRunner};
use crate::tools::Platform;

use super::GlobalArgs;

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Tool to install (r, quarto, uv, gh, rig, tinytex, duckdb, togi)
    #[arg(value_name = "TOOL")]
    pub tool: String,

    /// Install this exact version (only for tools that support pinning)
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,

    /// Skip the confirmation prompt; the plan of what will run is still
    /// printed first
    #[arg(short = 'y', long)]
    pub yes: bool,
}

pub fn run(args: InstallArgs, global: &GlobalArgs) -> anyhow::Result<()> {
    let installer = install::registry::find(&args.tool)?;
    if args.version.is_some() && !installer.supports_pin() {
        return Err(super::usage_error(
            format!("`{}` does not support `--version` pinning", args.tool),
            format!(
                "re-run `hpds install {}` without --version to get the supported release",
                args.tool
            ),
        ));
    }
    let platform = Platform::current()?;
    let runner = SystemRunner;
    let fetcher = CacheFetcher::new(global.verbose);
    let ctx = InstallCtx {
        os: platform.os,
        yes: args.yes,
        verbose: global.verbose,
        pin: args.version,
        plan_approved: false,
        sudo_approved: std::cell::Cell::new(false),
        runner: &runner,
        fetcher: &fetcher,
    };
    install::run_installer(installer, &ctx)
}
