//! Extension → language registry (spec §5 adapter table).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Language buckets files are batched into, one per adapter family (spec §5).
///
/// Adding a language is source-level (spec §1 non-goals): add a variant here,
/// register its extensions in [`ExtensionRegistry::with_defaults`], and add
/// the adapter in M2's registry.
// TODO(M2): consumed by the adapter registry; only tests use it until then.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// `.R` / `.r` — formatted and linted by air.
    R,
    /// `.py` and `.ipynb` — ruff handles notebooks natively.
    Python,
    /// `.qmd` / `.Rmd` — panache. Separate from [`Language::Markdown`] so the
    /// `[lint]`/`[format]` language lists (spec §3) can differ on plain md.
    Quarto,
    /// `.md` — panache; format-only by default (spec §3).
    Markdown,
    /// `.sql` — sqlfluff.
    Sql,
}

/// Maps file extensions to [`Language`] buckets.
///
/// Lookups are ASCII case-insensitive so `.R`/`.r` and `.Rmd`/`.rmd` land in
/// the same bucket. Extensions are stored without a leading dot.
// TODO(M2): consumed by the adapter registry; only tests use it until then.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ExtensionRegistry {
    map: HashMap<String, Language>,
}

#[allow(dead_code)] // TODO(M2): consumed by the adapter registry.
impl ExtensionRegistry {
    /// Registry with the spec §5 default extension table.
    pub fn with_defaults() -> Self {
        let mut registry = Self {
            map: HashMap::new(),
        };
        registry.register("r", Language::R);
        registry.register("py", Language::Python);
        registry.register("ipynb", Language::Python);
        registry.register("qmd", Language::Quarto);
        registry.register("rmd", Language::Quarto);
        registry.register("md", Language::Markdown);
        registry.register("sql", Language::Sql);
        registry
    }

    /// Map `extension` (with or without a leading dot, any case) to `language`,
    /// replacing any existing mapping.
    pub fn register(&mut self, extension: &str, language: Language) {
        let key = extension.trim_start_matches('.').to_ascii_lowercase();
        self.map.insert(key, language);
    }

    /// The language bucket for `path`, or `None` if its extension is
    /// unregistered (such files are simply not format/lint targets).
    pub fn language_for(&self, path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?;
        self.map.get(&ext.to_ascii_lowercase()).copied()
    }
}

/// Batch `files` into per-language groups for the adapter runner (spec §5:
/// each tool invocation gets the whole file list, not one process per file).
///
/// Files with unregistered extensions are dropped. Input order is preserved
/// within each group; the map itself iterates in [`Language`] order.
// TODO(M2): consumed by the format/lint batch runner; only tests use it until then.
#[allow(dead_code)]
pub fn group_by_language(
    files: &[PathBuf],
    registry: &ExtensionRegistry,
) -> BTreeMap<Language, Vec<PathBuf>> {
    let mut groups: BTreeMap<Language, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        if let Some(language) = registry.language_for(file) {
            groups.entry(language).or_default().push(file.clone());
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn registry_maps_spec_extensions_to_languages() {
        let reg = ExtensionRegistry::with_defaults();
        let cases = [
            ("analysis.R", Language::R),
            ("helpers.r", Language::R),
            ("model.py", Language::Python),
            ("notebook.ipynb", Language::Python),
            ("report.qmd", Language::Quarto),
            ("report.Rmd", Language::Quarto),
            ("notes.md", Language::Markdown),
            ("query.sql", Language::Sql),
        ];
        for (name, lang) in cases {
            assert_eq!(
                reg.language_for(Path::new(name)),
                Some(lang),
                "extension mapping for {name}"
            );
        }
    }

    #[test]
    fn registry_lookup_is_case_insensitive() {
        let reg = ExtensionRegistry::with_defaults();
        assert_eq!(reg.language_for(Path::new("a.PY")), Some(Language::Python));
        assert_eq!(reg.language_for(Path::new("a.rmd")), Some(Language::Quarto));
    }

    #[test]
    fn registry_returns_none_for_unknown_files() {
        let reg = ExtensionRegistry::with_defaults();
        assert_eq!(reg.language_for(Path::new("data.csv")), None);
        assert_eq!(reg.language_for(Path::new("Makefile")), None);
    }

    #[test]
    fn registry_is_extensible() {
        let mut reg = ExtensionRegistry::with_defaults();
        // New extension into an existing bucket.
        reg.register("pyi", Language::Python);
        assert_eq!(
            reg.language_for(Path::new("stubs.pyi")),
            Some(Language::Python)
        );
        // Re-registering an extension overrides the default mapping.
        reg.register("md", Language::Quarto);
        assert_eq!(
            reg.language_for(Path::new("notes.md")),
            Some(Language::Quarto)
        );
    }

    #[test]
    fn group_by_language_batches_files_per_adapter() {
        let reg = ExtensionRegistry::with_defaults();
        let files: Vec<PathBuf> = [
            "a.R", "b.r", "c.py", "d.qmd", "e.md", "f.sql", "g.ipynb", "data.csv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let groups = group_by_language(&files, &reg);

        assert_eq!(
            groups[&Language::R],
            vec![PathBuf::from("a.R"), PathBuf::from("b.r")]
        );
        assert_eq!(
            groups[&Language::Python],
            vec![PathBuf::from("c.py"), PathBuf::from("g.ipynb")]
        );
        assert_eq!(groups[&Language::Quarto], vec![PathBuf::from("d.qmd")]);
        assert_eq!(groups[&Language::Markdown], vec![PathBuf::from("e.md")]);
        assert_eq!(groups[&Language::Sql], vec![PathBuf::from("f.sql")]);
        // Unrecognized files are left out entirely.
        assert_eq!(groups.len(), 5);
    }
}
