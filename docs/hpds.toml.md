# `hpds.toml` configuration reference

`hpds` works with **zero configuration**. Every key below is optional and has a
built-in default; a config file only *overrides* those defaults. Run `hpds config`
at any time to print the fully resolved configuration and the path of each file that
contributed to it.

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
`[format] exclude` in project config does not discard `[format] languages` from a
lower layer. For `[tools]`, pins and args merge per tool name; a higher layer's pin
for `air` replaces a lower one but leaves other tools' pins intact.

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

[format]
# Languages to include when formatting; default = all detected.
languages = ["r", "python", "quarto", "sql", "markdown"]
exclude = ["renv/**", "vendor/**"]  # gitignore-style globs, additive to .gitignore

[lint]
languages = ["r", "python", "quarto", "sql"]
exclude = []

[sql]
dialect = "bigquery"             # passed to sqlfluff when the project sets no dialect

[audit]
stale-days = 90                  # branches idle longer than this count as stale
required-watchers = ["malcolmbarrett", "sherrirose"]  # user config only (see below)

# Version pins for managed tools; omit to use the versions baked into this hpds
# release. A bare `name = "x.y.z"` is a pin.
[tools]
ruff = "0.14.0"

# A tool needing both a pin and passthrough args uses a `[tools.<name>]` table
# instead (TOML forbids `air = "..."` and `[tools.air]` together).
[tools.air]
version = "0.10.0"
args = ["--fast"]
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

## `[format]` and `[lint]`

Which languages `hpds format` / `hpds lint` operate on, and which paths to skip. The
two tables are independent so, for example, plain Markdown can be formatted but not
linted.

| Key | Type | Default (`[format]`) | Default (`[lint]`) | Description |
|---|---|---|---|---|
| `languages` | array of strings | `["r", "python", "quarto", "sql", "markdown"]` | `["r", "python", "quarto", "sql"]` | Language buckets to include. Recognized names: `r`, `python`, `quarto`, `markdown`, `sql` (case-insensitive). Unrecognized names warn and are skipped. |
| `exclude` | array of strings | `[]` | `[]` | gitignore-style glob patterns, **additive** to the repo's `.gitignore`. Matching files are never formatted or linted. |

## `[sql]`

| Key | Type | Default | Description |
|---|---|---|---|
| `dialect` | string | `"bigquery"` | SQL dialect passed to sqlfluff. Only applied when the project has not configured sqlfluff itself (a `.sqlfluff` file wins). |

## `[audit]`

Knobs for `hpds audit`.

| Key | Type | Default | Description |
|---|---|---|---|
| `stale-days` | integer ≥ 0 | `90` | A branch (or remote branch) with no commits in more than this many days is reported stale. |
| `required-watchers` | array of strings | `["malcolmbarrett", "sherrirose"]` | GitHub logins that must watch every lab repo, in addition to the project's `primary-author`. **User config only:** an audited repo cannot rewrite the required-watcher list for everyone who audits it, so this key is honored only from your user config; a value in a project `hpds.toml` is ignored with a warning. |

## `[tools]` and `[tools.<name>]`

Version pins and passthrough arguments for the managed formatter/linter tools
(`air`, `ruff`, `panache`, `sqlfluff`; `uv` bootstraps sqlfluff). Omit everything here
to use the versions baked into this `hpds` release.

There are two shapes:

- **A bare pin** under `[tools]`: `ruff = "0.14.0"`. The value is the exact version
  `hpds` installs and runs.
- **A `[tools.<name>]` table** when a tool needs passthrough args (and optionally a
  pin). Use this form instead of a bare pin whenever you also set `args`, because TOML
  forbids a `name = "..."` key and a `[tools.name]` table for the same name.

| Key | Type | Default | Description |
|---|---|---|---|
| `<name>` (under `[tools]`) | string | release default | Version pin for the tool `<name>`, e.g. `air = "0.10.0"`. |
| `version` (under `[tools.<name>]`) | string | release default | Version pin, equivalent to the bare-pin form; used when the same table also sets `args`. |
| `args` (under `[tools.<name>]`) | array of strings | `[]` | Extra arguments appended to every invocation of the tool — the escape hatch for options `hpds` does not expose directly. |

```toml
# Pass a dialect to sqlfluff without pinning its version.
[tools.sqlfluff]
args = ["--dialect", "duckdb"]
```
