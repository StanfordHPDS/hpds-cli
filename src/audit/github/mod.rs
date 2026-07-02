//! GitHub-side audit checks, run against the repo's `origin` remote via the
//! `gh` CLI (`gh api ...`). Like the rest of the audit core, everything here
//! returns data — findings and strings — and never prints.
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
use std::process::Command;

use super::{Check, Finding, Severity};
use crate::gitx::{self, GhAuth};

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
    /// The endpoint answered HTTP 404 — meaningful to some checks (e.g. a
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
/// where it came from — checks word their findings differently when `HEAD`
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
    /// [`model::parse_pages`]); without it, gh fetches the endpoint once —
    /// the right mode for single-object endpoints, where pagination only
    /// multiplies round trips (and, for `compare`, response documents).
    fn api(&self, endpoint: &str, paginate: bool) -> Result<String, GhApiError>;

    /// Commit sha of the local branch with this name, falling back to
    /// `HEAD` (flagged via [`LocalTip::from_head`]); `None` when neither
    /// resolves.
    fn local_branch_commit(&self, branch: &str) -> Option<LocalTip>;
}

/// The real [`GithubApi`]: shells out to `gh` (and `git`, when a local
/// checkout exists). Without a checkout — the org sweep's metadata-only
/// mode — `gh api` runs from the process cwd and local branch lookups
/// always answer `None`.
struct GhCli {
    repo: Option<PathBuf>,
}

impl GithubApi for GhCli {
    fn api(&self, endpoint: &str, paginate: bool) -> Result<String, GhApiError> {
        let failed = |detail: String| GhApiError::Failed {
            endpoint: endpoint.to_string(),
            detail,
        };
        let mut cmd = Command::new("gh");
        cmd.args(["api", endpoint]);
        if paginate {
            cmd.arg("--paginate");
        }
        if let Some(repo) = &self.repo {
            cmd.current_dir(repo);
        }
        let out = cmd.output().map_err(|err| match err.kind() {
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
    /// though a paginating proxy may still concatenate — see
    /// [`model::parse_one`]).
    fn api_one(&self, endpoint: &str) -> Result<String, GhApiError> {
        self.api(endpoint, false)
    }

    /// Fetch a list endpoint, following pagination (one JSON array per
    /// page, concatenated — see [`model::parse_pages`]).
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
    /// No GitHub `origin` remote: the GitHub checks do not apply, silently
    /// (a purely local repo is a legitimate state; the local checks cover it).
    NoRemote,
    /// The checks apply but cannot run; the finding is an Info notice for
    /// the report (e.g. `gh` missing or unauthenticated).
    Skipped(Finding),
}

/// A GitHub context for a repo with no local checkout, keyed by slug
/// alone — the org sweep's `--no-clone` metadata pass. The caller is
/// responsible for having verified `gh` auth first.
pub fn ctx_without_checkout(slug: RepoSlug) -> GithubCtx {
    GithubCtx::new(slug, Box::new(GhCli { repo: None }))
}

/// Probe the repo's `origin` remote and `gh` auth state.
pub fn probe(repo: &Path) -> GithubStatus {
    let Some(slug) = origin_slug(repo) else {
        return GithubStatus::NoRemote;
    };
    match gitx::gh_auth() {
        Ok(GhAuth::Authenticated) => GithubStatus::Ready(GithubCtx::new(
            slug,
            Box::new(GhCli {
                repo: Some(repo.to_path_buf()),
            }),
        )),
        // Not installed, not logged in, or unprobeable all mean the same
        // thing for the report: we could not talk to GitHub as anyone.
        Ok(GhAuth::Unauthenticated(_)) | Ok(GhAuth::NotInstalled) | Err(_) => {
            GithubStatus::Skipped(skipped_notice())
        }
    }
}

/// The Info notice reported when the GitHub checks are skipped for lack of
/// gh authentication.
pub fn skipped_notice() -> Finding {
    Finding {
        check_id: "github".to_string(),
        severity: Severity::Info,
        message: "GitHub checks skipped: gh not authenticated".to_string(),
        remediation: "install the GitHub CLI (https://cli.github.com/) if needed, \
                      run `gh auth login`, then re-run `hpds audit`"
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
    fn skipped_notice_is_the_documented_info_finding() {
        let notice = skipped_notice();
        assert_eq!(notice.check_id, "github");
        assert_eq!(notice.severity, Severity::Info);
        assert_eq!(
            notice.message,
            "GitHub checks skipped: gh not authenticated"
        );
        assert!(notice.remediation.contains("gh auth login"));
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
}
