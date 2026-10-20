//! Progress bar wrapper around `indicatif`.
//!
//! Bars draw to stderr so they never pollute piped stdout. A bar is hidden
//! when stderr is not a TTY, where it cannot render, and on a thread that
//! is capturing its output, where it would draw over the foreground's
//! lines. A hidden bar stands in with its label as one plain notice line
//! (recorded in the transcript while capturing): long work is never
//! silent.

use std::io::IsTerminal;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::capture::{Stream, emit, is_capturing};

/// Create a progress bar of `len` steps labeled with `msg`.
///
/// On a TTY the bar (label included) is on screen immediately, before any
/// progress is reported. On a non-TTY stderr the bar is hidden and `msg`
/// is printed once as a plain line (suppressed by `--quiet`), so logs and
/// CI runs still say what is happening.
///
/// While the calling thread is inside [`fn@super::capture`] the bar is always
/// hidden, since a live bar would draw over the foreground's output, and the
/// notice line is recorded in the transcript instead.
pub fn progress_bar(len: u64, msg: impl Into<String>) -> ProgressBar {
    progress_bar_for(
        len,
        msg.into(),
        std::io::stderr().is_terminal(),
        is_capturing(),
        super::is_quiet(),
    )
}

/// Whether a bar created on the calling thread right now would draw live.
pub(crate) fn bar_is_live() -> bool {
    draw_target(std::io::stderr().is_terminal(), is_capturing()) == BarTarget::Stderr
}

/// Where a new bar draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BarTarget {
    /// Live on stderr.
    Stderr,
    /// Not drawn; a notice line stands in for it.
    Hidden,
}

/// Pure draw decision: a live bar needs a terminal and a thread that is not
/// capturing its output.
fn draw_target(is_tty: bool, capturing: bool) -> BarTarget {
    if is_tty && !capturing {
        BarTarget::Stderr
    } else {
        BarTarget::Hidden
    }
}

fn progress_bar_for(
    len: u64,
    msg: String,
    is_tty: bool,
    capturing: bool,
    quiet: bool,
) -> ProgressBar {
    match draw_target(is_tty, capturing) {
        BarTarget::Stderr => new_progress_bar(len, msg, ProgressDrawTarget::stderr()),
        BarTarget::Hidden => {
            if let Some(notice) = non_tty_notice(&msg, quiet) {
                emit(Stream::Stderr, &notice);
            }
            new_progress_bar(len, msg, ProgressDrawTarget::hidden())
        }
    }
}

/// Target-independent core, factored out so unit tests are deterministic.
/// The immediate `tick` puts the label on screen at creation: a bar that
/// only rendered on its first `inc` would show nothing at all for work
/// that reports no increments (e.g. waiting on a subprocess).
fn new_progress_bar(len: u64, msg: String, target: ProgressDrawTarget) -> ProgressBar {
    let style = ProgressStyle::with_template("{msg} [{bar:30}] {pos}/{len}")
        // Static template; a failure here is a programming error, not user input.
        .expect("progress bar template is valid")
        .progress_chars("=> ");
    let bar = ProgressBar::with_draw_target(Some(len), target)
        .with_style(style)
        .with_message(msg);
    bar.tick();
    bar
}

/// The one-line stand-in for a bar on non-TTY stderr; `None` under
/// `--quiet`.
fn non_tty_notice(msg: &str, quiet: bool) -> Option<String> {
    (!quiet).then(|| msg.to_string())
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
    fn terminal_progress_bar_draws_its_label_before_any_progress() {
        // A bar that only renders on the first `inc` shows nothing at all
        // for work that reports no increments (or none yet): the label
        // must be on screen from the moment the bar exists.
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let _pb = new_progress_bar(1, "Fetching R formatter…".into(), target);
        let drawn = term.contents();
        assert!(
            drawn.contains("Fetching R formatter…"),
            "label must render immediately, drawn was: {drawn:?}"
        );
    }

    #[test]
    fn non_tty_fallback_is_a_single_notice_line() {
        assert_eq!(
            non_tty_notice("Fetching R formatter…", false).as_deref(),
            Some("Fetching R formatter…")
        );
    }

    #[test]
    fn non_tty_fallback_is_silent_under_quiet() {
        assert_eq!(non_tty_notice("Fetching R formatter…", true), None);
    }

    #[test]
    fn bar_draws_live_only_on_a_tty_outside_capture() {
        assert_eq!(draw_target(true, false), BarTarget::Stderr);
        assert_eq!(draw_target(false, false), BarTarget::Hidden);
        assert_eq!(draw_target(true, true), BarTarget::Hidden);
        assert_eq!(draw_target(false, true), BarTarget::Hidden);
    }

    #[test]
    fn captured_bar_is_hidden_and_records_its_notice() {
        let (bar, transcript) = crate::ui::capture(|| {
            progress_bar_for(10, "downloading quarto".into(), true, true, false)
        });
        assert!(bar.is_hidden());
        assert_eq!(
            transcript.lines(),
            [(Stream::Stderr, "downloading quarto".to_string())]
        );
    }

    #[test]
    fn captured_bar_notice_is_suppressed_under_quiet() {
        let (bar, transcript) = crate::ui::capture(|| {
            progress_bar_for(10, "downloading quarto".into(), false, true, true)
        });
        assert!(bar.is_hidden());
        assert!(transcript.is_empty());
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
