#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, anyhow};

    use super::{
        DebianMetadata, RstudioBackend, RstudioRelease, RstudioServer, STABLE_ENDPOINT,
        parse_debian_metadata, parse_stable_redirect,
    };
    use crate::install::test_support::{FakeFetcher, FakeRunner, ctx_on};
    use crate::install::{InstallCtx, Installer, run_installer};
    use crate::tools::Os;

    const LATEST: &str = "2026.09.0+174";
    const LATEST_URL: &str = "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb";

    #[test]
    fn discovery_uses_posits_stable_server_endpoint() {
        assert_eq!(
            STABLE_ENDPOINT,
            "https://rstudio.org/download/latest/stable/server/jammy/rstudio-server-latest-amd64.deb"
        );
    }

    #[test]
    fn stable_redirect_parses_the_canonical_release() {
        let release = parse_stable_redirect(LATEST_URL).expect("valid stable redirect");
        assert_eq!(release.version, LATEST);
        assert_eq!(release.filename, "rstudio-server-2026.09.0-174-amd64.deb");
        assert_eq!(release.url, LATEST_URL);
    }

    #[test]
    fn stable_redirect_rejects_untrusted_or_malformed_targets() {
        let invalid = [
            "http://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://example.com/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com@example.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/not-rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/desktop/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-workbench-2026.09.0-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/arm64/rstudio-server-2026.09.0-174-arm64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09-174-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-rc1-amd64.deb",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb?download=1",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-2026.09.0-174-amd64.deb#fragment",
            "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/%2e%2e%2frstudio-server-2026.09.0-174-amd64.deb",
        ];

        for url in invalid {
            assert!(parse_stable_redirect(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn debian_metadata_accepts_only_the_discovered_package() {
        let metadata = parse_debian_metadata(
            "Package: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            LATEST,
        )
        .expect("matching package metadata");
        assert_eq!(
            metadata,
            DebianMetadata {
                package: "rstudio-server".to_string(),
                architecture: "amd64".to_string(),
                version: LATEST.to_string(),
            }
        );
    }

    #[test]
    fn debian_metadata_rejects_wrong_missing_duplicate_or_malformed_fields() {
        let invalid = [
            "Package: rstudio-workbench\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: arm64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+173\n",
            "Architecture: amd64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: amd64\n",
            "Package: rstudio-server\nPackage: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            "Package rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
        ];

        for metadata in invalid {
            assert!(
                parse_debian_metadata(metadata, LATEST).is_err(),
                "accepted {metadata:?}"
            );
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        phases: RefCell<Vec<String>>,
        installed: RefCell<Option<String>>,
        downloaded: RefCell<Option<String>>,
        metadata_override: Option<String>,
        fail_inspect: bool,
    }

    impl FakeBackend {
        fn installed(version: &str) -> Self {
            Self {
                installed: RefCell::new(Some(version.to_string())),
                ..Self::default()
            }
        }
    }

    impl RstudioBackend for FakeBackend {
        fn detect(&self) -> Option<String> {
            self.installed.borrow().clone()
        }

        fn latest(&self) -> Result<RstudioRelease> {
            self.phases.borrow_mut().push("resolve latest".to_string());
            parse_stable_redirect(LATEST_URL)
        }

        fn exact(&self, version: &str) -> Result<RstudioRelease> {
            self.phases
                .borrow_mut()
                .push(format!("resolve exact {version}"));
            let filename_version = version.replace('+', "-");
            parse_stable_redirect(&format!(
                "https://s3.amazonaws.com/rstudio-server/server/jammy/amd64/rstudio-server-{filename_version}-amd64.deb"
            ))
        }

        fn download(&self, release: &RstudioRelease) -> Result<PathBuf> {
            self.phases
                .borrow_mut()
                .push(format!("download {}", release.version));
            *self.downloaded.borrow_mut() = Some(release.version.clone());
            Ok(PathBuf::from("/tmp/test-rstudio-server.deb"))
        }

        fn inspect(&self, _package: &Path) -> Result<String> {
            self.phases.borrow_mut().push("inspect".to_string());
            if self.fail_inspect {
                Err(anyhow!("dpkg-deb failed"))
            } else {
                Ok(self.metadata_override.clone().unwrap_or_else(|| {
                    format!(
                        "Package: rstudio-server\nArchitecture: amd64\nVersion: {}\n",
                        self.downloaded.borrow().as_deref().expect("download first")
                    )
                }))
            }
        }

        fn install(&self, _ctx: &InstallCtx<'_>, _package: &Path) -> Result<()> {
            self.phases.borrow_mut().push("sudo gdebi".to_string());
            *self.installed.borrow_mut() = self.downloaded.borrow().clone();
            Ok(())
        }
    }

    #[test]
    fn unpinned_latest_updates_older_and_retains_equal_or_newer() {
        for (installed, installs) in [
            ("2025.05.1+513", true),
            (LATEST, false),
            ("2026.10.0+1", false),
        ] {
            let backend = FakeBackend::installed(installed);
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
                .expect("latest install decision");

            let phases = backend.phases.borrow();
            assert_eq!(phases.first().map(String::as_str), Some("resolve latest"));
            assert_eq!(phases.iter().any(|phase| phase == "sudo gdebi"), installs);
        }
    }

    #[test]
    fn exact_pin_bypasses_latest_and_may_downgrade() {
        const PIN: &str = "2025.05.1+513";
        let backend = FakeBackend::installed("2026.10.0+1");
        let installer = RstudioServer::with_backend(&backend);
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();
        let ctx = InstallCtx {
            pin: Some(PIN.to_string()),
            ..ctx_on(Os::Linux, &runner, &fetcher)
        };

        assert!(installer.supports_pin());
        run_installer(&installer, &ctx).expect("an exact pin may downgrade");
        let phases = backend.phases.borrow();
        assert_eq!(
            phases.first().map(String::as_str),
            Some("resolve exact 2025.05.1+513")
        );
        assert!(!phases.iter().any(|phase| phase == "resolve latest"));
        assert!(phases.iter().any(|phase| phase == "download 2025.05.1+513"));
        assert!(phases.iter().any(|phase| phase == "sudo gdebi"));
        assert_eq!(backend.installed.borrow().as_deref(), Some(PIN));
    }

    #[test]
    fn non_linux_fails_before_network_or_commands() {
        for os in [Os::Mac, Os::Windows] {
            let backend = FakeBackend::default();
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();

            let error = run_installer(&installer, &ctx_on(os, &runner, &fetcher))
                .expect_err("RStudio Server is Linux-only");
            assert!(error.to_string().contains("Linux"), "{error:#}");
            assert!(backend.phases.borrow().is_empty());
            assert!(runner.calls.borrow().is_empty());
        }
    }

    #[test]
    fn install_orders_phases_and_inspector_failure_prevents_install() {
        let runner = FakeRunner::default();
        let fetcher = FakeFetcher::default();

        let backend = FakeBackend::installed("2025.05.1+513");
        let installer = RstudioServer::with_backend(&backend);
        run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
            .expect("valid package installs");
        assert_eq!(
            *backend.phases.borrow(),
            [
                "resolve latest",
                "download 2026.09.0+174",
                "inspect",
                "sudo gdebi"
            ]
        );

        let failing = FakeBackend {
            installed: RefCell::new(Some("2025.05.1+513".to_string())),
            fail_inspect: true,
            ..FakeBackend::default()
        };
        let installer = RstudioServer::with_backend(&failing);
        run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
            .expect_err("inspector failure must stop installation");
        assert_eq!(
            *failing.phases.borrow(),
            ["resolve latest", "download 2026.09.0+174", "inspect"]
        );
    }

    #[test]
    fn invalid_inspected_identity_prevents_install() {
        for metadata in [
            "Package: rstudio-workbench\nArchitecture: amd64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: arm64\nVersion: 2026.09.0+174\n",
            "Package: rstudio-server\nArchitecture: amd64\nVersion: 2026.09.0+173\n",
        ] {
            let backend = FakeBackend {
                installed: RefCell::new(Some("2025.05.1+513".to_string())),
                metadata_override: Some(metadata.to_string()),
                ..FakeBackend::default()
            };
            let installer = RstudioServer::with_backend(&backend);
            let runner = FakeRunner::default();
            let fetcher = FakeFetcher::default();

            run_installer(&installer, &ctx_on(Os::Linux, &runner, &fetcher))
                .expect_err("invalid package identity must stop installation");
            assert_eq!(
                *backend.phases.borrow(),
                ["resolve latest", "download 2026.09.0+174", "inspect"],
                "{metadata:?}"
            );
        }
    }
}
