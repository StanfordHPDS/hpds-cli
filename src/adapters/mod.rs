//! The format/lint adapter layer: one adapter per tool, all speaking the
//! same two traits so `hpds format`/`hpds lint` never care which tool is
//! underneath.
//!
//! Adapters return data — [`FormatOutcome`] and [`Diagnostic`]s — and never
//! print; rendering belongs to the commands via `ui/`. Adding a language is
//! source-level: implement [`Adapter`] and register it for its
//! [`Language`](crate::fsx::Language) bucket in [`AdapterRegistry`].

mod diagnostic;
mod outcome;
mod panache;
mod python;
mod r;
mod registry;
mod runner;
mod sql;
#[cfg(test)]
pub(crate) mod test_support;

use std::path::PathBuf;

use crate::config::Config;
use crate::tools::{InstallContext, ToolSpec};
use crate::ui::HintExt;

pub use diagnostic::{Diagnostic, Position, Range, Severity};
pub use outcome::FormatOutcome;
pub use panache::PanacheAdapter;
pub use python::RuffAdapter;
pub use r::AirAdapter;
pub use registry::AdapterRegistry;
pub use runner::{format_all, lint_all};
pub use sql::SqlFluffAdapter;

/// Formats a batch of files with one underlying tool invocation.
pub trait Formatter {
    /// Format `files` in place, or with `check` report what would change
    /// without touching anything.
    fn format(
        &self,
        files: &[PathBuf],
        check: bool,
        ctx: &ToolCtx,
    ) -> anyhow::Result<FormatOutcome>;
}

/// Lints a batch of files with one underlying tool invocation.
pub trait Linter {
    /// Lint `files`, applying safe autofixes first when `fix` is set, and
    /// report the remaining findings.
    fn lint(&self, files: &[PathBuf], fix: bool, ctx: &ToolCtx) -> anyhow::Result<Vec<Diagnostic>>;
}

/// One tool's adapter: both capabilities plus a stable name.
///
/// The name keys batching and result ordering in the runner, so it must be
/// unique per underlying tool; registering one adapter under several
/// [`Language`](crate::fsx::Language) buckets merges their files into a
/// single invocation.
pub trait Adapter: Formatter + Linter + Send + Sync {
    /// Stable adapter name (the tool it wraps, e.g. `"ruff"`).
    fn name(&self) -> &'static str;
}

/// Everything an adapter needs at run time: how to find its tool binary,
/// the resolved project config, and how chatty to be.
///
/// Adapters resolve binaries through [`ToolCtx::tool_path`] instead of
/// calling the installer directly, so tests can substitute a fake
/// [`ToolPaths`] and no adapter hardwires download logic.
pub struct ToolCtx<'a> {
    tools: &'a dyn ToolPaths,
    pub config: &'a Config,
    /// Verbosity for adapters that want to expose extra detail at `-v`.
    /// Carried on the shared ctx so growing such output never changes the
    /// trait signatures; no adapter reads it yet (hence the allow).
    #[allow(dead_code)]
    pub verbose: bool,
}

impl<'a> ToolCtx<'a> {
    pub fn new(tools: &'a dyn ToolPaths, config: &'a Config, verbose: bool) -> ToolCtx<'a> {
        ToolCtx {
            tools,
            config,
            verbose,
        }
    }

    /// The binary for `tool`, installing it first if needed.
    pub fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
        self.tools.tool_path(tool)
    }
}

/// Resolves a tool name to a runnable binary path.
///
/// `Sync` because the batch runner shares one context across adapter
/// threads.
pub trait ToolPaths: Sync {
    fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf>;
}

/// The production [`ToolPaths`]: resolves through the managed tool cache,
/// downloading on first use (`tools::ensure_installed`).
pub struct InstalledToolPaths<'a> {
    config: &'a Config,
    /// The hpds command on whose behalf tools are installed, e.g.
    /// `"hpds format"`; named in offline-install errors.
    command: &'a str,
    verbose: bool,
}

impl<'a> InstalledToolPaths<'a> {
    pub fn new(config: &'a Config, command: &'a str, verbose: bool) -> InstalledToolPaths<'a> {
        InstalledToolPaths {
            config,
            command,
            verbose,
        }
    }
}

impl ToolPaths for InstalledToolPaths<'_> {
    fn tool_path(&self, tool: &str) -> anyhow::Result<PathBuf> {
        let spec = ToolSpec::builtin(tool)
            .ok_or_else(|| anyhow::anyhow!("no managed tool named `{tool}`"))
            .hint("this is a bug in hpds: an adapter asked for a tool it does not manage; please report it")?;
        let ctx = InstallContext {
            // Friendly label: at normal verbosity the download notice says
            // "Fetching R formatter…", never the tool name.
            label: crate::tools::label_for(tool),
            command: self.command,
            verbose: self.verbose,
        };
        crate::tools::ensure_installed(&spec, &self.config.tools, &ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::FakeToolPaths;

    #[test]
    fn tool_ctx_resolves_binaries_through_the_injected_provider() {
        let paths = FakeToolPaths::with_tool("ruff", "/fake/bin/ruff");
        let config = Config::default();
        let ctx = ToolCtx::new(&paths, &config, false);

        let resolved = ctx.tool_path("ruff").expect("fake provider has ruff");
        assert_eq!(resolved, PathBuf::from("/fake/bin/ruff"));
        // The provider saw the request — nothing touched the real installer.
        assert_eq!(paths.requests(), vec!["ruff".to_string()]);
    }

    #[test]
    fn fake_provider_errors_on_unknown_tools() {
        let paths = FakeToolPaths::with_tool("ruff", "/fake/bin/ruff");
        let config = Config::default();
        let ctx = ToolCtx::new(&paths, &config, false);
        let err = ctx.tool_path("air").expect_err("air is not configured");
        assert!(err.to_string().contains("air"), "{err}");
    }

    #[test]
    fn installed_tool_paths_rejects_unmanaged_tool_names_with_a_hint() {
        let config = Config::default();
        let paths = InstalledToolPaths::new(&config, "hpds format", false);
        let err = paths
            .tool_path("not-a-tool")
            .expect_err("unknown tools must not reach the installer");
        let rendered = crate::ui::render_error(&err, false);
        assert!(
            rendered.contains("not-a-tool"),
            "names the tool: {rendered}"
        );
        assert!(rendered.contains("hint:"), "says what to do: {rendered}");
    }
}
