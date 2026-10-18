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
    metadata: JsonMetadata,
    documents: Vec<JsonDocument>,
    #[allow(dead_code)]
    external_symbols: Vec<JsonSymbolInfo>,
    #[serde(default)]
    references: Option<Vec<JsonIndexReference>>,
}

#[derive(Deserialize)]
struct JsonIndexReference {
    symbol: String,
    file: String,
    start_line: u32,
    end_line: u32,
    kind: String,
}

/// Output of `scip-csharp index`.
#[derive(Debug)]
pub struct ParsedIndex {
    /// Definition occurrences per canonical symbol.
    pub definitions: ScipIndex,
    /// Solution-wide references per canonical symbol; `None` when the helper
    /// did not compute them.
    pub references: Option<ScipIndex>,
}

#[derive(Deserialize)]
struct JsonMetadata {
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

/// The scip-csharp `find-refs` / `batch-find-refs` output version this parser
/// understands; `index` versions are [`crate::constants::SCIP_CSHARP_INDEX_VERSIONS`].
/// Bump together with the helper whenever either JSON schema changes.
pub const SUPPORTED_INDEX_VERSION: &str = "2.0";

// ── find-refs output format ───────────────────────────────────────

/// Result of `scip-csharp find-refs` — references for a single symbol.
pub struct FindRefsResult {
    /// Reference locations (kind = "reference"). Does not include definitions.
    pub references: Vec<ScipReference>,
    /// Non-fatal problems the helper survived while resolving (project
    /// compile failures, FindReferencesAsync exceptions). Non-empty means
    /// `references` may be incomplete. Empty when the helper pre-dates the
    /// field (serde default) or the run was clean.
    pub warnings: Vec<String>,
}

#[derive(Deserialize)]
struct JsonFindRefsOutput {
    version: String,
    #[allow(dead_code)]
    symbol: String,
    references: Vec<JsonFindRefsOccurrence>,
    /// Absent in helper output from before warnings existed — default to
    /// empty so old binaries keep parsing as "complete".
    #[serde(default)]
    warnings: Vec<String>,
}

#[derive(Deserialize)]
struct JsonFindRefsOccurrence {
    file: String,
    start_line: u32,
    end_line: u32,
    kind: String,
}

/// Parse the JSON output of `scip-csharp find-refs` into a list of references.
pub fn parse_find_refs_output(data: &[u8]) -> Result<FindRefsResult> {
    let output: JsonFindRefsOutput =
        serde_json::from_slice(data).with_context(|| "Failed to parse find-refs JSON")?;

    if output.version != SUPPORTED_INDEX_VERSION {
        anyhow::bail!(
            "Unsupported find-refs version: '{}' (expected '{}').",
            output.version,
            SUPPORTED_INDEX_VERSION
        );
    }

    let references = output
        .references
        .into_iter()
        .map(|r| ScipReference {
            file: PathBuf::from(&r.file),
            start_line: r.start_line,
            end_line: r.end_line,
            kind: r.kind,
        })
        .collect();

    Ok(FindRefsResult {
        references,
        warnings: output.warnings,
    })
}

/// Parse `scip-csharp index` JSON into definitions plus optional references.
///
/// The JSON format mirrors the SCIP schema with snake_case field names. Line
/// numbers in the JSON are already 1-based.
pub fn parse_json_index(data: &[u8]) -> Result<ParsedIndex> {
    let index: JsonIndex =
        serde_json::from_slice(data).with_context(|| "Failed to parse symbol index JSON")?;

    let accepted = crate::constants::SCIP_CSHARP_INDEX_VERSIONS;
    if !accepted.contains(&index.metadata.version.as_str()) {
        anyhow::bail!(
            "Unsupported scip-csharp index version: '{}' (expected one of {:?}). \
             The scip-csharp helper may need to be rebuilt.",
            index.metadata.version,
            accepted
        );
    }

    let references = index.references.map(|refs| {
        let mut by_symbol: ScipIndex = HashMap::new();
        for r in refs.into_iter().filter(|r| !r.symbol.is_empty()) {
            by_symbol.entry(r.symbol).or_default().push(ScipReference {
                file: PathBuf::from(r.file),
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
            });
        }
        by_symbol
    });

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
            let kind = occurrence
                .kind
                .as_deref()
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

    Ok(ParsedIndex {
        definitions: result,
        references,
    })
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
            "metadata": {"version": "2.0", "tool_info": "scip-csharp"},
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

        let parsed = parse_json_index(json.as_bytes()).unwrap();
        assert!(parsed.references.is_none(), "2.0 carries no references");
        let index = parsed.definitions;
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
            "metadata": {"version": "2.0", "tool_info": "test"},
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

        let index = parse_json_index(json.as_bytes()).unwrap().definitions;
        assert!(index.is_empty());
    }

