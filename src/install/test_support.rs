//! Shared fakes for installer unit tests: a recording [`CommandRunner`],
//! a recording [`ReleaseFetcher`], and an [`InstallCtx`] builder wiring
//! them together with an injected OS.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::anyhow;

use crate::tools::{Os, ToolSpec};

use super::fetch::ReleaseFetcher;
use super::{CommandOutput, CommandRunner, InstallCtx};

/// Fake `CommandRunner`: `paths` answers `which`, `outputs` answers `run`
/// (keyed by the full command line), and every run is recorded.
#[derive(Default)]
pub struct FakeRunner {
    paths: HashMap<String, PathBuf>,
    outputs: HashMap<String, CommandOutput>,
    calls: Mutex<Vec<String>>,
}

impl FakeRunner {
    /// Every recorded command line, in the order it ran.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Record a call that did not go through [`CommandRunner::run`], so
    /// tests can interleave other seams into the same ordered log.
    pub fn record(&self, call: &str) {
        self.calls.lock().unwrap().push(call.to_string());
    }

    pub fn on_path(mut self, program: &str) -> Self {
        self.paths.insert(
            program.to_string(),
            PathBuf::from("/fake/bin").join(program),
        );
        self
    }

    pub fn with_output(mut self, command_line: &str, stdout: &str) -> Self {
        self.outputs.insert(
            command_line.to_string(),
            CommandOutput {
                success: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        );
        self
    }

    pub fn with_failure(mut self, command_line: &str, stderr: &str) -> Self {
        self.outputs.insert(
            command_line.to_string(),
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.to_string(),
            },
        );
        self
    }
}

impl CommandRunner for FakeRunner {
    fn which(&self, program: &str) -> Option<PathBuf> {
        self.paths.get(program).cloned()
    }

    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        let key = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        self.record(&key);
        self.outputs
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("no fake output recorded for `{key}`"))
    }
}

/// One recorded [`ReleaseFetcher::fetch_binary`] request.
#[derive(Debug, Clone)]
pub struct FetchCall {
    pub spec: ToolSpec,
    pub version: String,
    pub bin_dir: PathBuf,
}

/// One recorded [`ReleaseFetcher::fetch_tree`] request.
#[derive(Debug, Clone)]
pub struct TreeFetchCall {
    pub spec: ToolSpec,
    pub version: String,
    pub opt_dir: PathBuf,
    pub bin_dir: PathBuf,
}

/// Fake `ReleaseFetcher`: records requests, downloads nothing.
#[derive(Default)]
pub struct FakeFetcher {
    calls: Mutex<Vec<FetchCall>>,
    tree_calls: Mutex<Vec<TreeFetchCall>>,
    latest_calls: Mutex<Vec<String>>,
    latest: Option<Result<String, String>>,
}

impl FakeFetcher {
    pub fn with_latest(mut self, version: &str) -> Self {
        self.latest = Some(Ok(version.to_string()));
        self
    }

    pub fn with_latest_error(mut self, message: &str) -> Self {
        self.latest = Some(Err(message.to_string()));
        self
    }

    /// Every recorded [`ReleaseFetcher::fetch_binary`] request.
    pub fn calls(&self) -> Vec<FetchCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Every recorded [`ReleaseFetcher::fetch_tree`] request.
    pub fn tree_calls(&self) -> Vec<TreeFetchCall> {
        self.tree_calls.lock().unwrap().clone()
    }

    /// The tool names whose latest release was looked up.
    pub fn latest_calls(&self) -> Vec<String> {
        self.latest_calls.lock().unwrap().clone()
    }
}

impl ReleaseFetcher for FakeFetcher {
    fn latest_version(&self, spec: &ToolSpec) -> anyhow::Result<String> {
        self.latest_calls
            .lock()
            .unwrap()
            .push(spec.name.to_string());
        match &self.latest {
            Some(Ok(version)) => Ok(version.clone()),
            Some(Err(error)) => Err(anyhow!(error.clone())),
            None => Err(anyhow!(
                "no fake latest release configured for {}",
                spec.name
            )),
        }
    }

    fn fetch_binary(
        &self,
        spec: &ToolSpec,
        version: &str,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        self.calls.lock().unwrap().push(FetchCall {
            spec: *spec,
            version: version.to_string(),
            bin_dir: bin_dir.to_path_buf(),
        });
        Ok(bin_dir.join(spec.name))
    }

    fn fetch_tree(
        &self,
        spec: &ToolSpec,
        version: &str,
        opt_dir: &Path,
        bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        self.tree_calls.lock().unwrap().push(TreeFetchCall {
            spec: *spec,
            version: version.to_string(),
            opt_dir: opt_dir.to_path_buf(),
            bin_dir: bin_dir.to_path_buf(),
        });
        Ok(bin_dir.join(spec.name))
    }
}

/// A fetcher for tests whose subject must never download anything.
pub struct PanicFetcher;

impl ReleaseFetcher for PanicFetcher {
    fn latest_version(&self, spec: &ToolSpec) -> anyhow::Result<String> {
        panic!(
            "this test must not look up a latest release (asked for {})",
            spec.name
        );
    }

    fn fetch_binary(
        &self,
        spec: &ToolSpec,
        _version: &str,
        _bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        panic!(
            "this test must not fetch a release binary (asked for {})",
            spec.name
        );
    }

    fn fetch_tree(
        &self,
        spec: &ToolSpec,
        _version: &str,
        _opt_dir: &Path,
        _bin_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        panic!(
            "this test must not fetch a release tree (asked for {})",
            spec.name
        );
    }
}

/// A recorded `--version` output from `tests/fixtures/tool-output/`.
pub fn probe_fixture(name: &str) -> String {
    tool_output_fixture(&format!("version-probes/{name}"))
}

/// A recorded external-tool output from `tests/fixtures/tool-output/`.
pub fn tool_output_fixture(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tool-output")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

/// An `InstallCtx` on `os` with sudo pre-approved (tests run without a
/// terminal, so prompting would refuse) and no version pin.
pub fn ctx_on<'a>(
    os: Os,
    runner: &'a FakeRunner,
    fetcher: &'a (dyn ReleaseFetcher + Sync),
) -> InstallCtx<'a> {
    InstallCtx {
        os,
        yes: true,
        verbose: false,
        pin: None,
        plan_approved: false,
        sudo_approved: std::cell::Cell::new(false),
        runner,
        fetcher,
    }
}
