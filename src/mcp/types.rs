//! MCP types and request/response structures
//!
//! The tool surface is consolidated into 5 tools:
//! - `search`   — semantic + literal search (mode: "semantic" | "literal")
//! - `find`     — symbol navigation (kind: "definition" | "usages" | "imports" | "dependents")
//! - `explore`  — file exploration (kind: "outline" | "similar")
//! - `get_chunk`— fetch chunk content by ID
//! - `status`   — index + project info (kind: "index" | "projects")

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════
// Consolidated request types (the 5 primary tools)
// ═══════════════════════════════════════════════════════════════════

/// Unified search request — replaces `semantic_search` and `literal_search`.
///
/// Set `mode` to choose the search backend:
/// - `"semantic"` (default) — vector embeddings + BM25 FTS + exact-identifier boosting, fused with RRF.
/// - `"literal"` — pure FTS, no embeddings. Supports regex, phrase, and exact-term matching.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchRequest {
    /// The search query (natural language, code snippet, regex, or exact term).
    pub query: String,

    /// Search backend: `"semantic"` (default) or `"literal"`.
    pub mode: Option<String>,

    // ── Semantic-mode options (ignored in literal mode) ──
    /// Return compact results (metadata only) to save tokens (default: true).
    /// Only applies in semantic mode.
    pub compact: Option<bool>,

    /// Override auto-detection of query intent (semantic mode only).
    /// `"auto"` (default) | `"semantic"` | `"lexical"` | `"hybrid"`
    pub semantic_mode: Option<String>,

    /// Only return results from files under this path prefix (semantic mode).
    pub filter_path: Option<String>,

    // ── Literal-mode options (ignored in semantic mode) ──
    /// Treat `query` as a regex pattern (literal mode only).
    pub regex: Option<bool>,

    /// Treat `query` as a phrase (literal mode only).
    pub phrase: Option<bool>,

    /// File glob filter (literal mode). e.g. "src/mcp/**", "**/*.rs"
    pub file_glob: Option<String>,

    /// Language filter (literal mode). e.g. "Rust", "Python"
    pub language: Option<String>,

    /// Output format for literal mode: `"json"` (default) or `"grep"`.
    pub format: Option<String>,

    // ── Common options ──
    /// Maximum number of results (default: 10 for semantic, 20 for literal).
    pub limit: Option<usize>,

    /// Route to a specific project (requires `codesearch serve`).
    #[serde(default)]
    pub project: Option<String>,

    /// Route to all projects in a group (requires `codesearch serve`).
    #[serde(default)]
    pub group: Option<String>,
}

/// Unified symbol navigation request — replaces `find_definition`, `find_usages`,
/// `find_imports`, and `find_dependents`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindRequest {
    /// What to find: `"definition"` (default) | `"usages"` | `"imports"` | `"dependents"`.
    pub kind: Option<String>,

    /// Symbol name (for definition/usages) or module path (for imports/dependents).
    pub symbol: String,

    /// Optional kind filter for definitions.
    /// Accepted: "Function" | "Class" | "Method" | "Struct" | "Trait" | "Enum" | "TypeAlias" | "Interface"
    pub definition_kind: Option<String>,

    /// Maximum number of results (default: 20).
    pub limit: Option<usize>,

    /// Route to a specific project (requires `codesearch serve`).
    #[serde(default)]
    pub project: Option<String>,

    /// Route to all projects in a group (requires `codesearch serve`).
    #[serde(default)]
    pub group: Option<String>,
}

/// Unified exploration request — replaces `file_outline` and `similar_chunks`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExploreRequest {
    /// What to explore: `"outline"` (default) | `"similar"`.
    pub kind: Option<String>,

    /// File path for outline mode, or chunk_id for similar mode.
    /// For outline: a file path relative to project root or absolute.
    /// For similar: a numeric chunk_id.
    pub target: String,

    /// Maximum number of results (default: 5 for similar, unlimited for outline).
    pub limit: Option<usize>,

    /// Route to a specific project (requires `codesearch serve`).
    #[serde(default)]
    pub project: Option<String>,

    /// Route to all projects in a group (requires `codesearch serve`).
    #[serde(default)]
    pub group: Option<String>,
}

