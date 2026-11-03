//! Terminal output for `hpds`: styling, error rendering, progress, prompts.
//!
//! This is the ONLY module allowed to print to the terminal. All
//! helpers degrade gracefully: color is dropped when `NO_COLOR` is set, when
//! `TERM=dumb`, or when the stream is not a TTY, and progress bars hide
//! themselves on non-TTY stderr. Human-facing lines can be captured per
//! thread and replayed later (see [`fn@capture`]). Every human-facing line
//! can also be appended to a log file, and a compact mode keeps
//! informational lines off the terminal (see [`open_log`] and
//! [`fn@compact`]).

mod capture;
mod error;
mod progress;
mod prompt;
mod sink;

#[cfg(test)]
pub(crate) use capture::Stream;
#[cfg(test)]
pub(crate) use capture::tap_terminal;
use capture::{LineKind, emit};
pub use capture::{Transcript, capture, replay};
#[cfg(test)]
pub(crate) use progress::active_spinner_message;
#[cfg(test)]
use sink::Report;
pub(crate) use sink::is_compact;
pub use sink::{ReportKind, close_log, compact, open_log, take_reports};
pub use sink::{log_path, with_log_path};

#[allow(unused_imports)] // consumed only by unit tests today; part of the ui API
pub use error::render_error;
pub use error::{HintExt, error};
pub(crate) use progress::bar_is_live;
#[allow(unused_imports)] // re-exported for later commands; no caller outside tests yet
pub use progress::progress_bar;
pub use progress::{Spinner, spinner};
pub use prompt::{
    can_prompt, confirm, multiselect_all, multiselect_with_defaults, select, set_non_interactive,
    text,
};

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

/// Set the process-wide quiet mode (wired to the global `--quiet` flag).
/// When quiet, informational stdout output ([`fn@println`], [`success`]) is
/// suppressed; errors (and warnings) still print to stderr.
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Whether `--quiet` is on, for callers that resolve it once and pass it
/// down explicitly (see [`status_line`]).
pub(crate) fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// How color should be decided for output streams. `Auto` (the default)
/// inspects `NO_COLOR`, `TERM`, and whether the stream is a TTY; the global
/// `--no-color` flag maps to `Never`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

static COLOR_CHOICE: AtomicU8 = AtomicU8::new(0);

/// Set the process-wide color choice (wired to the global `--no-color` flag).
pub fn set_color_choice(choice: ColorChoice) {
    let raw = match choice {
        ColorChoice::Auto => 0,
        ColorChoice::Always => 1,
        ColorChoice::Never => 2,
    };
    COLOR_CHOICE.store(raw, Ordering::Relaxed);
}

fn color_choice() -> ColorChoice {
    match COLOR_CHOICE.load(Ordering::Relaxed) {
        1 => ColorChoice::Always,
        2 => ColorChoice::Never,
        _ => ColorChoice::Auto,
    }
}

/// Pure color decision, factored out of env/TTY probing so it is unit-testable.
fn resolve_color(
    choice: ColorChoice,
    no_color: Option<&str>,
    term: Option<&str>,
    is_tty: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            // Per https://no-color.org: disable when present and non-empty.
            if no_color.is_some_and(|v| !v.is_empty()) {
                return false;
            }
            if term == Some("dumb") {
                return false;
            }
            is_tty
        }
    }
}

fn env_resolve_color(is_tty: bool) -> bool {
    resolve_color(
        color_choice(),
        std::env::var("NO_COLOR").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        is_tty,
    )
}

/// Whether styled output should be emitted on stdout right now.
pub(crate) fn stdout_colors() -> bool {
    env_resolve_color(std::io::stdout().is_terminal())
}

/// Whether styled output should be emitted on stderr right now.
pub(crate) fn stderr_colors() -> bool {
    env_resolve_color(std::io::stderr().is_terminal())
}

pub(crate) const ERROR_STYLE: anstyle::Style = anstyle::Style::new()
    .bold()
    .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)));
