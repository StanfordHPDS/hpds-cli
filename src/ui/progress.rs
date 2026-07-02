//! Progress bar wrapper around `indicatif` (spec §2, M0.3).
//!
//! Bars draw to stderr so they never pollute piped stdout, and hide
//! themselves entirely when stderr is not a TTY.

use std::io::IsTerminal;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Create a progress bar of `len` steps labeled with `msg`.
///
/// Hidden (no output at all) when stderr is not a TTY, so logs and CI runs
/// stay clean.
#[allow(dead_code)] // not yet consumed; ui lands before its callers (M0.3)
pub fn progress_bar(len: u64, msg: impl Into<String>) -> ProgressBar {
    let target = if std::io::stderr().is_terminal() {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    };
    new_progress_bar(len, msg.into(), target)
}

/// Target-independent core, factored out so unit tests are deterministic.
fn new_progress_bar(len: u64, msg: String, target: ProgressDrawTarget) -> ProgressBar {
    let style = ProgressStyle::with_template("{msg} [{bar:30}] {pos}/{len}")
        // Static template; a failure here is a programming error, not user input.
        .expect("progress bar template is valid")
        .progress_chars("=> ");
    ProgressBar::with_draw_target(Some(len), target)
        .with_style(style)
        .with_message(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::InMemoryTerm;

    #[test]
    fn non_tty_progress_bar_is_hidden() {
        let pb = new_progress_bar(10, "downloading ruff".into(), ProgressDrawTarget::hidden());
        assert!(pb.is_hidden());
    }

    #[test]
    fn terminal_progress_bar_is_visible_and_draws_progress() {
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let pb = new_progress_bar(10, "downloading ruff".into(), target);
        assert!(!pb.is_hidden());
        pb.inc(3);
        let drawn = term.contents();
        assert!(drawn.contains("downloading ruff"), "drawn was: {drawn}");
        assert!(drawn.contains("3/10"), "drawn was: {drawn}");
    }

    #[test]
    fn progress_bar_carries_len_and_message() {
        let pb = new_progress_bar(
            42,
            "verifying checksums".into(),
            ProgressDrawTarget::hidden(),
        );
        assert_eq!(pb.length(), Some(42));
        assert_eq!(pb.message(), "verifying checksums");
    }
}
