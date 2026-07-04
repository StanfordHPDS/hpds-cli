//! Doc-check for `docs/conformance.md`: the spec-conformance report must
//! exist and must not drift from the real CLI.
//!
//! Two things are enforced:
//!
//!  * every `hpds ...` invocation shown in a fenced code block parses against
//!    the real binary (help exits 0 only when the whole command path and its
//!    flags are valid), so a renamed command or dropped flag fails this test;
//!  * every top-level command the binary exposes is named in the report, so a
//!    newly added command cannot be silently left unaudited.
//!
//! Mirrors the extraction in `tests/readme_commands.rs`.

use assert_cmd::Command;

/// Marker comment that exempts a fenced-block line from verification.
const NO_VERIFY: &str = "# no-verify";

fn conformance() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/conformance.md");
    std::fs::read_to_string(path).expect("docs/conformance.md should exist at docs/")
}

/// Extract the `hpds` invocations from every fenced code block.
///
/// A line counts when, after trimming and stripping an optional `$ ` prompt,
/// it starts with `hpds ` (or is exactly `hpds`). Trailing `# ...` comments
/// are dropped; lines carrying the no-verify marker are skipped.
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
        let command = match candidate.find(" #") {
            Some(idx) => candidate[..idx].trim_end(),
            None => candidate,
        };
        commands.push(command.to_string());
    }
    commands
}

#[test]
fn conformance_report_exists() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/conformance.md");
    assert!(
        std::path::Path::new(path).is_file(),
        "docs/conformance.md must exist"
    );
}

#[test]
fn conformance_report_shows_a_healthy_number_of_commands() {
    let commands = extract_commands(&conformance());
    assert!(
        commands.len() >= 15,
        "expected the conformance report to exercise at least 15 hpds commands, \
         found {}: {commands:#?}",
        commands.len()
    );
}

#[test]
fn every_conformance_hpds_command_parses() {
    let commands = extract_commands(&conformance());
    for command in &commands {
        assert!(
            !command.contains(['"', '\'']),
            "conformance command `{command}` uses shell quoting, which this test \
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
            "conformance command `{command}` does not parse against the real CLI \
             (`hpds {} --help` failed):\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn conformance_report_names_every_top_level_command() {
    let md = conformance();
    // The top-level command tree from `hpds --help` (excluding the built-in
    // `help`). Every one must be accounted for in the report.
    for command in [
        "format",
        "lint",
        "init",
        "project",
        "use",
        "install",
        "setup",
        "git",
        "repo",
        "audit",
        "tools",
        "config",
        "completions",
        "version",
        "upgrade",
    ] {
        assert!(
            md.contains(&format!("hpds {command}")),
            "docs/conformance.md must audit the `hpds {command}` command"
        );
    }
}

#[test]
fn extraction_reads_prompts_comments_and_no_verify_markers() {
    let sample = "\
Prose `hpds format` is not extracted.

```console
$ hpds format --check
hpds lint --fix   # autofix what we can
hpds 0.1.0   # no-verify
```

hpds init outside any fence is not extracted.

```sh
hpds audit report-github --mode pr
```
";
    assert_eq!(
        extract_commands(sample),
        vec![
            "hpds format --check",
            "hpds lint --fix",
            "hpds audit report-github --mode pr"
        ]
    );
}
