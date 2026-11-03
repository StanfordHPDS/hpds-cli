//! Per-thread output capture.
//!
//! Work that runs on a background thread cannot write to the terminal as it
//! goes: its lines would interleave with the foreground's. [`capture`] runs a
//! closure with the calling thread's hpds output recorded in a [`Transcript`]
//! instead of written, and [`replay`] writes a transcript out later, on
//! whichever thread owns the terminal at that point.
//!
//! Every human-facing line in `ui` passes through [`emit`], the one place
//! that decides between recording and writing. Machine output
//! ([`super::data`]) bypasses it and is never captured. A line that is
//! written (not recorded) also goes to the calling thread's open log (see
//! [`super::sink`]), and in compact mode only warnings and errors are kept
//! for the caller instead of reaching the terminal.
//!
//! Capture is strictly per thread: while one thread captures, output from
//! any other thread goes to the terminal as usual. Captures nest: an inner
//! [`capture`] records into its own transcript, which is returned to its
//! caller and not added to the outer one; the outer capture resumes when the
//! inner one returns.

use std::cell::RefCell;

use super::progress::suspend_spinner;
use super::sink::{self, Report, ReportKind};

/// Which standard stream a line is destined for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// What a line is, which decides where it goes: the terminal stream, the
/// log, and whether compact mode keeps it for the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineKind {
    /// A plain informational line on stdout.
    Info,
    /// An informational line on stdout that is never written to the log,
    /// such as the `-v` echo of a command something else already logs.
    Echo,
    /// A line on stdout that stays on the terminal in compact mode, such as
    /// a setup step's result. Never kept as a report.
    Status,
    /// A line on stdout that always reaches the terminal, compact mode and
    /// `--quiet` included, and is never written to the log, for a line the
    /// log already holds in another form (a failed step's error repeated
    /// under its step line, or a collected warning listed as a next step).
    /// Never kept as a report.
    Report,
    /// A `✓`-prefixed success line on stdout.
    Success,
    /// An informational line on stderr, such as a hidden progress bar's
    /// stand-in notice.
    Notice,
    /// A `warning:` line on stderr.
    Warning,
    /// A rendered error on stderr.
    Error,
    /// A log-only line: never written to the terminal.
    Detail,
}

impl LineKind {
    /// The terminal stream this kind of line is written to, or `None` for
    /// log-only lines.
    pub(crate) fn stream(self) -> Option<Stream> {
        match self {
            LineKind::Info
            | LineKind::Echo
            | LineKind::Status
            | LineKind::Report
            | LineKind::Success => Some(Stream::Stdout),
            LineKind::Notice | LineKind::Warning | LineKind::Error => Some(Stream::Stderr),
            LineKind::Detail => None,
        }
    }
}

/// One emitted line: its kind, its unstyled message, and the text the
/// terminal shows (which may carry color codes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) kind: LineKind,
    /// The message as the caller gave it, without prefix or color. For
    /// errors this is the full uncolored rendering.
    pub(crate) message: String,
    /// The line as written to the terminal.
    pub(crate) styled: String,
}

impl Line {
    /// The line as written to the log: the terminal text without color.
    pub(crate) fn plain(&self) -> String {
        match self.kind {
            LineKind::Success => format!("✓ {}", self.message),
            LineKind::Warning => format!("warning: {}", self.message),
            LineKind::Info
            | LineKind::Echo
            | LineKind::Status
            | LineKind::Report
            | LineKind::Notice
            | LineKind::Error
            | LineKind::Detail => self.message.clone(),
        }
    }
}

/// Lines recorded by [`capture`], in the order they were emitted, each with
/// its kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    lines: Vec<Line>,
}

impl Transcript {
    /// Whether nothing was recorded.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The recorded terminal lines, in emission order, each with the stream
    /// it would have been written to. Log-only lines are left out.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<(Stream, String)> {
        self.lines
            .iter()
            .filter_map(|line| line.kind.stream().map(|s| (s, line.styled.clone())))
            .collect()
    }

    /// Every recorded line, log-only lines included, in emission order.
    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[Line] {
        &self.lines
    }
}

thread_local! {
    /// The calling thread's open captures, innermost last. Empty when the
    /// thread is not capturing.
    static CAPTURE: RefCell<Vec<Vec<Line>>> = const { RefCell::new(Vec::new()) };
}

