# `hpds.toml` configuration reference

`hpds` works with **zero configuration**. Every key below is optional and has a
built-in default; a config file only *overrides* those defaults. Run `hpds config`
at any time to print the fully resolved configuration and the path of each file that
contributed to it.

Formatter/linter configuration lives with the lab's separate togi tool, not here.

## Where config lives and how it layers

`hpds` reads configuration from two files:

- **Project config** — `hpds.toml`, discovered by walking up from the current
  directory to the git root (or the filesystem root).
- **User config** — `config.toml` in your platform config directory
  (`~/.config/hpds/config.toml` on Linux, `~/Library/Application Support/hpds` on
  macOS, `%APPDATA%\hpds` on Windows). `hpds config` prints its exact path.

Values are resolved by layering, lowest priority first:

```
built-in defaults  ←  user config  ←  project config  ←  CLI flags
```

Each layer overrides only the keys it sets; everything else falls through from the
layer beneath. Layering is **key-by-key**, not table-by-table: setting
`[audit] stale-days` in project config does not discard `required-watchers` from a
lower layer.

**Unknown keys warn, they do not error.** A key `hpds` does not recognize is ignored
with a warning (forward compatibility), so a newer `hpds.toml` still loads on an older
binary. A *wrong type* for a known key (for example `status = 3`) is a real error.

## Complete annotated example

Every key, set to a representative value. This whole file parses; it is the example
the reference test loads.

```toml
[project]
# Lifecycle metadata read by `hpds audit`.
status = "active"                # active | submitted | published | retired
primary-author = "malcolmbarrett"  # GitHub login; audit checks they watch the repo

[audit]
stale-days = 90                  # branches idle longer than this count as stale
required-watchers = ["malcolmbarrett", "sherrirose"]  # user config only (see below)
```

A realistic minimal file usually sets only lifecycle metadata:

```toml
[project]
status = "submitted"
primary-author = "your-github-login"
```

---

## `[project]`

Lifecycle metadata consumed by `hpds audit`.

| Key | Type | Default | Description |
|---|---|---|---|
| `status` | string | `"active"` | Project lifecycle: one of `active`, `submitted`, `published`, `retired`. Audit checks use it (e.g. a `submitted`/`published` repo is expected to have a release; a `retired` repo is expected to be archived). |
| `primary-author` | string | `""` | GitHub login of the project's primary author. The `watchers` audit check verifies this account watches the repo, and `contributors` verifies they appear in the contributor list. |

## `[audit]`

Knobs for `hpds audit`.

| Key | Type | Default | Description |
|---|---|---|---|
| `stale-days` | integer ≥ 0 | `90` | A branch (or remote branch) with no commits in more than this many days is reported stale. |
| `required-watchers` | array of strings | `["malcolmbarrett", "sherrirose"]` | GitHub logins that must watch every lab repo, in addition to the project's `primary-author`. **User config only:** an audited repo cannot rewrite the required-watcher list for everyone who audits it, so this key is honored only from your user config; a value in a project `hpds.toml` is ignored with a warning. |
