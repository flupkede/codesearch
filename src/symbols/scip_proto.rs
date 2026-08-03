//! Standard SCIP protobuf parsing.
//!
//! Parses `.scip` files emitted by Sourcegraph indexers such as
//! `scip-typescript` into the same `ScipIndex` shape the C# JSON parser
//! (`scip_parse.rs`) produces, so all downstream storage/resolution code is
//! reusable. Unlike the C# helper's custom JSON, this reads the canonical
//! SCIP protobuf wire format via the `scip` crate (rust-protobuf bindings).
//!
//! Wired into `SymbolIndexerRegistry` via `TypeScriptSymbolIndexer::rebuild()`
//! (`typescript.rs`), which calls `parse_scip_protobuf` on the raw `.scip`
//! bytes produced by `scip-typescript index`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use protobuf::Message;
use scip::types::Index as ScipProtoIndex;

use crate::symbols::scip_parse::{ScipIndex, ScipReference};

/// Standard SCIP `SymbolRole` bitmask values (see `scip.proto`).
///
/// These mirror the `scip::types::SymbolRole` enum discriminants. scip-typescript
/// sets these on each occurrence so we can classify it as definition / call /
/// import / etc. NOTE: these are the *standard* protobuf values and intentionally
/// differ from the C# helper's custom JSON role encoding in `scip_parse::roles`
/// (`READ_ACCESS=2`, `IMPORT=64` there) — do not mix the two.
mod proto_roles {
    use scip::types::SymbolRole;
    pub const DEFINITION: i32 = SymbolRole::Definition as i32;
    pub const FORWARD_DEFINITION: i32 = SymbolRole::ForwardDefinition as i32;
    pub const IMPORT: i32 = SymbolRole::Import as i32;
    pub const WRITE_ACCESS: i32 = SymbolRole::WriteAccess as i32;
    pub const READ_ACCESS: i32 = SymbolRole::ReadAccess as i32;
}

/// Parse a SCIP protobuf byte slice (a `.scip` file's contents) into a
/// symbol → references map.
///
/// Line numbers in SCIP protobuf are 0-based; the returned `ScipReference`
/// lines are 1-based (matching the C# parser and the rest of the pipeline).
/// External symbol *information* (documentation etc.) is not needed here —
/// occurrences already carry the symbol string they reference, which is all
/// the reference map keys on.
pub fn parse_scip_protobuf(data: &[u8]) -> Result<ScipIndex> {
    let index =
        ScipProtoIndex::parse_from_bytes(data).context("Failed to parse SCIP protobuf index")?;

    let mut result: ScipIndex = HashMap::new();

    for document in &index.documents {
        let rel_path = document.relative_path.as_str();
        if rel_path.is_empty() {
            continue;
        }

        for occurrence in &document.occurrences {
            let symbol = occurrence.symbol.as_str();
            if symbol.is_empty() {
                continue;
            }

            let Some((start_line, end_line)) = decode_range(&occurrence.range) else {
                // Newer producers MAY set `typed_range` instead of the deprecated
                // `range` Vec; that path is not handled yet (TODO). Skip silently.
                continue;
            };

            let kind = role_to_kind(occurrence.symbol_roles);

            result
                .entry(symbol.to_string())
                .or_default()
                .push(ScipReference {
                    file: PathBuf::from(rel_path),
                    start_line,
                    end_line,
                    kind,
                });
        }
    }

    Ok(result)
}

/// Decode a SCIP compact range (`repeated int32`) into 1-based `(start_line, end_line)`.
///
/// Encoding (0-based, half-open `[start, end)`):
/// - 3 elements `[startLine, startChar, endChar]` → single line.
/// - 4 elements `[startLine, startChar, endLine, endChar]` → possibly multi-line.
fn decode_range(range: &[i32]) -> Option<(u32, u32)> {
    match range.len() {
        3 => {
            let line = range[0];
            if line < 0 {
                return None;
            }
            Some(((line + 1) as u32, (line + 1) as u32))
        }
        4 => {
            let start_line = range[0];
            let end_line = range[2];
            if start_line < 0 || end_line < start_line {
                return None;
            }
            Some(((start_line + 1) as u32, (end_line + 1) as u32))
        }
        _ => None,
    }
}