/// Whether the calling thread is inside [`capture`].
pub(crate) fn is_capturing() -> bool {
    CAPTURE
        .try_with(|frames| !frames.borrow().is_empty())
        .unwrap_or(false)
}

/// Run `f` with this thread's hpds output recorded instead of written, and
/// return its value with everything it emitted.
///
/// While capturing, progress bars are hidden and record their notice line,
/// and prompts fail with an error rather than wait on input nobody can see.
/// If `f` panics, the capture is closed and its lines are discarded.
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Transcript) {
    CAPTURE.with(|frames| frames.borrow_mut().push(Vec::new()));
    let frame = OpenFrame;
    let value = f();
    let lines = frame.close();
    (value, Transcript { lines })
}

/// Write `transcript` out, each line where its kind sends it, in the order
/// it was recorded. Inside a capture the lines are recorded into the
/// enclosing transcript instead.
pub fn replay(transcript: &Transcript) {
    for line in &transcript.lines {
        emit(line.kind, &line.message, &line.styled);
    }
}

/// Record a line when this thread is capturing; otherwise write it out.
/// `message` is the unstyled text (see [`Line::message`]) and `styled` the
/// terminal text. During thread teardown, once the capture state is gone,
/// the line is written directly.
pub(crate) fn emit(kind: LineKind, message: &str, styled: &str) {
    let line = Line {
        kind,
        message: message.to_string(),
        styled: styled.to_string(),
    };
    let unrecorded = CAPTURE
        .try_with(|frames| match frames.borrow_mut().last_mut() {
            Some(frame) => {
                frame.push(line.clone());
                None
            }
            None => Some(line.clone()),
        })
        .unwrap_or(Some(line));
    if let Some(line) = unrecorded {
        write_out(&line);
    }
}

/// Write an unrecorded line out: to the log, if one is open (echo and
/// report lines excepted), and then to the terminal. In compact mode only
/// status and report lines reach the terminal; warnings and errors are kept
/// as reports instead, and other lines go to the log only. Log-only lines
/// never reach the terminal.
fn write_out(line: &Line) {
    if !matches!(line.kind, LineKind::Echo | LineKind::Report) {
        sink::write_log(&line.plain());
    }
    if sink::is_compact() {
        match line.kind {
            LineKind::Status | LineKind::Report => {}
            LineKind::Warning => {
                sink::push_report(Report {
                    kind: ReportKind::Warning,
                    message: line.message.clone(),
                });
                return;
            }
            LineKind::Error => {
                sink::push_report(Report {
                    kind: ReportKind::Error,
                    message: line.message.clone(),
                });
                return;
            }
            LineKind::Info
            | LineKind::Echo
            | LineKind::Success
            | LineKind::Notice
            | LineKind::Detail => return,
        }
    }
    if let Some(stream) = line.kind.stream() {
        suspend_spinner(|| write_terminal(stream, &line.styled));
    }
}

/// Write one line to a terminal stream.
#[cfg(not(test))]
fn write_terminal(stream: Stream, text: &str) {
    match stream {
        Stream::Stdout => std::println!("{text}"),
        Stream::Stderr => std::eprintln!("{text}"),
    }
}

/// Write one line to a terminal stream, or to the calling thread's tap
/// when [`tap_terminal`] is collecting.
#[cfg(test)]
fn write_terminal(stream: Stream, text: &str) {
    let tapped = TAP
        .try_with(|tap| match tap.borrow_mut().as_mut() {
            Some(written) => {
                written.push((stream, text.to_string()));
                true
            }
            None => false,
        })
        .unwrap_or(false);
    if tapped {
        let _ = PROBE.try_with(|probe| {
            if let Some((look, seen)) = probe.borrow_mut().as_mut() {
                seen.push(look());
            }
        });
    }
    if !tapped {
        match stream {
            Stream::Stdout => std::println!("{text}"),
            Stream::Stderr => std::eprintln!("{text}"),
        }
    }
}

/// A closure run at each tapped terminal write, and what it returned.
#[cfg(test)]
type Probe = (Box<dyn Fn() -> String>, Vec<String>);