/// Unified status/info request — replaces `index_status` and `list_projects`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct StatusRequest {
    /// What status to return: `"index"` (default) | `"projects"`.
    pub kind: Option<String>,

    /// Route to a specific project's index status (requires `codesearch serve`).
    #[serde(default)]
    pub project: Option<String>,

    /// Route to all projects in a group for aggregated index status (requires `codesearch serve`).
    #[serde(default)]
    pub group: Option<String>,
}

/// Symbol impact analysis request — returns transitive call-sites of a symbol
/// with file/line precision, using language-specific semantic analysis (SCIP).
///
/// Input variants:
/// - By name: `{ "symbol_name": "FieldDefinition.Validate", "project": "myrepo" }`
/// - By position: `{ "file": "src/Validation/FieldDefinition.cs", "line": 42, "project": "myrepo" }`
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FindImpactRequest {
    /// Symbol name to look up (e.g. `"FieldDefinition.Validate"`).
    /// Used when you know the name. Mutually exclusive with `file`+`line`.
    pub symbol_name: Option<String>,

    /// File path for position-based lookup (relative to project root or absolute).
    /// Must be combined with `line`.
    pub file: Option<String>,

    /// 1-based line number for position-based lookup.
    /// Must be combined with `file`.
    pub line: Option<u32>,

    /// Language filter (e.g. `"csharp"`). If omitted, auto-detects from file extension
    /// or searches all installed language adapters.
    pub language: Option<String>,

    /// Route to a specific project (requires `codesearch serve`).
    #[serde(default)]
    pub project: Option<String>,

    /// Route to all projects in a group (requires `codesearch serve`).
    #[serde(default)]
    pub group: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Internal parameter types (used by consolidated tools to dispatch to implementations)
// ═══════════════════════════════════════════════════════════════════

/// Internal params for semantic search (used by `search` tool dispatch).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SemanticSearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub compact: Option<bool>,
    pub filter_path: Option<String>,
    pub mode: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for literal search (used by `search` tool dispatch).
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct LiteralSearchRequest {
    pub query: String,
    pub regex: Option<bool>,
    pub phrase: Option<bool>,
    pub limit: Option<usize>,
    pub file_glob: Option<String>,
    pub language: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for find-definition (used by `find` tool dispatch).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDefinitionRequest {
    pub symbol: String,
    pub kind: Option<String>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for find-usages (used by `find` tool dispatch).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindUsagesRequest {
    pub symbol: String,
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for file-outline (used by `explore` tool dispatch).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileOutlineRequest {
    pub path: String,
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for find-imports (used by `find` tool dispatch).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindImportsRequest {
    pub path: String,
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for find-dependents (used by `find` tool dispatch).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDependentsRequest {
    pub symbol_or_path: String,
    pub limit: Option<usize>,
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Internal params for similar-chunks (used by `explore` tool dispatch).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilarChunksRequest {
    pub chunk_id: u32,
    pub limit: Option<usize>,
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Response types
// ═══════════════════════════════════════════════════════════════════

/// Search result item — returned by semantic search
#[derive(Debug, Serialize)]
pub struct SearchResultItem {
    pub chunk_id: u32,
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

/// Reference/call site item — returned by find_references, find_definition, find_usages
#[derive(Debug, Serialize)]
pub struct ReferenceItem {
    /// Chunk ID of the containing chunk
    pub chunk_id: u32,
    /// File path containing the reference
    pub path: String,
    /// Line number of the reference
    pub line: usize,
    /// The kind of chunk (e.g., "Function", "Method", "Class")
    pub kind: String,
    /// Signature of the containing function/method (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Relevance score (BM25 or RRF-fused)
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
    /// MCP mode: "serve_hub" or "stdio"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// Search result item — returned by literal search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiteralSearchResultItem {
    /// File path (relative to project root)
    pub path: String,
    /// Line number of the matching line (not the chunk start)
    pub start_line: usize,
    /// Same as start_line — literal search pinpoints a single matching line
    pub end_line: usize,
    /// The matching line (truncated to 200 chars centered on the match)
    pub snippet: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Response from `search(mode="literal")`.
///
/// Replaces the previous bare `Vec<LiteralSearchResultItem>` JSON array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiteralSearchResponse {
    pub results: Vec<LiteralSearchResultItem>,

    /// True when codesearch auto-escaped the query and enabled regex mode
    /// because the original query contained code-like punctuation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_promoted_to_regex: Option<bool>,

    /// Actionable note for the LLM caller (present iff auto_promoted_to_regex
    /// or low_confidence is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    /// True when results are empty or top BM25 score is below threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,

    /// Suggested next tool when low_confidence is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
}

/// Semantic search response wrapper with low-confidence signaling
#[derive(Debug, Serialize)]
pub struct SemanticSearchResponse {
    pub results: Vec<SearchResultItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
}

/// File outline entry
#[derive(Debug, Serialize)]
pub struct FileOutlineItem {
    pub chunk_id: u32,
    pub kind: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

/// Request to fetch a chunk by ID
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetChunkRequest {
    pub chunk_id: u32,
    pub context_lines: Option<usize>,
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// Response payload for get_chunk
#[derive(Debug, Serialize)]
pub struct GetChunkResponse {
    pub chunk_id: u32,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub signature: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_lines_clamped: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Import/dependency item found in a file
#[derive(Debug, Serialize)]
pub struct ImportItem {
    pub imported: String,
    pub line: usize,
    pub kind: String,
}

/// File/path dependent item
#[derive(Debug, Serialize)]
pub struct DependentItem {
    pub path: String,
    pub line: usize,
    pub import_statement: String,
}

/// Response for the `list_projects` tool.
#[derive(Debug, Serialize)]
pub struct ListProjectsResponse {
    pub repos: Vec<RepoInfo>,
    pub groups: HashMap<String, Vec<String>>,
    pub serve_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serve_url: Option<String>,
    pub current_directory: String,
}

/// Information about a single registered project/repo.
#[derive(Debug, Serialize)]
pub struct RepoInfo {
    pub alias: String,
    pub project_path: String,
    pub database_path: String,
    pub total_chunks: usize,
    pub total_files: usize,
    pub model: String,
    pub lock_status: String,
}

/// Health response served by `codesearch serve` at GET /health.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub codesearch_server: bool,
    pub version: String,
}

// ═══════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════

/// Validate project/group routing params — returns error message if invalid.
pub fn validate_project_group(
    project: &Option<String>,
    group: &Option<String>,
    serve_active: bool,
) -> Result<(), String> {
    match (project, group) {
        // Both project and group are mutually exclusive.
        (Some(_), Some(_)) => Err(
            "Cannot specify both `project` and `group` — they are mutually exclusive.".to_string(),
        ),
        // Either project or group is set but empty/whitespace.
        // The pattern binds `p` from whichever side is Some — both arms share the same guard.
        (Some(p), None) | (None, Some(p)) if p.trim().is_empty() => {
            Err("`project`/`group` must not be empty.".to_string())
        }
        // Either project or group is set but serve is not running.
        (Some(_), None) | (None, Some(_)) if !serve_active => {
            Err("project/group routing requires `codesearch serve` to be running.".to_string())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_neither_set() {
        assert!(validate_project_group(&None, &None, false).is_ok());
        assert!(validate_project_group(&None, &None, true).is_ok());
    }

    #[test]
    fn test_validate_both_set_rejected() {
        let err =
            validate_project_group(&Some("foo".into()), &Some("bar".into()), true).unwrap_err();
        assert!(
            err.contains("mutually exclusive"),
            "Expected mutual exclusion error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_project_requires_serve() {
        let err = validate_project_group(&Some("myrepo".into()), &None, false).unwrap_err();
        assert!(
            err.contains("serve"),
            "Expected serve-required error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_group_requires_serve() {
        let err = validate_project_group(&None, &Some("mygroup".into()), false).unwrap_err();
        assert!(
            err.contains("serve"),
            "Expected serve-required error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_project_ok_when_serving() {
        assert!(validate_project_group(&Some("myrepo".into()), &None, true).is_ok());
    }

    #[test]
    fn test_validate_group_ok_when_serving() {
        assert!(validate_project_group(&None, &Some("mygroup".into()), true).is_ok());
    }

    #[test]
    fn test_validate_empty_project_rejected() {
        let err = validate_project_group(&Some("".into()), &None, true).unwrap_err();
        assert!(
            err.contains("must not be empty"),
            "Expected empty error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_empty_group_rejected() {
        let err = validate_project_group(&None, &Some("  ".into()), true).unwrap_err();
        assert!(
            err.contains("must not be empty"),
            "Expected empty error, got: {}",
            err
        );
    }
}
