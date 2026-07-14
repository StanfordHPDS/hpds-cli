//! Every `hpds ...` command shown in README.md must actually exist and parse.
//!
//! Commands are extracted from fenced code blocks and re-run with `--help`
//! appended: clap prints help and exits 0 only when the subcommand path and
//! every flag/value on the line parse against the real CLI, and exits 2
//! otherwise. Output lines that merely start with `hpds` (e.g. the
//! `hpds version` banner) opt out with a trailing `# no-verify` comment.

use assert_cmd::Command;

/// Marker comment that exempts a code-block line from verification.
const NO_VERIFY: &str = "# no-verify";

fn readme() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
    std::fs::read_to_string(path).expect("README.md should exist at the crate root")
}

/// Extract the `hpds` invocations from every fenced code block.
///
/// A line counts as an invocation when, after trimming and stripping an
/// optional `$ ` prompt, it starts with `hpds ` (or is exactly `hpds`).
/// Trailing `# ...` comments are stripped; lines carrying the no-verify
/// marker are skipped entirely.
fn extract_commands(markdown: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence || trimmed.contains(NO_VERIFY) {
            continue;
        }
        let candidate = trimmed.strip_prefix("$ ").unwrap_or(trimmed);
        if candidate != "hpds" && !candidate.starts_with("hpds ") {
            continue;
        }
        // Drop any trailing explanatory comment (`hpds init   # wizard`).
        let command = match candidate.find(" #") {
            Some(idx) => candidate[..idx].trim_end(),
            None => candidate,
        };
        commands.push(command.to_string());
    }
    commands
}

#[test]
fn readme_quickstart_says_formatting_comes_from_togi() {
    // hpds has no format/lint commands; the quickstart must point at the
    // lab's separate togi tool so nobody goes looking for `hpds format`.
    let readme = readme();
    let quickstart = readme
        .split("## Quickstart")
        .nth(1)
        .expect("README has a Quickstart section");
    assert!(
        quickstart.contains("togi"),
        "the quickstart must mention togi for formatting/linting"
    );
}

#[test]
fn readme_audit_example_recommends_the_hpds_toml_component() {
    let readme = readme();
    assert!(
        readme.contains("fix: run `hpds use hpds.toml`"),
        "the lifecycle audit example should point at the executable remediation"
    );
}

#[test]
fn readme_shows_a_healthy_number_of_hpds_commands() {
    let commands = extract_commands(&readme());
    assert!(
        commands.len() >= 10,
        "expected the README quickstart to show at least 10 hpds commands, \
         found {}: {commands:#?}",
        commands.len()
    );
}

#[test]
fn every_readme_hpds_command_parses() {
    let commands = extract_commands(&readme());
    for command in &commands {
        assert!(
            !command.contains(['"', '\'']),
            "README command `{command}` uses shell quoting, which this test \
             does not interpret; rewrite it without quotes or mark the line \
             `{NO_VERIFY}`"
        );
        let args: Vec<&str> = command.split_whitespace().skip(1).collect();
        let assert = Command::cargo_bin("hpds")
            .expect("hpds binary should build")
            .args(&args)
            .arg("--help")
            .assert();
        let output = assert.get_output();
        assert!(
            output.status.success(),
            "README command `{command}` does not parse against the real CLI \
             (`hpds {} --help` failed):\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn extraction_reads_prompts_comments_and_no_verify_markers() {
    let sample = "\
Run `hpds audit` to audit (prose, not extracted).

```console
$ hpds audit --strict
0 errors, 0 warnings
hpds git vaccinate   # patch the global ignore
hpds 0.1.0   # no-verify
```

hpds init outside any fence is not extracted.

```sh
hpds init --yes
```
";
    assert_eq!(
        extract_commands(sample),
        vec![
            "hpds audit --strict",
            "hpds git vaccinate",
            "hpds init --yes"
        ]
    );
}
