//! Progress bar wrapper around `indicatif`.
//!
//! Bars draw to stderr so they never pollute piped stdout. A bar is hidden
//! when stderr is not a TTY, where it cannot render, and on a thread that
//! is capturing its output, where it would draw over the foreground's
//! lines. A hidden bar stands in with its label as one plain notice line
//! (recorded in the transcript while capturing): long work is never
//! silent. In compact mode bars are hidden too, and the notice goes to the
//! log only. Under `--quiet` the notice is a log-only line as well.
//!
//! A [`Spinner`] marks one running step in compact mode. It draws only on a
//! TTY outside capture, and the prompt wrappers suspend it while they wait
//! for an answer.

use std::cell::{Cell, RefCell};
use std::io::IsTerminal;
use std::marker::PhantomData;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::capture::{LineKind, emit, is_capturing};
use super::sink::is_compact;

/// Create a progress bar of `len` steps labeled with `msg`.
///
/// On a TTY the bar (label included) is on screen immediately, before any
/// progress is reported. On a non-TTY stderr the bar is hidden and `msg`
/// is printed once as a plain line, so logs and CI runs still say what is
/// happening. Under `--quiet` that line goes to the log only.
///
/// While the calling thread is inside [`fn@super::capture`] the bar is always
/// hidden, since a live bar would draw over the foreground's output, and the
/// notice line is recorded in the transcript instead. In compact mode the
/// bar is hidden as well.
pub fn progress_bar(len: u64, msg: impl Into<String>) -> ProgressBar {
    progress_bar_for(
        len,
        msg.into(),
        std::io::stderr().is_terminal(),
        is_capturing(),
        is_compact(),
        super::is_quiet(),
    )
}

/// Whether a bar created on the calling thread right now would draw live.
pub(crate) fn bar_is_live() -> bool {
    draw_target(
        std::io::stderr().is_terminal(),
        is_capturing(),
        is_compact(),
    ) == BarTarget::Stderr
}

/// Where a new bar draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BarTarget {
    /// Live on stderr.
    Stderr,
    /// Not drawn; a notice line stands in for it.
    Hidden,
}

