//! Installer for `rig` (the R Installation Manager).
//!
//! macOS: the r-lib/rig Homebrew tap (rig's supported installer path when
//! brew is around). Linux: rig's apt repository, per its docs. Windows:
//! winget (`posit.rig`). Where the needed package manager is missing, the
//! error says exactly where to get rig instead.

use anyhow::anyhow;

use crate::install::{InstallCtx, Installer};
use crate::tools::Os;
use crate::ui::HintExt;

use super::on_path;

pub struct Rig;

impl Installer for Rig {
    fn name(&self) -> &'static str {
        "rig"
    }

    fn detect(&self, ctx: &InstallCtx) -> Option<String> {
        ctx.probe_version("rig")
    }

    fn plan(&self, ctx: &InstallCtx) -> Vec<String> {
        // Each OS has exactly one strategy; when its package manager is
        // missing, `install` errors with guidance before running anything.
        match ctx.os {
            Os::Mac => vec![
                "brew tap r-lib/rig".to_string(),
                "brew install --cask rig".to_string(),
            ],
            Os::Linux => vec![
                "sudo curl -L https://rig.r-pkg.org/deb/rig.gpg \
                 -o /etc/apt/trusted.gpg.d/rig.gpg"
                    .to_string(),
                "register the rig apt repository in /etc/apt/sources.list.d/rig.list (sudo)"
                    .to_string(),
                "sudo apt-get update".to_string(),
                "sudo apt-get install -y r-rig".to_string(),
            ],
            Os::Windows => vec!["winget install --id posit.rig --exact".to_string()],
        }
    }

    fn install(&self, ctx: &InstallCtx) -> anyhow::Result<()> {
        match ctx.os {
            Os::Mac => {
                if !on_path(ctx, "brew") {
                    return Err(anyhow!(
                        "installing rig on macOS needs Homebrew, which is not on PATH"
                    ))
                    .hint(
                        "install Homebrew (https://brew.sh) and re-run, or download the rig \
                         installer from https://github.com/r-lib/rig/releases",
                    );
                }
                ctx.run_step(
                    "adding the r-lib/rig Homebrew tap",
                    "brew",
                    &["tap", "r-lib/rig"],
                )?;
                ctx.run_step(
                    "installing rig with Homebrew",
                    "brew",
                    &["install", "--cask", "rig"],
                )?;
                Ok(())
            }
            Os::Linux => {
                if !on_path(ctx, "apt-get") {
                    return Err(anyhow!(
                        "installing rig on Linux needs apt, which is not on this machine"
                    ))
                    .hint(
                        "see https://github.com/r-lib/rig for RPM packages and tarball installs",
                    );
                }
                install_from_apt(ctx)
            }
            Os::Windows => {
                if !on_path(ctx, "winget") {
                    return Err(anyhow!(
                        "installing rig on Windows needs winget, which is not on PATH"
                    ))
                    .hint(
                        "download the rig installer from https://github.com/r-lib/rig/releases \
                         and run it",
                    );
                }
                ctx.run_step(
                    "installing rig with winget",
                    "winget",
                    &["install", "--id", "posit.rig", "--exact"],
                )?;
                Ok(())
            }
        }
    }
}

