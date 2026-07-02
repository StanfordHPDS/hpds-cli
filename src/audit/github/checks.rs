//! The six GitHub-side checks. Each one is a pure inspector over
//! [`GithubCtx`]: it fetches what it needs through the [`GithubApi`] seam,
//! parses via [`model`], and returns findings. Failures to reach or
//! understand GitHub become Warn findings on the same check — never a
//! panic, never an aborted audit.

use std::collections::BTreeSet;
use std::time::SystemTime;

use super::model::{
    self, Account, BranchDetail, BranchSummary, Comparison, ModelError, Release, RepoInfo,
};
use super::{GhApiError, GithubCtx};
use crate::audit::{AuditCtx, Check, Finding, Severity};

/// The GitHub checks, in report order.
pub(super) fn registry() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(Watchers),
        Box::new(Contributors),
        Box::new(DefaultBranchStaleness),
        Box::new(StaleRemoteBranches),
        Box::new(Releases),
        Box::new(LifecycleConsistency),
    ]
}

/// Anything that can go wrong while a check talks to GitHub.
#[derive(Debug, thiserror::Error)]
enum CheckError {
    #[error(transparent)]
    Api(#[from] GhApiError),
    #[error(transparent)]
    Model(#[from] ModelError),
}

/// Run a check body against the GitHub context, if there is one. Checks
/// no-op without it (the command layer only schedules them when GitHub is
/// reachable), and any error becomes a Warn finding on the check itself.
fn with_github(
    ctx: &AuditCtx,
    check_id: &str,
    body: impl FnOnce(&GithubCtx) -> Result<Vec<Finding>, CheckError>,
) -> Vec<Finding> {
    let Some(github) = ctx.github.as_ref() else {
        return Vec::new();
    };
    match body(github) {
        Ok(findings) => findings,
        Err(err) => vec![Finding {
            check_id: check_id.to_string(),
            severity: Severity::Warn,
            message: format!("could not complete this GitHub check: {err}"),
            remediation: "check `gh auth status`, your network, and your access to the \
                          repo, then re-run `hpds audit`"
                .to_string(),
        }],
    }
}

fn finding(check_id: &str, severity: Severity, message: String, remediation: String) -> Finding {
    Finding {
        check_id: check_id.to_string(),
        severity,
        message,
        remediation,
    }
}

/// `repos/{owner}/{repo}` metadata (memoized across checks by [`GithubCtx`]).
fn repo_info(github: &GithubCtx) -> Result<RepoInfo, CheckError> {
    Ok(model::parse_one(
        &github.api_one(&format!("repos/{}", github.slug))?,
    )?)
}

/// A paginated list of accounts (subscribers, contributors, org members).
fn accounts(github: &GithubCtx, endpoint: &str) -> Result<Vec<Account>, CheckError> {
    Ok(model::parse_pages(&github.api_pages(endpoint)?)?)
}

/// GitHub logins are case-insensitive; compare them folded.
fn fold(login: &str) -> String {
    login.to_lowercase()
}

/// `watchers`: the primary author plus the configured lab leads must be
/// watching (subscribed to) the repo.
struct Watchers;

impl Check for Watchers {
    fn id(&self) -> &str {
        "watchers"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        with_github(ctx, self.id(), |github| {
            let subscribers = accounts(github, &format!("repos/{}/subscribers", github.slug))?;
            let watching: BTreeSet<String> = subscribers.iter().map(|a| fold(&a.login)).collect();

            let mut required: Vec<&str> = ctx
                .config
                .audit
                .required_watchers
                .iter()
                .map(String::as_str)
                .collect();
            let author = ctx.config.project.primary_author.as_str();
            if !author.is_empty() && !required.iter().any(|r| fold(r) == fold(author)) {
                required.push(author);
            }

            let missing: Vec<&str> = required
                .into_iter()
                .filter(|login| !watching.contains(&fold(login)))
                .collect();
            if missing.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![finding(
                self.id(),
                Severity::Warn,
                format!("not watching the repo on GitHub: {}", missing.join(", ")),
                format!(
                    "have them open https://github.com/{} and set Watch → All activity",
                    github.slug
                ),
            )])
        })
    }
}

/// `contributors`: the primary author must appear in the contributor list;
/// additionally, flag repos whose contributors have all left the org.
struct Contributors;

impl Check for Contributors {
    fn id(&self) -> &str {
        "contributors"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        with_github(ctx, self.id(), |github| {
            let contributors = accounts(github, &format!("repos/{}/contributors", github.slug))?;
            let mut findings = Vec::new();

            let author = ctx.config.project.primary_author.as_str();
            if !author.is_empty() && !contributors.iter().any(|c| fold(&c.login) == fold(author)) {
                findings.push(finding(
                    self.id(),
                    Severity::Warn,
                    format!("primary author `{author}` is not among the repo's contributors"),
                    "check `primary-author` in hpds.toml [project], or have them \
                     commit to the repo"
                        .to_string(),
                ));
            }

            if let Some(flag) = self.all_left_org(github, &contributors)? {
                findings.push(flag);
            }
            Ok(findings)
        })
    }
}

impl Contributors {
    /// Best-effort "everyone left the org" flag. Limitations, by design:
    /// listing org members requires the `gh` caller to be an org member
    /// with adequate token scopes, so a 403/404 there silently skips the
    /// flag instead of failing the check; membership can also be private,
    /// making members invisible and this flag a false positive — hence
    /// Info severity. Bot accounts are excluded from "contributors".
    /// A members payload that arrives but does not parse is NOT skipped:
    /// malformed gh JSON always becomes an error finding.
    fn all_left_org(
        &self,
        github: &GithubCtx,
        contributors: &[Account],
    ) -> Result<Option<Finding>, CheckError> {
        let info = repo_info(github)?;
        if info.owner.kind != "Organization" {
            return Ok(None);
        }
        let Ok(body) = github.api_pages(&format!("orgs/{}/members", info.owner.login)) else {
            return Ok(None);
        };
        let members: Vec<Account> = model::parse_pages(&body)?;
        let member_logins: BTreeSet<String> = members.iter().map(|m| fold(&m.login)).collect();
        let humans: Vec<&Account> = contributors
            .iter()
            .filter(|c| !c.login.ends_with("[bot]"))
            .collect();
        if humans.is_empty()
            || humans
                .iter()
                .any(|c| member_logins.contains(&fold(&c.login)))
        {
            return Ok(None);
        }
        Ok(Some(finding(
            self.id(),
            Severity::Info,
            format!(
                "no contributor is currently a member of the {} organization \
                 (best-effort check)",
                info.owner.login
            ),
            "confirm someone still stewards this repo; update `primary-author` \
             in hpds.toml or transfer ownership"
                .to_string(),
        )))
    }
}

/// `default-branch-staleness`: is the remote default branch ahead of or
/// behind the local checkout? Compared via `gh api compare` between the
/// local branch tip and the remote branch, so no fetch is needed; a local
/// tip GitHub has never seen (404) means unpushed or rewritten history.
struct DefaultBranchStaleness;

impl Check for DefaultBranchStaleness {
    fn id(&self) -> &str {
        "default-branch-staleness"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        with_github(ctx, self.id(), |github| {
            let info = repo_info(github)?;
            let branch = &info.default_branch;
            let Some(tip) = github.local_branch_commit(branch) else {
                return Ok(vec![finding(
                    self.id(),
                    Severity::Info,
                    format!("no local commit found to compare with the remote `{branch}`"),
                    format!("check out or fetch `{branch}`, then re-run `hpds audit`"),
                )]);
            };
            // When no local branch matched the remote default (e.g. a
            // single-branch clone of a feature branch), HEAD stood in for
            // it: say so, and never advise pushing/pulling `branch` from a
            // checkout that does not have it.
            let switch_remedy = format!(
                "create a local `{branch}` tracking the remote: \
                 `git switch {branch}`, then re-run `hpds audit`"
            );

            let local_sha = &tip.sha;
            let endpoint = format!(
                "repos/{}/compare/{local_sha}...{}",
                github.slug,
                encode_ref(branch)
            );
            let comparison: Comparison = match github.api_one(&endpoint) {
                Ok(body) => model::parse_one(&body)?,
                Err(GhApiError::NotFound) => {
                    // GitHub does not know the local tip at all.
                    let (message, remediation) = if tip.from_head {
                        (
                            format!(
                                "the local HEAD commit is not on GitHub (compared HEAD \
                                 because there is no local `{branch}` branch)"
                            ),
                            switch_remedy,
                        )
                    } else {
                        (
                            format!(
                                "the local `{branch}` commit is not on GitHub \
                                 (unpushed or rewritten history)"
                            ),
                            format!("push it: `git push origin {branch}`"),
                        )
                    };
                    return Ok(vec![finding(
                        self.id(),
                        Severity::Warn,
                        message,
                        remediation,
                    )]);
                }
                Err(err) => return Err(err.into()),
            };

            // Base is the local tip and head is the remote branch, so
            // `ahead_by` counts commits only the remote has, and
            // `behind_by` counts commits only the local side has.
            let mut findings = Vec::new();
            if comparison.ahead_by > 0 {
                let (message, remediation) = if tip.from_head {
                    (
                        format!(
                            "the remote `{branch}` is {} ahead of the local checkout \
                             (compared against HEAD; no local `{branch}` branch)",
                            commits(comparison.ahead_by)
                        ),
                        switch_remedy.clone(),
                    )
                } else {
                    (
                        format!(
                            "the remote `{branch}` is {} ahead of the local checkout",
                            commits(comparison.ahead_by)
                        ),
                        format!("pull the latest changes: `git pull origin {branch}`"),
                    )
                };
                findings.push(finding(self.id(), Severity::Warn, message, remediation));
            }
            if comparison.behind_by > 0 {
                let (message, remediation) = if tip.from_head {
                    (
                        format!(
                            "the local checkout (HEAD; no local `{branch}` branch) is {} \
                             ahead of the remote `{branch}`",
                            commits(comparison.behind_by)
                        ),
                        switch_remedy,
                    )
                } else {
                    (
                        format!(
                            "the local `{branch}` is {} ahead of the remote",
                            commits(comparison.behind_by)
                        ),
                        format!("push them: `git push origin {branch}`"),
                    )
                };
                findings.push(finding(self.id(), Severity::Warn, message, remediation));
            }
            Ok(findings)
        })
    }
}

/// `1 commit` / `2 commits`.
fn commits(n: u64) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} commit{s}")
}

