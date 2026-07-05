# The hpds audit bot

The audit bot keeps a repo's `hpds audit` findings visible where the lab
actually looks: as a comment on every pull request and as issues in the
tracker. It is a GitHub Actions workflow that runs the same audit you run
locally and mirrors the results to GitHub — no separate service, no state
outside the repo itself.

## What it does

The workflow (`.github/workflows/hpds-audit.yml`) runs on two triggers:

- **Every pull request.** The bot posts one sticky comment containing the
  findings table (severity, check, finding, suggested fix). On later pushes
  to the same PR it edits that comment in place rather than posting a new
  one, so the PR never fills up with stale audit noise. The comment is
  identified by an invisible HTML marker, `<!-- hpds-audit -->`; if you
  delete the comment, the next run simply recreates it.

- **A weekly schedule** (Monday morning UTC). The bot files one GitHub
  issue per *new* error-severity finding, labeled `hpds-audit`. Each issue
  body embeds a stable fingerprint (a hash of the check id and the repo)
  in a marker comment, which makes the issue lifecycle idempotent:

  - a finding that already has an open issue never gets a duplicate;
  - several findings from the same check share one issue;
  - when a scheduled audit no longer reports a finding, the bot comments
    on its issue and closes it;
  - issues without a fingerprint marker (i.e. filed by humans, even under
    the `hpds-audit` label) are never touched.

Warnings and info findings appear in the PR comment but do not get issues;
only errors do.

## Anatomy of a run

Each run is three steps:

1. Install hpds by piping the release installer script to `sh`, which
   pulls the latest published release. Until the first `v0.1.0` tag is
   published no release exists, so this step fails — expected, and the
   template comment says so.
2. `hpds audit --format json > audit.json`. The audit exits 1 when it
   finds errors — expected here, so the workflow captures the exit code
   and continues; only exit codes above 1 (usage error, crash) fail the
   job before reporting.
3. `hpds audit report-github --input audit.json`, with `GITHUB_TOKEN`
   provided to the step. This subcommand contains all the bot logic, so
   the workflow file stays a thin shim and bot improvements ship with
   hpds releases — you do not need to regenerate the workflow to get them.

## Installing it

From the repo you want audited:

```console
$ hpds use gha
```

and pick **audit-bot** from the menu, or non-interactively:

```console
$ hpds use gha --workflows audit-bot
```

Either writes `.github/workflows/hpds-audit.yml`. Commit and push it; the
schedule and PR triggers take effect immediately.

## Required permissions

The workflow declares the least privilege it needs:

```yaml
permissions:
  contents: read
  issues: write
  pull-requests: write
```

`contents: read` covers checkout; the two `write` grants let the default
`GITHUB_TOKEN` post the PR comment and manage issues. No personal access
token is needed. If your organization restricts the default workflow token
to read-only, these per-workflow grants still apply — no admin settings
change is required.

## Running the reporter by hand

`hpds audit report-github` defaults everything from the GitHub Actions
environment, but every input has a flag, so you can run (or debug) the bot
anywhere `gh` is authenticated:

| Flag | Meaning | Actions default |
| --- | --- | --- |
| `--input <FILE>` | Audit JSON from `hpds audit --format json` (stdin when omitted) | — |
| `--repo <OWNER/REPO>` | Repository to report to | `GITHUB_REPOSITORY` |
| `--mode <MODE>` | `pr` (sticky comment) or `schedule` (issue lifecycle) | from `GITHUB_EVENT_NAME` |
| `--pr <NUMBER>` | Pull request to comment on (`pr` mode) | from `GITHUB_REF` / event payload |

For example, to preview the issue lifecycle pass against a repo:

```console
$ hpds audit --format json > audit.json
$ hpds audit report-github --input audit.json --repo StanfordHPDS/demo --mode schedule
```

## Tuning the audit with `[audit]` config

The bot reports whatever `hpds audit` finds, so you tune it the same way
you tune the audit — via the `[audit]` table in `hpds.toml` (or your user
config):

```toml
[audit]
# Branches with no commits in more than this many days count as stale.
stale-days = 120
```

`required-watchers` (the GitHub logins that must watch every lab repo) is
also an `[audit]` key, but it is honored from *user* config only — a repo
cannot rewrite the lab-lead watcher list for everyone who audits it:

```toml
# ~/.config/hpds/config.toml
[audit]
required-watchers = ["malcolmbarrett", "sherrirose"]
```

Findings you consider expected for a given repo are best fixed at the
source (e.g. set `[project]` `status` and `primary-author` in `hpds.toml`
so the lifecycle checks pass) rather than ignored.
