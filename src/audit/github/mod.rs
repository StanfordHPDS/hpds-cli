//! GitHub-side audit checks, run against the repo's `origin` remote via the
//! `gh` CLI (`gh api ...`). Like the rest of the audit core, everything here
//! returns data (findings and strings) and never prints.
//!
//! All external commands sit behind the [`GithubApi`] trait so the checks
//! are tested against recorded `gh` output (`tests/fixtures/tool-output/gh/`)
//! without a network or a `gh` binary.

mod checks;
// The typed gh JSON views are shared with the bot (`super::report_github`),
// which reads the same endpoints' output shapes.
pub(super) mod model;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::{Check, Finding, Severity};
use crate::gitx::{self, GhAuth};

/// Message prefix of the `watchers` finding; the logins that follow are
/// joined with `, `. The audit bot matches on it to render those logins as
/// GitHub mentions, so the check and the bot share this one definition.
pub const WATCHERS_MESSAGE_PREFIX: &str = "not watching the repo on GitHub: ";

/// `owner/repo` on github.com, detected from the `origin` remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSlug {
    pub owner: String,
    pub repo: String,
}

impl std::fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// Errors from a [`GithubApi::api`] call. Checks turn these into error
/// findings; they never abort the audit.
#[derive(Debug, thiserror::Error)]
pub enum GhApiError {
    /// The endpoint answered HTTP 404, meaningful to some checks (e.g. a
    /// compare against a commit GitHub has never seen).
    #[error("GitHub answered 404 Not Found")]
    NotFound,

    /// Anything else: network trouble, auth expiry mid-run, rate limits...
    #[error("`gh api {endpoint}` failed{}", render_detail(detail))]
    Failed { endpoint: String, detail: String },
}

fn render_detail(detail: &str) -> String {
    let trimmed = detail.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    }
}

/// The local commit the default-branch staleness comparison uses, and
/// where it came from; checks word their findings differently when `HEAD`
/// stood in for a missing local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTip {
    pub sha: String,
    /// True when no local branch with the requested name existed and the
    /// sha is `HEAD`'s instead (e.g. a single-branch clone of a feature
    /// branch, or local `main` vs remote `master`).
    pub from_head: bool,
}

/// The seam between the GitHub checks and the outside world: remote data
/// via `gh api`, plus the one local fact the staleness comparison needs.
/// Both live on one trait so tests fake the whole world in one place.
pub trait GithubApi {
    /// Raw stdout of `gh api <endpoint>`. With `paginate`, gh follows Link
    /// headers and concatenates one JSON document per page (see
    /// [`model::parse_pages`]); without it, gh fetches the endpoint once,
    /// the right mode for single-object endpoints, where pagination only
    /// multiplies round trips (and, for `compare`, response documents).
    fn api(&self, endpoint: &str, paginate: bool) -> Result<String, GhApiError>;

    /// Fetch several endpoints, answering one result per request in
    /// request order. Implementations may overlap the requests in time;
    /// this default simply calls [`Self::api`] once per request, so fakes
    /// and simple implementations need nothing extra.
    fn api_many(&self, requests: &[(String, bool)]) -> Vec<Result<String, GhApiError>> {
        requests
            .iter()
            .map(|(endpoint, paginate)| self.api(endpoint, *paginate))
            .collect()
    }

    /// Commit sha of the local branch with this name, falling back to
    /// `HEAD` (flagged via [`LocalTip::from_head`]); `None` when neither
    /// resolves.
    fn local_branch_commit(&self, branch: &str) -> Option<LocalTip>;
}

/// The real [`GithubApi`]: shells out to `gh` (and `git`, when a local
/// checkout exists). Without a checkout (the org sweep's metadata-only
/// mode), `gh api` runs from the process cwd and local branch lookups
/// always answer `None`.
struct GhCli {
    repo: Option<PathBuf>,
}

