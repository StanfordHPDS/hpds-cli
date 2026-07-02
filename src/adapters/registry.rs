//! Language bucket → adapter instance registry.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::adapters::Adapter;
use crate::fsx::Language;

/// Maps [`Language`] buckets (from the fsx extension registry) to the
/// adapter that handles them — the one place to wire up a language.
///
/// Several buckets may share one adapter instance (e.g. Quarto and
/// Markdown both go to the markdown formatter); the batch runner merges
/// their files into a single invocation by adapter name.
#[derive(Default)]
pub struct AdapterRegistry {
    map: BTreeMap<Language, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    /// An empty registry; real adapters register themselves here as they
    /// are wired into the commands.
    pub fn new() -> AdapterRegistry {
        AdapterRegistry::default()
    }

    /// Route `language` to `adapter`, replacing any existing routing.
    pub fn register(&mut self, language: Language, adapter: Arc<dyn Adapter>) {
        self.map.insert(language, adapter);
    }

    /// The adapter handling `language`, or `None` if the language has no
    /// adapter (its files are simply skipped).
    pub fn adapter_for(&self, language: Language) -> Option<&Arc<dyn Adapter>> {
        self.map.get(&language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeAdapter;

    #[test]
    fn adapter_for_returns_the_registered_adapter() {
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::new(FakeAdapter::new("ruff")));

        let adapter = registry
            .adapter_for(Language::Python)
            .expect("python was registered");
        assert_eq!(adapter.name(), "ruff");
        assert!(registry.adapter_for(Language::Sql).is_none());
    }

    #[test]
    fn registering_a_language_twice_replaces_the_adapter() {
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Sql, Arc::new(FakeAdapter::new("old")));
        registry.register(Language::Sql, Arc::new(FakeAdapter::new("new")));

        let adapter = registry.adapter_for(Language::Sql).expect("registered");
        assert_eq!(adapter.name(), "new");
    }

    #[test]
    fn one_adapter_instance_can_serve_several_buckets() {
        let markdown: Arc<dyn Adapter> = Arc::new(FakeAdapter::new("panache"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Quarto, Arc::clone(&markdown));
        registry.register(Language::Markdown, Arc::clone(&markdown));

        for language in [Language::Quarto, Language::Markdown] {
            let adapter = registry.adapter_for(language).expect("registered");
            assert_eq!(adapter.name(), "panache");
        }
    }
}
