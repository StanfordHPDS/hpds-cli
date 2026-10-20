# Contributing to hpds-cli

This is the developer-facing guide: how the code is extended and how to get a change merged.
For user-facing installation and usage, see the [README](README.md).
For the `hpds.toml` keys, see [docs/hpds.toml.md](docs/hpds.toml.md).

`hpds` is deliberately extensible at the source level (no plugin system): adding a managed tool or an audit check is a small, mechanical change to a handful of well-known files.
The recipes below name those files and their real symbols.
Formatting and linting live in the lab's separate togi tool; contributions to the format/lint pipeline belong there.

## Development workflow

- **Add tests with your change.** Unit tests live beside the code they cover; CLI behavior is tested with `assert_cmd` against fixtures in `tests/fixtures/`.
- **External tool output is tested against recorded output**, not a live tool.
  Record the real output once and check it into `tests/fixtures/tool-output/`; parse tests run against that.
  Tests that hit the network or download real tools go behind the `online-tests` feature; they are optional and not required to pass locally (see Quality gates below).
- **All terminal output goes through `src/`'s `ui` module**: no stray `println!`.
  Every user-facing error says what to do next.
  Never panic on user input, and prefer `Path`/`PathBuf` over hardcoded separators so the code stays cross-platform.
- Issues and planned work are tracked on [GitHub Issues](https://github.com/StanfordHPDS/hpds-cli/issues).

### Quality gates

These are the gates CI requires on every pull request, run on Linux, macOS, and Windows.
Run them from the repo root before you commit:

```
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
typos
```

The `typos` spell check runs on Linux only in CI.
Install it with `cargo install typos-cli` or `brew install typos-cli`.
If `cargo fmt --check` fails, run `cargo fmt` to fix formatting and re-check.

CI also runs `cargo test --features online-tests` in a separate job that is allowed to fail; those tests exercise the network and real tool downloads, so they are not required and you do not need to run them locally.

Do not reach for `#[allow]` on a clippy lint without a genuine reason written in a comment beside it.

### Server setup in a container

The unit and integration tests drive `hpds setup --profile server` against a fake host, so they cannot show that the real installers work on a real machine.
The `server setup (Linux container)` workflow, `.github/workflows/server-setup.yml`, fills that gap.
It builds a release binary and runs `tests/server-setup/run.py`, which starts a fresh Ubuntu 24.04 container with systemd as PID 1 (`tests/server-setup/Dockerfile`), runs `hpds setup --profile server --yes` as an ordinary sudo user with the environment an ssh login gives, and then checks:

- the setup summary marks every step from `hpds setup --plan --profile server` with ✓, so a new step is covered without editing the harness;
- each component works as that user: R and its package repository, Python and pip, Quarto, TinyTeX, RStudio Server and code-server (version, systemd service, and an HTTP probe), gh, DuckDB, uv, ruff, sqlfluff, rig, Rust, the Docker CLI and group membership, and the git defaults, including the warnings for a missing git identity;
- a second `hpds setup --profile server --yes` also succeeds on every step.

The harness prints a pass/fail table and removes its container and image when it finishes, fails, or receives SIGINT or SIGTERM; only a SIGKILL leaves them behind, in which case remove the `hpds-server-setup-*` container with `docker rm --force --volumes` and the `hpds-server-setup:local` image with `docker image rm`.
The job runs on pull requests and pushes to `main` that touch the code the setup path runs (`src/setup/`, `src/install/`, `src/tools/`, `src/gitx/`, `src/ui/`, `src/cli/setup.rs`, `src/cli/git.rs`, `src/cli/mod.rs`, `src/main.rs`), `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, the harness, or the workflow; run it by hand from the Actions tab with **Run workflow**.
A full run downloads every component and can take tens of minutes.
The harness allows the first setup run 50 minutes and the second 20 minutes (`--setup-timeout` and `--rerun-timeout`, in seconds), inside the job's 90-minute limit.

To run it locally you need an x86_64 Linux host with Docker: the server profile is x86_64 only, and its systemd services do not start under emulation on Apple Silicon.

```
cargo build --release --locked
python3 -m unittest discover -s tests/server-setup -p 'test_*.py'
python3 tests/server-setup/run.py --hpds target/release/hpds
```

On a host that cannot run the full setup, `--platform linux/amd64 --stop-after-plan` with a Linux x86_64 binary checks the image, the systemd boot, and the plan parsing only.

## Recipe: adding a managed tool

A "managed tool" is anything `hpds install` can put on a machine (`uv`, `gh`, `duckdb`, and so on).
Work through these files:

1. **Installer**: create `src/install/installers/<tool>.rs` with a struct implementing the `Installer` trait: `detect` probes for an existing install through the runner seam, `plan` names the exact steps a run would take, and `install` performs them.
   Model it on `src/install/installers/gh.rs`: pick a per-OS strategy (package manager when present, release binary otherwise) and never touch the network or the machine directly; go through the `CommandRunner` and `ReleaseFetcher` seams so every strategy is assertable offline.

2. **Release spec**: when the tool ships prebuilt GitHub release binaries, add a default version constant to `src/tools/versions.rs` and a `release_spec()` returning a `ToolSpec` (repo plus asset/checksum filename patterns) in your installer file. Togi is intentionally different: an unpinned install resolves its latest stable GitHub release at runtime.
   The shared downloader handles fetching, checksum verification, and atomic installs.

3. **Registry line**: register the installer in `INSTALLERS` (and its name in `KNOWN_TOOLS`) in `src/install/registry.rs`; that is the single place tools are wired in to `hpds install <tool>`.

4. **Recorded fixtures**: record the tool's real `--version` output once into `tests/fixtures/tool-output/version-probes/` and test detection against it, so the installer has coverage without invoking the tool during the offline run.

## Recipe: adding an audit check

Each audit check is a `Check` (defined in `src/audit/mod.rs`), a pure inspector that returns `Finding`s and never prints or mutates the repo.

1. Create a new file under `src/audit/checks/` (model it on `src/audit/checks/junk.rs`) with a struct implementing `Check`: `id` returns the stable check id used in findings and bot fingerprints; `run` inspects the repo and returns zero or more `Finding`s; override `needs_repo` to `false` if the check does not touch git.
2. Build each `Finding` with a `check_id`, a `severity` (`Error`, `Warn`, or `Info`), a one-line `message`, and a one-line `remediation` that says what to do next.
3. Register the check in the `all()` list in `src/audit/checks/mod.rs`; that ordered registry is the single place checks are wired in.
   Add a unit test in your new file that builds a throwaway repo (see the `testutil` helpers in `src/audit/checks/mod.rs`) and asserts the finding fires and stays quiet when it should.

GitHub-backed checks follow the same shape but live under `src/audit/github/` and only run when `gh` is authenticated.

## A note on comments

Comments explain the code as it is, and ideally why it works that way, not the history of how it got there.
Keep them true to the current code: when you change behavior, update or remove the comments that describe it rather than leaving them stale.
