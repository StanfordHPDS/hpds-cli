//! `hpds` — unified tooling for the Stanford HPDS lab.
//!
//! Thin entry point: parse the CLI, dispatch, render top-level errors.

mod adapters;
mod audit;
mod cli;
mod config;
mod fsx;
mod gitx;
mod install;
mod setup;
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
    let usage_hint = err
        .downcast_ref::<cli::NotYetImplemented>()
        .map(|nyi| nyi.hint())
        .or_else(|| err.downcast_ref::<cli::UsageError>().map(|u| u.hint()))
        .or_else(|| {
            err.downcast_ref::<install::registry::RegistryError>()
                .map(|e| e.hint())
        })
        .or_else(|| {
            // A bad `--config` value is a usage error like any other bad
            // flag value.
            err.downcast_ref::<config::MissingConfigFile>()
                .map(|e| e.hint())
        });
    match usage_hint {
        Some(hint) => {
            // Stubs and usage errors carry their hint on the type; attach it
            // so `ui::error` renders the standard `hint:` line.
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
