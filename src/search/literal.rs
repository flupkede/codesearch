//! Literal FTS-only search — the CLI face of what MCP `literal_search` does.
//!
//! Runs the Tantivy primitives directly: no embeddings, no RRF fusion, no
//! model load. All three variants match ANALYZED TOKENS, never raw text —
//! the tokenizer splits on punctuation, so punctuation never reaches the
//! token index (see the module docs on `--regex` in the CLI).

use anyhow::Result;
use colored::Colorize;
use regex::Regex;
use std::path::Path;
use std::time::Instant;

use super::{JsonOutput, JsonResult, JsonTiming, SearchOptions};
use crate::fts::FtsStore;
use crate::vectordb::ChunkStore;
#[cfg(test)]
use crate::vectordb::VectorStore;

pub async fn run(
    query: &str,
    db_path: &Path,
    project_path: &Path,
    dimensions: usize,
    options: &SearchOptions,
) -> Result<()> {
    let start = Instant::now();
    let (results, scan_fallback) =
        search_fts(query, db_path, project_path, dimensions, options).await?;
    let search_ms = start.elapsed().as_millis() as u64;
    emit(query, &results, options, search_ms, scan_fallback)
}

/// The search core: FTS primitive dispatch → chunk resolution → filter →
/// dedup → truncate. Separated from `run` so tests can drive it without
/// stdout. The bool reports whether the raw-content scan fallback ran.
async fn search_fts(
    query: &str,
    db_path: &Path,
    project_path: &Path,
    dimensions: usize,
    options: &SearchOptions,
) -> Result<(Vec<crate::vectordb::SearchResult>, bool)> {
    let fts = FtsStore::new(db_path)?;
    let limit = options.max_results.max(1);

    let mut fts_results = if options.regex {
        let hits = fts.search_regex(query, limit)?;
        if hits.is_empty() {
            // Token-regex cannot span separators (the analyzer splits on
            // punctuation), so a pattern like `ref:classification\s+search=`
            // can be 0-for-N even when the text exists. Fall back to a raw
            // chunk-content scan — the same escape hatch MCP literal_search
            // uses for tokenless patterns.
            let scanned =
                scan_chunks_for_regex(query, db_path, project_path, dimensions, options).await?;
            return Ok((scanned, true));
        }
        hits
    } else if options.phrase {
        fts.search_phrase(query, limit)?
    } else {
        let hits = fts.search(query, limit, None)?;
        if hits.is_empty() {
            // Mirror MCP literal_search: exact (AND) empty → relaxed OR retry.
            fts.search_relaxed(query, limit)?
        } else {
            hits
        }
    };
    fts_results.truncate(limit);

    // Resolve chunk content via the vector store's metadata side of the DB.
    let model = crate::embed::ModelType::from_index_metadata(db_path).unwrap_or_default();
    let store =
        ChunkStore::open_for_path(db_path, project_path, model.short_name(), dimensions, true)
            .await?;
    let mut results: Vec<crate::vectordb::SearchResult> = Vec::with_capacity(fts_results.len());
    for hit in fts_results {
        // A store error must never silently become "no results".
        match store.get_chunk_as_result(hit.chunk_id).await {
            Ok(Some(mut r)) => {
                r.score = hit.score;
                results.push(r);
            }
            Ok(None) => {}
            Err(e) => anyhow::bail!("chunk {} unreadable: {e:#}", hit.chunk_id),
        }
    }

    // Path filter, same normalization as the hybrid tail.
    Ok((finalize_results(results, project_path, options), false))
}

