//! Integration tests for codesearch
//!
//! These tests verify the end-to-end functionality of the codesearch system.

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Test helper to create a temporary project with sample code
fn create_test_project() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Create a simple Rust file
    let rust_file = temp_dir.path().join("src/lib.rs");
    fs::create_dir_all(temp_dir.path().join("src")).expect("Failed to create src dir");
    fs::write(
        &rust_file,
        r#"
/// A simple function to add two numbers
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// A function to multiply two numbers
pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

/// A struct to hold user data
pub struct User {
    pub name: String,
    pub age: u32,
}

impl User {
    pub fn new(name: String, age: u32) -> Self {
        Self { name, age }
    }

    pub fn is_adult(&self) -> bool {
        self.age >= 18
    }
}
"#,
    )
    .expect("Failed to write test file");

    // Create a JavaScript file
    let js_file = temp_dir.path().join("src/utils.js");
    fs::write(
        &js_file,
        r#"
// Utility function to format dates
function formatDate(date) {
    return date.toISOString();
}

// Function to validate email
function validateEmail(email) {
    return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email);
}

// Class for managing user sessions
class SessionManager {
    constructor() {
        this.sessions = new Map();
    }

    createSession(userId) {
        const sessionId = generateId();
        this.sessions.set(sessionId, { userId, createdAt: Date.now() });
        return sessionId;
    }

    getSession(sessionId) {
        return this.sessions.get(sessionId);
    }
}
"#,
    )
    .expect("Failed to write test file");

    temp_dir
}

#[test]
fn test_search_options_default() {
    use codesearch::search::SearchOptions;

    let options = SearchOptions::default();

    assert_eq!(options.max_results, 10);
    assert_eq!(options.per_file, None);
    assert_eq!(options.content_lines, 3);
    assert_eq!(options.show_scores, false);
    assert_eq!(options.compact, false);
    assert_eq!(options.sync, false);
    assert_eq!(options.json, false);
    assert_eq!(options.filter_path, None);
    assert_eq!(options.model_override, None);
    assert_eq!(options.vector_only, false);
    assert_eq!(options.rrf_k, None);
    assert_eq!(options.rerank, false);
    assert_eq!(options.rerank_top, None);
}

#[test]
fn test_search_options_custom() {
    use codesearch::search::SearchOptions;

    let options = SearchOptions {
        max_results: 20,
        per_file: Some(5),
        content_lines: 5,
        show_scores: true,
        compact: false,
        sync: true,
        json: false,
        filter_path: Some("src/".to_string()),
        model_override: Some("bge-small".to_string()),
        vector_only: false,
        rrf_k: Some(50),
        rerank: true,
        rerank_top: Some(100),
    };

    assert_eq!(options.max_results, 20);
    assert_eq!(options.per_file, Some(5));
    assert_eq!(options.content_lines, 5);
    assert_eq!(options.show_scores, true);
    assert_eq!(options.sync, true);
    assert_eq!(options.filter_path, Some("src/".to_string()));
    assert_eq!(options.model_override, Some("bge-small".to_string()));
    assert_eq!(options.rrf_k, Some(50));
    assert_eq!(options.rerank, true);
    assert_eq!(options.rerank_top, Some(100));
}

#[test]
fn test_error_creation() {
    use codesearch::error::{CodeSearchError, Result};

    // Test database error
    let err = CodeSearchError::database("Test database error");
    assert!(err.to_string().contains("Database error"));
    assert!(err.to_string().contains("Test database error"));

    // Test validation error
    let err = CodeSearchError::validation("Invalid input");
    assert!(err.to_string().contains("Validation error"));
    assert!(err.to_string().contains("Invalid input"));

    // Test I/O error
    let path = PathBuf::from("/test/path");
    let err = CodeSearchError::io(&path, "File not found");
    assert!(err.to_string().contains("I/O error"));
    assert!(err.to_string().contains("/test/path"));

    // Test Result type
    let result: Result<()> = Ok(());
    assert!(result.is_ok());

    let result: Result<()> = Err(CodeSearchError::search("Search failed"));
    assert!(result.is_err());
}

#[test]
fn test_file_walker() {
    use codesearch::file::FileWalker;

    let temp_dir = create_test_project();

    let walker = FileWalker::new(temp_dir.path());

    let (files, _stats) = walker.walk().unwrap();

    // Should find at least our 2 test files
    assert!(
        files.len() >= 2,
        "Expected at least 2 files, found {}",
        files.len()
    );

    // Check that we found the expected files
    let rust_file = files.iter().find(|f| f.path.ends_with("src/lib.rs"));
    assert!(rust_file.is_some(), "Should find src/lib.rs");

    let js_file = files.iter().find(|f| f.path.ends_with("src/utils.js"));
    assert!(js_file.is_some(), "Should find src/utils.js");
}

#[test]
fn test_model_type_dimensions() {
    use codesearch::embed::ModelType;

    // Test that different models have correct dimensions
    assert_eq!(ModelType::default().dimensions(), 384);
    assert_eq!(ModelType::AllMiniLML6V2.dimensions(), 384);
    assert_eq!(ModelType::BGESmallENV15.dimensions(), 384);
    assert_eq!(ModelType::BGEBaseENV15.dimensions(), 768);
    assert_eq!(ModelType::BGELargeENV15.dimensions(), 1024);
}

#[test]
fn test_model_type_from_str() {
    use codesearch::embed::ModelType;

    // Test model type parsing
    assert_eq!(
        ModelType::from_str("minilm-l6"),
        Some(ModelType::AllMiniLML6V2)
    );
    assert_eq!(
        ModelType::from_str("bge-small"),
        Some(ModelType::BGESmallENV15)
    );
    assert_eq!(
        ModelType::from_str("bge-base"),
        Some(ModelType::BGEBaseENV15)
    );
    assert_eq!(
        ModelType::from_str("bge-large"),
        Some(ModelType::BGELargeENV15)
    );
    assert_eq!(ModelType::from_str("invalid-model"), None);
}

#[test]
fn test_chunk_kind() {
    use codesearch::chunker::ChunkKind;

    // Test chunk kind display (using Debug instead of Display)
    assert_eq!(format!("{:?}", ChunkKind::Function), "Function");
    assert_eq!(format!("{:?}", ChunkKind::Struct), "Struct");
    assert_eq!(format!("{:?}", ChunkKind::Class), "Class");
    assert_eq!(format!("{:?}", ChunkKind::Interface), "Interface");
    assert_eq!(format!("{:?}", ChunkKind::Enum), "Enum");
    assert_eq!(format!("{:?}", ChunkKind::Method), "Method");
    assert_eq!(format!("{:?}", ChunkKind::Other), "Other");
}

#[test]
#[ignore] // Config module was removed - test kept for future config implementation
fn test_config_validation() {
    // Test default config
    // let config = Config::default();
    // assert_eq!(config.indexing.max_chunk_lines, 75);
    // assert_eq!(config.indexing.overlap_lines, 10);
    // assert_eq!(config.indexing.max_chunk_chars, 2000);

    // Test that chunk_size is validated
    // (This would require adding validation to Config::new or similar)
}

// Note: Full end-to-end indexing and search tests are omitted because:
// 1. They require downloading embedding models (slow, network-dependent)
// 2. They require creating full database structures (slow)
// 3. They are better suited for manual testing or CI environments
//
// For production, consider adding:
// - Integration tests that use a mock embedding service
// - Tests that verify database creation and querying
// - Performance benchmarks for large codebases