#[cfg(test)]
thread_local! {
    /// Terminal writes collected by [`tap_terminal`] on the calling thread.
    static TAP: RefCell<Option<Vec<(Stream, String)>>> = const { RefCell::new(None) };
    /// The probe [`tap_terminal_probing`] runs at each tapped write.
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

/// Run `f` with this thread's terminal writes collected instead of printed,
/// and return them with `f`'s value. Unlike [`capture`], everything else
/// behaves as on a live terminal: the log, compact mode, and reports all
/// apply. Test-only, so tests can assert on exactly what a user would see.
#[cfg(test)]
pub(crate) fn tap_terminal<T>(f: impl FnOnce() -> T) -> (T, Vec<(Stream, String)>) {
    let previous = TAP.with(|tap| tap.borrow_mut().replace(Vec::new()));
    let value = f();
    let written = TAP.with(|tap| std::mem::replace(&mut *tap.borrow_mut(), previous));
    (value, written.unwrap_or_default())
}

/// [`tap_terminal`], also running `probe` at the moment of each tapped
/// write and returning what it saw then, one entry per write. Lets a test
/// look at another surface (such as a spinner's in-memory terminal) while
/// a line is being written.
#[cfg(test)]
pub(crate) fn tap_terminal_probing<T>(
    probe: impl Fn() -> String + 'static,
    f: impl FnOnce() -> T,
) -> (T, Vec<(Stream, String)>, Vec<String>) {
    let previous = PROBE.with(|slot| slot.borrow_mut().replace((Box::new(probe), Vec::new())));
    let (value, written) = tap_terminal(f);
    let seen = PROBE.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), previous));
    (
        value,
        written,
        seen.map(|(_, seen)| seen).unwrap_or_default(),
    )
}

/// The innermost open capture on this thread. Dropping it without
/// [`OpenFrame::close`] (that is, when the captured closure panics) still
/// pops the frame, so the thread does not stay in capture mode.
struct OpenFrame;

impl OpenFrame {
    fn close(self) -> Vec<Line> {
        let lines = pop_frame();
        std::mem::forget(self);
        lines
    }
}

impl Drop for OpenFrame {
    fn drop(&mut self) {
        pop_frame();
    }
}

