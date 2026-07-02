//! Minimal `{{variable}}` substitution.
//!
//! Only identifier-shaped tokens are treated as variables; an unknown
//! `{{variable}}` is a hard error at render time to catch typos in
//! templates. Anything else between doubled braces passes through
//! untouched, and a `$` immediately before the braces always marks a
//! GitHub Actions expression (`${{ github }}`, `${{ matrix.os }}`) —
//! never a variable, even when the contents are identifier-shaped.

use std::collections::BTreeMap;

use super::TemplateError;

/// The substitution map handed to [`render`]: the standard variables
/// (project, language, year, author) plus anything a component adds.
#[derive(Debug, Clone, Default)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    /// An empty map; add variables with [`Vars::with`].
    #[allow(dead_code)] // tests-only until the `hpds use` components consume it
    pub fn new() -> Self {
        Self::default()
    }

    /// The standard the design variables: project name, language, author, and
    /// the current year.
    #[allow(dead_code)] // tests-only until the `hpds use` components consume it
    pub fn standard(project: &str, language: &str, author: &str) -> Self {
        Self::new()
            .with("project", project)
            .with("language", language)
            .with("author", author)
            .with("year", current_year().to_string())
    }

    /// Builder-style insert; later inserts win.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Sorted, comma-separated variable names for error messages.
    fn available(&self) -> String {
        if self.0.is_empty() {
            "none".to_string()
        } else {
            self.0
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// Substitute every `{{variable}}` in `template` from `vars`.
///
/// `template_name` labels error messages (use the template-relative path).
/// Unknown identifier-shaped variables are a hard error; non-identifier
/// brace sequences pass through unchanged.
pub fn render(template: &str, template_name: &str, vars: &Vars) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        // `${{ ... }}` is a GitHub Actions expression, never one of our
        // variables — even when the contents are identifier-shaped, like
        // `${{ github }}`. The `$` escape hatch keeps such tokens from
        // being rewritten or rejected as unknown variables.
        let dollar_escaped = rest[..start].ends_with('$');
        let token = after.find("}}").map(|end| (after[..end].trim(), end));
        match token {
            Some((name, end)) if !dollar_escaped && is_identifier(name) => match vars.get(name) {
                Some(value) => {
                    out.push_str(value);
                    rest = &after[end + 2..];
                }
                None => {
                    return Err(TemplateError::UnknownVariable {
                        name: name.to_string(),
                        template: template_name.to_string(),
                        available: vars.available(),
                    });
                }
            },
            // Not a variable (unclosed, empty, or non-identifier contents):
            // emit the `{{` literally and keep scanning after it.
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// `true` for tokens we treat as variable names: `[A-Za-z0-9_-]+`.
fn is_identifier(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The current year in UTC, for the `year` template variable.
///
/// Computed from the system clock with the standard civil-from-days
/// algorithm (Howard Hinnant's `civil_from_days`) to avoid a date-time
/// dependency for a single field.
fn current_year() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // A clock before 1970 means the machine is misconfigured; the epoch
        // year is a harmless fallback for a template variable.
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    year_from_unix_days(secs.div_euclid(86_400))
}

/// Civil (Gregorian) year containing the given days-since-1970-01-01.
fn year_from_unix_days(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // `y` is the March-based year; January and February (mp 10, 11) belong
    // to the next civil year.
    if mp >= 10 { y + 1 } else { y }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_vars() -> Vars {
        Vars::new()
            .with("project", "malaria-icu")
            .with("language", "r")
            .with("year", "2026")
            .with("author", "HPDS Lab")
    }

    #[test]
    fn substitutes_every_known_variable() {
        let out = render(
            "Hello {{project}}!\nlanguage: {{language}}\nyear: {{year}}\nauthor: {{author}}\n",
            "hello.txt",
            &all_vars(),
        )
        .unwrap();
        assert_eq!(
            out,
            "Hello malaria-icu!\nlanguage: r\nyear: 2026\nauthor: HPDS Lab\n"
        );
    }

    #[test]
    fn substitutes_a_repeated_variable_everywhere() {
        let out = render("{{project}} and {{project}}", "t", &all_vars()).unwrap();
        assert_eq!(out, "malaria-icu and malaria-icu");
    }

    #[test]
    fn allows_whitespace_inside_the_braces() {
        let out = render("{{ project }}", "t", &all_vars()).unwrap();
        assert_eq!(out, "malaria-icu");
    }

    #[test]
    fn unknown_variable_is_a_hard_error_naming_the_variable() {
        let err = render("Hello {{projct}}!", "hello.txt", &all_vars()).unwrap_err();
        match err {
            TemplateError::UnknownVariable {
                name,
                template,
                available,
            } => {
                assert_eq!(name, "projct");
                assert_eq!(template, "hello.txt");
                assert_eq!(available, "author, language, project, year");
            }
            other => panic!("expected UnknownVariable, got {other:?}"),
        }
    }

    #[test]
    fn unknown_variable_error_with_empty_map_says_none_available() {
        let err = render("{{project}}", "t", &Vars::new()).unwrap_err();
        assert!(err.to_string().contains("available: none"));
    }

    #[test]
    fn non_identifier_brace_sequences_pass_through() {
        let vars = all_vars();
        // GitHub Actions expressions and literal braces are not variables.
        for literal in [
            "${{ matrix.os }}",
            "{{ not a var }}",
            "{{}}",
            "{{unclosed",
            "plain {} braces",
        ] {
            assert_eq!(render(literal, "t", &vars).unwrap(), literal);
        }
    }

    #[test]
    fn dollar_prefixed_expressions_pass_through_even_when_identifier_shaped() {
        let vars = all_vars();
        // GitHub Actions contexts can be bare identifiers (`${{ github }}`,
        // `${{ inputs }}`); a `$` before the braces always means "not ours".
        for literal in ["${{ github }}", "${{ inputs }}", "${{project}}"] {
            assert_eq!(render(literal, "t", &vars).unwrap(), literal);
        }
        // ...and unknown identifiers behind a `$` are not typo errors.
        assert_eq!(
            render("${{ secrets }}", "t", &vars).unwrap(),
            "${{ secrets }}"
        );
    }

    #[test]
    fn substitution_still_happens_after_a_dollar_escaped_expression() {
        let out = render("${{ github }} in {{project}}", "t", &all_vars()).unwrap();
        assert_eq!(out, "${{ github }} in malaria-icu");
    }

    #[test]
    fn text_after_a_literal_brace_pair_is_still_substituted() {
        let out = render("{{! }} then {{project}}", "t", &all_vars()).unwrap();
        assert_eq!(out, "{{! }} then malaria-icu");
    }

    #[test]
    fn standard_vars_carry_the_four_spec_variables() {
        let vars = Vars::standard("proj", "python", "Someone");
        assert_eq!(vars.get("project"), Some("proj"));
        assert_eq!(vars.get("language"), Some("python"));
        assert_eq!(vars.get("author"), Some("Someone"));
        let year: i64 = vars.get("year").unwrap().parse().unwrap();
        assert!(year >= 2026, "year variable is the current year: {year}");
    }

    #[test]
    fn current_year_is_computed_from_the_clock() {
        assert!(current_year() >= 2026);
        assert!(current_year() < 2200, "sanity upper bound");
    }

    #[test]
    fn year_from_unix_days_handles_known_dates() {
        assert_eq!(year_from_unix_days(0), 1970); // 1970-01-01
        assert_eq!(year_from_unix_days(365), 1971); // 1971-01-01
        assert_eq!(year_from_unix_days(19_723), 2024); // 2024-01-01 (leap year)
        assert_eq!(year_from_unix_days(20_088), 2024); // 2024-12-31
        assert_eq!(year_from_unix_days(20_089), 2025); // 2025-01-01
        assert_eq!(year_from_unix_days(-1), 1969); // 1969-12-31
    }
}