/// Shared literal-path tail: path filter → dedup by (path, start, end),
/// newest chunk wins → score sort → truncate. Both the FTS path and the
/// raw-content scan fallback end here so the two cannot drift.
fn finalize_results(
    mut results: Vec<crate::vectordb::SearchResult>,
    project_path: &Path,
    options: &SearchOptions,
) -> Vec<crate::vectordb::SearchResult> {
    let filter_path_normalized = options
        .filter_path
        .as_ref()
        .map(|f| crate::cache::normalize_filter_path(f));
    if let Some(ref filter) = filter_path_normalized {
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };
        results.retain(|r| {
            crate::cache::path_matches_filter(&r.path, filter, &project_root_normalized)
        });
    }

    // Deduplicate by (path, start_line, end_line), newest chunk wins — same
    // rule as the hybrid tail.
    results.sort_by_key(|b| std::cmp::Reverse(b.id));
    {
        let mut seen: std::collections::HashSet<(String, usize, usize)> =
            std::collections::HashSet::new();
        results.retain(|r| seen.insert((r.path.clone(), r.start_line, r.end_line)));
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(options.max_results);

    results
}

/// Raw chunk-content scan for regex patterns the token index cannot answer.
/// The FTS analyzer splits on punctuation, so a token-regex like
/// `ref:classification\s+search=` never matches even when the text exists;
/// this walks every chunk's raw content instead. Unranked (score 0.0,
/// insertion order), bounded by `max_results`.
async fn scan_chunks_for_regex(
    query: &str,
    db_path: &Path,
    project_path: &Path,
    dimensions: usize,
    options: &SearchOptions,
) -> Result<Vec<crate::vectordb::SearchResult>> {
    let re = regex::Regex::new(query).map_err(|e| anyhow::anyhow!("invalid regex: {e:#}"))?;
    let model = crate::embed::ModelType::from_index_metadata(db_path).unwrap_or_default();
    let store =
        ChunkStore::open_for_path(db_path, project_path, model.short_name(), dimensions, true)
            .await?;
    let limit = options.max_results.max(1);

    let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
    for (chunk_id, meta) in store.iter_all_chunks().await? {
        if results.len() >= limit {
            break;
        }
        let Some((offset, snippet)) = match_line_for_literal(&meta.content, query, Some(&re))
        else {
            continue;
        };
        // Filter inside the loop (like the MCP scan): the scan is unranked
        // insertion order, so breaking at `limit` unfiltered can stop before
        // any filter-matching chunk is reached.
        let filter_normalized = options
            .filter_path
            .as_deref()
            .map(crate::cache::normalize_path_str)
            .unwrap_or_default();
        let project_root_normalized =
            crate::cache::normalize_path_str(project_path.to_str().unwrap_or(""))
                .trim_end_matches('/')
                .to_string();
        if !crate::cache::path_matches_filter(
            &meta.path,
            &filter_normalized,
            &project_root_normalized,
        ) {
            continue;
        }
        results.push(crate::vectordb::SearchResult {
            id: chunk_id,
            content: snippet,
            path: meta.path,
            start_line: meta.start_line + offset,
            end_line: meta.start_line + offset,
            kind: meta.kind,
            signature: meta.signature,
            docstring: None,
            context: None,
            hash: meta.hash,
            distance: 0.0,
            score: 0.0,
            context_prev: None,
            context_next: None,
            origin: None,
            also_in: Vec::new(),
        });
    }

    Ok(finalize_results(results, project_path, options))
}