    const INDEX_2_1_HEAD: &str = r#"{
        "metadata": {"version": "2.1", "tool_info": "scip-csharp"},
        "documents": [{
            "relative_path": "src/A.cs",
            "occurrences": [{
                "range": [1, 0, 1, 5],
                "symbol": "csharp Ns . A#Run().",
                "symbol_roles": 1,
                "kind": "definition"
            }]
        }],
        "external_symbols": []"#;

    fn index_2_1(references: &str) -> String {
        format!("{INDEX_2_1_HEAD}, \"references\": {references}}}")
    }

    #[test]
    fn index_2_1_with_references_groups_them_per_symbol() {
        let json = index_2_1(
            r#"[
            {"symbol": "csharp Ns . A#Run().", "file": "src/B.cs", "start_line": 3, "end_line": 3, "kind": "reference"},
            {"symbol": "csharp Ns . A#Run().", "file": "src/C.cs", "start_line": 7, "end_line": 8, "kind": "reference"}
        ]"#,
        );
        let parsed = parse_json_index(json.as_bytes()).unwrap();
        assert_eq!(parsed.definitions.len(), 1);
        let refs = parsed.references.expect("2.1 list must parse as Some");
        let run = &refs["csharp Ns . A#Run()."];
        assert_eq!(run.len(), 2);
        assert_eq!(run[0].file, PathBuf::from("src/B.cs"));
        assert_eq!((run[1].start_line, run[1].end_line), (7, 8));
        assert_eq!(run[1].kind, "reference");
    }

    #[test]
    fn index_2_1_with_empty_references_is_some_empty() {
        let refs = parse_json_index(index_2_1("[]").as_bytes())
            .unwrap()
            .references;
        assert!(refs.is_some_and(|r| r.is_empty()));
    }

    #[test]
    fn index_2_1_with_null_references_is_none() {
        let parsed = parse_json_index(index_2_1("null").as_bytes()).unwrap();
        assert_eq!(parsed.definitions.len(), 1);
        assert!(parsed.references.is_none());
    }

    #[test]
    fn test_parse_json_index_rejects_unknown_version() {
        let json = r#"{
            "metadata": {"version": "9.9", "tool_info": "x"},
            "documents": [],
            "external_symbols": []
        }"#;
        let err = parse_json_index(json.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported"),
            "expected 'Unsupported' in error, got: {}",
            err
        );
    }

    #[test]
    fn test_parse_find_refs_output_carries_warnings_through() {
        let json = r#"{
            "version": "2.0",
            "symbol": "csharp Ns . V#Validate().",
            "references": [{
                "file": "src/A.cs",
                "start_line": 3,
                "end_line": 3,
                "kind": "reference"
            }],
            "warnings": [
                "could not compile project 'Broken' — its symbols are missing from the map",
                "FindReferencesAsync failed for Validate: InvalidOperationException: boom"
            ]
        }"#;

        let result = parse_find_refs_output(json.as_bytes()).unwrap();
        assert_eq!(result.references.len(), 1);
        assert_eq!(result.warnings.len(), 2, "warnings must survive the parse");
        assert!(result.warnings[0].contains("could not compile"));
        assert!(
            result.warnings[1].contains("FindReferencesAsync failed for Validate"),
            "warning text must round-trip verbatim, got: {}",
            result.warnings[1]
        );
    }

    #[test]
    fn test_parse_find_refs_output_without_warnings_field_is_complete() {
        // Old helper binaries emit no warnings field: the serde default must
        // parse them as EMPTY (complete), never fail.
        let json = r#"{
            "version": "2.0",
            "symbol": "csharp Ns . V#Validate().",
            "references": [{
                "file": "src/A.cs",
                "start_line": 3,
                "end_line": 3,
                "kind": "reference"
            }]
        }"#;

        let result = parse_find_refs_output(json.as_bytes()).unwrap();
        assert_eq!(result.references.len(), 1);
        assert!(
            result.warnings.is_empty(),
            "missing field must default to complete, got: {:?}",
            result.warnings
        );
    }
}
