# Contributing to hpds-cli

This is the developer-facing guide: how the code is extended and how to get a change merged.
For user-facing installation and usage, see the [README](README.md).
For the `hpds.toml` keys, see [docs/hpds.toml.md](docs/hpds.toml.md).

`hpds` is deliberately extensible at the source level (no plugin system): adding a managed tool or an audit check is a small, mechanical change to a handful of well-known files.
The recipes below name those files and their real symbols.
Formatting and linting live in the lab's separate togi tool; contributions to the format/lint pipeline belong there.

## Development workflow

- **Test-first (TDD), always.** Write the failing test, watch it fail for the right reason, then write the minimal code to pass.
  Every acceptance criterion maps to at least one test.
  Unit tests are colocated with the code they cover; CLI behavior is tested with `assert_cmd` against fixtures in `tests/fixtures/`.
- **External tool output is tested against recorded output**, not a live tool.
  Record the real output once and check it into `tests/fixtures/tool-output/`; parse tests run against that.
  Tests that hit the network or download real tools go behind the `online-tests` feature.
- **All terminal output goes through `src/`'s `ui` module** --- no stray `println!`.
  Every user-facing error says what to do next.
  Never panic on user input, and prefer `Path`/`PathBuf` over hardcoded separators so the code stays cross-platform.
- **Issue tracking is `br` (beads).** `br ready` lists unblocked work, `br show <id>` has the acceptance criteria, `br update <id> --status in_progress` when you start, and `br close <id>` only once every gate below is green.

### Quality gates

All four must pass before you commit.
Run them from the repo root:

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo fmt --check
```

Do not weaken or delete tests to get green, and do not reach for `#[allow]` on a clippy lint without a genuine reason written in a comment beside it.

## Recipe: adding a managed tool

A "managed tool" is anything `hpds install` can put on a machine (`uv`, `gh`, `duckdb`, and so on).
Work through these files:

1. **Installer** --- create `src/install/installers/<tool>.rs` with a struct implementing the `Installer` trait: `detect` probes for an existing install through the runner seam, `plan` names the exact steps a run would take, and `install` performs them.
   Model it on `src/install/installers/gh.rs`: pick a per-OS strategy (package manager when present, release binary otherwise) and never touch the network or the machine directly --- go through the `CommandRunner` and `ReleaseFetcher` seams so every strategy is assertable offline.

2. **Release spec** --- when the tool ships prebuilt GitHub release binaries, add a default version constant to `src/tools/versions.rs` and a `release_spec()` returning a `ToolSpec` (repo plus asset/checksum filename patterns) in your installer file.
   The shared downloader handles fetching, checksum verification, and atomic installs.

3. **Registry line** --- register the installer in `INSTALLERS` (and its name in `KNOWN_TOOLS`) in `src/install/registry.rs`; that is the single place tools are wired in to `hpds install <tool>`.

4. **Recorded fixtures** --- record the tool's real `--version` output once into `tests/fixtures/tool-output/version-probes/` and test detection against it, so the installer has coverage without invoking the tool during the offline run.

## Recipe: adding an audit check

Each audit check is a `Check` (defined in `src/audit/mod.rs`) --- a pure inspector that returns `Finding`s and never prints or mutates the repo.

1. Create a new file under `src/audit/checks/` (model it on `src/audit/checks/junk.rs`) with a struct implementing `Check`: `id` returns the stable check id used in findings and bot fingerprints; `run` inspects the repo and returns zero or more `Finding`s; override `needs_repo` to `false` if the check does not touch git.
2. Build each `Finding` with a `check_id`, a `severity` (`Error`, `Warn`, or `Info`), a one-line `message`, and a one-line `remediation` that says what to do next.
3. Register the check in the `all()` list in `src/audit/checks/mod.rs` --- that ordered registry is the single place checks are wired in.
   Add a unit test in your new file that builds a throwaway repo (see the `testutil` helpers in `src/audit/checks/mod.rs`) and asserts the finding fires and stays quiet when it should.

GitHub-backed checks follow the same shape but live under `src/audit/github/` and only run when `gh` is authenticated.

## A note on comments

Comments describe the code as it is.
Do not reference the design spec, milestone or issue ids, or the build process in source comments --- those are ephemeral.