fn emit(
    query: &str,
    results: &[crate::vectordb::SearchResult],
    options: &SearchOptions,
    search_ms: u64,
    scan_fallback: bool,
) -> Result<()> {
    if options.json {
        let compact = options.compact;
        let json_results: Vec<JsonResult> = results
            .iter()
            .map(|r| JsonResult {
                path: r.path.clone(),
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind.clone(),
                content: if compact {
                    None
                } else {
                    Some(r.content.clone())
                },
                score: r.score,
                signature: r.signature.clone(),
                context_prev: if compact {
                    None
                } else {
                    r.context_prev.clone()
                },
                context_next: if compact {
                    None
                } else {
                    r.context_next.clone()
                },
            })
            .collect();
        let timing = if options.show_scores {
            Some(JsonTiming {
                total_ms: search_ms,
                embed_ms: 0,
                search_ms,
                rerank_ms: None,
            })
        } else {
            None
        };
        println!(
            "{}",
            serde_json::to_string(&JsonOutput {
                query: query.to_string(),
                results: json_results,
                timing,
                scan_fallback: if scan_fallback { Some(true) } else { None },
            })?
        );
        return Ok(());
    }

    if options.compact {
        let mut seen_files = std::collections::HashSet::new();
        for r in results {
            if seen_files.insert(r.path.clone()) {
                println!("{}", r.path);
            }
        }
        return Ok(());
    }

    // Human output.
    println!(
        "{}",
        "🔍 Literal search results (FTS tokens)"
            .bright_cyan()
            .bold()
    );
    println!("{}", "=".repeat(60));
    println!("Query: \"{}\"", query);
    if scan_fallback {
        println!(
            "{}",
            "Token-regex matched nothing in the token index — showing unranked \
             raw chunk-content scan results."
                .dimmed()
        );
    }
    if results.is_empty() {
        println!(
            "{}",
            "No matches. Note: literal mode matches analyzed tokens — punctuation \
             (quotes, colons, equals signs) never reaches the token index. Drop the \
             punctuation, or use semantic search for raw-text intent."
                .dimmed()
        );
        return Ok(());
    }
    for (i, r) in results.iter().enumerate() {
        let score = if options.show_scores {
            format!("  [{:.4}]", r.score)
        } else {
            String::new()
        };
        println!(
            "{}. {}:{}-{} [{}]{}",
            i + 1,
            r.path,
            r.start_line,
            r.end_line,
            r.kind,
            score
        );
        let snippet: String = r
            .content
            .lines()
            .take(options.content_lines.max(1))
            .collect::<Vec<_>>()
            .join("\n   ");
        println!("   {}", snippet);
    }
    Ok(())
}

