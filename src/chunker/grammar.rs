use crate::file::Language;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, warn};
use tree_sitter::Language as TsLanguage;

/// Manages tree-sitter grammars for multiple languages
///
/// This uses compiled-in grammars (no downloads needed!), making it:
/// - Fast: No network requests or WASM loading
/// - Reliable: Works offline
/// - Version-controlled: Grammar versions pinned to crate versions
pub struct GrammarManager {
    /// Cache of loaded grammars
    grammars: DashMap<Language, Arc<TsLanguage>>,
}

impl GrammarManager {
    /// Create a new grammar manager with pre-compiled grammars
    pub fn new() -> Self {
        let manager = Self {
            grammars: DashMap::new(),
        };

        debug!(
            "GrammarManager initialized with {} pre-compiled grammars",
            manager.supported_languages().len()
        );

        manager
    }

    /// Get a grammar for the given language
    ///
    /// Returns None if the language is not supported for tree-sitter parsing
    pub fn get_grammar(&self, language: Language) -> Option<Arc<TsLanguage>> {
        // Check cache first
        if let Some(grammar) = self.grammars.get(&language) {
            return Some(grammar.clone());
        }

        // Load grammar on-demand
        match self.load_grammar(language) {
            Ok(grammar) => {
                let grammar = Arc::new(grammar);
                self.grammars.insert(language, grammar.clone());
                debug!("Loaded grammar for {}", language.name());
                Some(grammar)
            }
            Err(e) => {
                warn!("Failed to load grammar for {}: {}", language.name(), e);
                None
            }
        }
    }

    /// Load the compiled grammar for a language
    fn load_grammar(&self, language: Language) -> Result<TsLanguage> {
        match language {
            Language::Rust => Ok(tree_sitter_rust::LANGUAGE.into()),
            Language::Python => Ok(tree_sitter_python::LANGUAGE.into()),
            Language::JavaScript => Ok(tree_sitter_javascript::LANGUAGE.into()),
            Language::TypeScript => {
                // TypeScript grammar requires special handling
                Ok(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            }
            Language::Go => {
                warn!("Go grammar not yet integrated");
                Err(anyhow!("Go grammar not available"))
            }
            Language::Java => {
                warn!("Java grammar not yet integrated");
                Err(anyhow!("Java grammar not available"))
            }
            Language::C => {
                warn!("C grammar not yet integrated");
                Err(anyhow!("C grammar not available"))
            }
            Language::Cpp => {
                warn!("C++ grammar not yet integrated");
                Err(anyhow!("C++ grammar not available"))
            }
            _ => Err(anyhow!(
                "Language {} does not support tree-sitter",
                language.name()
            )),
        }
    }

    /// Get list of languages that have tree-sitter support
    pub fn supported_languages(&self) -> Vec<Language> {
        vec![
            Language::Rust,
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            // More will be added as we integrate their grammars
        ]
    }

    /// Check if a language has tree-sitter support
    pub fn is_supported(&self, language: Language) -> bool {
        self.supported_languages().contains(&language)
    }

    /// Pre-load all supported grammars into cache
    ///
    /// This is useful for warming up the cache at startup
    pub fn preload_all(&self) {
        debug!("Pre-loading all grammars...");
        for lang in self.supported_languages() {
            let _ = self.get_grammar(lang);
        }
        debug!("Pre-loaded {} grammars", self.grammars.len());
    }

    /// Get cache statistics
    pub fn stats(&self) -> GrammarStats {
        GrammarStats {
            cached_grammars: self.grammars.len(),
            supported_languages: self.supported_languages().len(),
        }
    }
}

impl Default for GrammarManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the grammar manager
#[derive(Debug, Clone)]
pub struct GrammarStats {
    pub cached_grammars: usize,
    pub supported_languages: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grammar_manager_creation() {
        let manager = GrammarManager::new();
        let stats = manager.stats();

        assert!(stats.supported_languages > 0);
        assert_eq!(stats.cached_grammars, 0); // No grammars loaded yet
    }

    #[test]
    fn test_load_rust_grammar() {
        let manager = GrammarManager::new();
        let grammar = manager.get_grammar(Language::Rust);

        assert!(grammar.is_some());
        assert_eq!(manager.stats().cached_grammars, 1);
    }

    #[test]
    fn test_load_python_grammar() {
        let manager = GrammarManager::new();
        let grammar = manager.get_grammar(Language::Python);

        assert!(grammar.is_some());
    }

    #[test]
    fn test_load_javascript_grammar() {
        let manager = GrammarManager::new();
        let grammar = manager.get_grammar(Language::JavaScript);

        assert!(grammar.is_some());
    }

    #[test]
    fn test_load_typescript_grammar() {
        let manager = GrammarManager::new();
        let grammar = manager.get_grammar(Language::TypeScript);

        assert!(grammar.is_some());
    }

    #[test]
    fn test_unsupported_language() {
        let manager = GrammarManager::new();
        let grammar = manager.get_grammar(Language::Markdown);

        assert!(grammar.is_none());
    }

    #[test]
    fn test_grammar_caching() {
        let manager = GrammarManager::new();

        // Load Rust grammar twice
        let grammar1 = manager.get_grammar(Language::Rust);
        let grammar2 = manager.get_grammar(Language::Rust);

        assert!(grammar1.is_some());
        assert!(grammar2.is_some());

        // Should only be cached once
        assert_eq!(manager.stats().cached_grammars, 1);

        // Should be the same Arc (pointer equality)
        assert!(Arc::ptr_eq(&grammar1.unwrap(), &grammar2.unwrap()));
    }

    #[test]
    fn test_preload_all() {
        let manager = GrammarManager::new();
        manager.preload_all();

        let stats = manager.stats();
        assert!(stats.cached_grammars > 0);
        assert_eq!(stats.cached_grammars, stats.supported_languages);
    }

    #[test]
    fn test_is_supported() {
        let manager = GrammarManager::new();

        assert!(manager.is_supported(Language::Rust));
        assert!(manager.is_supported(Language::Python));
        assert!(manager.is_supported(Language::JavaScript));
        assert!(manager.is_supported(Language::TypeScript));
        assert!(!manager.is_supported(Language::Markdown));
        assert!(!manager.is_supported(Language::Json));
    }
}