impl GhCli {
    /// The `gh api` invocation for one endpoint. Both fetch paths
    /// ([`GithubApi::api`] and [`GithubApi::api_many`]) build their
    /// commands here so their behavior cannot drift apart.
    fn command(&self, endpoint: &str, paginate: bool) -> Command {
        let mut cmd = Command::new(crate::gitx::gh_program());
        cmd.args(["api", endpoint]);
        if paginate {
            cmd.arg("--paginate");
        }
        if let Some(repo) = &self.repo {
            cmd.current_dir(repo);
        }
        cmd
    }
}

/// Map a finished (or failed-to-run) `gh api` invocation onto the result
/// the checks consume; shared by both fetch paths.
fn interpret_gh_output(endpoint: &str, result: io::Result<Output>) -> Result<String, GhApiError> {
    let failed = |detail: String| GhApiError::Failed {
        endpoint: endpoint.to_string(),
        detail,
    };
    let out = result.map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => failed("gh is not installed or not on PATH".into()),
        _ => failed(err.to_string()),
    })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // gh reports HTTP errors as e.g. `gh: Not Found (HTTP 404)`; match
    // either half of that wording so a rephrasing on gh's side degrades
    // 404 detection only if both change at once.
    if stderr.contains("HTTP 404") || stderr.contains("Not Found") {
        return Err(GhApiError::NotFound);
    }
    Err(failed(stderr.into_owned()))
}

impl GithubApi for GhCli {
    fn api(&self, endpoint: &str, paginate: bool) -> Result<String, GhApiError> {
        interpret_gh_output(endpoint, self.command(endpoint, paginate).output())
    }

    /// Concurrent batch fetch: every `gh api` process is spawned before
    /// any is waited on, so the round trips overlap instead of queueing.
    fn api_many(&self, requests: &[(String, bool)]) -> Vec<Result<String, GhApiError>> {
        let children: Vec<io::Result<std::process::Child>> = requests
            .iter()
            .map(|(endpoint, paginate)| {
                self.command(endpoint, *paginate)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            })
            .collect();
        children
            .into_iter()
            .zip(requests)
            .map(|(child, (endpoint, _))| {
                interpret_gh_output(endpoint, child.and_then(|child| child.wait_with_output()))
            })
            .collect()
    }

    fn local_branch_commit(&self, branch: &str) -> Option<LocalTip> {
        let repo = self.repo.as_deref()?;
        let rev_parse = |rev: &str| -> Option<String> {
            let out = super::checks::git_maybe(repo, &["rev-parse", "--verify", "--quiet", rev])?;
            let sha = out.trim().to_string();
            (!sha.is_empty()).then_some(sha)
        };
        if let Some(sha) = rev_parse(&format!("refs/heads/{branch}")) {
            return Some(LocalTip {
                sha,
                from_head: false,
            });
        }
        rev_parse("HEAD").map(|sha| LocalTip {
            sha,
            from_head: true,
        })
    }
}

/// GitHub-side context for [`super::AuditCtx`]: which repo on GitHub, and
/// how to reach it. Successful `gh api` responses are memoized so checks
/// that need the same endpoint (e.g. repo metadata) share one call.
pub struct GithubCtx {
    pub slug: RepoSlug,
    gh: Box<dyn GithubApi>,
    cache: RefCell<BTreeMap<String, String>>,
}

impl GithubCtx {
    pub fn new(slug: RepoSlug, gh: Box<dyn GithubApi>) -> Self {
        GithubCtx {
            slug,
            gh,
            cache: RefCell::new(BTreeMap::new()),
        }
    }

    /// Fetch a single-object endpoint (no pagination; one JSON document,
    /// though a paginating proxy may still concatenate; see
    /// [`model::parse_one`]).
    fn api_one(&self, endpoint: &str) -> Result<String, GhApiError> {
        self.api(endpoint, false)
    }

    /// Fetch a list endpoint, following pagination (one JSON array per
    /// page, concatenated; see [`model::parse_pages`]).
    fn api_pages(&self, endpoint: &str) -> Result<String, GhApiError> {
        self.api(endpoint, true)
    }

