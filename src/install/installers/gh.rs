//! Installer for `gh` (the GitHub CLI).
//!
//! macOS: Homebrew when present, else the release binary. Linux: the
//! official GitHub CLI apt repository when apt is present, else the
//! release binary. Windows: winget when present, else the release binary.
//! A `--version` pin always takes the release-binary path (package
//! managers install whatever they ship), except on Windows where winget
//! pins natively.

use crate::install::{InstallCtx, Installer};
use crate::tools::{Os, ToolSpec, versions};
use crate::ui::HintExt;

use super::{fetch_plan, fetch_to_user_bin, on_path};

pub struct Gh;

/// Where the apt strategy installs the GitHub CLI signing key.
const KEYRING: &str = "/etc/apt/keyrings/githubcli-archive-keyring.gpg";

/// The gh release archive for one OS. gh names assets `macOS`/`linux`/
/// `windows` with Go-style arches, and macOS archives are zips.
pub(super) fn release_spec(os: Os) -> ToolSpec {
    ToolSpec {
        name: "gh",
        default_version: versions::GH,
        repo: "cli/cli",
        asset_pattern: match os {
            Os::Mac => "gh_{version}_macOS_{alt-arch}.zip",
            Os::Linux => "gh_{version}_linux_{alt-arch}.tar.gz",
            Os::Windows => "gh_{version}_windows_{alt-arch}.zip",
        },
        checksum_pattern: Some("gh_{version}_checksums.txt"),
    }
}

impl Installer for Gh {
    fn name(&self) -> &'static str {
        "gh"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("gh")
    }

    fn supports_pin(&self) -> bool {
        true
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        let version = ctx.pin.clone().unwrap_or_else(|| versions::GH.to_string());
        match ctx.os {
            Os::Mac => {
                if ctx.pin.is_none() && on_path(ctx, "brew") {
                    vec!["brew install gh".to_string()]
                } else {
                    vec![fetch_plan(&release_spec(ctx.os), &version)]
                }
            }
            Os::Linux => {
                if ctx.pin.is_none() && on_path(ctx, "apt-get") {
                    apt_plan()
                } else {
                    vec![fetch_plan(&release_spec(ctx.os), &version)]
                }
            }
            Os::Windows => {
                if on_path(ctx, "winget") {
                    let mut line = "winget install --id GitHub.cli --exact".to_string();
                    if let Some(pin) = ctx.pin.as_deref() {
                        line.push_str(&format!(" --version {pin}"));
                    }
                    vec![line]
                } else {
                    vec![fetch_plan(&release_spec(ctx.os), &version)]
                }
            }
        }
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        let version = ctx.pin.clone().unwrap_or_else(|| versions::GH.to_string());
        match ctx.os {
            Os::Mac => {
                if ctx.pin.is_none() && on_path(ctx, "brew") {
                    ctx.run_step("installing gh with Homebrew", "brew", &["install", "gh"])?;
                } else {
                    fetch_to_user_bin(ctx, &release_spec(ctx.os), &version)?;
                }
            }
            Os::Linux => {
                if ctx.pin.is_none() && on_path(ctx, "apt-get") {
                    install_from_apt(ctx)?;
                } else {
                    fetch_to_user_bin(ctx, &release_spec(ctx.os), &version)?;
                }
            }
            Os::Windows => {
                if on_path(ctx, "winget") {
                    let mut args = vec!["install", "--id", "GitHub.cli", "--exact"];
                    if let Some(pin) = ctx.pin.as_deref() {
                        args.extend(["--version", pin]);
                    }
                    ctx.run_step("installing gh with winget", "winget", &args)?;
                } else {
                    fetch_to_user_bin(ctx, &release_spec(ctx.os), &version)?;
                }
            }
        }
        Ok(())
    }
}

/// The plan lines for [`install_from_apt`]: the same commands, one per
/// line, with the arch spelled as the probe that resolves it.
fn apt_plan() -> Vec<String> {
    vec![
        "sudo mkdir -p -m 755 /etc/apt/keyrings".to_string(),
        format!(
            "sudo curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
             -o {KEYRING}"
        ),
        format!("sudo chmod go+r {KEYRING}"),
        "register the GitHub CLI apt repository in \
         /etc/apt/sources.list.d/github-cli.list (sudo)"
            .to_string(),
        "sudo apt-get update".to_string(),
        "sudo apt-get install -y gh".to_string(),
    ]
}

