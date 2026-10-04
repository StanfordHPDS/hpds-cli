# hpds-cli

[![CI](https://github.com/StanfordHPDS/hpds-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/StanfordHPDS/hpds-cli/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/StanfordHPDS/hpds-cli?label=release)](https://github.com/StanfordHPDS/hpds-cli/releases/latest)

`hpds` is the command-line tool for the Stanford Health Policy Data Science lab.
It is a single binary for macOS, Linux, and Windows that does three jobs:

1. **Scaffold** projects from lab templates (`hpds init`, `hpds use ...`).
2. **Set up machines** with the lab toolchain (`hpds install ...`, `hpds setup`).
3. **Audit repos** against lab standards, locally and across the GitHub org (`hpds audit`, `hpds audit all`).

Formatting and linting are provided by the lab's separate [togi](https://github.com/StanfordHPDS/togi) tool.

Everything works with zero configuration; `hpds.toml` only overrides defaults.
The defaults encode several of the lab's agreements.

## Install

The install script downloads a prebuilt binary for your platform and places it on your `PATH`:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/StanfordHPDS/hpds-cli/releases/latest/download/hpds-installer.sh | sh
```

On Windows, use the PowerShell installer:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/StanfordHPDS/hpds-cli/releases/latest/download/hpds-installer.ps1 | iex"
```

With Homebrew:

```sh
brew install StanfordHPDS/tap/hpds
```

From source with Cargo (needs a stable Rust toolchain):

```sh
cargo install --git https://github.com/StanfordHPDS/hpds-cli hpds
```

Confirm the install:

```console
$ hpds version
hpds 0.1.0   # no-verify
```

## Quickstart

### Scaffold a project

`hpds init` walks you through a new or existing project interactively.
For scripts and CI, drive it non-interactively with flags:

```console
$ hpds init
$ hpds init --yes --language both --use pipeline,readme
```

Add individual components to a project that already exists.
`hpds use` with no argument lists what's available:

```console
$ hpds use
$ hpds use hpds.toml
$ hpds use readme
$ hpds use pipeline --kind targets
```

Generate Docker, Apptainer, or both container definitions with the project
runtime metadata available in the current directory:

```console
$ hpds use container --kind docker --language r
$ hpds use container --kind both --language both --r-version 4.4.3
```

For R projects, `--r-version` takes precedence over `R.Version` in
`renv.lock`. When neither is available, hpds resolves the current R release.
For Python projects, an existing `.python-version` is copied before the first
locked `uv sync`; projects without that optional file continue to use their
`pyproject.toml` and `uv.lock` metadata. Regenerate the container files if you
add or remove `.python-version` later. Docker generation also creates a
`.dockerignore` that excludes host environments and caches while retaining
project source, lockfiles, `.Rprofile`, and vendored files under `renv/`.
During a container build, the generated files download the matching hpds
release for Linux amd64 or arm64 and verify its published SHA256 checksum
before installing it.

Formatting and linting the code you write there is [togi](https://github.com/StanfordHPDS/togi)'s job (`togi format`, `togi lint`); install it with `hpds install togi`.

### Slides, posters, and dissertations

Fetch the lab's document templates into the current directory:

```console
$ hpds use slides
$ hpds use poster
$ hpds use thesis
```

Each command creates a subdirectory named after its template repository.
Use `thesis` for the Stanford dissertation and thesis template.

| Component | Template repository | Destination subdirectory |
| --- | --- | --- |
| `slides` | [StanfordHPDS/hpds-slides-theme](https://github.com/StanfordHPDS/hpds-slides-theme) | `hpds-slides-theme/` |
| `poster` | [StanfordHPDS/hpds-poster](https://github.com/StanfordHPDS/hpds-poster) | `hpds-poster/` |
| `thesis` | [StanfordHPDS/typst-stanford-thesis](https://github.com/StanfordHPDS/typst-stanford-thesis) | `typst-stanford-thesis/` |

All three commands require network access. For slides and posters, `hpds`
uses `quarto use template` when Quarto is on `PATH`; otherwise it clones
the repository. The thesis template is always cloned. Cloning uses the
GitHub CLI (`gh`) when available, or `git` otherwise, and requires Git
even when using `gh`.

Existing destination directories are refused, including empty directories
and when `--force` is given. Choose another working directory or move the
existing destination before fetching again.

### Set up a machine

Install a single tool:

```console
$ hpds install quarto
✓ quarto 1.8.27 already installed
```

`hpds setup` runs the whole toolchain bundle.
Preview the plan before it runs:

```console
$ hpds setup --plan
$ hpds setup --profile dev
```

The `server` profile provisions a full lab server (Linux only); `dev` (the default) installs the toolchain on your own machine.

### Audit a repo

Audit the current repo against lab standards, or emit JSON for the bot:

```console
$ hpds audit
errors:
  ✗ [lifecycle-metadata] the repo has no hpds.toml
    fix: run `hpds use hpds.toml`
  ✗ [readme] the repo has no README
    fix: add one, for example with `hpds use readme`

$ hpds audit --format json
```

Sweep every repo in the org into one report:

```console
$ hpds audit all --limit 50
```

### Git & GitHub helpers

Apply the lab's global git ignore patterns, configure sensible git defaults, and create a repo the lab-manual way:

```console
$ hpds git vaccinate
✓ added 21 ignore pattern(s) to ~/.gitignore

$ hpds git setup
$ hpds repo create
```

## Documentation

- [docs/audit-bot.md](docs/audit-bot.md): how the audit bot files issues and comments on pull requests.

## Development

Requires a stable Rust toolchain (Rust 2024 edition; `rust-toolchain.toml` pins the channel and components).
See [CONTRIBUTING.md](CONTRIBUTING.md) for the developer workflow. CI requires these gates on Linux, macOS, and Windows (the typos check runs on Linux only):

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
typos
```

CI also runs `cargo test --features online-tests` in a separate job that is allowed to fail; those tests exercise the network and real tool downloads and are not required.

Container runtime validation is explicit because it downloads base images and
package runtimes. Build hpds, start Docker, and run:

```sh
cargo build --locked
python3 tests/container-runtime/run.py
```

The harness exits with an error when Docker or its daemon is unavailable. It
builds and runs R, Python, and mixed fixtures, then removes the temporary image
tags it created. Each fixture also checks the installed hpds release. On Linux
with Apptainer installed, pass `--engine all` to validate both generated
formats, as the native Linux CI job does. Where unprivileged Apptainer builds
are unavailable, add `--apptainer-build-as-root`; runtime checks still execute
as the calling user with a fresh home directory.

## License

MIT.
See [LICENSE](LICENSE).
