//! Doc-check for `docs/hpds.toml.md`: the config reference must stay in sync
//! with the binary's real parse surface. Two directions are enforced:
//!
//!  * every TOML example in the doc parses against the binary with no
//!    "unknown key" warning (so the doc never shows a key hpds does not know);
//!  * every config key hpds actually parses is documented (the canonical list
//!    below mirrors `src/config/raw.rs`).

use std::path::PathBuf;

use assert_cmd::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn doc() -> String {
    let path = manifest_dir().join("docs/hpds.toml.md");
    std::fs::read_to_string(&path).expect("docs/hpds.toml.md must exist and be UTF-8")
}

/// The bodies of every fenced ```toml block in the doc.
fn toml_blocks(md: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some(body) = current.as_mut() {
            if trimmed.starts_with("```") {
                blocks.push(std::mem::take(body));
                current = None;
            } else {
                body.push_str(line);
                body.push('\n');
            }
        } else if trimmed == "```toml" {
            current = Some(String::new());
        }
    }
    blocks
}

/// Run `hpds config` with `toml` supplied as the *user* config layer (via
/// `HPDS_CONFIG_DIR`, an isolated temp dir) so the block is honored whole
/// — including `audit.required-watchers`, which project config would strip.
/// Returns (success, combined stderr).
fn run_config_with(toml: &str) -> (bool, String) {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    std::fs::write(config_dir.path().join("config.toml"), toml).expect("write user config");
    // A separate empty working directory so no project hpds.toml is discovered.
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let output = Command::cargo_bin("hpds")
        .expect("hpds binary should build")
        .arg("config")
        .env("HPDS_CONFIG_DIR", config_dir.path())
        .current_dir(cwd.path())
        .output()
        .expect("hpds config runs");

    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    (output.status.success(), stderr)
}

#[test]
fn config_reference_exists() {
    assert!(
        manifest_dir().join("docs/hpds.toml.md").is_file(),
        "docs/hpds.toml.md must exist"
    );
}

#[test]
fn every_documented_toml_example_parses_with_no_unknown_keys() {
    let blocks = toml_blocks(&doc());
    assert!(
        !blocks.is_empty(),
        "sanity: the reference should contain at least one ```toml example"
    );
    for block in &blocks {
        let (ok, stderr) = run_config_with(block);
        assert!(
            ok,
            "a documented TOML example failed to parse against the binary:\n{block}\n--- stderr ---\n{stderr}"
        );
        assert!(
            !stderr.contains("unknown key"),
            "a documented TOML example uses a key hpds does not recognize:\n{block}\n--- stderr ---\n{stderr}"
        );
    }
}

/// Every config key hpds parses today, dotted. Mirrors the parse surface in
/// `src/config/raw.rs`.
const CANONICAL_KEYS: &[&str] = &[
    "project.status",
    "project.primary-author",
    "audit.stale-days",
    "audit.required-watchers",
];

#[test]
fn every_parsed_config_key_is_documented() {
    let md = doc();
    for key in CANONICAL_KEYS {
        // Document by the leaf key name under its section header, which is
        // how the reference is written (e.g. a `[project]` section that
        // documents `status`).
        let (section, leaf) = key.rsplit_once('.').expect("dotted key");
        assert!(
            md.contains(leaf),
            "docs/hpds.toml.md must document `{key}` (leaf `{leaf}` not found)"
        );
        // The section itself must be named somewhere in the reference.
        let section_root = section.split('.').next().expect("section root");
        assert!(
            md.contains(&format!("[{section_root}")),
            "docs/hpds.toml.md must name the `[{section_root}]` section for `{key}`"
        );
    }
}

#[test]
fn the_annotated_example_exercises_every_fixed_key() {
    // Every key must appear in the doc's TOML examples, so "documented" is
    // anchored to something the binary actually parsed.
    let example: String = toml_blocks(&doc()).join("\n");
    for leaf in [
        "status",
        "primary-author",
        "stale-days",
        "required-watchers",
    ] {
        assert!(
            example.contains(leaf),
            "the TOML examples should set `{leaf}` so the reference is executable"
        );
    }
    // The formatter/linter tables belong to togi now, not hpds.toml.
    for gone in ["[format]", "[lint]", "[sql]", "[tools]"] {
        assert!(
            !example.contains(gone),
            "the reference must not document the retired `{gone}` table"
        );
    }
}