/// Pure draw decision: a live bar needs a terminal, a thread that is not
/// capturing its output, and a terminal not in compact mode.
fn draw_target(is_tty: bool, capturing: bool, compact: bool) -> BarTarget {
    if is_tty && !capturing && !compact {
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
    compact: bool,
    quiet: bool,
) -> ProgressBar {
    match draw_target(is_tty, capturing, compact) {
        BarTarget::Stderr => new_progress_bar(len, msg, ProgressDrawTarget::stderr()),
        BarTarget::Hidden => {
            emit(notice_kind(quiet), &msg, &msg);
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

/// The kind of the one-line stand-in for a hidden bar: a notice on stderr,
/// or under `--quiet` a log-only line.
pub(crate) fn notice_kind(quiet: bool) -> LineKind {
    if quiet {
        LineKind::Detail
    } else {
        LineKind::Notice
    }
}

/// A spinner for one running step, drawn on stderr.
///
/// While it exists it is the calling thread's active spinner, which the
/// prompt wrappers suspend (clear, then redraw) around a prompt. Dropping
/// it, or [`Spinner::finish_and_clear`], removes it from the screen.
#[derive(Debug)]
pub struct Spinner {
    bar: ProgressBar,
    /// Identifies this spinner in the thread's active-spinner stack.
    id: u64,
    /// The registration lives in the creating thread's stack, so the spinner
    /// must stay on that thread.
    _not_send: PhantomData<*const ()>,
}

#[allow(dead_code)] // part of the ui API for setup's compact output; no caller outside tests yet
impl Spinner {
    /// Replace the text beside the spinner.
    pub fn set_message(&self, msg: impl Into<String>) {
        self.bar.set_message(msg.into());
    }

    /// Whether the spinner draws nothing.
    pub fn is_hidden(&self) -> bool {
        self.bar.is_hidden()
    }

    /// Remove the spinner from the screen.
    pub fn finish_and_clear(self) {
        drop(self);
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        let id = self.id;
        let _ = ACTIVE_SPINNERS.try_with(|active| {
            active.borrow_mut().retain(|(other, _)| *other != id);
        });
        self.bar.finish_and_clear();
    }
}

thread_local! {
    /// The calling thread's live spinners, newest last. The newest one is
    /// the one a prompt suspends.
    static ACTIVE_SPINNERS: RefCell<Vec<(u64, ProgressBar)>> = const { RefCell::new(Vec::new()) };
    /// The id the calling thread gives its next spinner.
    static NEXT_SPINNER_ID: Cell<u64> = const { Cell::new(0) };
}

/// How often a live spinner advances on its own.
const SPINNER_TICK: Duration = Duration::from_millis(100);

/// Start a spinner labeled `msg`. It draws only when stderr is a TTY and
/// the calling thread is not capturing; otherwise it is hidden and prints
/// nothing, since the caller prints the step's line when the step finishes.
#[allow(dead_code)] // part of the ui API for setup's compact output; no caller outside tests yet
pub fn spinner(msg: impl Into<String>) -> Spinner {
    match spinner_target(std::io::stderr().is_terminal(), is_capturing()) {
        BarTarget::Stderr => {
            let spinner = new_spinner(msg.into(), ProgressDrawTarget::stderr());
            spinner.bar.enable_steady_tick(SPINNER_TICK);
            spinner
        }
        BarTarget::Hidden => new_spinner(msg.into(), ProgressDrawTarget::hidden()),
    }
}

/// Pure draw decision for a spinner: a terminal and a thread that is not
/// capturing. Compact mode does not hide it; the spinner is compact mode's
/// progress display.
fn spinner_target(is_tty: bool, capturing: bool) -> BarTarget {
    if is_tty && !capturing {
        BarTarget::Stderr
    } else {
        BarTarget::Hidden
    }
}

/// Target-independent core of [`spinner`], factored out so unit tests are
/// deterministic: it never starts the background tick. Registers the new
/// spinner as the thread's active one and draws its label at once.
pub(super) fn new_spinner(msg: String, target: ProgressDrawTarget) -> Spinner {
    let bar = ProgressBar::with_draw_target(None, target)
        .with_style(ProgressStyle::default_spinner())
        .with_message(msg);
    bar.tick();
    let id = NEXT_SPINNER_ID
        .try_with(|next| {
            let id = next.get();
            next.set(id.wrapping_add(1));
            id
        })
        .unwrap_or(0);
    let _ = ACTIVE_SPINNERS.try_with(|active| active.borrow_mut().push((id, bar.clone())));
    Spinner {
        bar,
        id,
        _not_send: PhantomData,
    }
}

/// Run `f` with the calling thread's active spinner, if any, cleared from
/// the screen, and redraw it afterwards.
pub(crate) fn suspend_spinner<T>(f: impl FnOnce() -> T) -> T {
    let active = ACTIVE_SPINNERS
        .try_with(|active| active.borrow().last().map(|(_, bar)| bar.clone()))
        .ok()
        .flatten();
    match active {
        Some(bar) => bar.suspend(f),
        None => f(),
    }
}

/// Whether the calling thread has a live spinner.
#[cfg(test)]
fn has_active_spinner() -> bool {
    ACTIVE_SPINNERS.with(|active| !active.borrow().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::capture::Stream;
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
    fn non_tty_fallback_is_a_notice_line() {
        assert_eq!(notice_kind(false), LineKind::Notice);
    }

    #[test]
    fn non_tty_fallback_is_log_only_under_quiet() {
        assert_eq!(notice_kind(true), LineKind::Detail);
    }

    #[test]
    fn non_tty_fallback_prints_its_label_once() {
        let (bar, written) = crate::ui::tap_terminal(|| {
            progress_bar_for(
                10,
                "Fetching R formatter…".into(),
                false,
                false,
                false,
                false,
            )
        });
        assert!(bar.is_hidden());
        assert_eq!(
            written,
            [(Stream::Stderr, "Fetching R formatter…".to_string())]
        );
    }

    #[test]
    fn quiet_bar_notice_stays_off_the_terminal_but_reaches_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        crate::ui::open_log(&path).unwrap();
        let (bar, written) = crate::ui::tap_terminal(|| {
            progress_bar_for(10, "downloading quarto".into(), false, false, false, true)
        });
        crate::ui::close_log();
        assert!(bar.is_hidden());
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            "downloading quarto\n"
        );
    }

    #[test]
    fn bar_draws_live_only_on_a_tty_outside_capture() {
        assert_eq!(draw_target(true, false, false), BarTarget::Stderr);
        assert_eq!(draw_target(false, false, false), BarTarget::Hidden);
        assert_eq!(draw_target(true, true, false), BarTarget::Hidden);
        assert_eq!(draw_target(false, true, false), BarTarget::Hidden);
    }

    #[test]
    fn bar_is_hidden_in_compact_mode_even_on_a_tty() {
        assert_eq!(draw_target(true, false, true), BarTarget::Hidden);
        assert_eq!(draw_target(false, false, true), BarTarget::Hidden);
    }

    #[test]
    fn compact_bar_notice_goes_to_the_log_not_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        crate::ui::open_log(&path).unwrap();
        let _compact = crate::ui::compact();
        let (bar, written) = crate::ui::tap_terminal(|| {
            progress_bar_for(10, "downloading quarto".into(), true, false, true, false)
        });
        crate::ui::close_log();
        assert!(bar.is_hidden());
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            "downloading quarto\n"
        );
    }

    #[test]
    fn spinner_draws_only_on_a_tty_outside_capture() {
        assert_eq!(spinner_target(true, false), BarTarget::Stderr);
        assert_eq!(spinner_target(false, false), BarTarget::Hidden);
        assert_eq!(spinner_target(true, true), BarTarget::Hidden);
        assert_eq!(spinner_target(false, true), BarTarget::Hidden);
    }

    #[test]
    fn captured_spinner_is_hidden_and_records_nothing() {
        let (spinner, transcript) = crate::ui::capture(|| spinner("stable Rust"));
        assert!(spinner.is_hidden());
        assert!(transcript.is_empty());
    }

    #[test]
    fn terminal_spinner_draws_its_label_immediately() {
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let spinner = new_spinner("[2/15] Docker Engine".into(), target);
        assert!(!spinner.is_hidden());
        let drawn = term.contents();
        assert!(
            drawn.contains("[2/15] Docker Engine"),
            "drawn was: {drawn:?}"
        );
    }

    #[test]
    fn spinner_is_active_while_it_lives() {
        assert!(!has_active_spinner());
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term));
        let spinner = new_spinner("[2/15] Docker Engine".into(), target);
        assert!(has_active_spinner());
        drop(spinner);
        assert!(!has_active_spinner());
    }

    #[test]
    fn finished_spinner_is_no_longer_active() {
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term));
        let spinner = new_spinner("[2/15] Docker Engine".into(), target);
        assert!(has_active_spinner());
        spinner.finish_and_clear();
        assert!(!has_active_spinner());
    }

    #[test]
    fn suspend_clears_the_active_spinner_and_redraws_it_after() {
        let term = InMemoryTerm::new(24, 80);
        let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
        let _spinner = new_spinner("[4/15] RStudio Server".into(), target);
        let during = suspend_spinner(|| term.contents());
        assert!(
            !during.contains("[4/15] RStudio Server"),
            "spinner must be cleared during a prompt, drawn was: {during:?}"
        );
        let after = term.contents();
        assert!(
            after.contains("[4/15] RStudio Server"),
            "spinner must be redrawn after a prompt, drawn was: {after:?}"
        );
    }

    #[test]
    fn suspend_without_a_spinner_just_runs_the_closure() {
        assert_eq!(suspend_spinner(|| 7), 7);
    }

    #[test]
    fn captured_bar_is_hidden_and_records_its_notice() {
        let (bar, transcript) = crate::ui::capture(|| {
            progress_bar_for(10, "downloading quarto".into(), true, true, false, false)
        });
        assert!(bar.is_hidden());
        assert_eq!(
            transcript.lines(),
            [(Stream::Stderr, "downloading quarto".to_string())]
        );
    }

    #[test]
    fn captured_bar_notice_is_log_only_under_quiet() {
        let (bar, transcript) = crate::ui::capture(|| {
            progress_bar_for(10, "downloading quarto".into(), false, true, false, true)
        });
        assert!(bar.is_hidden());
        assert!(
            transcript.lines().is_empty(),
            "lines were: {:?}",
            transcript.lines()
        );
        let recorded: Vec<_> = transcript
            .entries()
            .iter()
            .map(|line| (line.kind, line.message.clone()))
            .collect();
        assert_eq!(
            recorded,
            [(LineKind::Detail, "downloading quarto".to_string())]
        );
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