/// rig's documented Debian/Ubuntu repository steps, run natively one
/// command at a time. The apt package is named `r-rig` (`rig` is an
/// unrelated Debian package).
fn install_from_apt(ctx: &InstallCtx) -> anyhow::Result<()> {
    ctx.run_sudo_step(
        "adding the rig signing key",
        "curl",
        &[
            "-L",
            "https://rig.r-pkg.org/deb/rig.gpg",
            "-o",
            "/etc/apt/trusted.gpg.d/rig.gpg",
        ],
    )?;
    ctx.run_sudo_step(
        "registering the rig apt repository",
        "sh",
        &[
            "-c",
            "echo 'deb http://rig.r-pkg.org/deb rig main' > /etc/apt/sources.list.d/rig.list",
        ],
    )?;
    ctx.run_sudo_step("refreshing apt package lists", "apt-get", &["update"])?;
    ctx.run_sudo_step(
        "installing rig with apt",
        "apt-get",
        &["install", "-y", "r-rig"],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on, probe_fixture};
    use crate::ui::render_error;

    #[test]
    fn rig_detects_installed_version_from_probe() {
        let runner = FakeRunner::default()
            .on_path("rig")
            .with_output("rig --version", &probe_fixture("rig.txt"));
        let fetcher = FakeFetcher::default();
        let ctx = ctx_on(Os::Mac, &runner, &fetcher);
        assert_eq!(Rig.detect(&ctx).as_deref(), Some("0.8.1"));
    }

    #[test]
    fn rig_mac_taps_and_installs_the_cask_with_brew() {
        let runner = FakeRunner::default()
            .on_path("brew")
            .with_output("brew tap r-lib/rig", "")
            .with_output("brew install --cask rig", "");
        let fetcher = FakeFetcher::default();
        Rig.install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect("brew install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["brew tap r-lib/rig", "brew install --cask rig"]
        );
        assert!(fetcher.calls.borrow().is_empty());
    }

    #[test]
    fn rig_mac_without_brew_errors_with_guidance() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let err = Rig
            .install(&ctx_on(Os::Mac, &runner, &fetcher))
            .expect_err("no brew must be a clean error");
        let out = render_error(&err, false);
        assert!(out.contains("Homebrew"), "{out}");
        assert!(out.contains("r-lib/rig/releases"), "{out}");
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn rig_linux_with_apt_adds_the_rig_repo_and_installs_r_rig() {
        let runner = FakeRunner::default()
            .on_path("apt-get")
            .with_output(
                "sudo curl -L https://rig.r-pkg.org/deb/rig.gpg -o /etc/apt/trusted.gpg.d/rig.gpg",
                "",
            )
            .with_output(
                "sudo sh -c echo 'deb http://rig.r-pkg.org/deb rig main' \
                 > /etc/apt/sources.list.d/rig.list",
                "",
            )
            .with_output("sudo apt-get update", "")
            .with_output("sudo apt-get install -y r-rig", "");
        let fetcher = FakeFetcher::default();
        Rig.install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect("apt install must succeed");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 4, "{calls:#?}");
        assert_eq!(calls[3], "sudo apt-get install -y r-rig");
    }

    #[test]
    fn rig_linux_without_apt_errors_with_guidance() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let err = Rig
            .install(&ctx_on(Os::Linux, &runner, &fetcher))
            .expect_err("no apt must be a clean error");
        let out = render_error(&err, false);
        assert!(out.contains("apt"), "{out}");
        assert!(out.contains("hint:"), "{out}");
    }

    #[test]
    fn rig_windows_uses_winget_when_present() {
        let runner = FakeRunner::default()
            .on_path("winget")
            .with_output("winget install --id posit.rig --exact", "");
        let fetcher = FakeFetcher::default();
        Rig.install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect("winget install must succeed");
        assert_eq!(
            *runner.calls.borrow(),
            vec!["winget install --id posit.rig --exact"]
        );
    }

    #[test]
    fn rig_windows_without_winget_errors_with_guidance() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let err = Rig
            .install(&ctx_on(Os::Windows, &runner, &fetcher))
            .expect_err("no winget must be a clean error");
        let out = render_error(&err, false);
        assert!(out.contains("winget"), "{out}");
        assert!(out.contains("r-lib/rig/releases"), "{out}");
    }

    #[test]
    fn rig_does_not_support_version_pins() {
        assert!(!Rig.supports_pin());
    }

    #[test]
    fn rig_plan_names_the_package_manager_commands_per_os() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        assert_eq!(
            Rig.plan(&ctx_on(Os::Mac, &runner, &fetcher)),
            vec![
                "brew tap r-lib/rig".to_string(),
                "brew install --cask rig".to_string()
            ]
        );
        let linux = Rig.plan(&ctx_on(Os::Linux, &runner, &fetcher));
        assert!(
            linux.iter().any(|l| l == "sudo apt-get install -y r-rig"),
            "{linux:?}"
        );
        assert_eq!(
            Rig.plan(&ctx_on(Os::Windows, &runner, &fetcher)),
            vec!["winget install --id posit.rig --exact".to_string()]
        );
        assert!(runner.calls.borrow().is_empty(), "planning must not run");
    }
}