/// Map a standard SCIP `SymbolRole` bitmask to a kind string.
///
/// Mirrors the priority used by the C# JSON parser (`scip_parse::role_to_kind`)
/// but uses the *standard* protobuf role values. Forward definitions count as
/// definitions; a bare read access is reported as `"call"` (function/property
/// usage) to match the existing convention.
fn role_to_kind(roles: i32) -> String {
    if roles & (proto_roles::DEFINITION | proto_roles::FORWARD_DEFINITION) != 0 {
        "definition".to_string()
    } else if roles & proto_roles::IMPORT != 0 {
        "import".to_string()
    } else if roles & proto_roles::WRITE_ACCESS != 0 {
        "write".to_string()
    } else if roles & proto_roles::READ_ACCESS != 0 {
        "call".to_string()
    } else {
        "reference".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{Document, Index, Occurrence};

    fn occ(symbol: &str, range: Vec<i32>, roles: i32) -> Occurrence {
        let mut o = Occurrence::new();
        o.symbol = symbol.to_string();
        o.range = range;
        o.symbol_roles = roles;
        o
    }

    #[test]
    fn test_parse_scip_protobuf_defs_and_refs() {
        // `add` is defined once and called three times across two files.
        let sym = "typescript ts-sample add().";
        let mut doc_a = Document::new();
        doc_a.relative_path = "src/math.ts".to_string();
        doc_a
            .occurrences
            .push(occ(sym, vec![3, 0, 3, 10], proto_roles::DEFINITION));

        let mut doc_b = Document::new();
        doc_b.relative_path = "src/consumer.ts".to_string();
        doc_b
            .occurrences
            .push(occ(sym, vec![5, 0, 8], proto_roles::READ_ACCESS));
        doc_b
            .occurrences
            .push(occ(sym, vec![6, 4, 6, 12], proto_roles::READ_ACCESS));

        let mut doc_c = Document::new();
        doc_c.relative_path = "src/other.ts".to_string();
        doc_c
            .occurrences
            .push(occ(sym, vec![9, 10, 9, 18], proto_roles::READ_ACCESS));

        let mut index = Index::new();
        index.documents.push(doc_a);
        index.documents.push(doc_b);
        index.documents.push(doc_c);

        let bytes = index.write_to_bytes().expect("serialize index");
        let parsed = parse_scip_protobuf(&bytes).expect("parse index");

        let refs = parsed.get(sym).expect("symbol present");
        assert_eq!(refs.len(), 4, "1 definition + 3 call-sites");

        let defs: Vec<_> = refs.iter().filter(|r| r.kind == "definition").collect();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file.to_str().unwrap(), "src/math.ts");
        assert_eq!(defs[0].start_line, 4, "0-based line 3 -> 1-based line 4");
        assert_eq!(defs[0].end_line, 4);

        let calls: Vec<_> = refs.iter().filter(|r| r.kind == "call").collect();
        assert_eq!(calls.len(), 3);
        let files: Vec<&str> = calls.iter().map(|r| r.file.to_str().unwrap()).collect();
        assert!(files.contains(&"src/consumer.ts"));
        assert!(files.contains(&"src/other.ts"));
    }

    #[test]
    fn test_decode_range_single_and_multi_line() {
        // 3-element single-line: [line 2, .., ..] -> line 3.
        assert_eq!(decode_range(&[2, 0, 5]), Some((3, 3)));
        // 4-element multi-line: [line 2, .., line 4, ..] -> lines 3..5.
        assert_eq!(decode_range(&[2, 0, 4, 9]), Some((3, 5)));
        // 4-element same-line: [line 7, .., line 7, ..] -> line 8.
        assert_eq!(decode_range(&[7, 0, 7, 9]), Some((8, 8)));
    }

    #[test]
    fn test_decode_range_rejects_bad() {
        assert_eq!(decode_range(&[]), None);
        assert_eq!(decode_range(&[1]), None);
        assert_eq!(decode_range(&[1, 2, 3, 4, 5]), None);
        // Negative line.
        assert_eq!(decode_range(&[-1, 0, 0]), None);
        // end_line < start_line.
        assert_eq!(decode_range(&[5, 0, 2, 9]), None);
    }

    #[test]
    fn test_role_to_kind_priority() {
        assert_eq!(role_to_kind(proto_roles::DEFINITION), "definition");
        assert_eq!(role_to_kind(proto_roles::FORWARD_DEFINITION), "definition");
        assert_eq!(role_to_kind(proto_roles::IMPORT), "import");
        assert_eq!(role_to_kind(proto_roles::WRITE_ACCESS), "write");
        assert_eq!(role_to_kind(proto_roles::READ_ACCESS), "call");
        assert_eq!(role_to_kind(0), "reference");
        // Definition wins over read access when both set.
        assert_eq!(
            role_to_kind(proto_roles::DEFINITION | proto_roles::READ_ACCESS),
            "definition"
        );
    }

    #[test]
    fn test_parse_skips_empty_symbol_and_bad_range() {
        let mut doc = Document::new();
        doc.relative_path = "src/a.ts".to_string();
        // Empty symbol -> skipped.
        doc.occurrences
            .push(occ("", vec![1, 0], proto_roles::DEFINITION));
        // Bad range (1 element) -> skipped.
        doc.occurrences.push(occ("typescript x foo().", vec![1], 0));
        // Well-formed definition survives.
        doc.occurrences.push(occ(
            "typescript x bar().",
            vec![2, 0, 5],
            proto_roles::DEFINITION,
        ));

        let mut index = Index::new();
        index.documents.push(doc);
        let bytes = index.write_to_bytes().unwrap();

        let parsed = parse_scip_protobuf(&bytes).unwrap();
        assert_eq!(parsed.len(), 1, "only the well-formed occurrence survives");
        assert!(parsed.contains_key("typescript x bar()."));
    }

    #[test]
    fn test_parse_empty_bytes_returns_empty() {
        // Empty input parses as a default (empty) Index, yielding an empty map.
        let parsed = parse_scip_protobuf(&[]).expect("empty bytes are a valid empty index");
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_parse_skips_document_with_empty_relative_path() {
        let mut doc = Document::new();
        doc.relative_path = String::new(); // no path -> document skipped
        doc.occurrences.push(occ(
            "typescript x ghost().",
            vec![1, 0],
            proto_roles::DEFINITION,
        ));

        let mut index = Index::new();
        index.documents.push(doc);
        let bytes = index.write_to_bytes().unwrap();

        let parsed = parse_scip_protobuf(&bytes).unwrap();
        assert!(
            parsed.is_empty(),
            "occurrences in path-less documents are dropped"
        );
    }
}
