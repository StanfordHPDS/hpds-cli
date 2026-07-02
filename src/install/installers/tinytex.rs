//! Installer for TinyTeX, via quarto's bundled tool manager.
//!
//! `quarto install tinytex --update-path` is the strategy on every OS, so
//! quarto must be installed first — a missing quarto is an error pointing
//! at `hpds install quarto`. Detection reads the tinytex row of
//! `quarto list tools`, and falls back to `tlmgr` for TeX installed
//! outside quarto (which must never be clobbered by a reinstall).

use anyhow::anyhow;

use crate::install::{InstallCtx, Installer, extract_version};
use crate::ui::HintExt;

use super::on_path;

pub struct TinyTex;

/// What the tinytex row of `quarto list tools` reports.
#[derive(Debug, PartialEq, Eq)]
enum ListedTinyTex {
    /// quarto manages a TinyTeX at this version.
    Installed(String),
    /// quarto found a TeX distribution it does not manage.
    External,
    /// quarto found no TeX at all (or the row is missing).
    Absent,
}

impl Installer for TinyTex {
    fn name(&self) -> &'static str {
        "tinytex"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        if on_path(ctx, "quarto")
            && let Ok(out) = ctx.runner.run("quarto", &["list", "tools"])
            && out.success
        {
            // quarto prints the table on stderr; scan both streams.
            let listing = format!("{}\n{}", out.stdout, out.stderr);
            match tinytex_row(&listing) {
                ListedTinyTex::Installed(version) => return Some(version),
                // An external TeX must read as installed even when tlmgr
                // is off PATH, so a reinstall never clobbers it. The
                // stand-in slots into "tinytex <version> already
                // installed", so phrase it as a parenthetical.
                ListedTinyTex::External => {
                    return Some(
                        tlmgr_version(ctx).unwrap_or_else(|| "(external TeX)".to_string()),
                    );
                }
                ListedTinyTex::Absent => {}
            }
        }
        tlmgr_version(ctx)
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        if !on_path(ctx, "quarto") {
            return Err(anyhow!(
                "installing tinytex needs quarto, which is not installed"
            ))
            .hint("run `hpds install quarto` first, then re-run `hpds install tinytex`");
        }
        let mut args = vec!["install", "tinytex", "--update-path"];
        if ctx.yes {
            args.push("--no-prompt");
        }
        ctx.run_step("installing tinytex with quarto", "quarto", &args)?;
        Ok(())
    }
}

/// Parse the tinytex row out of a `quarto list tools` listing. The row
/// reads `tinytex <status words...> <installed> <latest>`, where the
/// installed column is a version like `v2026.07` or `---`.
fn tinytex_row(listing: &str) -> ListedTinyTex {
    for line in listing.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.first() != Some(&"tinytex") {
            continue;
        }
        if tokens.len() >= 3
            && let Some(version) = extract_version(tokens[tokens.len() - 2])
        {
            return ListedTinyTex::Installed(version);
        }
        if line.contains("External Installation") {
            return ListedTinyTex::External;
        }
        return ListedTinyTex::Absent;
    }
    ListedTinyTex::Absent
}

/// The TeX Live year reported by `tlmgr --version`, when tlmgr is on
/// PATH (`TeX Live (https://tug.org/texlive) version 2024` → `2024`).
fn tlmgr_version(ctx: &InstallCtx) -> Option<String> {
    ctx.runner.which("tlmgr")?;
    let out = ctx.runner.run("tlmgr", &["--version"]).ok()?;
    if !out.success {
        return None;
    }
    texlive_year(&out.stdout)
}