/// Find the first line of `content` matching the literal query (or regex) and
/// return `(0-based line index within the chunk, truncated snippet)`.
///
/// Shared by the MCP literal-scan path and the CLI literal scan fallback —
/// both must agree on what "a match" is, so the logic lives here once.
pub(crate) fn match_line_for_literal(
    content: &str,
    query: &str,
    regex: Option<&Regex>,
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
pub(crate) fn truncate_line_around_match(
    line: &str,
    match_start_byte: usize,
    max_chars: usize,
) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{Chunk, ChunkKind};
    use crate::embed::EmbeddedChunk;

    fn seed(db_path: &Path) {
        std::fs::create_dir_all(db_path).unwrap();
        let mut vs = VectorStore::new(db_path, 2).unwrap();
        // insert_chunks assigns ids from 0 per store, in insert order.
        let chunk_a = Chunk::new(
            "<rule>ref:classification search=\"x\" and no watermark</rule>".to_string(),
            0,
            0,
            ChunkKind::Block,
            "src/a.xml".to_string(),
        );
        let chunk_b = Chunk::new(
            "fn main() {} unrelated body".to_string(),
            0,
            0,
            ChunkKind::Function,
            "src/b.rs".to_string(),
        );
        vs.insert_chunks(vec![
            EmbeddedChunk::new(chunk_a, vec![0.0, 1.0]),
            EmbeddedChunk::new(chunk_b, vec![1.0, 0.0]),
        ])
        .unwrap();
        vs.build_index().unwrap();

        let mut fts = FtsStore::new_with_writer(db_path).unwrap();
        fts.add_chunk(
            0,
            "<rule>ref:classification search=\"x\" and no watermark</rule>",
            "src/a.xml",
            None,
            "Block",
        )
        .unwrap();
        fts.add_chunk(
            1,
            "fn main() {} unrelated body",
            "src/b.rs",
            None,
            "Function",
        )
        .unwrap();
        fts.commit().unwrap();
    }

    fn opts(fields: &str) -> SearchOptions {
        SearchOptions {
            literal: fields.contains('l'),
            phrase: fields.contains('p'),
            regex: fields.contains('r'),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn literal_tokens_find_xml_content() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        let (hits, scan_fallback) =
            search_fts("ref:classification", tmp.path(), tmp.path(), 2, &opts("l"))
                .await
                .unwrap();
        assert!(!scan_fallback);
        assert_eq!(
            hits.len(),
            1,
            "expected the xml chunk, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert_eq!(hits[0].path, "src/a.xml");
    }

    #[tokio::test]
    async fn phrase_matches_token_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        let (hits, _) = search_fts("no watermark", tmp.path(), tmp.path(), 2, &opts("lp"))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/a.xml");
    }

    #[tokio::test]
    async fn scan_fallback_applies_filter_path_inside_the_loop() {
        // The scan must filter BEFORE consuming a limit slot: with the first
        // matching chunk outside the filter and max_results 1, an unfiltered
        // break would return 0 while a filtered hit exists (round-1 review
        // finding; this test fails under the old break-at-limit-unfiltered
        // order because chunk 0 would consume the only slot).
        let tmp = tempfile::tempdir().unwrap();
        seed_filtered(tmp.path());
        let mut o = opts("lr");
        o.filter_path = Some("src".to_string());
        o.max_results = 1;
        let (hits, scan_fallback) =
            search_fts(r"ref:classification\s+id=", tmp.path(), tmp.path(), 2, &o)
                .await
                .unwrap();
        assert!(scan_fallback, "separator-spanning regex must scan");
        assert_eq!(
            hits.len(),
            1,
            "the filtered hit must survive, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert_eq!(hits[0].path, "src/a.xml");
    }

    /// Two chunks both matching the separator-spanning pattern; the FIRST one
    /// sits outside the `src/` filter, so an unfiltered limit-break would
    /// consume its slot before the filtered chunk is ever reached.
    fn seed_filtered(db_path: &Path) {
        std::fs::create_dir_all(db_path).unwrap();
        let mut vs = VectorStore::new(db_path, 2).unwrap();
        let outside = Chunk::new(
            "<ref:classification id=\"@outside\" />".to_string(),
            0,
            0,
            ChunkKind::Block,
            "other/b.xml".to_string(),
        );
        let inside = Chunk::new(
            "<ref:classification id=\"@inside\" />".to_string(),
            0,
            0,
            ChunkKind::Block,
            "src/a.xml".to_string(),
        );
        vs.insert_chunks(vec![
            crate::embed::EmbeddedChunk::new(outside, vec![0.0, 1.0]),
            crate::embed::EmbeddedChunk::new(inside, vec![1.0, 0.0]),
        ])
        .unwrap();
        vs.build_index().unwrap();
        drop(vs);
        let mut fts = FtsStore::new_with_writer(db_path).unwrap();
        fts.add_chunk(
            0,
            "<ref:classification id=\"@outside\" />",
            "other/b.xml",
            None,
            "Block",
        )
        .unwrap();
        fts.add_chunk(
            1,
            "<ref:classification id=\"@inside\" />",
            "src/a.xml",
            None,
            "Block",
        )
        .unwrap();
        fts.commit().unwrap();
    }

    #[tokio::test]
    async fn regex_spanning_separators_falls_back_to_raw_scan() {
        // The XML rule-config class of query: anchorable tokens exist, but the
        // token-regex can never match a separator-spanning pattern. The 0-hits
        // fallback scans raw chunk content, so the hit is still found.
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        let (hits, scan_fallback) = search_fts(
            r"ref:classification\s+search=",
            tmp.path(),
            tmp.path(),
            2,
            &opts("lr"),
        )
        .await
        .unwrap();
        assert!(scan_fallback, "separator-spanning regex must scan");
        assert_eq!(
            hits.len(),
            1,
            "scan fallback must surface the xml chunk, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert_eq!(hits[0].path, "src/a.xml");
        assert!(
            hits[0].content.contains("ref:classification"),
            "snippet must come from the raw chunk content, got {:?}",
            hits[0].content
        );
    }

    #[tokio::test]
    async fn empty_literal_falls_back_to_relaxed() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        // "unrelated" alone hits chunk 1; AND-ing it with a missing term stays
        // empty, which triggers the relaxed OR retry.
        let (hits, _) = search_fts(
            "unrelated zzzznotpresent",
            tmp.path(),
            tmp.path(),
            2,
            &opts("l"),
        )
        .await
        .unwrap();
        assert!(
            hits.iter().any(|h| h.path == "src/b.rs"),
            "relaxed fallback should surface the OR match, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }
}