fn pop_frame() -> Vec<Line> {
    CAPTURE
        .try_with(|frames| frames.borrow_mut().pop())
        .ok()
        .flatten()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::sink::{Report, ReportKind, compact, read_log, take_reports, temp_log};

    fn line(stream: Stream, text: &str) -> (Stream, String) {
        (stream, text.to_string())
    }

    /// Emit an unstyled line of `kind`.
    fn say(kind: LineKind, text: &str) {
        emit(kind, text, text);
    }

    fn out(text: &str) {
        say(LineKind::Info, text);
    }

    fn err(text: &str) {
        say(LineKind::Notice, text);
    }

    /// One line of each terminal kind, styled with color codes, plus a
    /// log-only line.
    fn emit_every_kind() {
        emit(LineKind::Info, "installing quarto", "installing quarto");
        emit(
            LineKind::Success,
            "quarto installed",
            "\x1b[32m✓\x1b[0m quarto installed",
        );
        emit(
            LineKind::Warning,
            "quarto is already on PATH",
            "\x1b[1;33mwarning:\x1b[0m quarto is already on PATH",
        );
        emit(
            LineKind::Error,
            "error: tinytex failed",
            "\x1b[1;31merror:\x1b[0m tinytex failed",
        );
        emit(LineKind::Detail, "$ quarto --version", "$ quarto --version");
    }

    const EVERY_KIND_IN_THE_LOG: &str = "installing quarto\n\
                                         ✓ quarto installed\n\
                                         warning: quarto is already on PATH\n\
                                         error: tinytex failed\n\
                                         $ quarto --version\n";

    #[test]
    fn capture_records_lines_in_order_with_their_streams() {
        let (value, transcript) = capture(|| {
            out("one");
            err("two");
            out("three");
            42
        });
        assert_eq!(value, 42);
        assert_eq!(
            transcript.lines(),
            vec![
                line(Stream::Stdout, "one"),
                line(Stream::Stderr, "two"),
                line(Stream::Stdout, "three"),
            ]
        );
        assert!(!transcript.is_empty());
    }

    #[test]
    fn is_capturing_is_true_only_inside_capture() {
        assert!(!is_capturing());
        let (inside, _) = capture(is_capturing);
        assert!(inside);
        assert!(!is_capturing());
    }

    #[test]
    fn capture_is_strictly_per_thread() {
        let (other, transcript) = capture(|| {
            out("from the capturing thread");
            let other = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        // Another thread is capturing right now; this one is not.
                        let capturing_here = is_capturing();
                        let (_, mine) = capture(|| err("from another thread"));
                        (capturing_here, mine)
                    })
                    .join()
                    .unwrap()
            });
            err("after the other thread");
            other
        });
        let (capturing_here, other_transcript) = other;
        assert!(!capturing_here);
        assert_eq!(
            other_transcript.lines(),
            vec![line(Stream::Stderr, "from another thread")]
        );
        assert_eq!(
            transcript.lines(),
            vec![
                line(Stream::Stdout, "from the capturing thread"),
                line(Stream::Stderr, "after the other thread"),
            ]
        );
    }

    #[test]
    fn nested_capture_records_into_its_own_transcript_and_restores_the_outer() {
        let (inner, outer) = capture(|| {
            out("outer before");
            let (_, inner) = capture(|| err("inner"));
            out("outer after");
            inner
        });
        assert_eq!(inner.lines(), vec![line(Stream::Stderr, "inner")]);
        assert_eq!(
            outer.lines(),
            vec![
                line(Stream::Stdout, "outer before"),
                line(Stream::Stdout, "outer after"),
            ]
        );
    }

    #[test]
    fn replay_inside_a_capture_records_into_the_enclosing_transcript() {
        let (_, inner) = capture(|| {
            out("a");
            err("b");
            say(LineKind::Detail, "c");
        });
        let (_, outer) = capture(|| replay(&inner));
        assert_eq!(outer, inner);
    }

    #[test]
    fn capture_that_panics_leaves_the_thread_not_capturing() {
        let result = std::panic::catch_unwind(|| capture(|| panic!("step blew up")));
        assert!(result.is_err());
        assert!(!is_capturing());
    }

    #[test]
    fn empty_capture_gives_an_empty_transcript() {
        let ((), transcript) = capture(|| ());
        assert!(transcript.is_empty());
    }

    #[test]
    fn tap_terminal_collects_what_would_be_written_in_order() {
        let ((), written) = tap_terminal(|| {
            out("one");
            err("two");
        });
        assert_eq!(
            written,
            vec![line(Stream::Stdout, "one"), line(Stream::Stderr, "two")]
        );
    }

    #[test]
    fn every_written_line_also_reaches_the_log_in_order_without_color() {
        let (_dir, path) = temp_log();
        let ((), _) = tap_terminal(emit_every_kind);
        assert_eq!(read_log(&path), EVERY_KIND_IN_THE_LOG);
    }

    #[test]
    fn verbose_terminal_gets_every_line_but_log_only_ones() {
        let (_dir, path) = temp_log();
        let ((), written) = tap_terminal(emit_every_kind);
        assert_eq!(
            written,
            vec![
                line(Stream::Stdout, "installing quarto"),
                line(Stream::Stdout, "\x1b[32m✓\x1b[0m quarto installed"),
                line(
                    Stream::Stderr,
                    "\x1b[1;33mwarning:\x1b[0m quarto is already on PATH"
                ),
                line(Stream::Stderr, "\x1b[1;31merror:\x1b[0m tinytex failed"),
            ]
        );
        assert!(take_reports().is_empty());
        assert_eq!(read_log(&path), EVERY_KIND_IN_THE_LOG);
    }

    #[test]
    fn log_only_lines_never_reach_the_terminal_even_without_a_log() {
        let ((), written) = tap_terminal(|| {
            say(LineKind::Detail, "$ apt-get install -y libcurl4");
            out("system libraries");
            say(LineKind::Detail, "exit status: 0");
        });
        assert_eq!(written, vec![line(Stream::Stdout, "system libraries")]);
    }

    #[test]
    fn compact_mode_writes_nothing_to_the_terminal_and_keeps_warnings_and_errors() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let ((), written) = tap_terminal(emit_every_kind);
        assert_eq!(written, Vec::<(Stream, String)>::new());
        assert_eq!(
            take_reports(),
            vec![
                Report {
                    kind: ReportKind::Warning,
                    message: "quarto is already on PATH".to_string(),
                },
                Report {
                    kind: ReportKind::Error,
                    message: "error: tinytex failed".to_string(),
                },
            ]
        );
        assert_eq!(read_log(&path), EVERY_KIND_IN_THE_LOG);
    }

    #[test]
    fn take_reports_drains_what_it_returns() {
        let _compact = compact();
        let ((), _) = tap_terminal(|| say(LineKind::Warning, "gh is not logged in"));
        assert_eq!(take_reports().len(), 1);
        assert!(take_reports().is_empty());
    }

    #[test]
    fn compact_reports_are_not_kept_while_capturing_but_on_replay() {
        let _compact = compact();
        let ((), transcript) = capture(|| say(LineKind::Warning, "docker group needs a login"));
        assert!(take_reports().is_empty());
        let ((), written) = tap_terminal(|| replay(&transcript));
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(
            take_reports(),
            vec![Report {
                kind: ReportKind::Warning,
                message: "docker group needs a login".to_string(),
            }]
        );
    }

    #[test]
    fn captured_lines_reach_the_log_when_replayed_not_when_recorded() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let ((), transcript) = capture(|| {
            out("[3/15] stable Rust");
            say(LineKind::Detail, "$ rustup toolchain install stable");
            say(LineKind::Detail, "exit status: 0");
            out("(ran in the background)");
        });
        out("[2/15] Docker Engine");
        replay(&transcript);
        assert_eq!(
            read_log(&path),
            "[2/15] Docker Engine\n\
             [3/15] stable Rust\n\
             $ rustup toolchain install stable\n\
             exit status: 0\n\
             (ran in the background)\n"
        );
    }

    #[test]
    fn log_only_lines_are_recorded_in_order_but_left_out_of_terminal_lines() {
        let ((), transcript) = capture(|| {
            out("a");
            say(LineKind::Detail, "b");
            out("c");
        });
        let kinds: Vec<_> = transcript.entries().iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [LineKind::Info, LineKind::Detail, LineKind::Info]);
        assert_eq!(
            transcript.lines(),
            vec![line(Stream::Stdout, "a"), line(Stream::Stdout, "c")]
        );
    }

    #[test]
    fn a_terminal_line_written_while_a_spinner_is_live_does_not_draw_over_it() {
        // The spinner owns the bottom row of the terminal; a line printed
        // straight to the stream would land on that row. Each terminal
        // write clears the spinner first and redraws it afterwards.
        let term = indicatif::InMemoryTerm::new(24, 80);
        let target = indicatif::ProgressDrawTarget::term_like(Box::new(term.clone()));
        let _spinner = super::super::progress::new_spinner("[2/15] Docker Engine".into(), target);
        let _compact = compact();
        let probe_term = term.clone();
        let ((), written, drawn) = tap_terminal_probing(
            move || probe_term.contents(),
            || say(LineKind::Status, "[1/15] system libraries (apt) ✓"),
        );
        assert_eq!(
            written,
            vec![line(Stream::Stdout, "[1/15] system libraries (apt) ✓")]
        );
        assert_eq!(drawn.len(), 1, "drawn was: {drawn:?}");
        assert!(
            !drawn[0].contains("[2/15] Docker Engine"),
            "the spinner must be cleared while the line is written, drawn was: {:?}",
            drawn[0]
        );
        let after = term.contents();
        assert!(
            after.contains("[2/15] Docker Engine"),
            "the spinner must be redrawn after the line, drawn was: {after:?}"
        );
    }

    #[test]
    fn background_thread_lines_reach_the_foreground_log_through_replay() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let transcript = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    capture(|| {
                        say(LineKind::Detail, "$ code-server --version");
                        say(LineKind::Warning, "code-server extension skipped");
                    })
                    .1
                })
                .join()
                .unwrap()
        });
        out("[10/15] code-server + extensions");
        replay(&transcript);
        assert_eq!(
            read_log(&path),
            "[10/15] code-server + extensions\n\
             $ code-server --version\n\
             warning: code-server extension skipped\n"
        );
        assert_eq!(take_reports().len(), 1);
    }
}
