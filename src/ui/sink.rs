//! The log sink, compact terminal mode, and collected reports.
//!
//! All of this state is scoped to the calling thread. Work that moves to
//! another thread already runs inside [`fn@super::capture`] (its lines would
//! otherwise interleave with the foreground's), and its transcript is
//! replayed on the thread that owns the terminal, so its lines reach that
//! thread's log and reports in order. Keeping the state per thread also
//! lets tests, which run on parallel threads, each open their own log
//! without seeing another test's lines.
//!
//! - **Log.** [`open_log`] appends every line the thread writes out (see
//!   [`super::capture::emit`]) to a file, without color, log-only lines
//!   included. [`close_log`] stops it.
//! - **Compact mode.** While a [`CompactGuard`] from [`compact`] is alive,
//!   informational lines go to the log only. Status lines still reach the
//!   terminal. Warnings and errors still go to the log, and are kept as
//!   [`Report`]s for the caller to drain with [`take_reports`] instead of
//!   being printed inline. Progress bars are hidden. Dropping the guard
//!   restores the previous mode, so an early return cannot leave compact
//!   mode on.
//! - **Inherited log path.** A worker thread does not share the log handle,
//!   but [`with_log_path`] lets it know where the spawning thread's log
//!   lives, so its errors can name the file its replayed lines end up in.
//!
//! Lines emitted by a background step that panics are lost: its capture is
//! discarded with the panic, so they never reach the log, and the panic
//! hook's own message is printed on the worker thread, outside the log.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Which kind of line a [`Report`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportKind {
    /// A [`fn@super::warn`] line.
    Warning,
    /// A [`fn@super::error`] line.
    Error,
}

/// A warning or error kept in compact mode instead of being printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub kind: ReportKind,
    /// The message without its `warning:` prefix or any color. For errors,
    /// the full uncolored rendering (`error:`, `caused by:`, `hint:` lines).
    pub message: String,
}

/// An open log: where it lives and the handle lines are appended through.
struct Log {
    path: PathBuf,
    file: File,
}

thread_local! {
    /// The calling thread's open log, if any.
    static LOG: RefCell<Option<Log>> = const { RefCell::new(None) };
    /// The log path a worker thread was given by [`with_log_path`]: the log
    /// of the thread that spawned it, which receives its replayed lines.
    static INHERITED_LOG_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// Whether the calling thread is in compact terminal mode.
    static COMPACT: Cell<bool> = const { Cell::new(false) };
    /// Warnings and errors kept in compact mode, oldest first.
    static REPORTS: RefCell<Vec<Report>> = const { RefCell::new(Vec::new()) };
}

/// Open `path` as the calling thread's log, appending to it (creating it if
/// absent; never truncating). Replaces any log the thread already had open.
/// On error no log is open, and the caller decides whether to warn and
/// continue.
pub fn open_log(path: &Path) -> io::Result<()> {
    close_log();
    let file = OpenOptions::new().append(true).create(true).open(path)?;
    let log = Log {
        path: path.to_path_buf(),
        file,
    };
    let _ = LOG.try_with(|slot| *slot.borrow_mut() = Some(log));
    Ok(())
}

/// Close the calling thread's log, if one is open. Later lines are no longer
/// written to it.
pub fn close_log() {
    let _ = LOG.try_with(|slot| slot.borrow_mut().take());
}

/// The path of the calling thread's open log, if any; otherwise the path
/// it was given by [`with_log_path`], if any.
pub fn log_path() -> Option<PathBuf> {
    LOG.try_with(|slot| slot.borrow().as_ref().map(|log| log.path.clone()))
        .ok()
        .flatten()
        .or_else(|| {
            INHERITED_LOG_PATH
                .try_with(|slot| slot.borrow().clone())
                .ok()
                .flatten()
        })
}

