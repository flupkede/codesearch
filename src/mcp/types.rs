//! MCP types and request/response structures

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Request for semantic search
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SemanticSearchRequest {
    /// The search query (natural language or code snippet)
    pub query: String,

    /// Maximum number of results to return (default: 10)
    pub limit: Option<usize>,

    /// Return compact results (metadata only) to save tokens (default: true).
    /// When true: returns only path, start_line, end_line, kind, signature, score.
    /// When false: also includes full code content and surrounding context.
    /// Use compact=true (default) and then read specific files with line offsets for the code you need.
    pub compact: Option<bool>,

    /// Only return results from files under this path prefix (e.g., "src/api/")
    pub filter_path: Option<String>,
}

/// Request to get file chunks
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileChunksRequest {
    /// Path to the file (relative to project root)
    pub path: String,

    /// Return compact results (metadata only) to save tokens (default: true).
    /// When true: returns only path, start_line, end_line, kind, signature.
    /// When false: also includes full code content.
    pub compact: Option<bool>,
}

/// Request to find references/call sites of a symbol.
/// Use this AFTER semantic_search to find where a function/class/variable is used.
/// Use this INSTEAD OF grep for finding symbol usages in the codebase.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindReferencesRequest {
    /// The symbol name to find references for (e.g., "authenticate", "User", "Config")
    pub symbol: String,

    /// Maximum number of references to return (default: 20)
    pub limit: Option<usize>,
}

/// Search result item - returned by semantic_search and get_file_chunks
#[derive(Debug, Serialize)]
pub struct SearchResultItem {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_prev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_next: Option<String>,
}

/// Reference/call site item - returned by find_references
#[derive(Debug, Serialize)]
pub struct ReferenceItem {
    /// File path containing the reference
    pub path: String,
    /// Line number of the reference
    pub line: usize,
    /// The kind of chunk containing the reference (e.g., "Function", "Method")
    pub kind: String,
    /// Signature of the containing function/method (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// FTS relevance score
    pub score: f32,
}

/// Index status response
#[derive(Debug, Serialize)]
pub struct IndexStatusResponse {
    pub indexed: bool,
    pub total_chunks: usize,
    pub total_files: usize,
    pub model: String,
    pub dimensions: usize,
    pub db_path: String,
    pub project_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Database info response
#[derive(Debug, Serialize)]
pub struct DatabaseInfoResponse {
    pub database_path: String,
    pub project_path: String,
    pub is_current_directory: bool,
    pub depth_from_current: usize,
    pub total_chunks: usize,
    pub total_files: usize,
    pub model: String,
}

/// Find databases response
#[derive(Debug, Serialize)]
pub struct FindDatabasesResponse {
    pub databases: Vec<DatabaseInfoResponse>,
    pub message: String,
    pub current_directory: String,
}
