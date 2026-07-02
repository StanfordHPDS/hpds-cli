# hpds-cli

[![CI](https://github.com/StanfordHPDS/hpds-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/StanfordHPDS/hpds-cli/actions/workflows/ci.yml)

`hpds` is the unified command-line tool for the Stanford HPDS lab: one binary
for formatting and linting (R, Python, Quarto, SQL), project templates,
machine setup, and repo audits.

## Build

Requires a stable Rust toolchain (Rust 2024 edition; `rust-toolchain.toml`
pins the channel and components).

```sh
cargo build
cargo test
```

## License

MIT — see [LICENSE](LICENSE).
