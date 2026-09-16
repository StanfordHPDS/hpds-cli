# `hpds.toml` configuration reference

`hpds` requires no configuration.
Every key below is optional and has a built-in default; a configuration file only *overrides* those defaults.
Run `hpds config` at any time to print the fully resolved configuration and the path of each file that contributed to it.

Formatter and linter configuration belongs to the lab's separate togi tool and is not covered here.

## Where config lives and how it layers

`hpds` reads configuration from two files:

- **Project config**: `hpds.toml`, discovered by walking up from the current directory to the git root (or the filesystem root).
- **User config**: `config.toml` in your platform config directory (`~/.config/hpds/config.toml` on Linux, `~/Library/Application Support/hpds` on macOS, `%APPDATA%\hpds` on Windows).
  `hpds config` prints its exact path.

Values are resolved by layering, lowest priority first:

```
built-in defaults  ←  user config  ←  project config  ←  CLI flags
```

Each layer overrides only the keys it sets; everything else falls through from the layer beneath.
Layering is applied **key by key**, not table by table: setting `[audit] stale-days` in project configuration does not discard `required-watchers` from a lower layer.

One key is additive rather than overriding: `[audit] required-watchers` in project configuration adds names to the list from the lower layers and never replaces or removes them (see [`[audit]`](#audit)).
A file passed with the global `--config <path>` flag takes the place of the discovered `hpds.toml`, so it is layered as project configuration.

**Unknown keys produce a warning, not an error.** A key `hpds` does not recognize is ignored with a warning, for forward compatibility, so a newer `hpds.toml` still loads on an older binary.
A *wrong type* for a known key (for example `status = 3`) is an error.

## Complete annotated example

Every key is shown below with a representative value.
The example parses as a complete configuration file and is verified against the binary by the test suite.

```toml
[project]
# Lifecycle metadata read by `hpds audit`.
status = "active"                # active | submitted | published | retired
primary-author = "malcolmbarrett"  # GitHub login; the audit verifies watching

[audit]
stale-days = 90                  # branches idle longer than this are stale
required-watchers = ["malcolmbarrett", "sherrirose"]  # project config adds to this list
```

A minimal file typically sets only the lifecycle metadata:

```toml
[project]
status = "submitted"
primary-author = "your-github-login"
```

--------------------------------------------------------------------------------

## `[project]`

Lifecycle metadata consumed by `hpds audit`.

  | Key              | Type   | Default    | Description                                                                                                                                                                                                                                                                                                                                                                                                               |
  | ---------------- | ------ | ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | `status`         | string | `"active"` | Project lifecycle: one of `active`, `submitted`, `published`, `retired`. Audit checks use it (for example, a `submitted` or `published` repository is expected to have a release, and a `retired` repository is expected to be archived).                                                                                                                                                                                 |
  | `primary-author` | string | `""`       | GitHub login of the project's primary author. The `watchers` audit check verifies that this account watches the repository, and `contributors` verifies that it appears in the contributor list. One leading `@` is accepted and removed. A value that is not a valid login (letters, digits, and hyphens only) is replaced by an empty value with a warning, so no primary author is set even if a lower layer sets one. |

## `[audit]`

Settings for `hpds audit`.

  | Key                 | Type             | Default                            | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
  | ------------------- | ---------------- | ---------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | `stale-days`        | integer ≥ 0      | `90`                               | A branch (or remote branch) with no commits in more than this many days is reported as stale.                                                                                                                                                                                                                                                                                                                                                                                                                                     |
  | `required-watchers` | array of strings | `["malcolmbarrett", "sherrirose"]` | GitHub logins that must watch every lab repository, in addition to the project's `primary-author`. User configuration replaces the default list. **Project configuration adds to the list and cannot remove names:** the effective list is the user (or default) entries followed by project entries not already present, compared case-insensitively. List each login as its own string; one leading `@` is accepted and removed, and an entry that is not a valid login (for example `"alice, bob"`) is dropped with a warning. |
