//! Thin wrapper around SCIP protobuf parsing.
//!
//! Uses the `scip` crate from Sourcegraph to decode SCIP index files.
//! The parsed index is converted to a map of symbol name → references.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use protobuf::Message;

/// A reference extracted from a SCIP index.
#[derive(Debug, Clone)]
pub struct ScipReference {
    /// File path relative to the project root.
    pub file: PathBuf,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based end line (inclusive).
    pub end_line: u32,
    /// Kind: "definition", "reference", "implementation", etc.
    pub kind: String,
}

/// Parsed SCIP index: map from canonical symbol string to its references.
pub type ScipIndex = HashMap<String, Vec<ScipReference>>;

/// Role flags from SCIP (mirrors the protobuf enum values).
mod roles {
    pub const DEFINITION: u32 = 1;
    pub const READ_ACCESS: u32 = 2;
    pub const WRITE_ACCESS: u32 = 4;
    pub const IMPORT: u32 = 64;
    pub const IMPLEMENTATION: u32 = 256;
}

/// Parse a SCIP protobuf byte slice into a symbol → references map.
pub fn parse_scip(data: &[u8]) -> Result<ScipIndex> {
    let index = scip::types::Index::parse_from_bytes(data)
        .with_context(|| "Failed to parse SCIP protobuf")?;

    let mut result: ScipIndex = HashMap::new();

    // Process documents
    for document in &index.documents {
        let doc_path = document.relative_path.clone();

        for occurrence in &document.occurrences {
            let symbol_name = &occurrence.symbol;
            if symbol_name.is_empty() {
                continue;
            }

            let range = &occurrence.range;
            if range.len() < 3 {
                continue;
            }

            let start_line = (range[0] + 1) as u32; // SCIP lines are 0-based
            let end_line = if range.len() >= 4 {
                (range[2] + 1) as u32
            } else {
                start_line
            };

            let kind = role_to_kind(occurrence.symbol_roles as u32);

            let reference = ScipReference {
                file: PathBuf::from(&doc_path),
                start_line,
                end_line,
                kind,
            };

            result
                .entry(symbol_name.clone())
                .or_default()
                .push(reference);
        }

        // Also track external symbols (defined outside this index)
        for ext_symbol in &document.symbols {
            let _ = ext_symbol; // Available for future use
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
}