/// Percent-encode a git ref for use as a path segment in a `gh api`
/// endpoint. Branch names may legally contain `#`, `%`, `?`, spaces, and
/// more, which would corrupt the request path if interpolated raw. `/`
/// stays literal: GitHub accepts it inside ref segments (`feature/foo`).
fn encode_ref(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `stale-remote-branches`: unmerged remote branches whose latest commit is
/// older than `[audit] stale-days`.
struct StaleRemoteBranches;

impl Check for StaleRemoteBranches {
    fn id(&self) -> &str {
        "stale-remote-branches"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        with_github(ctx, self.id(), |github| {
            let info = repo_info(github)?;
            let branches: Vec<BranchSummary> =
                model::parse_pages(&github.api_pages(&format!("repos/{}/branches", github.slug))?)?;
            let stale_days = i64::from(ctx.config.audit.stale_days);
            let now = SystemTime::now();

            let mut stale = Vec::new();
            for branch in branches.iter().filter(|b| b.name != info.default_branch) {
                let detail: BranchDetail = model::parse_one(&github.api_one(&format!(
                    "repos/{}/branches/{}",
                    github.slug,
                    encode_ref(&branch.name)
                ))?)?;
                let age = model::days_since(&detail.commit.commit.committer.date, now)?;
                if age <= stale_days {
                    continue;
                }
                // Only branches with commits the default branch lacks are
                // unmerged; merged leftovers are the local checks' concern.
                let comparison: Comparison = model::parse_one(&github.api_one(&format!(
                    "repos/{}/compare/{}...{}",
                    github.slug,
                    encode_ref(&info.default_branch),
                    encode_ref(&branch.name)
                ))?)?;
                if comparison.ahead_by > 0 {
                    stale.push(format!("{} ({age} days)", branch.name));
                }
            }

            if stale.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![finding(
                self.id(),
                Severity::Warn,
                format!(
                    "unmerged remote branches with no commits in over {stale_days} days: {}",
                    stale.join(", ")
                ),
                "merge what still matters, then delete the rest: \
                 `git push origin --delete <branch>`"
                    .to_string(),
            )])
        })
    }
}

