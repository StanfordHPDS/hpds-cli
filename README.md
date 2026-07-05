# hpds-cli

[![CI](https://github.com/StanfordHPDS/hpds-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/StanfordHPDS/hpds-cli/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/StanfordHPDS/hpds-cli?label=release)](https://github.com/StanfordHPDS/hpds-cli/releases/latest)

`hpds` is the command-line tool for the Stanford Health Policy Data Science lab.
It is a single binary for macOS, Linux, and Windows that does three jobs:

1. **Scaffold** projects from lab templates (`hpds init`, `hpds use ...`).
2. **Set up machines** with the lab toolchain (`hpds install ...`, `hpds setup`).
3. **Audit repos** against lab standards, locally and across the GitHub org
   (`hpds audit`, `hpds audit all`).

Formatting and linting are provided by the lab's separate
[togi](https://github.com/StanfordHPDS/togi) tool.

Everything works with zero configuration; `hpds.toml` only overrides defaults. The
defaults encode the lab's agreements — snake_case scaffolds, private-first repos in
the `StanfordHPDS` org.

## Install

The install script downloads a prebuilt binary for your platform and places it on
your `PATH`:

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

`hpds init` walks you through a new or existing project interactively. For scripts
and CI, drive it non-interactively with flags:

```console
$ hpds init
$ hpds init --yes --language both --use pipeline,readme
```

Add individual components to a project that already exists. `hpds use` with no
argument lists what's available:

```console
$ hpds use
$ hpds use readme
$ hpds use pipeline --kind targets
```

### Set up a machine

Install a single tool (idempotent — already-installed tools are a no-op):

```console
$ hpds install quarto
✓ quarto 1.8.27 already installed
```

`hpds setup` runs the whole toolchain bundle. Preview the plan before it runs:

```console
$ hpds setup --plan
$ hpds setup --profile dev
```

The `server` profile provisions a full lab server (Linux only); `dev` (the default)
installs the toolchain on your own machine.

### Audit a repo

Audit the current repo against lab standards, or emit JSON for the bot:

```console
$ hpds audit
errors:
  ✗ [lifecycle-metadata] the repo has no hpds.toml
    fix: create hpds.toml with a [project] table setting `status` and `primary-author`
warnings:
  ! [readme] `README.md` is missing the lab-manual sections: Description, File structure
    fix: add the missing `## <section>` headings (`hpds use readme` generates the full structure)

$ hpds audit --format json
```

Sweep every repo in the org into one report:

```console
$ hpds audit all --limit 50
```

### Git & GitHub helpers

Apply the lab's global git ignore patterns, configure sensible git defaults, and
create a repo the lab-manual way:

```console
$ hpds git vaccinate
✓ added 21 ignore pattern(s) to ~/.gitignore

$ hpds git setup
$ hpds repo create --org StanfordHPDS
```

## Documentation

- [docs/audit-bot.md](docs/audit-bot.md) — how the audit bot files issues and
  comments on pull requests.

## Development

Requires a stable Rust toolchain (Rust 2024 edition; `rust-toolchain.toml` pins the
channel and components). The four quality gates must pass before every commit:

```sh
cargo build
cargo test                              # offline tests
cargo test --features online-tests      # network/tool-download tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests are test-first: write the failing test, watch it fail, then implement.
Integration tests drive the real binary against fixtures in `tests/fixtures/`.

## License

MIT — see [LICENSE](LICENSE).
