//! Repo audit core: checks produce [`Finding`]s, and this module turns
//! them into reports and an exit code.
//!
//! This module returns data and rendered strings only — it never prints.
//! The command layer (`cli::audit`) does all terminal output through `ui/`.

pub mod all;
mod checks;
pub mod github;
mod report;
pub mod report_github;

pub use report::{Summary, render_json, render_text, summarize};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Everything a check may inspect: the repo root, the resolved config, and
/// (when available) the GitHub side of the repo.
pub struct AuditCtx {
    /// Root of the repository being audited.
    pub repo: PathBuf,
    /// Fully layered configuration (checks read `[project]` metadata).
    pub config: Config,
    /// GitHub context (repo slug + `gh` access), present only when the
    /// repo has a github.com `origin` and `gh` is authenticated. The
    /// GitHub checks no-op without it.
    pub github: Option<github::GithubCtx>,
}

/// How serious a finding is.
///
/// Serialized (stable, consumed by the audit bot): `"error"`, `"warn"`,
/// `"info"`. Deserialization is the bot side (`report_github`) reading
/// that same schema back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Must be fixed; fails the audit (exit 1).
    Error,
    /// Should be fixed; fails the audit only under `--strict`.
    Warn,
    /// Worth knowing; never fails the audit.
    Info,
}

/// One problem (or observation) reported by a check.
///
/// Serialized field names are a STABLE schema consumed by the audit bot:
/// `check_id`, `severity` (see [`Severity`]), `message`, `remediation`.
/// Deserialization is the bot side (`report_github`) reading that same
/// schema back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Id of the check that produced this finding (e.g. `"dirty-files"`).
    pub check_id: String,
    /// How serious it is.
    pub severity: Severity,
    /// What is wrong, in one line.
    pub message: String,
    /// What to do about it, in one line.
    pub remediation: String,
}

/// A single audit check. Checks are pure inspectors: they return findings
/// and never print or mutate the repo.
pub trait Check {
    /// Stable identifier, used as [`Finding::check_id`] and in bot issue
    /// fingerprints.
    fn id(&self) -> &str;

    /// Inspect the repo and report zero or more findings. An empty vec
    /// means the check passed.
    fn run(&self, ctx: &AuditCtx) -> Vec<Finding>;

    /// Whether the check asks git about the repo. When the audited
    /// directory is not a repository, the command layer skips these and
    /// reports one [`not_a_repo_finding`] instead of a git error per check.
    fn needs_repo(&self) -> bool {
        true
    }
}

/// All registered local checks, in the order they run and report. The
/// GitHub-side checks live in [`github::registry`]; the command layer
/// appends them when the repo's GitHub context is available.
pub fn registry() -> Vec<Box<dyn Check>> {
    checks::all()
}

/// The repository root containing `dir` (so an audit started in a subdir
/// inspects the whole repo). `None` when `dir` is not inside a git repo —
/// the command layer then skips the git-backed checks and reports
/// [`not_a_repo_finding`].
pub fn repo_root(dir: &std::path::Path) -> Option<PathBuf> {
    checks::repo_root(dir)
}

/// The one Error finding reported when the audited directory is not a git
/// repository (in place of a near-identical git error from every
/// git-backed check).
pub fn not_a_repo_finding() -> Finding {
    Finding {
        check_id: "git".to_string(),
        severity: Severity::Error,
        message: "not a git repository (the checks that inspect git were skipped)".to_string(),
        remediation: "run `git init` to make this directory a repository, then re-run \
                      `hpds audit`"
            .to_string(),
    }
}

/// Run every check against `ctx`, collecting findings in registry order.
pub fn run_checks(checks: &[Box<dyn Check>], ctx: &AuditCtx) -> Vec<Finding> {
    checks.iter().flat_map(|check| check.run(ctx)).collect()
}