    /// `gh api` with per-endpoint memoization of successful responses.
    /// Keyed by endpoint alone: each endpoint is only ever fetched in one
    /// mode (objects via [`Self::api_one`], lists via [`Self::api_pages`]).
    fn api(&self, endpoint: &str, paginate: bool) -> Result<String, GhApiError> {
        if let Some(hit) = self.cache.borrow().get(endpoint) {
            return Ok(hit.clone());
        }
        let body = self.gh.api(endpoint, paginate)?;
        self.cache
            .borrow_mut()
            .insert(endpoint.to_string(), body.clone());
        Ok(body)
    }

    fn local_branch_commit(&self, branch: &str) -> Option<LocalTip> {
        self.gh.local_branch_commit(branch)
    }

    /// Warm the memoization cache by fetching, concurrently where the
    /// backend supports it, every endpoint the checks are about to
    /// request. Purely an optimization: it prints nothing, and an
    /// endpoint that fails here is simply left uncached, so the check
    /// that needs it refetches and reports its own finding exactly as it
    /// would without prefetch.
    pub fn prefetch(&self, config: &crate::config::Config, pull_request_run: bool) {
        checks::prefetch(self, config, pull_request_run);
    }

    /// Batch-fetch the not-yet-cached endpoints among `requests` and
    /// memoize the successes, mirroring [`Self::api`]'s semantics;
    /// failures are dropped for the owning check to rediscover.
    fn cache_many(&self, requests: Vec<(String, bool)>) {
        let uncached: Vec<(String, bool)> = {
            let cache = self.cache.borrow();
            requests
                .into_iter()
                .filter(|(endpoint, _)| !cache.contains_key(endpoint))
                .collect()
        };
        if uncached.is_empty() {
            return;
        }
        let results = self.gh.api_many(&uncached);
        let mut cache = self.cache.borrow_mut();
        for ((endpoint, _), result) in uncached.into_iter().zip(results) {
            if let Ok(body) = result {
                cache.insert(endpoint, body);
            }
        }
    }

    /// A cached response body, if the endpoint has one.
    fn cached(&self, endpoint: &str) -> Option<String> {
        self.cache.borrow().get(endpoint).cloned()
    }
}

/// The GitHub checks, in report order. Run these only when [`probe`]
/// returned [`GithubStatus::Ready`]; each check individually no-ops when
/// `ctx.github` is `None`.
pub fn registry() -> Vec<Box<dyn Check>> {
    checks::registry()
}

/// Whether the GitHub checks can run against this repo, decided by the
/// command layer before the audit starts.
pub enum GithubStatus {
    /// `origin` points at github.com and `gh` is authenticated.
    Ready(GithubCtx),
    /// No GitHub `origin` remote: the GitHub checks do not apply. The
    /// single-repo audit reports this as the [`no_remote_notice`] Info
    /// finding so the report says why those checks are absent.
    NoRemote,
    /// The checks apply but cannot run (e.g. `gh` missing or
    /// unauthenticated); the finding is the [`skipped_notice`] for the
    /// report, an Info notice locally and a warning in GitHub Actions.
    Skipped(Finding),
}

/// A GitHub context for a repo with no local checkout, keyed by slug
/// alone: the org sweep's `--no-clone` metadata pass. The caller is
/// responsible for having verified `gh` auth first.
pub fn ctx_without_checkout(slug: RepoSlug) -> GithubCtx {
    GithubCtx::new(slug, Box::new(GhCli { repo: None }))
}

/// Where the audit runs, as read from the environment by the command
/// layer: decides how [`probe`] treats gh auth and which
/// [`skipped_notice`] it reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunEnv {
    /// The audit runs inside GitHub Actions.
    pub github_actions: bool,
    /// `GH_TOKEN` or `GITHUB_TOKEN` is set to a non-empty value.
    pub env_token: bool,
}