/// Run `f` with [`log_path`] returning `path` on the calling thread when no
/// log is open there, then restore whatever path it returned before (also
/// when `f` panics). Only the path moves: lines `f` writes out still go to
/// this thread's own log, if any, so a worker thread runs `f` inside
/// [`fn@super::capture`] and the spawning thread replays its lines into the
/// log that `path` names. Capture `path` with [`log_path`] before spawning.
pub fn with_log_path<T>(path: Option<PathBuf>, f: impl FnOnce() -> T) -> T {
    /// Restores the inherited path in force before [`with_log_path`].
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            let _ = INHERITED_LOG_PATH.try_with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let previous = INHERITED_LOG_PATH
        .try_with(|slot| std::mem::replace(&mut *slot.borrow_mut(), path))
        .ok()
        .flatten();
    let _restore = Restore(previous);
    f()
}

/// Append `line` to the calling thread's open log, if any.
///
/// A write that fails after the log was opened (a full disk, a file
/// removed from under a network mount) is dropped without a warning: a
/// warning would itself be a line bound for the same failing log, and the
/// terminal output the user is watching is unaffected either way.
pub(crate) fn write_log(line: &str) {
    let _ = LOG.try_with(|slot| {
        if let Some(log) = slot.borrow_mut().as_mut() {
            let _ = writeln!(log.file, "{line}");
        }
    });
}

/// Turn compact terminal mode on or off for the calling thread.
fn set_compact(compact: bool) {
    let _ = COMPACT.try_with(|mode| mode.set(compact));
}

/// Keeps the calling thread in compact terminal mode while it lives; see
/// [`compact`]. Not `Send`: the guard restores the mode of the thread that
/// created it, so it must be dropped there.
#[must_use = "compact mode ends when the guard is dropped"]
#[derive(Debug)]
pub struct CompactGuard {
    /// The mode in force before the guard was created, restored on drop.
    previous: bool,
    /// Keeps the guard on the thread whose mode it restores.
    _not_send: PhantomData<*const ()>,
}

impl Drop for CompactGuard {
    fn drop(&mut self) {
        set_compact(self.previous);
    }
}

/// Put the calling thread in compact terminal mode until the returned guard
/// is dropped, which restores whatever mode was in force before. Guards
/// nest: dropping an inner guard leaves the outer one's compact mode on.
pub fn compact() -> CompactGuard {
    let previous = is_compact();
    set_compact(true);
    CompactGuard {
        previous,
        _not_send: PhantomData,
    }
}

/// Whether the calling thread is in compact terminal mode.
pub(crate) fn is_compact() -> bool {
    COMPACT.try_with(Cell::get).unwrap_or(false)
}

/// Keep a warning or error for the calling thread's [`take_reports`].
pub(crate) fn push_report(report: Report) {
    let _ = REPORTS.try_with(|reports| reports.borrow_mut().push(report));
}

