//! `hpds` — unified tooling for the Stanford HPDS lab.
//!
//! Thin entry point: parse the CLI, dispatch, render top-level errors.

mod audit;
mod cli;
mod config;
mod fsx;
mod gitx;
mod templates;
mod tools;
mod ui;

use std::process::ExitCode;

use clap::Parser;

use ui::HintExt;

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => render_error(err),
    }
}

/// Render a top-level error through `ui` and pick the exit code (the design:
/// 1 = failure, 2 = usage error / not-yet-implemented stub).
fn render_error(err: anyhow::Error) -> ExitCode {
    match err
        .downcast_ref::<cli::NotYetImplemented>()
        .map(|nyi| nyi.hint())
    {
        Some(hint) => {
            // Stubs carry their hint on the type; attach it so `ui::error`
            // renders the standard `hint:` line.
            let hinted = Err::<(), _>(err).hint(hint).expect_err("just wrapped");
            ui::error(&hinted);
            ExitCode::from(2)
        }
        None => {
            ui::error(&err);
            ExitCode::FAILURE
        }
    }
}
