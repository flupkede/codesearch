//! Thin wrapper around JSON symbol index parsing.
//!
//! Parses the JSON output produced by the `scip-csharp` helper.
//! The parsed index is converted to a map of symbol name -> references.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// A reference extracted from a symbol index.
#[derive(Debug, Clone)]
pub struct ScipReference {
    /// File path relative to the project root.
    pub file: PathBuf,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based end line (inclusive).
    pub end_line: u32,
    /// Kind: "definition", "reference", "call", etc.
    pub kind: String,
}

/// Parsed symbol index: map from canonical symbol string to its references.
pub type ScipIndex = HashMap<String, Vec<ScipReference>>;

// ── JSON deserialization types (mirror C# ScipModels.cs) ─────────

#[derive(Deserialize)]
struct JsonIndex {
    #[allow(dead_code)]
    metadata: JsonMetadata,
    documents: Vec<JsonDocument>,
    #[allow(dead_code)]
    external_symbols: Vec<JsonSymbolInfo>,
}

#[derive(Deserialize)]
struct JsonMetadata {
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    tool_info: String,
}

#[derive(Deserialize)]
struct JsonDocument {
    relative_path: String,
    occurrences: Vec<JsonOccurrence>,
}

#[derive(Deserialize)]
struct JsonOccurrence {
    range: Vec<i64>,
    symbol: String,
    #[allow(dead_code)]
    symbol_roles: i64,
    /// "definition" or "reference" (or empty string).
    kind: Option<String>,
}

#[derive(Deserialize)]
struct JsonSymbolInfo {
    #[allow(dead_code)]
    symbol: String,
    #[allow(dead_code)]
    documentation: Vec<String>,
}

/// Role flags from SCIP (mirrors the protobuf enum values).
mod roles {
    pub const DEFINITION: u32 = 1;
    pub const READ_ACCESS: u32 = 2;
    pub const WRITE_ACCESS: u32 = 4;
    pub const IMPORT: u32 = 64;
    pub const IMPLEMENTATION: u32 = 256;
}

/// Parse a JSON byte slice into a symbol -> references map.
///
/// The JSON format is produced by `scip-csharp` and mirrors the SCIP schema
/// with snake_case field names. Line numbers in the JSON are already 1-based.
pub fn parse_json_index(data: &[u8]) -> Result<ScipIndex> {
    let index: JsonIndex = serde_json::from_slice(data)
        .with_context(|| "Failed to parse symbol index JSON")?;

    let mut result: ScipIndex = HashMap::new();

    for document in &index.documents {
        let doc_path = &document.relative_path;

        for occurrence in &document.occurrences {
            let symbol_name = &occurrence.symbol;
            if symbol_name.is_empty() {
                continue;
            }

            let range = &occurrence.range;
            if range.len() < 2 {
                continue;
            }

            // C# helper outputs 1-based lines already
            let start_line = range[0] as u32;
            let end_line = if range.len() >= 4 {
                range[2] as u32
            } else {
                start_line
            };

            // Use explicit kind from JSON if present, otherwise derive from roles
            let kind = occurrence.kind.as_deref()
                .filter(|k| !k.is_empty())
                .map(|k| k.to_string())
                .unwrap_or_else(|| role_to_kind(occurrence.symbol_roles as u32));

            let reference = ScipReference {
                file: PathBuf::from(doc_path),
                start_line,
                end_line,
                kind,
            };

            result
                .entry(symbol_name.clone())
                .or_default()
                .push(reference);
        }
    }

    Ok(result)
}

/// Convert SCIP symbol_roles bitmask to a human-readable kind string.
fn role_to_kind(roles: u32) -> String {
    if roles & roles::DEFINITION != 0 {
        "definition".to_string()
    } else if roles & roles::IMPLEMENTATION != 0 {
        "implementation".to_string()
    } else if roles & roles::IMPORT != 0 {
        "import".to_string()
    } else if roles & roles::WRITE_ACCESS != 0 {
        "write".to_string()
    } else if roles & roles::READ_ACCESS != 0 {
        "call".to_string()
    } else {
        "reference".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_to_kind() {
        assert_eq!(role_to_kind(1), "definition");
        assert_eq!(role_to_kind(2), "call");
        assert_eq!(role_to_kind(64), "import");
        assert_eq!(role_to_kind(256), "implementation");
        assert_eq!(role_to_kind(0), "reference");
        assert_eq!(role_to_kind(1 | 2), "definition"); // definition takes priority
    }

    #[test]
    fn test_parse_json_index_basic() {
        let json = r#"{
            "metadata": {"version": "1.0", "tool_info": "scip-csharp"},
            "documents": [{
                "relative_path": "src/Program.cs",
                "occurrences": [{
                    "range": [10, 0, 10, 5],
                    "symbol": "csharp App . Program.Main().",
                    "symbol_roles": 1,
                    "kind": "definition"
                }, {
                    "range": [20, 4],
                    "symbol": "csharp App . Program.Main().",
                    "symbol_roles": 0,
                    "kind": "reference"
                }]
            }],
            "external_symbols": []
        }"#;

        let index = parse_json_index(json.as_bytes()).unwrap();
        assert_eq!(index.len(), 1);

        let refs = &index["csharp App . Program.Main()."];
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].start_line, 10);
        assert_eq!(refs[0].end_line, 10);
        assert_eq!(refs[0].kind, "definition");
        assert_eq!(refs[1].start_line, 20);
        assert_eq!(refs[1].kind, "reference");
    }

    #[test]
    fn test_parse_json_index_empty_symbol_skipped() {
        let json = r#"{
            "metadata": {"version": "1.0", "tool_info": "test"},
            "documents": [{
                "relative_path": "src/A.cs",
                "occurrences": [{
                    "range": [1, 0],
                    "symbol": "",
                    "symbol_roles": 0,
                    "kind": ""
                }]
            }],
            "external_symbols": []
        }"#;

        let index = parse_json_index(json.as_bytes()).unwrap();
        assert!(index.is_empty());
    }
}
