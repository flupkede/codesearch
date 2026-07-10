use crate::constants::global_extension_map_path;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use tracing::warn;

/// Process-global, user-defined extension→language overrides, loaded once from
/// `~/.codesearch/extensions.json` (or the path in `$CODESEARCH_EXTENSION_MAP`).
/// Empty when no file is present — a missing/invalid map never fails indexing.
static EXTENSION_OVERRIDES: OnceLock<HashMap<String, Language>> = OnceLock::new();

/// Supported programming languages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Swift,
    Kotlin,
    Dart,
    Shell,
    Markdown,
    Json,
    Yaml,
    Toml,
    Sql,
    Html,
    Css,
    Xml,
    Jupyter,
    Unknown,
}

impl Language {
    /// Detect language from file path (extension + known extensionless filenames).
    ///
    /// User-defined extension overrides (see [`global_extension_map_path`]) are
    /// consulted first, so a repo using a non-standard convention (e.g.
    /// `*.class.inc` PHP, or `.h` as C++) can be mapped to the right grammar.
    pub fn from_path(path: &Path) -> Self {
        Self::from_path_with_overrides(path, extension_overrides())
    }

    /// Same as [`Self::from_path`] but against an explicit override map instead
    /// of the process-global one — the testable core of extension resolution.
    ///
    /// Resolution order: user override (by extension) → built-in extension
    /// table → built-in extensionless-filename table. User overrides therefore
    /// take precedence over the built-ins (a user may deliberately remap a
    /// known extension), while unmapped extensions behave exactly as before.
    pub fn from_path_with_overrides(path: &Path, overrides: &HashMap<String, Language>) -> Self {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        // User overrides win — keyed on the lowercased, dot-less extension.
        if !extension.is_empty() {
            if let Some(&lang) = overrides.get(&extension.to_lowercase()) {
                return lang;
            }
        }

        // Built-in extension table.
        let by_ext = Self::from_extension(extension);
        if by_ext != Self::Unknown {
            return by_ext;
        }

        // Fallback: match on exact filename for extensionless files.
        let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        Self::from_filename(filename)
    }

    /// Parse a language name (as written in the extension map) into a variant.
    ///
    /// Accepts the canonical names from [`Self::name`] plus common aliases,
    /// case-insensitively (`"php"`, `"C++"`, `"c#"`, `"golang"`, …). Returns
    /// `None` for unrecognised names and for `"unknown"` (never a valid target).
    pub fn from_name(name: &str) -> Option<Self> {
        let lang = match name.trim().to_lowercase().as_str() {
            "rust" | "rs" => Self::Rust,
            "python" | "py" => Self::Python,
            "javascript" | "js" => Self::JavaScript,
            "typescript" | "ts" => Self::TypeScript,
            "go" | "golang" => Self::Go,
            "java" => Self::Java,
            "c" => Self::C,
            "cpp" | "c++" => Self::Cpp,
            "csharp" | "c#" | "cs" => Self::CSharp,
            "ruby" | "rb" => Self::Ruby,
            "php" => Self::Php,
            "swift" => Self::Swift,
            "kotlin" | "kt" => Self::Kotlin,
            "dart" => Self::Dart,
            "shell" | "sh" | "bash" => Self::Shell,
            "markdown" | "md" => Self::Markdown,
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "sql" => Self::Sql,
            "html" => Self::Html,
            "css" => Self::Css,
            "xml" => Self::Xml,
            "jupyter" => Self::Jupyter,
            _ => return None,
        };
        Some(lang)
    }

    /// Detect language from extensionless filename
    pub fn from_filename(name: &str) -> Self {
        match name {
            "Dockerfile" | "Containerfile" => Self::Shell,
            "Makefile" | "GNUmakefile" | "makefile" => Self::Shell,
            "Jenkinsfile" | "Vagrantfile" | "Fastfile" | "Appfile" | "Podfile" => Self::Ruby,
            ".env" | ".envrc" => Self::Shell,
            "CMakeLists" => Self::Shell,
            _ => Self::Unknown,
        }
    }