/// Probe the repo's `origin` remote and `gh` auth state.
///
/// Locally, `gh auth status` decides whether the GitHub checks run, so a
/// stale token left in the shell environment changes nothing. Inside
/// GitHub Actions, an environment token is trusted as long as gh is
/// installed, because `gh auth status` can exit non-zero for the
/// workflow's installation token even when `gh api` calls with it work.
pub fn probe(repo: &Path, env: RunEnv) -> GithubStatus {
    let Some(slug) = origin_slug(repo) else {
        return GithubStatus::NoRemote;
    };
    if gh_ready(gh_state(&gitx::gh_auth()), env) {
        GithubStatus::Ready(GithubCtx::new(
            slug,
            Box::new(GhCli {
                repo: Some(repo.to_path_buf()),
            }),
        ))
    } else {
        GithubStatus::Skipped(skipped_notice(env.github_actions))
    }
}

/// What a `gh auth status` probe says, reduced to what [`gh_ready`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhState {
    /// `gh auth status` exited 0.
    LoggedIn,
    /// gh ran but `gh auth status` exited non-zero.
    LoggedOut,
    /// gh is not installed, or could not be run at all.
    Unavailable,
}

fn gh_state(auth: &anyhow::Result<GhAuth>) -> GhState {
    match auth {
        Ok(GhAuth::Authenticated) => GhState::LoggedIn,
        Ok(GhAuth::Unauthenticated(_)) => GhState::LoggedOut,
        Ok(GhAuth::NotInstalled) | Err(_) => GhState::Unavailable,
    }
}

/// Whether the GitHub checks can run, given gh's state and where the
/// audit runs. In GitHub Actions with an environment token, a failed
/// `gh auth status` is not trusted; if the token really is invalid, each
/// GitHub check reports its own "could not complete" warning instead.
fn gh_ready(state: GhState, env: RunEnv) -> bool {
    match state {
        GhState::LoggedIn => true,
        GhState::LoggedOut => env.github_actions && env.env_token,
        GhState::Unavailable => false,
    }
}

/// The notice reported when the GitHub checks are skipped for lack of gh
/// authentication. Locally it is Info and points at `gh auth login`.
/// Inside GitHub Actions (`github_actions`) it is a warning, because there
/// it means the workflow does not pass its token to the audit.
pub fn skipped_notice(github_actions: bool) -> Finding {
    let (severity, remediation) = if github_actions {
        (
            Severity::Warn,
            "set `GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}` in the workflow's \
             `env` so the audit step can reach GitHub, or regenerate the \
             workflow with `hpds use gha --workflows audit-bot --force`",
        )
    } else {
        (
            Severity::Info,
            "install the GitHub CLI (https://cli.github.com/) if needed, \
             run `gh auth login`, then re-run `hpds audit`",
        )
    };
    Finding {
        check_id: "github".to_string(),
        severity,
        message: "GitHub checks skipped: gh not authenticated".to_string(),
        remediation: remediation.to_string(),
    }
}

/// The Info notice reported when the GitHub checks are skipped because the
/// repo has no github.com `origin` remote; symmetric with
/// [`skipped_notice`], so the report always says why those checks are
/// absent.
pub fn no_remote_notice() -> Finding {
    Finding {
        check_id: "github".to_string(),
        severity: Severity::Info,
        message: "GitHub checks skipped: no origin remote".to_string(),
        remediation: "if this project should live on GitHub, create the repo \
                      (`hpds repo create`) or point origin at it \
                      (`git remote add origin <url>`); purely local repos can \
                      ignore this"
            .to_string(),
    }
}

/// The github.com `owner/repo` of the `origin` remote, or `None` when there
/// is no repo, no origin, or origin points elsewhere.
fn origin_slug(repo: &Path) -> Option<RepoSlug> {
    let out = super::checks::git_maybe(repo, &["remote", "get-url", "origin"])?;
    parse_github_url(out.trim())
}

