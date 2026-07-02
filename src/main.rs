//! `hpds` — unified tooling for the Stanford HPDS lab.
//!
//! Thin entry point: parse the CLI, dispatch, render top-level errors.

mod cli;
mod ui;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => render_error(&err),
    }
}

/// Render a top-level error and pick the exit code (spec §2: 1 = failure,
/// 2 = usage error / not-yet-implemented stub).
///
/// TODO(M0.3): this is the one allowed print site outside `src/ui/`; once the
/// `ui` module lands (bd-2di.3), route rendering through it with a styled
/// prefix, cause chain, and hint line.
fn render_error(err: &anyhow::Error) -> ExitCode {
    if let Some(nyi) = err.downcast_ref::<cli::NotYetImplemented>() {
        eprintln!("error: {nyi}");
        eprintln!("hint: {}", nyi.hint());
        return ExitCode::from(2);
    }
    eprintln!("error: {err:#}");
    ExitCode::FAILURE
}
