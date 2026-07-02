//! Typed views of the `gh api` JSON the GitHub checks consume, plus the
//! parsing helpers that turn raw `gh` stdout into them.
//!
//! `gh api --paginate` concatenates one JSON document per page back to
//! back (`[...][...]`), so list endpoints parse through [`parse_pages`],
//! which flattens the stream. Malformed JSON is an ordinary error here —
//! callers convert it into an error [`super::super::Finding`], never a
//! panic.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Parsing/interpretation failures; rendered into error findings upstream.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("unexpected JSON from gh: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unexpected timestamp from gh: `{0}` is not an ISO 8601 date-time")]
    Timestamp(String),
}

/// `repos/{owner}/{repo}` — the fields the checks care about.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoInfo {
    pub default_branch: String,
    pub archived: bool,
    pub owner: Owner,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Owner {
    pub login: String,
    /// `"Organization"` or `"User"`.
    #[serde(rename = "type")]
    pub kind: String,
}

/// One user in `subscribers`, `contributors`, or `orgs/{org}/members`.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub login: String,
}

/// One entry of `repos/{owner}/{repo}/branches`.
#[derive(Debug, Clone, Deserialize)]
pub struct BranchSummary {
    pub name: String,
}

/// `repos/{owner}/{repo}/branches/{branch}` — enough to date the tip.
#[derive(Debug, Clone, Deserialize)]
pub struct BranchDetail {
    #[allow(dead_code)] // read by tests to pin the recorded shape; checks key on the tip date
    pub name: String,
    pub commit: BranchTip,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchTip {
    pub commit: TipCommit,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TipCommit {
    pub committer: CommitSignature,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitSignature {
    /// ISO 8601, e.g. `2023-01-15T10:12:00Z`.
    pub date: String,
}

/// `repos/{owner}/{repo}/compare/{base}...{head}`.
#[derive(Debug, Clone, Deserialize)]
pub struct Comparison {
    /// `identical`, `ahead`, `behind`, or `diverged` (head vs base).
    #[allow(dead_code)] // read by tests to pin the recorded shape; checks use the two counts
    pub status: String,
    /// Commits in head that base lacks.
    pub ahead_by: u64,
    /// Commits in base that head lacks.
    pub behind_by: u64,
}

/// One entry of `repos/{owner}/{repo}/releases`.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    #[allow(dead_code)] // deserialized to keep the shape honest; checks only count releases
    pub tag_name: String,
}

/// Parse a single-object endpoint response, taking the first JSON document
/// and ignoring any concatenated later ones.
///
/// Object endpoints normally answer with exactly one document, but a
/// paginated `compare` (>100 commits between the refs) repeats the whole
/// comparison object once per page — same `ahead_by`/`behind_by` totals,
/// different `commits` window — so the first document always carries
/// everything the checks read.
pub fn parse_one<T: DeserializeOwned>(json: &str) -> Result<T, ModelError> {
    match serde_json::Deserializer::from_str(json)
        .into_iter::<T>()
        .next()
    {
        Some(first) => Ok(first?),
        // Empty input: re-parse the whole string for a plain EOF error.
        None => Ok(serde_json::from_str(json)?),
    }
}

/// Parse a (possibly paginated) list endpoint response: one JSON array per
/// page, concatenated. A single array is the one-page special case.
pub fn parse_pages<T: DeserializeOwned>(json: &str) -> Result<Vec<T>, ModelError> {
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_str(json).into_iter::<Vec<T>>() {
        items.extend(page?);
    }
    Ok(items)
}

/// Whole days elapsed from an ISO 8601 timestamp (as emitted by the GitHub
/// API, e.g. `2023-01-15T10:12:00Z`) to `now`. Negative when the timestamp
/// is in the future.
pub fn days_since(timestamp: &str, now: SystemTime) -> Result<i64, ModelError> {
    let then = parse_epoch_seconds(timestamp)
        .ok_or_else(|| ModelError::Timestamp(timestamp.to_string()))?;
    // Work in signed seconds relative to the Unix epoch so pre-1970 or
    // future dates cannot underflow.
    let now = match now.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    Ok((now - then).div_euclid(86_400))
}

/// `YYYY-MM-DDTHH:MM:SS` plus `Z` or a `±hh:mm` offset → Unix seconds.
/// Fractional seconds are accepted and ignored. Returns `None` on any
/// shape or range violation.
fn parse_epoch_seconds(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(range)?;
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // The remainder is optional fractional seconds, then `Z` or `±hh:mm`.
    let mut rest = &s[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &after_dot[digits..];
    }
    let offset_seconds = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return None,
            };
            let rest = &rest[1..];
            let (h, m) = rest.split_once(':')?;
            if h.len() != 2 || m.len() != 2 {
                return None;
            }
            let h: i64 = h.parse().ok()?;
            let m: i64 = m.parse().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };

    Some(
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_seconds,
    )
}