/// Exit-code semantics, as a pure function: 1 when any finding is an
/// [`Severity::Error`], else 0. `strict` promotes [`Severity::Warn`] to
/// error *for this decision only* — findings keep their real severity in
/// all output.
pub fn exit_code(findings: &[Finding], strict: bool) -> u8 {
    let fails = findings.iter().any(|finding| match finding.severity {
        Severity::Error => true,
        Severity::Warn => strict,
        Severity::Info => false,
    });
    if fails { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configurable fake check for exercising the framework.
    struct FakeCheck {
        id: &'static str,
        findings: Vec<Finding>,
    }

    impl Check for FakeCheck {
        fn id(&self) -> &str {
            self.id
        }

        fn run(&self, _ctx: &AuditCtx) -> Vec<Finding> {
            self.findings.clone()
        }
    }

    fn finding(check_id: &str, severity: Severity) -> Finding {
        Finding {
            check_id: check_id.to_string(),
            severity,
            message: format!("{check_id} went wrong"),
            remediation: format!("fix {check_id}"),
        }
    }

    fn ctx() -> AuditCtx {
        AuditCtx {
            repo: PathBuf::from("/tmp/demo"),
            config: Config::default(),
            github: None,
        }
    }

    #[test]
    fn run_checks_collects_findings_in_registry_order() {
        let checks: Vec<Box<dyn Check>> = vec![
            Box::new(FakeCheck {
                id: "first",
                findings: vec![finding("first", Severity::Warn)],
            }),
            Box::new(FakeCheck {
                id: "clean",
                findings: vec![],
            }),
            Box::new(FakeCheck {
                id: "second",
                findings: vec![
                    finding("second", Severity::Error),
                    finding("second", Severity::Info),
                ],
            }),
        ];
        let findings = run_checks(&checks, &ctx());
        let ids: Vec<&str> = findings.iter().map(|f| f.check_id.as_str()).collect();
        assert_eq!(ids, ["first", "second", "second"]);
    }

    #[test]
    fn registry_checks_all_pass_on_a_compliant_repo() {
        let (_tmp, repo) = checks::testutil::compliant_repo();
        let findings = run_checks(
            &registry(),
            &AuditCtx {
                repo,
                config: Config::default(),
                github: None,
            },
        );
        assert_eq!(findings, Vec::new());
    }

    #[test]
    fn only_the_git_free_checks_run_outside_a_repo() {
        let ids: Vec<String> = registry()
            .iter()
            .filter(|check| !check.needs_repo())
            .map(|check| check.id().to_string())
            .collect();
        assert_eq!(ids, ["readme", "lifecycle-metadata"]);
    }

    #[test]
    fn not_a_repo_finding_is_one_error_pointing_at_git_init() {
        let finding = not_a_repo_finding();
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding.message.contains("not a git repository"),
            "{finding:?}"
        );
        assert!(finding.remediation.contains("git init"), "{finding:?}");
    }

    #[test]
    fn registry_check_ids_are_unique() {
        let mut ids: Vec<String> = registry().iter().map(|c| c.id().to_string()).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate check id in registry");
    }

    #[test]
    fn no_findings_exits_zero() {
        assert_eq!(exit_code(&[], false), 0);
        assert_eq!(exit_code(&[], true), 0);
    }

    #[test]
    fn any_error_finding_exits_one() {
        let findings = vec![
            finding("a", Severity::Info),
            finding("b", Severity::Error),
            finding("c", Severity::Warn),
        ];
        assert_eq!(exit_code(&findings, false), 1);
        assert_eq!(exit_code(&findings, true), 1);
    }

    #[test]
    fn warnings_alone_exit_zero_without_strict() {
        let findings = vec![finding("a", Severity::Warn), finding("b", Severity::Info)];
        assert_eq!(exit_code(&findings, false), 0);
    }

    #[test]
    fn strict_promotes_warnings_to_a_failing_exit() {
        let findings = vec![finding("a", Severity::Warn)];
        assert_eq!(exit_code(&findings, true), 1);
    }

    #[test]
    fn strict_promotion_does_not_change_the_findings_themselves() {
        let findings = vec![finding("a", Severity::Warn)];
        exit_code(&findings, true);
        assert_eq!(findings[0].severity, Severity::Warn);
    }

    #[test]
    fn info_findings_never_fail_even_under_strict() {
        let findings = vec![finding("a", Severity::Info)];
        assert_eq!(exit_code(&findings, false), 0);
        assert_eq!(exit_code(&findings, true), 0);
    }
}
