//! Error rendering: styled `error:` prefix, cause chain, and an optional
//! `hint:` line when a fix can be suggested.

use std::fmt;

use super::{ERROR_STYLE, HINT_STYLE, Stream, emit, paint, stderr_colors};

/// A remediation hint riding along an error chain, attached via
/// [`HintExt::hint`].
///
/// The wrapper takes the place of the error it wraps in the chain: it
/// displays as that error's message, and its source is that error's
/// source. Each message therefore appears once in `{:#}` formatting and
/// in [`render_error`], which also renders the hint text as a trailing
/// `hint:` line so commands can tell users what to do next.
#[derive(Debug)]
struct Hinted {
    hint: String,
    source: anyhow::Error,
}

impl Hinted {
    /// The hint attached closest to the failure among directly nested
    /// wrappers, which skip one another in the chain.
    fn innermost(&self) -> &Hinted {
        let mut hinted = self;
        while let Some(inner) = hinted.source.downcast_ref::<Hinted>() {
            hinted = inner;
        }
        hinted
    }
}

impl fmt::Display for Hinted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.source, f)
    }
}

impl std::error::Error for Hinted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// Attach a remediation hint to an error result.
pub trait HintExt<T> {
    /// Wrap the error with a hint that [`render_error`] shows as a
    /// `hint:` line.
    fn hint(self, hint: impl Into<String>) -> anyhow::Result<T>;
}

impl<T> HintExt<T> for anyhow::Result<T> {
    fn hint(self, hint: impl Into<String>) -> anyhow::Result<T> {
        self.map_err(|source| {
            anyhow::Error::new(Hinted {
                hint: hint.into(),
                source,
            })
        })
    }
}

/// Render an error for the terminal: `error: <message>`, one `caused by:`
/// line per cause, and a final `hint:` line when the chain carries a hint
/// attached via [`HintExt::hint`].
pub fn render_error(err: &anyhow::Error, use_color: bool) -> String {
    // Hints ride along as `Hinted` wrappers (see `HintExt`); pull them out so
    // they render as a trailing `hint:` line rather than as causes. A
    // `Hinted` stands in for the error it wraps, so its message stays.
    let mut hint = None;
    let mut messages = Vec::new();
    for cause in err.chain() {
        // Innermost hint wins: it was attached closest to the failure.
        if let Some(h) = cause.downcast_ref::<Hinted>() {
            hint = Some(h.innermost());
        }
        messages.push(cause.to_string());
    }

    let mut out = String::new();
    let mut messages = messages.into_iter();
    let first = messages
        .next()
        .unwrap_or_else(|| "unknown error".to_string());
    out.push_str(&paint(ERROR_STYLE, "error:", use_color));
    out.push(' ');
    out.push_str(&first);
    for cause in messages {
        out.push('\n');
        out.push_str(&paint(ERROR_STYLE, "caused by:", use_color));
        out.push(' ');
        out.push_str(&cause);
    }
    if let Some(hint) = hint {
        out.push('\n');
        out.push_str(&paint(HINT_STYLE, "hint:", use_color));
        out.push(' ');
        out.push_str(&hint.hint);
    }
    out
}

/// Print `err` to stderr in the standard `hpds` error format.
pub fn error(err: &anyhow::Error) {
    emit(Stream::Stderr, &render_error(err, stderr_colors()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    const ESC: &str = "\x1b[";

    #[test]
    fn renders_error_prefix_for_a_simple_error() {
        let err = anyhow!("could not read hpds.toml");
        assert_eq!(render_error(&err, false), "error: could not read hpds.toml");
    }

    #[test]
    fn renders_cause_chain_outermost_first() {
        let err = anyhow!("permission denied")
            .context("could not open `hpds.toml`")
            .context("failed to load config");
        let out = render_error(&err, false);
        assert_eq!(
            out,
            "error: failed to load config\n\
             caused by: could not open `hpds.toml`\n\
             caused by: permission denied"
        );
    }

    #[test]
    fn renders_hint_as_trailing_hint_line_not_a_cause() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("`gh` not found on PATH"))
            .hint("run `hpds install gh` to install it")
            .unwrap_err();
        let out = render_error(&err, false);
        assert_eq!(
            out,
            "error: `gh` not found on PATH\n\
             hint: run `hpds install gh` to install it"
        );
        assert!(!out.contains("caused by"));
    }

    #[test]
    fn renders_hint_after_cause_chain() {
        let err: anyhow::Error =
            Err::<(), _>(anyhow!("connection refused").context("could not download gh"))
                .hint("check your network connection or proxy settings")
                .unwrap_err();
        let out = render_error(&err, false);
        assert_eq!(
            out,
            "error: could not download gh\n\
             caused by: connection refused\n\
             hint: check your network connection or proxy settings"
        );
    }

    #[test]
    fn alternate_format_of_a_hinted_error_shows_its_message_once() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("user.name is not set"))
            .hint("set it with git config")
            .unwrap_err();
        assert_eq!(format!("{err:#}"), "user.name is not set");
        assert_eq!(err.to_string(), "user.name is not set");
    }

    #[test]
    fn alternate_format_keeps_the_context_chain_around_a_hint() {
        let err: anyhow::Error =
            Err::<(), _>(anyhow!("connection refused").context("could not download gh"))
                .hint("check your network connection")
                .unwrap_err()
                .context("installing gh");
        assert_eq!(
            format!("{err:#}"),
            "installing gh: could not download gh: connection refused"
        );
        assert_eq!(
            render_error(&err, false),
            "error: installing gh\n\
             caused by: could not download gh\n\
             caused by: connection refused\n\
             hint: check your network connection"
        );
    }

    #[test]
    fn nested_hints_render_once_with_the_innermost_hint() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("boom"))
            .hint("inner hint")
            .hint("outer hint")
            .unwrap_err();
        assert_eq!(format!("{err:#}"), "boom");
        assert_eq!(render_error(&err, false), "error: boom\nhint: inner hint");
    }

    #[test]
    fn non_tty_error_output_has_no_ansi_codes() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("root cause").context("outer message"))
            .hint("try again")
            .unwrap_err();
        assert!(!render_error(&err, false).contains(ESC));
    }

    #[test]
    fn colored_error_output_styles_the_prefixes() {
        let err: anyhow::Error = Err::<(), _>(anyhow!("boom"))
            .hint("do the thing")
            .unwrap_err();
        let out = render_error(&err, true);
        assert!(out.contains(ESC));
        assert!(out.contains("error:"));
        assert!(out.contains("hint:"));
    }
}
