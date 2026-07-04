//! Doc-check for `CONTRIBUTING.md`: the developer guide must exist and every
//! repo path it points contributors at (in inline code) must actually be
//! there, so a renamed or removed file fails this test instead of misleading
//! someone following the recipe.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn contributing() -> String {
    let path = manifest_dir().join("CONTRIBUTING.md");
    std::fs::read_to_string(&path).expect("CONTRIBUTING.md must exist and be UTF-8")
}

/// Every inline-code span (`` `like this` ``) outside fenced code blocks.
fn inline_code_spans(md: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let parts: Vec<&str> = line.split('`').collect();
        let mut i = 1;
        while i < parts.len() {
            spans.push(parts[i].to_string());
            i += 2;
        }
    }
    spans
}

/// True when `token` reads as a path into this repository's tree, so the
/// test knows to assert it exists (symbols like `with_defaults` do not
/// match and are left alone).
fn looks_like_repo_path(token: &str) -> bool {
    let stem = token.trim_end_matches('/');
    let rooted = ["src/", "tests/", "templates/", "docs/"]
        .iter()
        .any(|prefix| stem.starts_with(prefix));
    rooted
        && !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

/// Every repo path the guide references, deduped.
fn referenced_paths(md: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for span in inline_code_spans(md) {
        for token in span.split_whitespace() {
            if looks_like_repo_path(token) && !paths.contains(&token.to_string()) {
                paths.push(token.to_string());
            }
        }
    }
    paths
}

#[test]
fn contributing_exists() {
    assert!(
        manifest_dir().join("CONTRIBUTING.md").is_file(),
        "CONTRIBUTING.md must exist at the repo root"
    );
}

#[test]
fn every_referenced_repo_path_exists() {
    let md = contributing();
    let paths = referenced_paths(&md);
    assert!(
        !paths.is_empty(),
        "sanity: the guide should reference real source paths in inline code"
    );
    for rel in &paths {
        let full: PathBuf = manifest_dir().join(Path::new(rel));
        assert!(
            full.exists(),
            "CONTRIBUTING.md references `{rel}`, which does not exist at {}",
            full.display()
        );
    }
}

#[test]
fn contributing_covers_the_three_extension_recipes() {
    let md = contributing().to_lowercase();
    for topic in [
        "adding a language",
        "adding a managed tool",
        "adding an audit check",
    ] {
        assert!(
            md.contains(topic),
            "CONTRIBUTING.md should walk through {topic}"
        );
    }
}

#[test]
fn contributing_states_the_quality_gates() {
    let md = contributing();
    for gate in ["cargo test", "cargo clippy", "cargo fmt"] {
        assert!(
            md.contains(gate),
            "CONTRIBUTING.md should name the `{gate}` gate"
        );
    }
}