/// `releases`: projects at the submitted/published milestones must have at
/// least one GitHub release.
struct Releases;

impl Check for Releases {
    fn id(&self) -> &str {
        "releases"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let status = ctx.config.project.status.clone();
        if status != "submitted" && status != "published" {
            return Vec::new();
        }
        with_github(ctx, self.id(), |github| {
            let releases: Vec<Release> =
                model::parse_pages(&github.api_pages(&format!("repos/{}/releases", github.slug))?)?;
            if !releases.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![finding(
                self.id(),
                Severity::Error,
                format!("project status is `{status}` but the repo has no GitHub release"),
                "create one for the milestone: `gh release create v1.0.0 --generate-notes`"
                    .to_string(),
            )])
        })
    }
}

/// `lifecycle-consistency`: the hpds.toml `status` and the repo's archived
/// flag must agree.
struct LifecycleConsistency;

impl Check for LifecycleConsistency {
    fn id(&self) -> &str {
        "lifecycle-consistency"
    }

    fn run(&self, ctx: &AuditCtx) -> Vec<Finding> {
        with_github(ctx, self.id(), |github| {
            let info = repo_info(github)?;
            let status = ctx.config.project.status.as_str();
            let mut findings = Vec::new();
            if status == "retired" && !info.archived {
                findings.push(finding(
                    self.id(),
                    Severity::Warn,
                    "project status is `retired` but the GitHub repo is not archived".to_string(),
                    format!("archive it: `gh repo archive {}`", github.slug),
                ));
            }
            if info.archived && status == "active" {
                findings.push(finding(
                    self.id(),
                    Severity::Warn,
                    "the GitHub repo is archived but project status is `active`".to_string(),
                    "update `status` in hpds.toml (e.g. to `retired`), or unarchive \
                     the repo if work continues"
                        .to_string(),
                ));
            }
            Ok(findings)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::tests::fixture;
    use super::super::{GithubApi, LocalTip, RepoSlug};
    use super::*;
    use crate::config::Config;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// Canned response for one endpoint.
    enum Canned {
        Body(String),
        NotFound,
        Fail,
    }

    /// A [`GithubApi`] that serves recorded fixtures per endpoint and fails
    /// loudly on anything unexpected. Every call is logged with its
    /// pagination flag so tests can assert how an endpoint was fetched.
    struct FakeGh {
        responses: BTreeMap<String, Canned>,
        local_sha: Option<String>,
        from_head: bool,
        calls: Rc<RefCell<Vec<(String, bool)>>>,
    }

    impl FakeGh {
        fn new() -> Self {
            FakeGh {
                responses: BTreeMap::new(),
                local_sha: Some("d6f2e5a9c0b1d2e3f4a5b6c7d8e9f0a1b2c3d4e5".to_string()),
                from_head: false,
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        /// A shared handle onto the call log, usable after the fake is
        /// boxed into a [`GithubCtx`].
        fn call_log(&self) -> Rc<RefCell<Vec<(String, bool)>>> {
            Rc::clone(&self.calls)
        }

        fn serve_fixture(mut self, endpoint: &str, name: &str) -> Self {
            self.responses
                .insert(endpoint.to_string(), Canned::Body(fixture(name)));
            self
        }

        fn serve_body(mut self, endpoint: &str, body: &str) -> Self {
            self.responses
                .insert(endpoint.to_string(), Canned::Body(body.to_string()));
            self
        }

        fn serve_not_found(mut self, endpoint: &str) -> Self {
            self.responses
                .insert(endpoint.to_string(), Canned::NotFound);
            self
        }

        fn serve_failure(mut self, endpoint: &str) -> Self {
            self.responses.insert(endpoint.to_string(), Canned::Fail);
            self
        }

        fn without_local_sha(mut self) -> Self {
            self.local_sha = None;
            self
        }

        /// The sha came from `HEAD` because no local branch matched.
        fn with_head_fallback(mut self) -> Self {
            self.from_head = true;
            self
        }
    }

    impl GithubApi for FakeGh {
        fn api(&self, endpoint: &str, paginate: bool) -> Result<String, GhApiError> {
            self.calls
                .borrow_mut()
                .push((endpoint.to_string(), paginate));
            match self.responses.get(endpoint) {
                Some(Canned::Body(body)) => Ok(body.clone()),
                Some(Canned::NotFound) => Err(GhApiError::NotFound),
                Some(Canned::Fail) => Err(GhApiError::Failed {
                    endpoint: endpoint.to_string(),
                    detail: "canned failure".to_string(),
                }),
                None => Err(GhApiError::Failed {
                    endpoint: endpoint.to_string(),
                    detail: "unexpected endpoint in test".to_string(),
                }),
            }
        }

        fn local_branch_commit(&self, _branch: &str) -> Option<LocalTip> {
            self.local_sha.clone().map(|sha| LocalTip {
                sha,
                from_head: self.from_head,
            })
        }
    }

    fn ctx(fake: FakeGh, config: Config) -> AuditCtx {
        AuditCtx {
            repo: PathBuf::from("/tmp/demo"),
            config,
            github: Some(GithubCtx::new(
                RepoSlug {
                    owner: "acme".to_string(),
                    repo: "demo".to_string(),
                },
                Box::new(fake),
            )),
        }
    }

    fn config_with_author(author: &str) -> Config {
        let mut config = Config::default();
        config.project.primary_author = author.to_string();
        config
    }

    fn run_one(check: &dyn Check, ctx: &AuditCtx) -> Vec<Finding> {
        check.run(ctx)
    }

    #[test]
    fn registry_has_the_six_documented_checks_in_order() {
        let ids: Vec<String> = registry().iter().map(|c| c.id().to_string()).collect();
        assert_eq!(
            ids,
            [
                "watchers",
                "contributors",
                "default-branch-staleness",
                "stale-remote-branches",
                "releases",
                "lifecycle-consistency",
            ]
        );
    }

    #[test]
    fn all_checks_no_op_without_a_github_context() {
        let ctx = AuditCtx {
            repo: PathBuf::from("/tmp/demo"),
            config: Config::default(),
            github: None,
        };
        for check in registry() {
            assert_eq!(check.run(&ctx), Vec::new(), "{}", check.id());
        }
    }

    // ---- watchers ----

    #[test]
    fn watchers_pass_when_leads_and_author_subscribe_case_insensitively() {
        // The fixture has `malcolmbarrett` and `SherriRose`; the default
        // required list uses lowercase logins.
        let fake = FakeGh::new().serve_fixture("repos/acme/demo/subscribers", "subscribers.json");
        let ctx = ctx(fake, config_with_author("MalcolmBarrett"));
        assert_eq!(run_one(&Watchers, &ctx), Vec::new());
    }

    #[test]
    fn watchers_flags_a_primary_author_who_is_not_watching() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo/subscribers", "subscribers.json");
        let ctx = ctx(fake, config_with_author("researcher1"));
        let findings = run_one(&Watchers, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_id, "watchers");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("researcher1"));
        assert!(!findings[0].message.contains("malcolmbarrett"));
        assert!(findings[0].remediation.contains("github.com/acme/demo"));
    }

    #[test]
    fn watchers_required_list_comes_from_config() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo/subscribers", "subscribers.json");
        let mut config = Config::default();
        config.audit.required_watchers = vec!["lead1".to_string()];
        let ctx = ctx(fake, config);
        let findings = run_one(&Watchers, &ctx);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("lead1"));
        // The built-in leads were overridden, so they are not required.
        assert!(!findings[0].message.contains("sherrirose"));
    }

    #[test]
    fn watchers_api_failure_is_a_warn_finding_not_a_crash() {
        let fake = FakeGh::new().serve_failure("repos/acme/demo/subscribers");
        let ctx = ctx(fake, Config::default());
        let findings = run_one(&Watchers, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("could not complete"));
        assert!(findings[0].remediation.contains("gh auth status"));
    }

    // ---- contributors ----

    /// Contributor endpoints for a repo whose org members are known.
    fn contributors_fake() -> FakeGh {
        FakeGh::new()
            .serve_fixture("repos/acme/demo/contributors", "contributors.json")
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_fixture("orgs/acme/members", "org-members.json")
    }

    #[test]
    fn contributors_pass_when_the_author_contributed() {
        let ctx = ctx(contributors_fake(), config_with_author("researcher1"));
        assert_eq!(run_one(&Contributors, &ctx), Vec::new());
    }

    #[test]
    fn contributors_flags_an_author_who_never_contributed() {
        let ctx = ctx(contributors_fake(), config_with_author("ghost"));
        let findings = run_one(&Contributors, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_id, "contributors");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("ghost"));
    }

