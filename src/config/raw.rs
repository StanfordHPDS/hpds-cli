//! Parse one TOML config file into a [`Layer`], collecting unknown keys.
//!
//! Unknown keys warn instead of erroring (forward compatibility), so each
//! table captures unrecognized entries via `#[serde(flatten)]` and we
//! surface them as dotted key paths. Type errors (e.g. `status = 3`) are
//! real errors: a wrong type is a mistake, not a future key.

use serde::Deserialize;

use super::Layer;

/// A parsed file: the layer it contributes plus its unknown key paths
/// (dotted, e.g. `project.frobnicate`).
#[derive(Debug)]
pub(crate) struct Parsed {
    pub(crate) layer: Layer,
    pub(crate) unknown_keys: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawConfig {
    project: Option<RawProject>,
    audit: Option<RawAudit>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawProject {
    status: Option<String>,
    primary_author: Option<String>,
    #[serde(flatten)]
    unknown: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawAudit {
    stale_days: Option<u32>,
    required_watchers: Option<Vec<String>>,
    #[serde(flatten)]
    unknown: toml::Table,
}

/// Parse a config file's contents. TOML syntax and type errors fail;
/// unrecognized keys are returned for the caller to warn about.
pub(crate) fn parse(text: &str) -> anyhow::Result<Parsed> {
    let raw: RawConfig = toml::from_str(text)?;
    let mut layer = Layer::default();
    let mut unknown_keys: Vec<String> = raw.unknown.keys().cloned().collect();

    let note_unknown = |table: &toml::Table, prefix: &str| {
        table
            .keys()
            .map(|k| format!("{prefix}.{k}"))
            .collect::<Vec<_>>()
    };

    if let Some(project) = raw.project {
        layer.project_status = project.status;
        layer.project_primary_author = project.primary_author;
        unknown_keys.extend(note_unknown(&project.unknown, "project"));
    }
    if let Some(audit) = raw.audit {
        layer.audit_stale_days = audit.stale_days;
        layer.audit_required_watchers = audit.required_watchers;
        unknown_keys.extend(note_unknown(&audit.unknown, "audit"));
    }

    Ok(Parsed {
        layer,
        unknown_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_unknown(parsed: &Parsed) {
        assert!(
            parsed.unknown_keys.is_empty(),
            "expected no unknown keys, got {:?}",
            parsed.unknown_keys
        );
    }

    #[test]
    fn parses_a_config_using_every_documented_key() {
        let parsed = parse(
            r#"
            [project]
            status = "active"
            primary-author = "malcolm"

            [audit]
            stale-days = 30
            required-watchers = ["lead1", "lead2"]
            "#,
        )
        .expect("a config using every documented key must parse");
        assert_no_unknown(&parsed);

        let layer = parsed.layer;
        assert_eq!(layer.project_status.as_deref(), Some("active"));
        assert_eq!(layer.project_primary_author.as_deref(), Some("malcolm"));
        assert_eq!(layer.audit_stale_days, Some(30));
        assert_eq!(
            layer.audit_required_watchers,
            Some(vec!["lead1".to_string(), "lead2".to_string()])
        );
    }

    #[test]
    fn audit_stale_days_parses_alone() {
        let parsed = parse("[audit]\nstale-days = 30\n").expect("valid audit table");
        assert_no_unknown(&parsed);
        assert_eq!(parsed.layer.audit_stale_days, Some(30));
        assert_eq!(parsed.layer.audit_required_watchers, None);
    }

    #[test]
    fn audit_table_wrong_types_are_errors() {
        assert!(parse("[audit]\nstale-days = \"soon\"\n").is_err());
        assert!(parse("[audit]\nstale-days = -1\n").is_err());
        assert!(parse("[audit]\nrequired-watchers = \"malcolm\"\n").is_err());
    }

    #[test]
    fn audit_table_unknown_keys_warn() {
        let parsed = parse("[audit]\nstale-days = 30\nfrobnicate = 1\n")
            .expect("unknown audit keys must not fail the parse");
        assert_eq!(parsed.unknown_keys, vec!["audit.frobnicate"]);
        assert_eq!(parsed.layer.audit_stale_days, Some(30));
    }

    #[test]
    fn empty_file_parses_to_an_empty_layer() {
        let parsed = parse("").expect("empty file is valid");
        assert_no_unknown(&parsed);
        assert_eq!(parsed.layer, Layer::default());
    }

    #[test]
    fn unknown_keys_are_collected_not_errors() {
        let parsed = parse(
            r#"
            future-section = { x = 1 }
            top-level = true

            [project]
            status = "active"
            frobnicate = 1
            "#,
        )
        .expect("unknown keys must not fail the parse");
        let mut keys = parsed.unknown_keys.clone();
        keys.sort();
        assert_eq!(
            keys,
            vec!["future-section", "project.frobnicate", "top-level"]
        );
        // known keys around the unknown ones still land
        assert_eq!(parsed.layer.project_status.as_deref(), Some("active"));
    }

    #[test]
    fn retired_formatting_tables_are_unknown_keys_now() {
        // `[format]`/`[lint]`/`[sql]`/`[tools]` moved to the separate togi
        // tool; hpds treats them like any other unrecognized table.
        let parsed = parse(
            r#"
            [format]
            languages = ["r"]

            [lint]
            exclude = []

            [sql]
            dialect = "bigquery"

            [tools]
            air = "0.10.0"
            "#,
        )
        .expect("retired tables must not fail the parse");
        let mut keys = parsed.unknown_keys.clone();
        keys.sort();
        assert_eq!(keys, vec!["format", "lint", "sql", "tools"]);
        assert_eq!(parsed.layer, Layer::default());
    }

    #[test]
    fn wrong_types_are_errors_not_warnings() {
        assert!(parse("[project]\nstatus = 3\n").is_err());
        assert!(parse("[project]\nprimary-author = []\n").is_err());
    }

    #[test]
    fn invalid_toml_syntax_is_an_error() {
        assert!(parse("[audit\nstale-days = 30").is_err());
    }
}
