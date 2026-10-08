#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use serde_json::{Value, json};

    use super::{
        DockerState, RustState, ServerHost, merge_code_server_settings, merge_rstudio_preferences,
        reconcile_code_server, reconcile_docker, reconcile_rust, reconcile_server,
    };
    use crate::install::InstallCtx;
    use crate::install::test_support::{FakeRunner, PanicFetcher};
    use crate::setup::{Profile, SetupDeps, execute, finish, steps, summary};
    use crate::tools::Os;

    const USER: &str = "analyst";
    const HOME: &str = "/home/analyst";
    const QUARTO: &str = "/home/analyst/.local/bin/quarto";

    #[derive(Default)]
    struct FakeHost {
        files: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
        writes: RefCell<Vec<(PathBuf, Vec<u8>, u32)>>,
        existing: RefCell<Vec<PathBuf>>,
        rustup_acquisitions: RefCell<usize>,
        rustup_acquisition_fails: bool,
    }

    impl FakeHost {
        fn with_file(self, path: &str, text: &str) -> Self {
            self.files
                .borrow_mut()
                .insert(PathBuf::from(path), text.as_bytes().to_vec());
            self
        }

        fn with_existing(self, path: &str) -> Self {
            self.existing.borrow_mut().push(PathBuf::from(path));
            self
        }

        fn json(&self, path: &str) -> Value {
            serde_json::from_slice(
                self.files
                    .borrow()
                    .get(Path::new(path))
                    .expect("file exists"),
            )
            .expect("valid JSON")
        }
    }

    impl ServerHost for FakeHost {
        fn current_user(&self) -> Result<String> {
            Ok(USER.to_string())
        }

        fn home_dir(&self, user: &str) -> Result<PathBuf> {
            assert_eq!(user, USER);
            Ok(PathBuf::from(HOME))
        }

        fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
            Ok(self.files.borrow().get(path).cloned())
        }

        fn exists(&self, path: &Path) -> bool {
            self.existing.borrow().iter().any(|item| item == path)
                || self.files.borrow().contains_key(path)
        }

        fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), bytes.to_vec());
            self.writes
                .borrow_mut()
                .push((path.to_path_buf(), bytes.to_vec(), mode));
            Ok(())
        }

        fn acquire_verified_rustup_init(&self) -> Result<PathBuf> {
            *self.rustup_acquisitions.borrow_mut() += 1;
            if self.rustup_acquisition_fails {
                Err(anyhow::anyhow!("could not acquire verified rustup-init"))
            } else {
                Ok(PathBuf::from("/verified-download/rustup-init"))
            }
        }
    }

    fn mutation_calls(runner: &FakeRunner) -> Vec<String> {
        runner
            .calls
            .borrow()
            .iter()
            .filter(|call| {
                [
                    "apt-get install",
                    "usermod",
                    "rustup update",
                    "rustup default",
                    "rustup-init",
                ]
                .iter()
                .any(|needle| call.contains(needle))
            })
            .cloned()
            .collect()
    }

    #[test]
    fn docker_absent_installs_official_repo_plugins_then_adds_injected_user() {
        let host =
            FakeHost::default().with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let runner = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output(
                "sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc",
                "",
            )
            .with_output("sudo chmod a+r /etc/apt/keyrings/docker.asc", "")
            .with_output("sudo apt-get update", "")
            .with_output(
                "sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
                "",
            )
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "");

        reconcile_docker(&host, &runner, DockerState::Absent).expect("install Docker");

        let calls = runner.calls.borrow();
        let install = calls
            .iter()
            .position(|call| call.contains("docker-ce docker-ce-cli containerd.io"))
            .expect("package install");
        let group = calls
            .iter()
            .position(|call| call == "sudo usermod -aG docker analyst")
            .expect("group mutation");
        assert!(install < group, "{calls:?}");
        assert!(
            calls.iter().all(|call| !call.contains("$USER")),
            "{calls:?}"
        );
        let sources = host
            .files
            .borrow()
            .get(Path::new("/etc/apt/sources.list.d/docker.sources"))
            .cloned()
            .expect("Docker apt source");
        let sources = String::from_utf8(sources).expect("UTF-8 source");
        assert!(sources.contains("https://download.docker.com/linux/ubuntu"));
        assert!(sources.contains("Signed-By: /etc/apt/keyrings/docker.asc"));
    }

    #[test]
    fn docker_current_packages_only_add_missing_group_and_then_noop() {
        let host = FakeHost::default();
        let outside = FakeRunner::default()
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "");
        reconcile_docker(&host, &outside, DockerState::Current).expect("add group");
        assert_eq!(
            mutation_calls(&outside),
            ["sudo usermod -aG docker analyst"]
        );

        let current = FakeRunner::default().with_output("id -nG analyst", "analyst sudo docker");
        reconcile_docker(&host, &current, DockerState::Current).expect("already current");
        assert!(mutation_calls(&current).is_empty());
        assert!(host.writes.borrow().is_empty());
    }

    #[test]
    fn invalid_docker_host_stops_before_write_or_install() {
        for (arch, distro) in [
            ("s390x", "ID=ubuntu\nVERSION_CODENAME=noble\n"),
            ("amd64", "ID=debian\nVERSION_CODENAME=bookworm\n"),
            ("amd64", "ID=ubuntu\n"),
        ] {
            let host = FakeHost::default().with_file("/etc/os-release", distro);
            let runner = FakeRunner::default().with_output("dpkg --print-architecture", arch);
            reconcile_docker(&host, &runner, DockerState::Absent)
                .expect_err("unsupported Docker host");
            assert!(host.writes.borrow().is_empty());
            assert!(mutation_calls(&runner).is_empty());
        }
    }

    #[test]
    fn rust_absent_outdated_nonstable_and_current_are_reconciled() {
        let cases = [
            (
                RustState::Absent,
                vec![
                    "/verified-download/rustup-init -y --profile default --default-toolchain stable",
                ],
            ),
            (RustState::UpdateAvailable, vec!["rustup update stable"]),
            (
                RustState::NonStable,
                vec!["rustup update stable", "rustup default stable"],
            ),
            (RustState::CurrentStable, vec![]),
        ];
        for (state, mutations) in cases {
            let host = FakeHost::default();
            let mut runner = FakeRunner::default();
            for command in &mutations {
                runner = runner.with_output(command, "");
            }
            reconcile_rust(&host, &runner, state).expect("reconcile stable Rust");
            assert_eq!(mutation_calls(&runner), mutations, "{state:?}");
        }

        let host = FakeHost::default();
        let current = FakeRunner::default();
        reconcile_rust(&host, &current, RustState::CurrentStable).expect("second run");
        assert!(mutation_calls(&current).is_empty());
    }

    #[test]
    fn absent_rust_acquires_verified_bootstrap_before_executing_its_path() {
        let host = FakeHost::default();
        let runner = FakeRunner::default().with_output(
            "/verified-download/rustup-init -y --profile default --default-toolchain stable",
            "",
        );
        reconcile_rust(&host, &runner, RustState::Absent).expect("install stable Rust");
        assert_eq!(*host.rustup_acquisitions.borrow(), 1);
        assert_eq!(
            *runner.calls.borrow(),
            ["/verified-download/rustup-init -y --profile default --default-toolchain stable"]
        );
        assert!(
            runner
                .calls
                .borrow()
                .iter()
                .all(|call| !call.starts_with("rustup-init "))
        );
    }

    #[test]
    fn rust_bootstrap_acquisition_failure_prevents_execution() {
        let host = FakeHost {
            rustup_acquisition_fails: true,
            ..FakeHost::default()
        };
        let runner = FakeRunner::default();
        reconcile_rust(&host, &runner, RustState::Absent)
            .expect_err("failed acquisition stops bootstrap");
        assert_eq!(*host.rustup_acquisitions.borrow(), 1);
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn malformed_rustup_check_is_an_error_without_mutation() {
        let host = FakeHost::default();
        let runner = FakeRunner::default().with_output("rustup check", "unexpected output");
        reconcile_rust(&host, &runner, RustState::Probe).expect_err("unknown rustup output");
        assert!(mutation_calls(&runner).is_empty());
    }

    #[test]
    fn rstudio_preferences_merge_exact_policy_atomically_and_idempotently() {
        let path = "/etc/rstudio/rstudio-prefs.json";
        let host = FakeHost::default().with_file(
            path,
            r#"{"unrelated":{"keep":true},"save_workspace":"ask"}"#,
        );
        merge_rstudio_preferences(&host).expect("merge preferences");
        assert_eq!(
            host.json(path),
            json!({
                "unrelated": {"keep": true},
                "insert_native_pipe_operator": true,
                "save_workspace": "never",
                "load_workspace": "never",
                "rainbow_parentheses": true,
                "rainbow_fenced_divs": true
            })
        );
        assert_eq!(host.writes.borrow().len(), 1);
        assert_eq!(host.writes.borrow()[0].2, 0o644);
        merge_rstudio_preferences(&host).expect("second merge");
        assert_eq!(
            host.writes.borrow().len(),
            1,
            "unchanged file is not rewritten"
        );
    }

    #[test]
    fn malformed_or_non_object_preferences_fail_without_write() {
        for text in ["not json", "[]", "null"] {
            let host = FakeHost::default().with_file("/etc/rstudio/rstudio-prefs.json", text);
            merge_rstudio_preferences(&host).expect_err("invalid preferences");
            assert!(host.writes.borrow().is_empty());
        }
    }

    #[test]
    fn code_server_merges_managed_quarto_and_enables_the_legacy_unit() {
        let settings = "/home/analyst/.local/share/code-server/User/settings.json";
        let host = FakeHost::default().with_existing(QUARTO).with_file(
            settings,
            r#"{"editor.fontSize":15,"quarto.path":"/old/quarto"}"#,
        );
        let runner = FakeRunner::default()
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_code_server(&host, &runner).expect("configure code-server");
        assert_eq!(
            host.json(settings),
            json!({"editor.fontSize": 15, "quarto.path": QUARTO})
        );
        assert_eq!(host.writes.borrow().len(), 1);
        assert_eq!(
            *runner.calls.borrow(),
            ["sudo systemctl enable --now code-server@analyst"]
        );

        merge_code_server_settings(&host, USER).expect("second settings merge");
        assert_eq!(
            host.writes.borrow().len(),
            1,
            "unchanged settings are not rewritten"
        );
    }

    #[test]
    fn missing_managed_quarto_fails_before_settings_or_service() {
        let host = FakeHost::default();
        let runner = FakeRunner::default();
        reconcile_code_server(&host, &runner).expect_err("managed Quarto launcher is required");
        assert!(host.writes.borrow().is_empty());
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn complete_server_reconciliation_is_stable_on_second_run() {
        let host = FakeHost::default()
            .with_existing(QUARTO)
            .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let first = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_output("sudo install -m 0755 -d /etc/apt/keyrings", "")
            .with_output(
                "sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc",
                "",
            )
            .with_output("sudo chmod a+r /etc/apt/keyrings/docker.asc", "")
            .with_output("sudo apt-get update", "")
            .with_output(
                "sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
                "",
            )
            .with_output("id -nG analyst", "analyst sudo")
            .with_output("sudo usermod -aG docker analyst", "")
            .with_output(
                "/verified-download/rustup-init -y --profile default --default-toolchain stable",
                "",
            )
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_server(&host, &first, DockerState::Absent, RustState::Absent)
            .expect("first reconciliation");

        let files_after_first = host.files.borrow().clone();
        let writes_after_first = host.writes.borrow().len();
        let second = FakeRunner::default()
            .with_output("id -nG analyst", "analyst sudo docker")
            .with_output("sudo systemctl enable --now code-server@analyst", "");
        reconcile_server(
            &host,
            &second,
            DockerState::Current,
            RustState::CurrentStable,
        )
        .expect("second reconciliation");

        assert_eq!(*host.files.borrow(), files_after_first);
        assert_eq!(host.writes.borrow().len(), writes_after_first);
        assert!(mutation_calls(&second).is_empty());
    }

    #[test]
    fn real_server_steps_continue_after_docker_failure_and_finish_reports_it() {
        let host = FakeHost::default()
            .with_existing(QUARTO)
            .with_file("/etc/os-release", "ID=ubuntu\nVERSION_CODENAME=noble\n");
        let docker = FakeRunner::default()
            .with_output("dpkg --print-architecture", "amd64")
            .with_failure(
                "sudo install -m 0755 -d /etc/apt/keyrings",
                "permission denied",
            );
        let git_setup = || Ok(());
        let deps = SetupDeps {
            install: InstallCtx {
                os: Os::Linux,
                yes: true,
                verbose: false,
                pin: None,
                plan_approved: true,
                sudo_approved: std::cell::Cell::new(false),
                runner: &docker,
                fetcher: &PanicFetcher,
            },
            git_setup: &git_setup,
            installer_lookup: &crate::install::registry::find,
            server_host: &host,
        };
        let selected: Vec<_> = steps(Profile::Server)
            .iter()
            .filter(|step| matches!(step.title, "Docker Engine" | "RStudio preferences"))
            .copied()
            .collect();
        assert_eq!(
            selected.iter().map(|step| step.title).collect::<Vec<_>>(),
            ["Docker Engine", "RStudio preferences"]
        );

        let results = execute(&selected, &deps);
        assert!(
            host.files
                .borrow()
                .contains_key(Path::new("/etc/rstudio/rstudio-prefs.json"))
        );
        let failures: Vec<_> = results
            .iter()
            .filter(|result| result.error.is_some())
            .map(|result| result.title)
            .collect();
        assert_eq!(failures, ["Docker Engine"]);
        let text = summary(&results);
        assert!(text.contains("✗ Docker Engine"), "{text}");
        assert!(text.contains("✓ RStudio preferences"), "{text}");
        let error = finish(&results, None).expect_err("one failed step fails setup");
        assert!(error.to_string().contains("1 of 2"), "{error:#}");
    }
}
