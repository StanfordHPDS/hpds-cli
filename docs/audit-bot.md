# The hpds audit bot

The audit bot publishes a repository's `hpds audit` findings as a comment on every pull request and as issues in the tracker.
It is a GitHub Actions workflow that runs the same audit you run locally and reports the results to GitHub.

## What it does

The workflow (`.github/workflows/hpds-audit.yml`) runs on two triggers:

- **Every pull request.** The bot posts a single sticky comment containing the findings table (severity, check, finding, and suggested fix).
  On subsequent pushes to the same pull request it edits that comment in place rather than posting a new one, so the pull request does not accumulate outdated audit comments.
  The comment is identified by an invisible HTML marker, `<!-- hpds-audit -->`; if the comment is deleted, the next run recreates it.

- **A weekly schedule** (Monday morning UTC).
  The bot files one GitHub issue per *new* error-severity finding, labeled `hpds-audit`.
  Each issue body embeds a stable fingerprint (a hash of the check id and the repository) in a marker comment, which makes the issue lifecycle idempotent:

  - A finding that already has an open issue never receives a duplicate.
  - Several findings from the same check share one issue.
  - When a scheduled audit no longer reports a finding, the bot comments on the corresponding issue and closes it.
  - Issues without a fingerprint marker (that is, issues filed by people, even under the `hpds-audit` label) are never touched.

Warnings and informational findings appear in the pull request comment but do not receive issues; only errors do.

## How a run works

Each run consists of three steps:

1. Install hpds.
2. Run `hpds audit --format json > audit.json`.
   The audit exits 1 when it finds errors, which is expected here, so the workflow captures the exit code and continues.
   Only exit codes above 1 (a usage error or a crash) fail the job before reporting.
3. Run `hpds audit report-github --input audit.json`, with `GITHUB_TOKEN` provided to the step.
   This subcommand contains all of the bot logic, so the workflow file remains a thin shim and bot improvements ship with hpds releases; the workflow does not need to be regenerated to receive them.

## Installation

From the repository you want audited:

```console
$ hpds use gha
```

and select **audit-bot** from the menu, or non-interactively:

```console
$ hpds use gha --workflows audit-bot
```

Either command writes `.github/workflows/hpds-audit.yml`.
Commit and push the file; the schedule and pull request triggers take effect immediately.

## Required permissions

The workflow declares the least privilege it needs:

```yaml
permissions:
  contents: read
  issues: write
  pull-requests: write
```

`contents: read` covers checkout, and the two `write` grants allow the default `GITHUB_TOKEN` to post the pull request comment and manage issues.
No personal access token is required.
If your organization restricts the default workflow token to read-only access, these per-workflow grants still apply, so no change to organization settings is needed.

## Running the reporter manually

`hpds audit report-github` takes its defaults from the GitHub Actions environment, but every input has a flag, so the bot can be run (or debugged) anywhere `gh` is authenticated:

  | Flag                  | Meaning                                                         | Actions default                        |
  | --------------------- | --------------------------------------------------------------- | -------------------------------------- |
  | `--input <FILE>`      | Audit JSON from `hpds audit --format json` (stdin when omitted) | none                                   |
  | `--repo <OWNER/REPO>` | Repository to report to                                         | `GITHUB_REPOSITORY`                    |
  | `--mode <MODE>`       | `pr` (sticky comment) or `schedule` (issue lifecycle)           | from `GITHUB_EVENT_NAME`               |
  | `--pr <NUMBER>`       | Pull request to comment on (`pr` mode)                          | from `GITHUB_REF` or the event payload |

For example, to preview the issue lifecycle against a repository:

```console
$ hpds audit --format json > audit.json
$ hpds audit report-github --input audit.json --repo StanfordHPDS/demo --mode schedule
```

## Tuning the audit with `[audit]` config

The bot reports whatever `hpds audit` finds, so it is tuned the same way as the audit itself: through the `[audit]` table in `hpds.toml` or in your user configuration.

```toml
[audit]
# Branches with no commits in more than this many days count as stale.
stale-days = 120
```

`required-watchers` (the GitHub logins that must watch every lab repository) is also an `[audit]` key, but it is honored from *user* configuration only; a repository cannot rewrite the lab-lead watcher list for everyone who audits it.

```toml
# ~/.config/hpds/config.toml
[audit]
required-watchers = ["malcolmbarrett", "sherrirose"]
```

Findings that are expected for a given repository are best resolved at the source (for example, set `status` and `primary-author` in the `[project]` table of `hpds.toml` so the lifecycle checks pass) rather than ignored.