pub(crate) const WARN_STYLE: anstyle::Style = anstyle::Style::new()
    .bold()
    .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)));
pub(crate) const SUCCESS_STYLE: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)));
pub(crate) const HINT_STYLE: anstyle::Style =
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Cyan)));

/// Wrap `text` in `style` when `use_color` is true; plain text otherwise.
pub(crate) fn paint(style: anstyle::Style, text: &str, use_color: bool) -> String {
    if use_color {
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

fn render_success(msg: &str, use_color: bool) -> String {
    format!("{} {msg}", paint(SUCCESS_STYLE, "✓", use_color))
}

fn render_warn(msg: &str, use_color: bool) -> String {
    format!("{} {msg}", paint(WARN_STYLE, "warning:", use_color))
}

/// Print an unstyled informational line to stdout. Suppressed on the
/// terminal by `--quiet`; still written to the log.
pub fn println(msg: &str) {
    info_line(msg, is_quiet());
}

fn info_line(msg: &str, quiet: bool) {
    emit(visible_or_detail(LineKind::Info, quiet), msg, msg);
}

/// `kind`, or a log-only line under `--quiet`.
fn visible_or_detail(kind: LineKind, quiet: bool) -> LineKind {
    if quiet { LineKind::Detail } else { kind }
}

/// Print an unstyled informational line to stdout that is never written to
/// the log, for a line the log already holds in another form (the `-v` echo
/// of a command a logging runner records). Hidden in compact mode like
/// [`fn@println`], and dropped entirely by `--quiet`.
pub fn echo(msg: &str) {
    if !is_quiet() {
        emit(LineKind::Echo, msg, msg);
    }
}

/// Print a status line to stdout: one of the few lines that stay on the
/// terminal in compact mode, such as a setup step's result or a run's
/// closing tally. Written to the log as well. Unlike a warning it is never
/// kept as a [`Report`](sink::Report). When `quiet` (the caller's resolved
/// `--quiet`, see [`is_quiet`]) it stays off the terminal, like
/// [`fn@println`], and is still written to the log.
pub(crate) fn status_line(msg: &str, quiet: bool) {
    emit(visible_or_detail(LineKind::Status, quiet), msg, msg);
}

/// Print a line to stdout that always reaches the terminal: compact mode
/// passes it through and `--quiet` does not suppress it. It is never
/// written to the log, so it suits a line the log already holds in another
/// form, such as a failed step's error repeated under its step line, or a
/// warning the run collected and lists as a next step.
pub fn report(msg: &str) {
    emit(LineKind::Report, msg, msg);
}

/// Write a line to the log only; it never reaches the terminal. Inside
/// [`fn@capture`] it is recorded with the other lines and written to the
/// log, in order, when the transcript is replayed. Without an open log the
/// line is dropped.
pub fn log_detail(text: &str) {
    emit(LineKind::Detail, text, text);
}

/// Print machine-readable output (`--format json`) to stdout. Never
/// suppressed by `--quiet`: quiet silences human chrome, not the data
/// another program asked for.
pub fn data(msg: &str) {
    std::println!("{msg}");
}

/// Print a green `✓`-prefixed success line to stdout. Suppressed on the
/// terminal by `--quiet`; still written to the log.
pub fn success(msg: &str) {
    success_line(msg, is_quiet());
}

fn success_line(msg: &str, quiet: bool) {
    if quiet {
        // Log-only, so the message carries the `✓` the log would show.
        let plain = render_success(msg, false);
        emit(LineKind::Detail, &plain, &plain);
    } else {
        emit(
            LineKind::Success,
            msg,
            &render_success(msg, stdout_colors()),
        );
    }
}

/// Print a `warning:`-prefixed line to stderr. Not suppressed by `--quiet`:
/// warnings flag conditions the user likely needs to act on, so they stay
/// visible alongside errors.
pub fn warn(msg: &str) {
    emit(LineKind::Warning, msg, &render_warn(msg, stderr_colors()));
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: &str = "\x1b[";

    #[test]
    fn auto_disables_color_when_not_a_tty() {
        assert!(!resolve_color(
            ColorChoice::Auto,
            None,
            Some("xterm-256color"),
            false
        ));
    }

    #[test]
    fn auto_enables_color_on_a_tty_with_clean_env() {
        assert!(resolve_color(
            ColorChoice::Auto,
            None,
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn no_color_env_disables_color_even_on_a_tty() {
        assert!(!resolve_color(
            ColorChoice::Auto,
            Some("1"),
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn empty_no_color_env_is_ignored_per_no_color_spec() {
        assert!(resolve_color(
            ColorChoice::Auto,
            Some(""),
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn dumb_term_disables_color() {
        assert!(!resolve_color(ColorChoice::Auto, None, Some("dumb"), true));
    }

    #[test]
    fn never_overrides_everything() {
        assert!(!resolve_color(
            ColorChoice::Never,
            None,
            Some("xterm-256color"),
            true
        ));
    }

    #[test]
    fn always_overrides_everything() {
        assert!(resolve_color(
            ColorChoice::Always,
            Some("1"),
            Some("dumb"),
            false
        ));
    }

    #[test]
    fn set_color_choice_round_trips() {
        set_color_choice(ColorChoice::Always);
        assert_eq!(color_choice(), ColorChoice::Always);
        set_color_choice(ColorChoice::Never);
        assert_eq!(color_choice(), ColorChoice::Never);
        set_color_choice(ColorChoice::Auto);
        assert_eq!(color_choice(), ColorChoice::Auto);
    }

    #[test]
    fn paint_without_color_emits_no_ansi_codes() {
        let out = paint(ERROR_STYLE, "boom", false);
        assert_eq!(out, "boom");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn paint_with_color_wraps_text_in_ansi_codes() {
        let out = paint(ERROR_STYLE, "boom", true);
        assert!(out.contains(ESC));
        assert!(out.contains("boom"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn success_line_without_color_has_check_prefix_and_no_ansi() {
        let out = render_success("42 files formatted", false);
        assert_eq!(out, "✓ 42 files formatted");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn success_line_with_color_contains_ansi() {
        assert!(render_success("done", true).contains(ESC));
    }

    #[test]
    fn warn_line_without_color_has_warning_prefix_and_no_ansi() {
        let out = render_warn("unknown key `foo` in hpds.toml", false);
        assert_eq!(out, "warning: unknown key `foo` in hpds.toml");
        assert!(!out.contains(ESC));
    }

    #[test]
    fn warn_line_with_color_contains_ansi() {
        assert!(render_warn("careful", true).contains(ESC));
    }

    #[test]
    fn captured_output_keeps_order_and_stream_across_helpers() {
        let ((), transcript) = capture(|| {
            info_line("installing quarto", false);
            warn("quarto is already on PATH");
            success_line("quarto 1.6 installed", false);
            error(&anyhow::anyhow!("tinytex failed"));
        });
        let (_, replayed) = capture(|| replay(&transcript));
        assert_eq!(replayed, transcript);
        let lines = transcript_lines(&transcript);
        assert_eq!(lines.len(), 4, "lines were: {lines:?}");
        assert_eq!(lines[0], (Stream::Stdout, "installing quarto".to_string()));
        assert_eq!(lines[1].0, Stream::Stderr);
        assert!(lines[1].1.contains("quarto is already on PATH"));
        assert_eq!(lines[2].0, Stream::Stdout);
        assert!(lines[2].1.contains("quarto 1.6 installed"));
        assert_eq!(lines[3].0, Stream::Stderr);
        assert!(lines[3].1.contains("tinytex failed"));
    }

    #[test]
    fn quiet_suppresses_the_same_lines_while_capturing() {
        let ((), transcript) = capture(|| {
            info_line("installing quarto", true);
            success_line("quarto installed", true);
            warn("still shown under quiet");
        });
        let lines = transcript_lines(&transcript);
        assert_eq!(lines.len(), 1, "lines were: {lines:?}");
        assert_eq!(lines[0].0, Stream::Stderr);
        assert!(lines[0].1.contains("still shown under quiet"));
    }

    fn transcript_lines(transcript: &Transcript) -> Vec<(Stream, String)> {
        transcript.lines()
    }

    fn temp_log() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        open_log(&path).unwrap();
        (dir, path)
    }

    fn read_log(path: &std::path::Path) -> String {
        close_log();
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn log_detail_goes_to_the_log_and_never_the_terminal() {
        let (_dir, path) = temp_log();
        let ((), written) = tap_terminal(|| {
            info_line("[1/15] system libraries (apt)", false);
            log_detail("$ apt-get install -y libcurl4-openssl-dev");
            log_detail("exit status: 0");
        });
        assert_eq!(
            written,
            vec![(Stream::Stdout, "[1/15] system libraries (apt)".to_string())]
        );
        assert_eq!(
            read_log(&path),
            "[1/15] system libraries (apt)\n\
             $ apt-get install -y libcurl4-openssl-dev\n\
             exit status: 0\n"
        );
    }

    #[test]
    fn log_detail_is_recorded_in_a_capture_and_replays_in_order() {
        let ((), transcript) = capture(|| {
            info_line("stable Rust", false);
            log_detail("$ rustup toolchain install stable");
            warn("rustup is already installed");
        });
        let messages: Vec<_> = transcript
            .entries()
            .iter()
            .map(|l| l.message.clone())
            .collect();
        assert_eq!(
            messages,
            [
                "stable Rust",
                "$ rustup toolchain install stable",
                "rustup is already installed",
            ]
        );
        let (_dir, path) = temp_log();
        let _compact = compact();
        replay(&transcript);
        assert_eq!(
            read_log(&path),
            "stable Rust\n\
             $ rustup toolchain install stable\n\
             warning: rustup is already installed\n"
        );
    }

    #[test]
    fn quiet_lines_stay_off_the_terminal_but_reach_the_log() {
        let (_dir, path) = temp_log();
        let ((), written) = tap_terminal(|| {
            info_line("installing quarto", true);
            success_line("quarto installed", true);
            warn("still shown under quiet");
        });
        assert_eq!(written.len(), 1, "written was: {written:?}");
        assert!(written[0].1.contains("still shown under quiet"));
        assert_eq!(
            read_log(&path),
            "installing quarto\n\
             ✓ quarto installed\n\
             warning: still shown under quiet\n"
        );
    }

    #[test]
    fn compact_helpers_report_warnings_and_errors_with_plain_messages() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let ((), written) = tap_terminal(|| {
            info_line("installing gh", false);
            success_line("gh installed", false);
            warn("gh is installed but not logged in to GitHub; run `gh auth login`");
            error(
                &Err::<(), _>(anyhow::anyhow!("code-server failed"))
                    .hint("rerun server setup")
                    .unwrap_err(),
            );
        });
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(
            take_reports(),
            vec![
                Report {
                    kind: ReportKind::Warning,
                    message: "gh is installed but not logged in to GitHub; run `gh auth login`"
                        .to_string(),
                },
                Report {
                    kind: ReportKind::Error,
                    message: "error: code-server failed\nhint: rerun server setup".to_string(),
                },
            ]
        );
        assert_eq!(
            read_log(&path),
            "installing gh\n\
             ✓ gh installed\n\
             warning: gh is installed but not logged in to GitHub; run `gh auth login`\n\
             error: code-server failed\n\
             hint: rerun server setup\n"
        );
    }

    #[test]
    fn verbose_helpers_print_as_today_and_keep_no_reports() {
        let ((), written) = tap_terminal(|| {
            info_line("installing gh", false);
            warn("careful");
        });
        // The warning's color depends on the process-wide color choice,
        // which another test may change concurrently, so only its stream
        // and text are checked.
        assert_eq!(written.len(), 2, "written was: {written:?}");
        assert_eq!(written[0], (Stream::Stdout, "installing gh".to_string()));
        assert_eq!(written[1].0, Stream::Stderr);
        assert!(
            written[1].1.contains("warning:"),
            "written was: {written:?}"
        );
        assert!(written[1].1.contains("careful"), "written was: {written:?}");
        assert!(take_reports().is_empty());
    }

    #[test]
    fn status_lines_reach_the_terminal_and_the_log_in_compact_mode() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let ((), written) = tap_terminal(|| {
            status_line("hpds setup --profile server: 15 steps.", false);
            info_line("[1/15] system libraries (apt)", false);
            status_line("[1/15] system libraries (apt) ✓", false);
        });
        assert_eq!(
            written,
            vec![
                (
                    Stream::Stdout,
                    "hpds setup --profile server: 15 steps.".to_string()
                ),
                (
                    Stream::Stdout,
                    "[1/15] system libraries (apt) ✓".to_string()
                ),
            ]
        );
        assert!(take_reports().is_empty());
        assert_eq!(
            read_log(&path),
            "hpds setup --profile server: 15 steps.\n\
             [1/15] system libraries (apt)\n\
             [1/15] system libraries (apt) ✓\n"
        );
    }

    #[test]
    fn status_lines_reach_the_terminal_outside_compact_mode() {
        let ((), written) = tap_terminal(|| status_line("14 of 15 steps succeeded.", false));
        assert_eq!(
            written,
            vec![(Stream::Stdout, "14 of 15 steps succeeded.".to_string())]
        );
    }

    #[test]
    fn status_is_recorded_in_a_capture_and_replays_to_the_terminal() {
        let ((), transcript) = capture(|| {
            log_detail("$ code-server --version");
            status_line("[10/15] code-server + extensions ✓", false);
        });
        let kinds: Vec<_> = transcript.entries().iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [LineKind::Detail, LineKind::Status]);
        let _compact = compact();
        let ((), written) = tap_terminal(|| replay(&transcript));
        assert_eq!(
            written,
            vec![(
                Stream::Stdout,
                "[10/15] code-server + extensions ✓".to_string()
            )]
        );
        assert!(take_reports().is_empty());
    }

    #[test]
    fn report_lines_reach_the_terminal_in_compact_mode_but_never_the_log() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let ((), written) = tap_terminal(|| {
            status_line("[2/3] git defaults ✗", true);
            report("        user.name is not set");
            report("Next steps:");
        });
        assert_eq!(
            written,
            vec![
                (Stream::Stdout, "        user.name is not set".to_string()),
                (Stream::Stdout, "Next steps:".to_string()),
            ]
        );
        assert!(take_reports().is_empty());
        assert_eq!(read_log(&path), "[2/3] git defaults ✗\n");
    }

    #[test]
    fn report_lines_are_recorded_in_a_capture_and_replay_to_the_terminal() {
        let ((), transcript) = capture(|| report("  - run `gh auth login`"));
        let kinds: Vec<_> = transcript.entries().iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [LineKind::Report]);
        let _compact = compact();
        let ((), written) = tap_terminal(|| replay(&transcript));
        assert_eq!(
            written,
            vec![(Stream::Stdout, "  - run `gh auth login`".to_string())]
        );
    }

    #[test]
    fn quiet_status_lines_stay_off_the_terminal_but_reach_the_log() {
        let (_dir, path) = temp_log();
        let _compact = compact();
        let ((), written) = tap_terminal(|| status_line("[2/15] Docker Engine ✓", true));
        assert!(written.is_empty(), "written was: {written:?}");
        assert_eq!(read_log(&path), "[2/15] Docker Engine ✓\n");
    }
}