    /// Detect language from extension string
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Self::Rust,
            "py" | "pyw" | "pyi" => Self::Python,
            "js" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" | "jsx" => Self::TypeScript, // Treat JSX/TSX as TypeScript
            "go" => Self::Go,
            "java" => Self::Java,
            "c" | "h" => Self::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Self::Cpp,
            "cs" => Self::CSharp,
            "rb" | "rake" => Self::Ruby,
            "php" => Self::Php,
            "swift" => Self::Swift,
            "kt" | "kts" => Self::Kotlin,
            "dart" => Self::Dart,
            "sh" | "bash" | "zsh" => Self::Shell,
            "md" | "markdown" | "txt" => Self::Markdown, // Treat txt as markdown-like
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "sql" => Self::Sql,
            "html" | "htm" => Self::Html,
            "css" | "scss" | "sass" | "less" => Self::Css,
            "xml" | "csproj" | "props" | "targets" | "resx" | "config" => Self::Xml,
            "ipynb" => Self::Jupyter,
            _ => Self::Unknown,
        }
    }

    /// Check if this language is supported for semantic chunking
    #[allow(dead_code)] // Reserved for tree-sitter chunking feature
    pub fn supports_tree_sitter(&self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::Python
                | Self::JavaScript
                | Self::TypeScript
                | Self::C
                | Self::Cpp
                | Self::CSharp
                | Self::Go
                | Self::Java
                | Self::Kotlin
                | Self::Dart
                | Self::Shell
                | Self::Ruby
                | Self::Php
                | Self::Yaml
                | Self::Json
                | Self::Markdown
        )
    }

    /// Check if this is a text-based language (should be indexed)
    pub fn is_indexable(&self) -> bool {
        !matches!(self, Self::Unknown)
    }

    /// Get the language name as a string
    pub fn name(&self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::CSharp => "C#",
            Self::Ruby => "Ruby",
            Self::Php => "PHP",
            Self::Swift => "Swift",
            Self::Kotlin => "Kotlin",
            Self::Dart => "Dart",
            Self::Shell => "Shell",
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Toml => "TOML",
            Self::Sql => "SQL",
            Self::Html => "HTML",
            Self::Css => "CSS",
            Self::Xml => "XML",
            Self::Jupyter => "Jupyter",
            Self::Unknown => "Unknown",
        }
    }
}

/// Lazily-loaded, process-global extension→language overrides.
fn extension_overrides() -> &'static HashMap<String, Language> {
    EXTENSION_OVERRIDES.get_or_init(load_extension_overrides)
}

