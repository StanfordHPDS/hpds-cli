# Contributing to hpds-cli

This is the developer-facing guide: how the code is extended and how to get a change
merged. For user-facing installation and usage, see the [README](README.md). For the
`hpds.toml` keys, see [docs/hpds.toml.md](docs/hpds.toml.md).

`hpds` is deliberately extensible at the source level (no plugin system): adding a
language, a managed tool, or an audit check is a small, mechanical change to a handful
of well-known files. The three recipes below name those files and their real symbols.

## Development workflow

- **Test-first (TDD), always.** Write the failing test, watch it fail for the right
  reason, then write the minimal code to pass. Every acceptance criterion maps to at
  least one test. Unit tests are colocated with the code they cover; CLI behavior is
  tested with `assert_cmd` against fixtures in `tests/fixtures/`.
- **External tool output is tested against recorded output**, not a live tool. Record
  the real output once and check it into `tests/fixtures/tool-output/`; parse tests run
  against that. Tests that hit the network or download real tools go behind the
  `online-tests` feature.
- **All terminal output goes through `src/`'s `ui` module** — no stray `println!`.
  Every user-facing error says what to do next. Never panic on user input, and prefer
  `Path`/`PathBuf` over hardcoded separators so the code stays cross-platform.
- **Issue tracking is `br` (beads).** `br ready` lists unblocked work, `br show <id>`
  has the acceptance criteria, `br update <id> --status in_progress` when you start,
  and `br close <id>` only once every gate below is green.

### Quality gates

All four must pass before you commit. Run them from the repo root:

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo fmt --check
```

Do not weaken or delete tests to get green, and do not reach for `#[allow]` on a
clippy lint without a genuine reason written in a comment beside it.

## Recipe: adding a language

Adding a language wires a file type through to an underlying formatter/linter. Nothing
in the command plumbing changes. Work through these files:

1. **Tool spec** — teach `hpds` how to install the underlying tool. Add a default
   version constant to `src/tools/versions.rs`, then add a `ToolSpec` to
   `ToolSpec::builtins()` in `src/tools/spec.rs`. Pick the `ToolKind`: a prebuilt
   release binary is `ToolKind::GithubBinary { repo, asset_pattern, checksum_pattern }`
   (as air, ruff, and panache use); a Python tool is `ToolKind::UvTool { package }`
   (as sqlfluff uses).

2. **Language bucket** — in `src/fsx/registry.rs`, add a variant to the `Language`
   enum, register the file extensions for it in `ExtensionRegistry::with_defaults`, and
   map its `[format]`/`[lint]` config name in `Language::from_config_name`.

3. **Adapter** — create `src/adapters/<lang>.rs` implementing the `Formatter` and
   `Linter` traits (and `Adapter`, which just names the tool). Model it on the ruff
   adapter in `src/adapters/python.rs`: shell out to the managed binary via
   `ToolCtx::tool_path`, and parse the tool's output into `hpds`'s own `Diagnostic` and
   `FormatOutcome` types — that normalization is what makes tools swappable. Export the
   new adapter from `src/adapters/mod.rs`.

4. **Registry line** — wire the bucket to the adapter with one line in
   `AdapterRegistry::with_defaults` in `src/adapters/registry.rs`. Several buckets may
   share one adapter instance (Quarto and Markdown both route to panache).

5. **Recorded fixtures** — capture the real tool's output once and check it into
   `tests/fixtures/tool-output/`, then test your parser against it so the adapter has
   coverage without invoking the tool during the offline test run.

## Recipe: adding a managed tool

A "managed tool" is anything `hpds` downloads and runs on the user's behalf (a
formatter, `uv`, and so on) — the same machinery, without necessarily wiring it to a
language. Add a default version constant to `src/tools/versions.rs` and a `ToolSpec`
with the appropriate `ToolKind` to `ToolSpec::builtins()` in `src/tools/spec.rs`. The
cache, download, checksum, and locking layers pick it up automatically; nothing else
needs to change for `hpds tools list`/`update`/`clean` to see it.

## Recipe: adding an audit check

Each audit check is a `Check` (defined in `src/audit/mod.rs`) — a pure inspector that
returns `Finding`s and never prints or mutates the repo.

1. Create a new file under `src/audit/checks/` (model it on `src/audit/checks/junk.rs`)
   with a struct implementing `Check`: `id` returns the stable check id used in
   findings and bot fingerprints; `run` inspects the repo and returns zero or more
   `Finding`s; override `needs_repo` to `false` if the check does not touch git.
2. Build each `Finding` with a `check_id`, a `severity` (`Error`, `Warn`, or `Info`), a
   one-line `message`, and a one-line `remediation` that says what to do next.
3. Register the check in the `all()` list in `src/audit/checks/mod.rs` — that ordered
   registry is the single place checks are wired in. Add a unit test in your new file
   that builds a throwaway repo (see the `testutil` helpers in
   `src/audit/checks/mod.rs`) and asserts the finding fires and stays quiet when it
   should.

GitHub-backed checks follow the same shape but live under `src/audit/github/` and only
run when `gh` is authenticated.

## A note on comments

Comments describe the code as it is. Do not reference the design spec, milestone or
issue ids, or the build process in source comments — those are ephemeral.