/// Drain the warnings and errors the calling thread kept in compact mode,
/// in the order they were emitted.
pub fn take_reports() -> Vec<Report> {
    REPORTS
        .try_with(|reports| std::mem::take(&mut *reports.borrow_mut()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::capture::{LineKind, Stream, emit, tap_terminal};
    use std::fs;

    fn info(text: &str) {
        emit(LineKind::Info, text, text);
    }

    #[test]
    fn open_log_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        open_log(&path).unwrap();
        let _compact = compact();
        info("first line");
        close_log();
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "first line\n"
        );
    }

    #[test]
    fn open_log_appends_to_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        fs::write(&path, "written by the bootstrap script\n").unwrap();
        open_log(&path).unwrap();
        let _compact = compact();
        info("from hpds");
        close_log();
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "written by the bootstrap script\nfrom hpds\n"
        );
    }

    #[test]
    fn reopening_the_log_appends_a_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        let _compact = compact();
        open_log(&path).unwrap();
        info("run one");
        close_log();
        open_log(&path).unwrap();
        info("run two");
        close_log();
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "run one\nrun two\n"
        );
    }

    #[test]
    fn open_log_returns_the_error_when_the_file_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-dir").join("hpds-setup.log");
        let result = open_log(&path);
        assert!(
            matches!(&result, Err(err) if err.kind() == io::ErrorKind::NotFound),
            "result was: {result:?}"
        );
        assert_eq!(log_path(), None);
    }

    #[test]
    fn log_path_names_the_open_log_until_it_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        assert_eq!(log_path(), None);
        open_log(&path).unwrap();
        assert_eq!(log_path(), Some(path.clone()));
        close_log();
        assert_eq!(log_path(), None);
    }

    #[test]
    fn lines_after_close_log_are_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        let _compact = compact();
        open_log(&path).unwrap();
        info("kept");
        close_log();
        info("after close");
        assert_eq!(fs::read_to_string(&path).unwrap_or_default(), "kept\n");
    }

    #[test]
    fn compact_mode_is_off_by_default_and_toggles() {
        assert!(!is_compact());
        set_compact(true);
        assert!(is_compact());
        set_compact(false);
        assert!(!is_compact());
    }

    #[test]
    fn compact_guard_turns_compact_mode_on_until_it_is_dropped() {
        assert!(!is_compact());
        let guard = compact();
        assert!(is_compact());
        drop(guard);
        assert!(!is_compact());
    }

    #[test]
    fn nested_compact_guards_restore_the_mode_each_one_found() {
        let outer = compact();
        let inner = compact();
        assert!(is_compact());
        drop(inner);
        assert!(
            is_compact(),
            "dropping the inner guard must keep compact mode on"
        );
        drop(outer);
        assert!(!is_compact());
    }

    #[test]
    fn compact_guard_dropped_by_an_early_return_lets_errors_print_again() {
        fn failing_step() -> anyhow::Result<()> {
            let _compact = compact();
            info("step output, log only");
            Err(anyhow::anyhow!("step failed"))?;
            info("never reached");
            Ok(())
        }
        assert!(failing_step().is_err());
        assert!(!is_compact());
        let ((), written) = tap_terminal(|| crate::ui::error(&anyhow::anyhow!("setup failed")));
        assert_eq!(written.len(), 1, "written was: {written:?}");
        assert_eq!(written[0].0, Stream::Stderr);
        assert!(
            written[0].1.contains("setup failed"),
            "written was: {written:?}"
        );
        assert!(take_reports().is_empty());
    }

    #[test]
    fn log_and_compact_mode_belong_to_the_thread_that_set_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        open_log(&path).unwrap();
        let _compact = compact();
        info("from the logging thread");
        let (other_compact, other_log) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let state = (is_compact(), log_path());
                    let ((), _) = crate::ui::capture::tap_terminal(|| {
                        info("from another thread");
                    });
                    state
                })
                .join()
                .unwrap()
        });
        close_log();
        assert!(!other_compact);
        assert_eq!(other_log, None);
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "from the logging thread\n"
        );
    }

    #[test]
    fn with_log_path_names_the_spawning_threads_log_without_writing_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hpds-setup.log");
        open_log(&path).unwrap();
        let _compact = compact();
        let inherited = log_path();
        let ((seen_inside, transcript), seen_after) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let inside = with_log_path(inherited, || {
                        crate::ui::capture(|| {
                            info("from the worker");
                            log_path()
                        })
                    });
                    (inside, log_path())
                })
                .join()
                .unwrap()
        });
        assert_eq!(seen_inside, Some(path.clone()));
        assert_eq!(
            seen_after, None,
            "the inherited path must not outlive the call"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "",
            "the worker must not write to the log directly"
        );
        crate::ui::replay(&transcript);
        close_log();
        assert_eq!(
            fs::read_to_string(&path).unwrap_or_default(),
            "from the worker\n"
        );
    }

    #[test]
    fn an_open_log_outranks_an_inherited_path() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("own.log");
        let other = dir.path().join("other.log");
        open_log(&own).unwrap();
        let seen = with_log_path(Some(other), log_path);
        close_log();
        assert_eq!(seen, Some(own));
    }

    #[test]
    fn with_log_path_restores_the_previous_path_when_f_panics() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer.log");
        let inner = dir.path().join("inner.log");
        with_log_path(Some(outer.clone()), || {
            let result = std::panic::catch_unwind(|| {
                with_log_path(Some(inner), || panic!("step panicked"));
            });
            assert!(result.is_err());
            assert_eq!(log_path(), Some(outer.clone()));
        });
        assert_eq!(log_path(), None);
    }
}
