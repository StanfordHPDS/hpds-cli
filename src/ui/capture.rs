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
//! ([`super::data`]) bypasses it and is never captured.
//!
//! Capture is strictly per thread: while one thread captures, output from
//! any other thread goes to the terminal as usual. Captures nest: an inner
//! [`capture`] records into its own transcript, which is returned to its
//! caller and not added to the outer one; the outer capture resumes when the
//! inner one returns.

use std::cell::RefCell;

/// Which standard stream a line is destined for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// Lines recorded by [`capture`], in the order they were emitted, each with
/// the stream it would have been written to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transcript {
    lines: Vec<(Stream, String)>,
}

impl Transcript {
    /// Whether nothing was recorded.
    #[allow(dead_code)] // part of the ui API; background setup work is its caller
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The recorded lines, in emission order.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[(Stream, String)] {
        &self.lines
    }
}

thread_local! {
    /// The calling thread's open captures, innermost last. Empty when the
    /// thread is not capturing.
    static CAPTURE: RefCell<Vec<Vec<(Stream, String)>>> = const { RefCell::new(Vec::new()) };
}

/// Whether the calling thread is inside [`capture`].
pub(crate) fn is_capturing() -> bool {
    CAPTURE.with(|frames| !frames.borrow().is_empty())
}

/// Run `f` with this thread's hpds output recorded instead of written, and
/// return its value with everything it emitted.
///
/// While capturing, progress bars are hidden and record their notice line,
/// and prompts fail with an error rather than wait on input nobody can see.
/// If `f` panics, the capture is closed and its lines are discarded.
#[allow(dead_code)] // part of the ui API; background setup work is its caller
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Transcript) {
    CAPTURE.with(|frames| frames.borrow_mut().push(Vec::new()));
    let frame = OpenFrame;
    let value = f();
    let lines = frame.close();
    (value, Transcript { lines })
}

/// Write `transcript` out, each line to the stream it was recorded for, in
/// the order it was recorded. Inside a capture the lines are recorded into
/// the enclosing transcript instead.
#[allow(dead_code)] // part of the ui API; background setup work is its caller
pub fn replay(transcript: &Transcript) {
    for (stream, line) in &transcript.lines {
        emit(*stream, line);
    }
}

/// Record `line` when this thread is capturing; otherwise write it to
/// `stream`.
pub(crate) fn emit(stream: Stream, line: &str) {
    let recorded = CAPTURE.with(|frames| match frames.borrow_mut().last_mut() {
        Some(frame) => {
            frame.push((stream, line.to_string()));
            true
        }
        None => false,
    });
    if !recorded {
        match stream {
            Stream::Stdout => std::println!("{line}"),
            Stream::Stderr => std::eprintln!("{line}"),
        }
    }
}

/// The innermost open capture on this thread. Dropping it without
/// [`OpenFrame::close`] (that is, when the captured closure panics) still
/// pops the frame, so the thread does not stay in capture mode.
struct OpenFrame;

impl OpenFrame {
    fn close(self) -> Vec<(Stream, String)> {
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

fn pop_frame() -> Vec<(Stream, String)> {
    CAPTURE
        .try_with(|frames| frames.borrow_mut().pop())
        .ok()
        .flatten()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(stream: Stream, text: &str) -> (Stream, String) {
        (stream, text.to_string())
    }

    #[test]
    fn capture_records_lines_in_order_with_their_streams() {
        let (value, transcript) = capture(|| {
            emit(Stream::Stdout, "one");
            emit(Stream::Stderr, "two");
            emit(Stream::Stdout, "three");
            42
        });
        assert_eq!(value, 42);
        assert_eq!(
            transcript.lines,
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
            emit(Stream::Stdout, "from the capturing thread");
            let other = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        // Another thread is capturing right now; this one is not.
                        let capturing_here = is_capturing();
                        let (_, mine) = capture(|| emit(Stream::Stderr, "from another thread"));
                        (capturing_here, mine)
                    })
                    .join()
                    .unwrap()
            });
            emit(Stream::Stderr, "after the other thread");
            other
        });
        let (capturing_here, other_transcript) = other;
        assert!(!capturing_here);
        assert_eq!(
            other_transcript.lines,
            vec![line(Stream::Stderr, "from another thread")]
        );
        assert_eq!(
            transcript.lines,
            vec![
                line(Stream::Stdout, "from the capturing thread"),
                line(Stream::Stderr, "after the other thread"),
            ]
        );
    }

    #[test]
    fn nested_capture_records_into_its_own_transcript_and_restores_the_outer() {
        let (inner, outer) = capture(|| {
            emit(Stream::Stdout, "outer before");
            let (_, inner) = capture(|| emit(Stream::Stderr, "inner"));
            emit(Stream::Stdout, "outer after");
            inner
        });
        assert_eq!(inner.lines, vec![line(Stream::Stderr, "inner")]);
        assert_eq!(
            outer.lines,
            vec![
                line(Stream::Stdout, "outer before"),
                line(Stream::Stdout, "outer after"),
            ]
        );
    }

    #[test]
    fn replay_inside_a_capture_records_into_the_enclosing_transcript() {
        let (_, inner) = capture(|| {
            emit(Stream::Stdout, "a");
            emit(Stream::Stderr, "b");
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
}