/// The official GitHub CLI apt repository steps, run natively one command
/// at a time (no shell pipelines), each gated behind the sudo prompt.
fn install_from_apt(ctx: &InstallCtx) -> anyhow::Result<()> {
    ctx.run_sudo_step(
        "creating the apt keyrings directory",
        "mkdir",
        &["-p", "-m", "755", "/etc/apt/keyrings"],
    )?;
    ctx.run_sudo_step(
        "adding the GitHub CLI signing key",
        "curl",
        &[
            "-fsSL",
            "https://cli.github.com/packages/githubcli-archive-keyring.gpg",
            "-o",
            KEYRING,
        ],
    )?;
    ctx.run_sudo_step(
        "making the signing key readable",
        "chmod",
        &["go+r", KEYRING],
    )?;
    let arch = ctx
        .run_step(
            "checking the apt architecture",
            "dpkg",
            &["--print-architecture"],
        )?
        .stdout
        .trim()
        .to_string();
    if arch.is_empty() {
        return Err(anyhow::anyhow!(
            "`dpkg --print-architecture` reported no architecture"
        ))
        .hint("this machine's apt setup looks broken; install gh from the release binary instead");
    }
    let sources_line = format!(
        "echo 'deb [arch={arch} signed-by={KEYRING}] https://cli.github.com/packages stable main' \
         > /etc/apt/sources.list.d/github-cli.list"
    );
    ctx.run_sudo_step(
        "registering the GitHub CLI apt repository",
        "sh",
        &["-c", &sources_line],
    )?;
    ctx.run_sudo_step("refreshing apt package lists", "apt-get", &["update"])?;
    ctx.run_sudo_step(
        "installing gh with apt",
        "apt-get",
        &["install", "-y", "gh"],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::tools::{Arch, Platform};
    use crate::ui::render_error;

    #[test]
    fn gh_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("gh")
            .with_output("gh --version", &probe_fixture("gh.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(Gh.detect(&ctx).as_deref(), Some("2.95.0"));
    }

    #[test]
    fn gh_mac_prefers_brew_when_present() {
        let runner = FakeRunner::default()
            .on_path("brew")
            .with_output("brew install gh", "");
        let fetcher = FakeFetcher::default();
        Gh.install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("brew install must succeed");
        assert_eq!(*runner.calls.borrow(), vec!["brew install gh"]);
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn gh_mac_without_brew_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Gh.install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("fetch must succeed");
        let calls = fetcher.calls.borrow();
        assert_eq!(calls[0].spec.name, "gh");
        assert_eq!(calls[0].version, versions::GH);
    }

    #[test]
    fn gh_linux_with_apt_mirrors_the_official_repo_steps() {
        let runner = FakeRunner::default()
            .on_path("apt-get")
            .with_output("sudo mkdir -p -m 755 /etc/apt/keyrings", "")
            .with_output(
                "sudo curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
                 -o /etc/apt/keyrings/githubcli-archive-keyring.gpg",
                "",
            )
            .with_output(
                "sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg",
                "",
            )
            .with_output("dpkg --print-architecture", "amd64\n")
            .with_output(
                "sudo sh -c echo 'deb [arch=amd64 \
                 signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] \
                 https://cli.github.com/packages stable main' \
                 > /etc/apt/sources.list.d/github-cli.list",
                "",
            )
            .with_output("sudo apt-get update", "")
            .with_output("sudo apt-get install -y gh", "");
        let fetcher = FakeFetcher::default();
        Gh.install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect("apt install must succeed");
        assert!(fetcher.calls.borrow().is_empty(), "apt path must not fetch");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 7, "{calls:#?}");
        assert!(calls[0].starts_with("sudo mkdir"), "{calls:#?}");
        assert!(calls[3].starts_with("dpkg"), "{calls:#?}");
        assert!(calls[4].contains("arch=amd64"), "{calls:#?}");
        assert_eq!(calls[6], "sudo apt-get install -y gh");
    }

    #[test]
    fn gh_linux_without_apt_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Gh.install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect("fetch must succeed");
        assert!(runner.calls.borrow().is_empty());
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "gh");
    }

    #[test]
    fn gh_windows_uses_winget_when_present() {
        let runner = FakeRunner::default()
            .on_path("winget")
            .with_output("winget install --id GitHub.cli --exact", "");
        let fetcher = FakeFetcher::default();
        Gh.install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("winget install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id GitHub.cli --exact"]
        );
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn gh_windows_winget_pins_with_its_version_flag() {
        let runner = FakeRunner::default().on_path("winget").with_output(
            "winget install --id GitHub.cli --exact --version 2.90.0",
            "",
        );
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some("2.90.0".to_string()),
            ..ctx_on(Os::Windows, &runner, &fetcher)
        };
        Gh.install(&ctx)
            .expect("pinned winget install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id GitHub.cli --exact --version 2.90.0"]
        );
    }

    #[test]
    fn gh_windows_without_winget_fetches_the_release_binary() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        Gh.install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("fetch must succeed");
        assert_eq!(fetcher.calls.borrow()[0].spec.name, "gh");
    }

    #[test]
    fn gh_pin_forces_the_release_binary_over_package_managers() {
        for os in [Os::Mac, Os::Linux] {
            let runner = FakeRunner::default().on_path("brew").on_path("apt-get");
            let fetcher = FakeFetcher::default();
            let ctx = InstallCtx {
                pin: Some("2.90.0".to_string()),
                ..ctx_on(os, &runner, &fetcher)
            };
            Gh.install(&ctx).expect("pinned fetch must succeed");
            assert!(runner.calls.borrow().is_empty(), "{os:?}");
            assert_eq!(fetcher.calls.borrow()[0].version, "2.90.0", "{os:?}");
        }
    }

    #[test]
    fn gh_plan_mirrors_the_strategy_selection() {
        let fetcher = FakeFetcher::default();

        let with_brew = FakeRunner::default().on_path("brew");
        assert_eq!(
            Gh.plan(&ctx_on(Os::Mac, &with_brew, &fetcher)),
            vec!["brew install gh".to_string()]
        );

        let bare = FakeRunner::default();
        let plan = Gh.plan(&ctx_on(Os::Mac, &bare, &fetcher));
        assert!(plan[0].contains("download"), "{plan:?}");
        assert!(plan[0].contains(versions::GH), "{plan:?}");
        assert!(
            plan[0].contains("github.com/cli/cli"),
            "the plan must name where the binary comes from: {plan:?}"
        );
    }

    #[test]
    fn gh_plan_on_linux_lists_the_apt_commands() {
        let runner = FakeRunner::default().on_path("apt-get");
        let fetcher = FakeFetcher::default();
        let plan = Gh.plan(&ctx_on(Os::Linux, &runner, &fetcher));
        assert!(
            plan.iter().any(|l| l == "sudo apt-get install -y gh"),
            "{plan:?}"
        );
        assert!(
            plan.iter().filter(|l| l.contains("sudo")).count() >= 4,
            "every privileged apt step must be visible: {plan:?}"
        );
        assert!(runner.calls.borrow().is_empty(), "planning must not run");
    }

    #[test]
    fn gh_release_assets_resolve_to_the_published_names() {
        let arm = |os| Platform {
            os,
            arch: Arch::Aarch64,
        };
        let cases = [
            (Os::Mac, "gh_2.96.0_macOS_arm64.zip"),
            (Os::Linux, "gh_2.96.0_linux_arm64.tar.gz"),
            (Os::Windows, "gh_2.96.0_windows_arm64.zip"),
        ];
        for (os, want) in cases {
            let spec = release_spec(os);
            assert_eq!(spec.asset_name(arm(os), "2.96.0"), want, "{os:?}");
            assert_eq!(
                spec.checksum_asset_name(arm(os), "2.96.0").as_deref(),
                Some("gh_2.96.0_checksums.txt"),
                "{os:?}"
            );
        }
    }

    #[test]
    fn gh_linux_apt_with_a_broken_dpkg_errors_with_guidance() {
        let runner = FakeRunner::default()
            .on_path("apt-get")
            .with_output("sudo mkdir -p -m 755 /etc/apt/keyrings", "")
            .with_output(
                "sudo curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
                 -o /etc/apt/keyrings/githubcli-archive-keyring.gpg",
                "",
            )
            .with_output(
                "sudo chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg",
                "",
            )
            .with_output("dpkg --print-architecture", "  ");
        let fetcher = FakeFetcher::default();
        let err = Gh
            .install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect_err("empty dpkg arch must fail");
        let out = render_error(&err, false);
        assert!(out.contains("dpkg"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }
}