    #[test]
    fn contributors_without_a_configured_author_skips_the_author_rule() {
        let ctx = ctx(contributors_fake(), Config::default());
        assert_eq!(run_one(&Contributors, &ctx), Vec::new());
    }

    #[test]
    fn contributors_flags_when_every_human_contributor_left_the_org() {
        // Org members share no login with the contributors fixture; the
        // bot contributor must not count as a remaining member.
        let members = r#"[{"login": "someoneelse", "id": 1, "type": "User"}]"#;
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo/contributors", "contributors.json")
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_body("orgs/acme/members", members);
        let ctx = ctx(fake, config_with_author("researcher1"));
        let findings = run_one(&Contributors, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].message.contains("acme"));
        assert!(findings[0].message.contains("best-effort"));
    }

    #[test]
    fn contributors_malformed_org_members_json_is_a_warn_finding() {
        // A members payload that arrives but does not parse is not a
        // permission problem: malformed gh JSON always surfaces as an
        // error finding rather than being silently swallowed.
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo/contributors", "contributors.json")
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_body("orgs/acme/members", "<!doctype html>");
        let ctx = ctx(fake, config_with_author("researcher1"));
        let findings = run_one(&Contributors, &ctx);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(
            findings[0].message.contains("could not complete"),
            "{findings:?}"
        );
    }

    #[test]
    fn contributors_left_org_flag_skips_silently_when_members_are_hidden() {
        // Listing members needs org membership; a 403-style failure must
        // not produce any finding (best-effort check).
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo/contributors", "contributors.json")
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_failure("orgs/acme/members");
        let ctx = ctx(fake, config_with_author("researcher1"));
        assert_eq!(run_one(&Contributors, &ctx), Vec::new());
    }

    // ---- default-branch-staleness ----

    const LOCAL_SHA: &str = "d6f2e5a9c0b1d2e3f4a5b6c7d8e9f0a1b2c3d4e5";

    fn staleness_fake(compare_fixture: &str) -> FakeGh {
        FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_fixture(
                &format!("repos/acme/demo/compare/{LOCAL_SHA}...main"),
                compare_fixture,
            )
    }

    #[test]
    fn staleness_identical_produces_no_findings() {
        let ctx = ctx(staleness_fake("compare-identical.json"), Config::default());
        assert_eq!(run_one(&DefaultBranchStaleness, &ctx), Vec::new());
    }

    #[test]
    fn staleness_remote_ahead_says_pull() {
        let ctx = ctx(staleness_fake("compare-ahead.json"), Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(
            findings[0].message.contains("3 commits ahead"),
            "{findings:?}"
        );
        assert!(findings[0].message.contains("remote"), "{findings:?}");
        assert!(findings[0].remediation.contains("git pull"));
    }

    #[test]
    fn staleness_local_ahead_says_push() {
        let ctx = ctx(staleness_fake("compare-behind.json"), Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].message.contains("2 commits ahead"),
            "{findings:?}"
        );
        assert!(findings[0].message.contains("local"), "{findings:?}");
        assert!(findings[0].remediation.contains("git push"));
    }

    #[test]
    fn staleness_survives_a_multi_page_compare_response() {
        // A comparison spanning >100 commits arrives as several
        // concatenated JSON objects when paginated; the check must report
        // the (page-repeated) totals, not degrade to could-not-complete —
        // large divergence is exactly the case this check exists for.
        let ctx = ctx(
            staleness_fake("compare-ahead-paginated.json"),
            Config::default(),
        );
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(
            findings[0].message.contains("150 commits ahead"),
            "{findings:?}"
        );
    }

    #[test]
    fn staleness_diverged_reports_both_directions() {
        let ctx = ctx(staleness_fake("compare-diverged.json"), Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn staleness_unknown_local_commit_means_unpushed_history() {
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_not_found(&format!("repos/acme/demo/compare/{LOCAL_SHA}...main"));
        let ctx = ctx(fake, Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(
            findings[0].message.contains("not on GitHub"),
            "{findings:?}"
        );
        assert!(findings[0].remediation.contains("git push"));
    }

    #[test]
    fn staleness_without_any_local_commit_is_an_info_notice() {
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .without_local_sha();
        let ctx = ctx(fake, Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Info);
    }

    #[test]
    fn staleness_head_fallback_local_ahead_does_not_say_push() {
        // No local `main` exists, so HEAD (e.g. a feature-branch checkout)
        // was compared instead; `git push origin main` would be wrong
        // advice for commits that live on a feature branch.
        let ctx = ctx(
            staleness_fake("compare-behind.json").with_head_fallback(),
            Config::default(),
        );
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("HEAD"), "{findings:?}");
        assert!(
            findings[0].message.contains("no local `main` branch"),
            "{findings:?}"
        );
        assert!(
            !findings[0].remediation.contains("git push origin main"),
            "{findings:?}"
        );
        assert!(
            findings[0].remediation.contains("git switch main"),
            "{findings:?}"
        );
    }

    #[test]
    fn staleness_head_fallback_remote_ahead_does_not_say_pull() {
        let ctx = ctx(
            staleness_fake("compare-ahead.json").with_head_fallback(),
            Config::default(),
        );
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("HEAD"), "{findings:?}");
        assert!(
            !findings[0].remediation.contains("git pull"),
            "{findings:?}"
        );
        assert!(
            findings[0].remediation.contains("git switch main"),
            "{findings:?}"
        );
    }

    #[test]
    fn staleness_head_fallback_unknown_commit_does_not_say_push() {
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_not_found(&format!("repos/acme/demo/compare/{LOCAL_SHA}...main"))
            .with_head_fallback();
        let ctx = ctx(fake, Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("HEAD"), "{findings:?}");
        assert!(
            !findings[0].remediation.contains("git push"),
            "{findings:?}"
        );
    }

    #[test]
    fn staleness_never_panics_on_malformed_repo_json() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo", "malformed.json");
        let ctx = ctx(fake, Config::default());
        let findings = run_one(&DefaultBranchStaleness, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_id, "default-branch-staleness");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("could not complete"));
    }

    // ---- stale-remote-branches ----

    fn branches_fake() -> FakeGh {
        // branch-old.json's tip is dated 2023, stale forever after early
        // 2023; branch-fresh.json's tip is dated 2099, fresh until then.
        FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_fixture("repos/acme/demo/branches", "branches.json")
            .serve_fixture("repos/acme/demo/branches/old-analysis", "branch-old.json")
            .serve_fixture("repos/acme/demo/branches/fresh-idea", "branch-fresh.json")
            .serve_fixture(
                "repos/acme/demo/compare/main...old-analysis",
                "compare-ahead.json",
            )
    }

    #[test]
    fn stale_remote_branches_flags_old_unmerged_branches_only() {
        let ctx = ctx(branches_fake(), Config::default());
        let findings = run_one(&StaleRemoteBranches, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_id, "stale-remote-branches");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("old-analysis"), "{findings:?}");
        assert!(!findings[0].message.contains("fresh-idea"), "{findings:?}");
        assert!(!findings[0].message.contains("main"), "{findings:?}");
        assert!(findings[0].message.contains("90 days"), "{findings:?}");
    }

    #[test]
    fn stale_remote_branches_ignores_merged_old_branches() {
        // Same old branch, but the default branch already contains it
        // (ahead_by 0): merged leftovers are not flagged here.
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_fixture("repos/acme/demo/branches", "branches.json")
            .serve_fixture("repos/acme/demo/branches/old-analysis", "branch-old.json")
            .serve_fixture("repos/acme/demo/branches/fresh-idea", "branch-fresh.json")
            .serve_fixture(
                "repos/acme/demo/compare/main...old-analysis",
                "compare-identical.json",
            );
        let ctx = ctx(fake, Config::default());
        assert_eq!(run_one(&StaleRemoteBranches, &ctx), Vec::new());
    }

    #[test]
    fn encode_ref_escapes_url_hostile_bytes_but_keeps_slashes() {
        assert_eq!(encode_ref("main"), "main");
        assert_eq!(encode_ref("feature/foo"), "feature/foo");
        assert_eq!(encode_ref("v1.2_x~y-z"), "v1.2_x~y-z");
        assert_eq!(encode_ref("hot#fix"), "hot%23fix");
        assert_eq!(encode_ref("50%done"), "50%25done");
        assert_eq!(encode_ref("what?now"), "what%3Fnow");
        assert_eq!(encode_ref("with space"), "with%20space");
    }

    #[test]
    fn stale_remote_branches_encodes_hostile_branch_names_in_endpoints() {
        // A git-legal `#` in the branch name must be percent-encoded in the
        // endpoint path (the fake serves only the encoded endpoints; a raw
        // interpolation would hit "unexpected endpoint" and degrade the
        // check to a could-not-complete warning).
        let branches = r#"[{"name": "main"}, {"name": "hot#fix"}]"#;
        let fake = FakeGh::new()
            .serve_fixture("repos/acme/demo", "repo.json")
            .serve_body("repos/acme/demo/branches", branches)
            .serve_fixture("repos/acme/demo/branches/hot%23fix", "branch-old.json")
            .serve_fixture(
                "repos/acme/demo/compare/main...hot%23fix",
                "compare-ahead.json",
            );
        let ctx = ctx(fake, Config::default());
        let findings = run_one(&StaleRemoteBranches, &ctx);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warn);
        // The report shows the real branch name, not the encoded form.
        assert!(findings[0].message.contains("hot#fix"), "{findings:?}");
    }

    #[test]
    fn object_endpoints_are_fetched_unpaginated_and_list_endpoints_paginated() {
        // Paginating a single-object endpoint is at best wasted round
        // trips and at worst (compare) a concatenated multi-document
        // response; list endpoints, by contrast, must follow Link headers
        // or silently truncate at 100 entries.
        let fake = branches_fake();
        let calls = fake.call_log();
        let ctx = ctx(fake, Config::default());
        run_one(&StaleRemoteBranches, &ctx);

        let calls = calls.borrow();
        let paginated = |endpoint: &str| {
            calls
                .iter()
                .find(|(e, _)| e == endpoint)
                .unwrap_or_else(|| panic!("{endpoint} was never fetched"))
                .1
        };
        assert!(!paginated("repos/acme/demo"));
        assert!(!paginated("repos/acme/demo/branches/old-analysis"));
        assert!(!paginated("repos/acme/demo/compare/main...old-analysis"));
        assert!(paginated("repos/acme/demo/branches"));
    }

    #[test]
    fn stale_remote_branches_honors_a_huge_configured_threshold() {
        let mut config = Config::default();
        // Larger than any plausible age of the 2023 fixture branch.
        config.audit.stale_days = 100_000;
        let ctx = ctx(branches_fake(), config);
        assert_eq!(run_one(&StaleRemoteBranches, &ctx), Vec::new());
    }

    // ---- releases ----

    fn config_with_status(status: &str) -> Config {
        let mut config = Config::default();
        config.project.status = status.to_string();
        config
    }

    #[test]
    fn releases_not_required_while_active() {
        // No endpoint is served: if the check called the API at all it
        // would produce an error finding.
        let ctx = ctx(FakeGh::new(), config_with_status("active"));
        assert_eq!(run_one(&Releases, &ctx), Vec::new());
    }

    #[test]
    fn releases_missing_at_submission_is_an_error() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo/releases", "releases-empty.json");
        let ctx = ctx(fake, config_with_status("submitted"));
        let findings = run_one(&Releases, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_id, "releases");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("submitted"));
        assert!(findings[0].remediation.contains("gh release create"));
    }

    #[test]
    fn releases_present_at_publication_pass() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo/releases", "releases.json");
        let ctx = ctx(fake, config_with_status("published"));
        assert_eq!(run_one(&Releases, &ctx), Vec::new());
    }

    // ---- lifecycle-consistency ----

    #[test]
    fn lifecycle_retired_but_unarchived_is_flagged() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo", "repo.json");
        let ctx = ctx(fake, config_with_status("retired"));
        let findings = run_one(&LifecycleConsistency, &ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_id, "lifecycle-consistency");
        assert!(findings[0].message.contains("not archived"));
        assert!(
            findings[0]
                .remediation
                .contains("gh repo archive acme/demo")
        );
    }

    #[test]
    fn lifecycle_archived_but_active_is_flagged() {
        let fake = FakeGh::new().serve_fixture("repos/acme/demo", "repo-archived.json");
        let ctx = ctx(fake, config_with_status("active"));
        let findings = run_one(&LifecycleConsistency, &ctx);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("archived"));
    }

    #[test]
    fn lifecycle_consistent_states_pass() {
        let active = ctx(
            FakeGh::new().serve_fixture("repos/acme/demo", "repo.json"),
            config_with_status("active"),
        );
        assert_eq!(run_one(&LifecycleConsistency, &active), Vec::new());

        let retired = ctx(
            FakeGh::new().serve_fixture("repos/acme/demo", "repo-archived.json"),
            config_with_status("retired"),
        );
        assert_eq!(run_one(&LifecycleConsistency, &retired), Vec::new());
    }
}