/// Parse a github.com remote URL (https, ssh scp-like, or ssh://) into a
/// slug. Non-GitHub hosts return `None`.
fn parse_github_url(url: &str) -> Option<RepoSlug> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(RepoSlug {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_ssh_and_git_protocol_remote_urls() {
        for url in [
            "https://github.com/acme/demo.git",
            "https://github.com/acme/demo",
            "https://github.com/acme/demo/",
            "git@github.com:acme/demo.git",
            "git@github.com:acme/demo",
            "ssh://git@github.com/acme/demo.git",
            "git://github.com/acme/demo.git",
        ] {
            let slug = parse_github_url(url).unwrap_or_else(|| panic!("no slug from {url}"));
            assert_eq!(slug.owner, "acme", "{url}");
            assert_eq!(slug.repo, "demo", "{url}");
        }
    }

    #[test]
    fn rejects_non_github_and_malformed_remote_urls() {
        for url in [
            "https://gitlab.com/acme/demo.git",
            "git@bitbucket.org:acme/demo.git",
            "https://github.com/acme",
            "https://github.com/",
            "https://github.com/acme/demo/extra",
            "/local/path/to/repo.git",
            "",
        ] {
            assert!(parse_github_url(url).is_none(), "accepted {url:?}");
        }
    }

    #[test]
    fn slug_displays_as_owner_slash_repo() {
        let slug = RepoSlug {
            owner: "acme".to_string(),
            repo: "demo".to_string(),
        };
        assert_eq!(slug.to_string(), "acme/demo");
    }

    #[test]
    fn gh_readiness_covers_every_state_and_environment() {
        let env = |github_actions, env_token| RunEnv {
            github_actions,
            env_token,
        };
        // A successful `gh auth status` is enough anywhere.
        for run in [
            env(false, false),
            env(false, true),
            env(true, false),
            env(true, true),
        ] {
            assert!(gh_ready(GhState::LoggedIn, run), "{run:?}");
        }
        // In Actions an environment token stands in for a failed status.
        assert!(gh_ready(GhState::LoggedOut, env(true, true)));
        // Without Actions, or without a token, gh auth status decides.
        assert!(!gh_ready(GhState::LoggedOut, env(true, false)));
        assert!(!gh_ready(GhState::LoggedOut, env(false, true)));
        assert!(!gh_ready(GhState::LoggedOut, env(false, false)));
        // Without gh, nothing can run.
        for run in [
            env(false, false),
            env(false, true),
            env(true, false),
            env(true, true),
        ] {
            assert!(!gh_ready(GhState::Unavailable, run), "{run:?}");
        }
    }

    #[test]
    fn gh_auth_results_map_onto_gh_states() {
        assert_eq!(gh_state(&Ok(GhAuth::Authenticated)), GhState::LoggedIn);
        assert_eq!(gh_state(&Ok(GhAuth::NotInstalled)), GhState::Unavailable);
        assert_eq!(
            gh_state(&Err(anyhow::anyhow!("spawn failed"))),
            GhState::Unavailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_gh_auth_status_maps_onto_logged_out() {
        use std::os::unix::process::ExitStatusExt;
        let output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"You are not logged into any GitHub hosts.\n".to_vec(),
        };
        assert_eq!(
            gh_state(&Ok(GhAuth::Unauthenticated(output))),
            GhState::LoggedOut
        );
    }

    #[test]
    fn skipped_notice_is_the_documented_info_finding() {
        let notice = skipped_notice(false);
        assert_eq!(notice.check_id, "github");
        assert_eq!(notice.severity, Severity::Info);
        assert_eq!(
            notice.message,
            "GitHub checks skipped: gh not authenticated"
        );
        assert!(notice.remediation.contains("gh auth login"));
        assert!(!notice.remediation.contains("secrets.GITHUB_TOKEN"));
    }

    #[test]
    fn skipped_notice_in_github_actions_is_a_warning_naming_the_workflow_token() {
        let notice = skipped_notice(true);
        assert_eq!(notice.check_id, "github");
        assert_eq!(notice.severity, Severity::Warn);
        assert_eq!(
            notice.message,
            "GitHub checks skipped: gh not authenticated"
        );
        assert!(
            notice
                .remediation
                .contains("GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}"),
            "remediation names the workflow token: {}",
            notice.remediation
        );
        assert!(
            notice
                .remediation
                .contains("hpds use gha --workflows audit-bot --force"),
            "remediation says how to regenerate the workflow: {}",
            notice.remediation
        );
        assert!(!notice.remediation.contains("gh auth login"));
    }

    #[test]
    fn no_remote_notice_is_info_and_symmetric_with_the_auth_notice() {
        let notice = no_remote_notice();
        assert_eq!(notice.check_id, "github");
        assert_eq!(notice.severity, Severity::Info);
        assert_eq!(notice.message, "GitHub checks skipped: no origin remote");
        assert!(notice.remediation.contains("git remote add origin"));
    }

    #[test]
    fn origin_slug_is_none_outside_a_git_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(origin_slug(tmp.path()).is_none());
    }

    #[test]
    fn gh_cli_without_a_checkout_answers_no_local_branch() {
        let gh = GhCli { repo: None };
        assert!(gh.local_branch_commit("main").is_none());
    }

    #[test]
    fn ctx_api_memoizes_successful_responses() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct CountingGh {
            calls: Rc<Cell<usize>>,
        }
        impl GithubApi for CountingGh {
            fn api(&self, _endpoint: &str, _paginate: bool) -> Result<String, GhApiError> {
                self.calls.set(self.calls.get() + 1);
                Ok("{}".to_string())
            }
            fn local_branch_commit(&self, _branch: &str) -> Option<LocalTip> {
                None
            }
        }

        let calls = Rc::new(Cell::new(0));
        let ctx = GithubCtx::new(
            RepoSlug {
                owner: "acme".to_string(),
                repo: "demo".to_string(),
            },
            Box::new(CountingGh {
                calls: Rc::clone(&calls),
            }),
        );
        ctx.api_one("repos/acme/demo").expect("first call");
        ctx.api_one("repos/acme/demo").expect("cached call");
        assert_eq!(calls.get(), 1, "second call must hit the cache");
        ctx.api_pages("repos/acme/demo/releases")
            .expect("other endpoint");
        assert_eq!(calls.get(), 2);
    }

    /// Online smoke test for the real `gh` shelling path: fetches a stable
    /// public repo and parses it with the same model the checks use. Needs
    /// network plus an authenticated `gh`, so it is opt-in twice over.
    #[cfg(feature = "online-tests")]
    #[test]
    #[ignore = "network + authenticated gh required; run with --features online-tests -- --ignored"]
    fn online_gh_api_fetches_and_parses_a_real_repo() {
        let gh = GhCli {
            repo: Some(std::env::temp_dir()),
        };
        let body = gh
            .api("repos/cli/cli", false)
            .expect("gh api repos/cli/cli");
        let info: model::RepoInfo = model::parse_one(&body).expect("parse real repo info");
        assert_eq!(info.default_branch, "trunk");
        assert!(!info.archived);
    }

    /// Online smoke test for the concurrent batch path: two real
    /// endpoints spawned together must come back in request order with
    /// the same bodies the sequential path would produce.
    #[cfg(feature = "online-tests")]
    #[test]
    #[ignore = "network + authenticated gh required; run with --features online-tests -- --ignored"]
    fn online_gh_api_many_fetches_endpoints_concurrently_in_order() {
        let gh = GhCli {
            repo: Some(std::env::temp_dir()),
        };
        let results = gh.api_many(&[
            ("repos/cli/cli".to_string(), false),
            ("repos/cli/cli/no-such-endpoint".to_string(), false),
        ]);
        assert_eq!(results.len(), 2);
        let info: model::RepoInfo =
            model::parse_one(results[0].as_ref().expect("first endpoint succeeds"))
                .expect("parse real repo info");
        assert_eq!(info.default_branch, "trunk");
        assert!(matches!(results[1], Err(GhApiError::NotFound)));
    }
}
