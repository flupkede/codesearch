//! Search query engine: metadata reading, query classification and ranking
//! helpers shared by the CLI search flow and the MCP tools.
//!
//! The CLI command flow (argument plumbing, auto-create-index preamble,
//! terminal output) stays in the CLI crate; everything consumed by the MCP
//! layer lives here.

use std::path::Path;

/// Read model metadata from database
pub fn read_metadata(db_path: &Path) -> Option<(String, usize, Option<String>)> {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let model = json.get("model_short_name")?.as_str()?.to_string();
            let dims = json.get("dimensions")?.as_u64()? as usize;
            let primary_language = json
                .get("primary_language")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return Some((model, dims, primary_language));
        }
    }
    None
}

/// Detect if query contains likely code identifiers
///
/// Returns identifiers that look like:
/// - PascalCase (Class, Struct, Interface)
/// - snake_case (function, method)
/// - camelCase (property, variable)
pub fn detect_identifiers(query: &str) -> Vec<String> {
    let mut identifiers = Vec::new();
    for token in query.split_whitespace() {
        let is_pascal = token
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
            && token.chars().any(|c| c.is_lowercase())
            && !["Find", "Show", "Get", "Where", "How", "What", "All"].contains(&token);
        let is_snake =
            token.contains('_') && token.chars().all(|c| c.is_alphanumeric() || c == '_');
        let is_camel = token
            .chars()
            .next()
            .map(|c| c.is_lowercase())
            .unwrap_or(false)
            && token.chars().any(|c| c.is_uppercase());

        if is_pascal || is_snake || is_camel {
            identifiers.push(token.to_string());
        }
    }
    identifiers
}

/// Detects structural intent in user queries (e.g., "class X", "function foo")
/// Returns the ChunkKind that matches the intent, if any
///
/// This function now only returns a kind when the query contains BOTH:
/// 1. A structural keyword (class, struct, function, method, enum, interface, trait)
/// 2. A PascalCase or snake_case identifier suggesting a specific type/function
///
/// This prevents excessive noise where "enum" would boost ALL enums in results
pub fn detect_structural_intent(query: &str) -> Option<crate::chunker::ChunkKind> {
    use crate::chunker::ChunkKind;

    let query_lower = query.to_lowercase();

    // Check if query contains a PascalCase or snake_case identifier
    // This indicates the user is looking for a specific type/function, not just any of that kind
    let has_identifier = contains_identifier(query);

    if !has_identifier {
        return None; // No specific identifier - don't apply kind boost
    }

    if query_lower.contains("class ") {
        Some(ChunkKind::Class)
    } else if query_lower.contains("struct ") {
        Some(ChunkKind::Struct)
    } else if query_lower.contains("function ") || query_lower.contains("fn ") {
        Some(ChunkKind::Function)
    } else if query_lower.contains("method ") {
        Some(ChunkKind::Method)
    } else if query_lower.contains("enum ") {
        Some(ChunkKind::Enum)
    } else if query_lower.contains("interface ") {
        Some(ChunkKind::Interface)
    } else if query_lower.contains("trait ") {
        Some(ChunkKind::Trait)
    } else {
        None
    }
}

/// Checks if query contains a PascalCase or snake_case identifier
/// indicating a specific type/function name is being searched for
///
/// Simple heuristic without regex dependency:
/// - PascalCase: contains uppercase letter followed by lowercase/digit
/// - snake_case: contains underscore with lowercase letters around it
/// - camelCase: contains lowercase letter followed by uppercase letter
fn contains_identifier(query: &str) -> bool {
    let chars: Vec<char> = query.chars().collect();

    // Look for PascalCase: uppercase letter followed by lowercase letter or digit
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i].is_uppercase() && (chars[i + 1].is_lowercase() || chars[i + 1].is_ascii_digit())
        {
            return true;
        }
    }

    // Look for snake_case: underscore surrounded by lowercase letters
    for i in 1..chars.len().saturating_sub(1) {
        if chars[i] == '_' && chars[i - 1].is_lowercase() && chars[i + 1].is_lowercase() {
            return true;
        }
    }

    // Look for camelCase: lowercase letter followed by uppercase letter
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i].is_lowercase() && chars[i + 1].is_uppercase() {
            return true;
        }
    }

    false
}

/// Boosts results that match a specific ChunkKind by a factor
pub fn boost_kind(
    results: &mut [crate::vectordb::SearchResult],
    target_kind: crate::chunker::ChunkKind,
) {
    let boost_factor = 0.15; // 15% boost for matching kind
                             // Convert ChunkKind to string for comparison
    let target_kind_str = format!("{:?}", target_kind);
    for result in results.iter_mut() {
        if result.kind == target_kind_str {
            result.score *= 1.0 + boost_factor;
        }
    }
    // Re-sort after boosting
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
}

/// Detect query type and adapt RRF-k accordingly
/// Returns (vector_k, fts_k) based on query characteristics
pub fn adapt_rrf_k(query: &str) -> (f64, f64) {
    let has_identifiers = !detect_identifiers(query).is_empty();
    let has_structural_intent = detect_structural_intent(query).is_some();

    match (has_identifiers, has_structural_intent) {
        // Identifier queries: Prioritize vector search (semantic similarity)
        (true, _) => (12.0, 28.0), // Lower vector k, higher FTS k

        // Structural queries: Balance both
        (_, true) => (15.0, 25.0),

        // Semantic queries: Balanced
        _ => (20.0, 20.0),
    }
}

/// Find the first line of `content` matching the literal query (or regex) and
/// return `(0-based line index within the chunk, truncated snippet)`.
///
/// Shared by the MCP literal-scan path and the CLI literal scan fallback —
/// both must agree on what "a match" is, so the logic lives here once.
pub fn match_line_for_literal(
    content: &str,
    query: &str,
    regex: Option<&regex::Regex>,
) -> Option<(usize, String)> {
    if query.is_empty() {
        return None;
    }

    for (idx, line) in content.lines().enumerate() {
        if let Some(re) = regex {
            if let Some(m) = re.find(line) {
                let snippet = truncate_line_around_match(line, m.start(), 200);
                return Some((idx, snippet));
            }
        } else if let Some(pos) = line.find(query) {
            let snippet = truncate_line_around_match(line, pos, 200);
            return Some((idx, snippet));
        }
    }

    None
}

/// Truncate a matched line to `max_chars` centered on the match byte offset.
pub fn truncate_line_around_match(line: &str, match_start_byte: usize, max_chars: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_chars {
        return line.to_string();
    }

    let match_char_idx = line[..match_start_byte.min(line.len())].chars().count();
    let half = max_chars / 2;
    let mut start = match_char_idx.saturating_sub(half);
    let end = (start + max_chars).min(chars.len());
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    chars[start..end].iter().collect()
}