/// Days from the Unix epoch to the given civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    /// A recorded `gh api` output from `tests/fixtures/tool-output/gh/`.
    pub(crate) fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool-output/gh")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    fn at(epoch_seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(epoch_seconds)
    }

    #[test]
    fn parses_the_recorded_repo_info() {
        let info: RepoInfo = parse_one(&fixture("repo.json")).expect("repo.json parses");
        assert_eq!(info.default_branch, "main");
        assert!(!info.archived);
        assert_eq!(info.owner.login, "acme");
        assert_eq!(info.owner.kind, "Organization");
    }

    #[test]
    fn parses_the_recorded_archived_repo_info() {
        let info: RepoInfo = parse_one(&fixture("repo-archived.json")).expect("parses");
        assert!(info.archived);
    }

    #[test]
    fn parses_the_recorded_subscribers_list() {
        let subs: Vec<Account> = parse_pages(&fixture("subscribers.json")).expect("parses");
        let logins: Vec<&str> = subs.iter().map(|a| a.login.as_str()).collect();
        assert_eq!(logins, ["malcolmbarrett", "SherriRose"]);
    }

    #[test]
    fn parses_the_recorded_contributors_list() {
        let contributors: Vec<Account> =
            parse_pages(&fixture("contributors.json")).expect("parses");
        let logins: Vec<&str> = contributors.iter().map(|a| a.login.as_str()).collect();
        assert_eq!(logins, ["researcher1", "malcolmbarrett", "dependabot[bot]"]);
    }

    #[test]
    fn parses_paginated_branch_pages_as_one_list() {
        // branches.json records two concatenated pages, the raw shape of
        // `gh api --paginate` output.
        let branches: Vec<BranchSummary> = parse_pages(&fixture("branches.json")).expect("parses");
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["main", "old-analysis", "fresh-idea"]);
    }

    #[test]
    fn parses_the_recorded_branch_detail_with_its_tip_date() {
        let branch: BranchDetail = parse_one(&fixture("branch-old.json")).expect("parses");
        assert_eq!(branch.name, "old-analysis");
        assert_eq!(branch.commit.commit.committer.date, "2023-01-15T10:12:00Z");
    }

    #[test]
    fn parses_the_recorded_comparisons() {
        for (name, status, ahead, behind) in [
            ("compare-identical.json", "identical", 0, 0),
            ("compare-ahead.json", "ahead", 3, 0),
            ("compare-behind.json", "behind", 0, 2),
            ("compare-diverged.json", "diverged", 2, 1),
        ] {
            let cmp: Comparison = parse_one(&fixture(name)).expect(name);
            assert_eq!(cmp.status, status, "{name}");
            assert_eq!(cmp.ahead_by, ahead, "{name}");
            assert_eq!(cmp.behind_by, behind, "{name}");
        }
    }

    #[test]
    fn parses_the_recorded_release_lists() {
        let releases: Vec<Release> = parse_pages(&fixture("releases.json")).expect("parses");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].tag_name, "v1.0.0");
        let none: Vec<Release> = parse_pages(&fixture("releases-empty.json")).expect("parses");
        assert!(none.is_empty());
    }

    #[test]
    fn parse_one_takes_the_totals_from_a_multi_page_compare_stream() {
        // A compare spanning >100 commits paginates: `gh api --paginate`
        // concatenates one full comparison object per page, each repeating
        // the same ahead_by/behind_by totals. The first page must win, not
        // a trailing-data parse error.
        let cmp: Comparison =
            parse_one(&fixture("compare-ahead-paginated.json")).expect("first page parses");
        assert_eq!(cmp.status, "ahead");
        assert_eq!(cmp.ahead_by, 150);
        assert_eq!(cmp.behind_by, 0);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse_one::<RepoInfo>(&fixture("malformed.json")).is_err());
        assert!(parse_pages::<Account>(&fixture("malformed.json")).is_err());
        assert!(parse_one::<RepoInfo>("").is_err());
        assert!(parse_pages::<Account>("<!doctype html>").is_err());
    }

    #[test]
    fn wrong_but_valid_json_shape_is_an_error() {
        // A JSON object where a list was expected, and vice versa.
        assert!(parse_pages::<Account>(&fixture("repo.json")).is_err());
        assert!(parse_one::<RepoInfo>(&fixture("subscribers.json")).is_err());
    }

    #[test]
    fn days_since_counts_whole_days() {
        // 2023-01-15T10:12:00Z is 1673777520; ten days later to the second.
        let now = at(1_673_777_520 + 10 * 86_400);
        assert_eq!(days_since("2023-01-15T10:12:00Z", now).unwrap(), 10);
        // One second short of ten days rounds down.
        let almost = at(1_673_777_520 + 10 * 86_400 - 1);
        assert_eq!(days_since("2023-01-15T10:12:00Z", almost).unwrap(), 9);
    }

    #[test]
    fn days_since_a_future_date_is_negative() {
        let now = at(1_673_777_520);
        assert!(days_since("2099-01-01T00:00:00Z", now).unwrap() < 0);
    }

    #[test]
    fn days_since_handles_offsets_and_fractional_seconds() {
        let now = at(1_673_777_520 + 86_400);
        // Same instant written three ways.
        assert_eq!(days_since("2023-01-15T10:12:00Z", now).unwrap(), 1);
        assert_eq!(days_since("2023-01-15T10:12:00.123Z", now).unwrap(), 1);
        assert_eq!(days_since("2023-01-15T05:12:00-05:00", now).unwrap(), 1);
    }

    #[test]
    fn days_since_rejects_garbage_timestamps() {
        let now = at(1_673_777_520);
        for bad in [
            "",
            "yesterday",
            "2023-01-15",
            "2023-13-15T10:12:00Z",
            "2023-01-32T10:12:00Z",
            "2023-01-15T25:12:00Z",
            "2023-01-15T10:12:00",
            "2023-01-15T10:12:00+5:00",
            "20230115T101200Z",
        ] {
            assert!(days_since(bad, now).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // 2023-01-15 = 1673740800 / 86400.
        assert_eq!(days_from_civil(2023, 1, 15), 19_372);
        // Leap day.
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
    }
}