/// Pull the release year out of the `TeX Live ... version <year>` line.
fn texlive_year(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains("TeX Live"))?
        .split_whitespace()
        .next_back()
        .filter(|token| !token.is_empty() && token.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, tool_output_fixture};
    use crate::tools::Os;
    use crate::ui::render_error;

    fn list_tools_fixture(name: &str) -> String {
        tool_output_fixture(&format!("tinytex/{name}"))
    }

    // --- parsing the recorded listings -------------------------------------

    #[test]
    fn tinytex_row_reads_a_quarto_managed_install() {
        assert_eq!(
            tinytex_row(&list_tools_fixture("quarto-list-tools-managed.txt")),
            ListedTinyTex::Installed("2026.07".to_string())
        );
    }

    #[test]
    fn tinytex_row_reads_an_external_installation() {
        // Recorded from a machine whose TeX predates quarto: the row says
        // `External Installation` with `---` in the installed column (and
        // the listing carries quarto's spinner/ANSI noise).
        assert_eq!(
            tinytex_row(&list_tools_fixture("quarto-list-tools-external.txt")),
            ListedTinyTex::External
        );
    }

    #[test]
    fn tinytex_row_reads_a_missing_install() {
        assert_eq!(
            tinytex_row(&list_tools_fixture("quarto-list-tools-not-installed.txt")),
            ListedTinyTex::Absent
        );
    }

    #[test]
    fn tinytex_row_tolerates_a_listing_without_the_row() {
        assert_eq!(
            tinytex_row("Tool Status\nchromium Not installed ---\n"),
            ListedTinyTex::Absent
        );
        assert_eq!(tinytex_row(""), ListedTinyTex::Absent);
    }

    #[test]
    fn texlive_year_parses_the_recorded_tlmgr_output() {
        assert_eq!(
            texlive_year(&list_tools_fixture("tlmgr-version.txt")).as_deref(),
            Some("2024")
        );
    }

    #[test]
    fn texlive_year_misses_when_the_line_is_absent() {
        assert_eq!(texlive_year("tlmgr revision 73493\n"), None);
        assert_eq!(texlive_year(""), None);
    }

    // --- detection ---------------------------------------------------------

    #[test]
    fn tinytex_detects_a_quarto_managed_install() {
        let runner = FakeRunner::default().on_path("quarto").with_output(
            "quarto list tools",
            &list_tools_fixture("quarto-list-tools-managed.txt"),
        );
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(TinyTex.detect(&ctx).as_deref(), Some("2026.07"));
    }

    #[test]
    fn tinytex_detects_an_external_tex_through_tlmgr() {
        let runner = FakeRunner::default()
            .on_path("quarto")
            .with_output(
                "quarto list tools",
                &list_tools_fixture("quarto-list-tools-external.txt"),
            )
            .on_path("tlmgr")
            .with_output("tlmgr --version", &list_tools_fixture("tlmgr-version.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Linux, &runner, &fetcher);
        assert_eq!(TinyTex.detect(&ctx).as_deref(), Some("2024"));
    }

    #[test]
    fn tinytex_reports_an_external_tex_even_without_tlmgr() {
        // quarto sees an external TeX but tlmgr is off PATH: still
        // installed — a reinstall over it would clobber the user's TeX.
        // The stand-in "version" must read sensibly inside the shared
        // "tinytex <version> already installed" message.
        let runner = FakeRunner::default().on_path("quarto").with_output(
            "quarto list tools",
            &list_tools_fixture("quarto-list-tools-external.txt"),
        );
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(TinyTex.detect(&ctx).as_deref(), Some("(external TeX)"));
    }

    #[test]
    fn tinytex_detects_through_tlmgr_when_quarto_is_absent() {
        let runner = FakeRunner::default()
            .on_path("tlmgr")
            .with_output("tlmgr --version", &list_tools_fixture("tlmgr-version.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Windows, &runner, &fetcher);
        assert_eq!(TinyTex.detect(&ctx).as_deref(), Some("2024"));
    }

    #[test]
    fn tinytex_detects_nothing_when_neither_quarto_nor_tlmgr_exist() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(TinyTex.detect(&ctx), None);
        assert!(
            runner.calls.borrow().is_empty(),
            "must not run anything that is not on PATH"
        );
    }

    #[test]
    fn tinytex_detects_nothing_when_quarto_reports_no_tex() {
        let runner = FakeRunner::default().on_path("quarto").with_output(
            "quarto list tools",
            &list_tools_fixture("quarto-list-tools-not-installed.txt"),
        );
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(TinyTex.detect(&ctx), None);
    }

    // --- install -----------------------------------------------------------

    #[test]
    fn tinytex_installs_with_quarto_on_every_os() {
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            let runner = FakeRunner::default()
                .on_path("quarto")
                .with_output("quarto install tinytex --update-path --no-prompt", "");
            let fetcher = FakeFetcher::default();
            TinyTex
                .install(&ctx_on(os, &runner, &fetcher))
                .expect("quarto install must succeed");
            assert_eq!(
                *runner.calls.borrow(),
                vec!["quarto install tinytex --update-path --no-prompt"],
                "{os:?}"
            );
            assert!(fetcher.calls.borrow().is_empty(), "{os:?}");
            assert!(fetcher.tree_calls.borrow().is_empty(), "{os:?}");
        }
    }

    #[test]
    fn tinytex_without_yes_lets_quarto_prompt() {
        let runner = FakeRunner::default()
            .on_path("quarto")
            .with_output("quarto install tinytex --update-path", "");
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            yes: false,
            ..ctx_on(Os::Mac, &runner, &fetcher)
        };
        TinyTex.install(&ctx).expect("quarto install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["quarto install tinytex --update-path"]
        );
    }

    #[test]
    fn tinytex_without_quarto_errors_with_the_install_quarto_hint() {
        for os in [Os::Mac, Os::Linux, Os::Windows] {
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();
            let err = TinyTex
                .install(&ctx_on(os, &runner, &fetcher))
                .expect_err("missing quarto must be a clean error");
            let out = render_error(&err, false);
            assert!(out.contains("needs quarto"), "{os:?}: {out}");
            assert!(out.contains("hpds install quarto"), "{os:?}: {out}");
            assert!(runner.calls.borrow().is_empty(), "{os:?}");
        }
    }

    #[test]
    fn tinytex_does_not_support_version_pins() {
        assert!(!TinyTex.supports_pin());
    }
}