/// Load user-defined extension→language overrides from the extension-map file.
///
/// Fail-safe by design: a missing file yields an empty map (no overrides), and
/// a malformed file or unknown language name is logged and skipped rather than
/// aborting indexing. Keys are normalised to a lowercased, dot-less extension
/// (`".INC"`, `"inc"` and `".inc"` all map to `"inc"`).
fn load_extension_overrides() -> HashMap<String, Language> {
    let mut map = HashMap::new();

    let Some(path) = global_extension_map_path() else {
        return map;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // No file = no overrides. Only surface genuinely unexpected read errors.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return map,
        Err(e) => {
            warn!(
                "Could not read extension map {}: {e} — ignoring",
                path.display()
            );
            return map;
        }
    };

    // Parse into a generic object so a single bad value (e.g. `{"inc": 3}`)
    // only drops that one entry rather than discarding the whole map.
    let raw: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "Ignoring malformed extension map {} (expected {{\"ext\": \"language\"}}): {e}",
                path.display()
            );
            return map;
        }
    };

    for (ext, value) in raw {
        let key = ext.trim().trim_start_matches('.').to_lowercase();
        if key.is_empty() {
            continue;
        }
        let Some(lang_name) = value.as_str() else {
            warn!(
                "Extension map {}: value for extension .{ext} must be a language name string — skipping",
                path.display()
            );
            continue;
        };
        match Language::from_name(lang_name) {
            Some(lang) => {
                map.insert(key, lang);
            }
            None => warn!(
                "Extension map {}: unknown language {lang_name:?} for extension .{ext} — skipping",
                path.display()
            ),
        }
    }

    if !map.is_empty() {
        tracing::info!(
            "Loaded {} extension override(s) from {}",
            map.len(),
            path.display()
        );
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Resolve a path against an *empty* override map — keeps `from_path`-style
    /// tests hermetic (the real `from_path` reads process-global user config
    /// from `~/.codesearch/extensions.json`, which must not influence tests).
    fn detect(path: &str) -> Language {
        Language::from_path_with_overrides(&PathBuf::from(path), &HashMap::new())
    }

    #[test]
    fn test_rust_detection() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(detect("main.rs"), Language::Rust);
    }

    #[test]
    fn test_python_detection() {
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("pyi"), Language::Python);
    }

    #[test]
    fn test_typescript_detection() {
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("tsx"), Language::TypeScript);
        assert_eq!(Language::from_extension("jsx"), Language::TypeScript);
    }

    #[test]
    fn test_php_detection() {
        assert_eq!(Language::from_extension("php"), Language::Php);
        // `.inc` is deliberately NOT a built-in mapping: it is language-agnostic
        // (assembly, SQL, C/C++ and PHP includes all use it). A repo that uses
        // the legacy `*.class.inc` PHP convention (#138) opts in via the
        // user-configurable extension map instead — see the override tests below.
        assert_eq!(Language::from_extension("inc"), Language::Unknown);
    }

    #[test]
    fn test_from_name_parses_canonical_and_aliases() {
        assert_eq!(Language::from_name("php"), Some(Language::Php));
        assert_eq!(Language::from_name("PHP"), Some(Language::Php));
        assert_eq!(Language::from_name("  Php  "), Some(Language::Php));
        assert_eq!(Language::from_name("c++"), Some(Language::Cpp));
        assert_eq!(Language::from_name("c#"), Some(Language::CSharp));
        assert_eq!(Language::from_name("golang"), Some(Language::Go));
        assert_eq!(Language::from_name("nonsense"), None);
        // "Unknown" is never a valid override target.
        assert_eq!(Language::from_name("unknown"), None);
    }

    #[test]
    fn test_extension_overrides_apply_and_take_precedence() {
        let mut overrides = HashMap::new();
        overrides.insert("inc".to_string(), Language::Php);
        // A user may deliberately remap a *known* extension too (.h → C++).
        overrides.insert("h".to_string(), Language::Cpp);

        // New mapping for a previously-unknown extension; note Path::extension()
        // returns only the last dot-suffix, so `Foo.class.inc` → "inc".
        assert_eq!(
            Language::from_path_with_overrides(&PathBuf::from("Foo.class.inc"), &overrides),
            Language::Php
        );
        // Override wins over the built-in (.h is normally C).
        assert_eq!(
            Language::from_path_with_overrides(&PathBuf::from("legacy.h"), &overrides),
            Language::Cpp
        );
        // Case-insensitive on the extension.
        assert_eq!(
            Language::from_path_with_overrides(&PathBuf::from("MODULE.INC"), &overrides),
            Language::Php
        );
        // Extensions not in the map still use the built-in table.
        assert_eq!(
            Language::from_path_with_overrides(&PathBuf::from("main.rs"), &overrides),
            Language::Rust
        );
        // An empty override map == pure built-in behaviour (so `.inc` stays Unknown).
        let empty = HashMap::new();
        assert_eq!(
            Language::from_path_with_overrides(&PathBuf::from("Foo.class.inc"), &empty),
            Language::Unknown
        );
    }

    #[test]
    fn test_shell_detection() {
        assert_eq!(Language::from_extension("sh"), Language::Shell);
        assert_eq!(Language::from_extension("bash"), Language::Shell);
        assert_eq!(Language::from_extension("zsh"), Language::Shell);
        assert_eq!(detect("scripts/deploy.sh"), Language::Shell);
        // Extensionless shell filenames
        assert_eq!(Language::from_filename("Dockerfile"), Language::Shell);
        assert_eq!(Language::from_filename("Makefile"), Language::Shell);
        assert_eq!(Language::from_filename(".env"), Language::Shell);
    }

    #[test]
    fn test_tree_sitter_support() {
        assert!(Language::Rust.supports_tree_sitter());
        assert!(Language::Python.supports_tree_sitter());
        assert!(Language::TypeScript.supports_tree_sitter());
        assert!(Language::Json.supports_tree_sitter());
        assert!(Language::Markdown.supports_tree_sitter());
        // Toml has no tree-sitter grammar yet.
        assert!(!Language::Toml.supports_tree_sitter());
    }

    #[test]
    fn test_indexable() {
        assert!(Language::Rust.is_indexable());
        assert!(Language::Markdown.is_indexable());
        assert!(!Language::Unknown.is_indexable());
    }

    #[test]
    fn test_jupyter_detection() {
        assert_eq!(Language::from_extension("ipynb"), Language::Jupyter);
        assert_eq!(detect("analysis.ipynb"), Language::Jupyter);
        assert!(
            Language::Jupyter.is_indexable(),
            "Jupyter should be indexable"
        );
        assert!(
            !Language::Jupyter.supports_tree_sitter(),
            "Jupyter should NOT support tree-sitter (uses custom JSON extraction)"
        );
    }
}
