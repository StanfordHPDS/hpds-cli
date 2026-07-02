//! `hpds audit` — repo and org audits, plus the bot reporter (spec §8).

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// With no subcommand, audit the current repo.
    #[command(subcommand)]
    pub command: Option<AuditCommand>,
}

#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Audit every repo in the GitHub org
    All,
    /// Post audit results to GitHub (sticky PR comment, dedup'd issues)
    ReportGithub,
}

pub fn run(args: AuditArgs) -> anyhow::Result<()> {
    match args.command {
        // Stubs until M5 (audit, audit all) and M6 (report-github).
        None => Err(super::not_yet_implemented("audit")),
        Some(AuditCommand::All) => Err(super::not_yet_implemented("audit all")),
        Some(AuditCommand::ReportGithub) => Err(super::not_yet_implemented("audit report-github")),
    }
}
