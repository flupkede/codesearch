//! MCP types and request/response structures

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Request for semantic/hybrid search
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

    /// Override auto-detection of query intent.
    /// "auto" (default) | "semantic" | "lexical" | "hybrid"
    /// - "semantic": skip FTS fusion, use vector results only
    /// - "lexical":  skip embedding, use FTS path only
    /// - "hybrid":   force full hybrid even if auto would choose a single path
    pub mode: Option<String>,
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

/// Search result item - returned by semantic_search
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
    /// Index status: "not_indexed", "building", "ready", "error"
    pub status: String,
    /// Human-readable status message
    pub status_message: String,
    pub total_chunks: usize,
    pub total_files: usize,
    pub model: String,
    pub dimensions: usize,
    pub max_chunk_id: u32,
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

/// Semantic search response wrapper with low-confidence signaling
#[derive(Debug, Serialize)]
pub struct SemanticSearchResponse {
    /// Search results
    pub results: Vec<SearchResultItem>,
    /// Set when the top RRF score is below the confidence threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,
    /// Populated alongside `low_confidence`. Suggests a better-suited tool for this query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
}

/// Request to find the definition of a symbol
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDefinitionRequest {
    /// Symbol name (function, class, method, struct, trait, enum, type)
    pub symbol: String,
    /// Optional filter to a specific kind. If omitted, all definition kinds are searched.
    /// Accepted: "Function" | "Class" | "Method" | "Struct" | "Trait" | "Enum" | "TypeAlias" | "Interface"
    pub kind: Option<String>,
    /// Maximum number of results to return (default: 20)
    pub limit: Option<usize>,
}

/// Request to find usages/call-sites of a symbol
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindUsagesRequest {
    /// Symbol name to find usages for
    pub symbol: String,
    /// Maximum number of results to return (default: 20)
    pub limit: Option<usize>,
}
