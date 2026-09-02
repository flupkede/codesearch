//! Literal (FTS-only, regex-aware) search internals — a dispatch target of
//! the `search` tool, not a tool of its own (no router). Extracted from
//! `mod.rs` (todo #105).

use super::*;
use rmcp::model::{CallToolResult, Content};

impl CodesearchService {
    pub(crate) async fn literal_search(
        &self,
        Parameters(request): Parameters<LiteralSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "search")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        let limit = request.limit.unwrap_or(20);
        let output_format = request.format.as_deref().unwrap_or("json");

        // Repos that failed during this search. Reported to the caller: an
        // agent that never sees the server log cannot otherwise distinguish a
        // broken store from a repo that holds no match.
        let mut literal_warnings: Vec<String> = Vec::new();

        // Auto-regex promotion: detect code patterns that BM25 would destroy
        let user_set_regex = request.regex.unwrap_or(false);
        let user_set_phrase = request.phrase.unwrap_or(false);
        let auto_promoted =
            !user_set_regex && !user_set_phrase && looks_like_code_pattern(&request.query);

        let (effective_query, effective_regex) = if auto_promoted {
            let escaped = regex::escape(&request.query);
            // Relax whitespace to \s+ so "foo = null" → "foo\s+=\s+null"
            // regex::escape does not escape spaces, so replace literal spaces.
            let relaxed = escaped.replace(' ', r"\s+");
            (relaxed, true)
        } else {
            (request.query.clone(), user_set_regex)
        };

        tracing::debug!(
            "MCP literal_search: query='{}', regex={:?}, phrase={:?}, limit={}, file_glob={:?}, language={:?}, format={}, multi={}",
            request.query, request.regex, request.phrase, limit,
            request.file_glob, request.language, output_format, ctx.is_multi
        );

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // Pre-compute normalized project root for stripping absolute paths in glob matching
        let lang_filter = request.language.clone();
        let glob_filter = request.file_glob.clone();
        let regex_enabled = effective_regex;
        let snippet_regex = if regex_enabled {
            Regex::new(&effective_query).ok()
        } else {
            None
        };
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };

        // Decide: BM25 path (for anchorable queries) or scan path (for tokenless regex
        // or disjunctive OR patterns like TODO|FIXME|HACK that BM25 treats as AND).
        let tokenless_regex = regex_enabled
            && snippet_regex.is_some()
            && (!regex_has_anchorable_token(&effective_query)
                || regex_has_disjunctive_or(&effective_query));

        let mut items: Vec<LiteralSearchResultItem> = if tokenless_regex {
            // ── Scan path ──────────────────────────────────────────────
            // Tokenless regex (e.g. \bfn\s+\w+) — BM25 cannot produce useful
            // candidates. Scan all chunks sequentially, apply regex post-filter.
            // Score is 0.0 for all results (no BM25 ranking applies).
            tracing::debug!("literal_search: tokenless regex detected, using scan path");
            if let Some(ref sv) = ctx.stores_vec {
                // Multi-store scan
                let mut items: Vec<LiteralSearchResultItem> = Vec::new();
                for store_arc in sv {
                    let store = store_arc.vector_store.read().await;
                    let all_chunks = match store.iter_all_chunks() {
                        Ok(chunks) => chunks,
                        Err(_) => continue,
                    };
                    for (_, chunk) in all_chunks {
                        if let Some(ref lang) = lang_filter {
                            let file_lang = Language::from_path(std::path::Path::new(&chunk.path));
                            if file_lang.name() != lang {
                                continue;
                            }
                        }
                        if let Some(ref glob) = glob_filter {
                            let relative_path = chunk
                                .path
                                .strip_prefix(&project_root_normalized)
                                .unwrap_or(&chunk.path)
                                .trim_start_matches('/');
                            if !simple_glob_match(glob, relative_path) {
                                continue;
                            }
                        }
                        if let Some((match_offset, snippet)) = match_line_for_literal(
                            &chunk.content,
                            &effective_query,
                            snippet_regex.as_ref(),
                        ) {
                            let match_line = chunk.start_line + match_offset;
                            items.push(LiteralSearchResultItem {
                                path: chunk.path,
                                start_line: match_line,
                                end_line: match_line,
                                snippet,
                                score: 0.0, // No BM25 score — scan-path results are unranked
                                kind: if chunk.kind.is_empty() {
                                    None
                                } else {
                                    Some(chunk.kind)
                                },
                                signature: chunk.signature.filter(|s| !s.is_empty()),
                            });
                            if items.len() >= limit {
                                break;
                            }
                        }
                    }
                    if items.len() >= limit {
                        break;
                    }
                }
                items
            } else {
                // Single-store scan
                match self
                    .with_vector_store_read_for(
                        |store| {
                            let all_chunks = store.iter_all_chunks()?;
                            let mut items: Vec<LiteralSearchResultItem> = Vec::new();
                            for (_, chunk) in all_chunks {
                                if let Some(ref lang) = lang_filter {
                                    let file_lang =
                                        Language::from_path(std::path::Path::new(&chunk.path));
                                    if file_lang.name() != lang {
                                        continue;
                                    }
                                }
                                if let Some(ref glob) = glob_filter {
                                    let relative_path = chunk
                                        .path
                                        .strip_prefix(&project_root_normalized)
                                        .unwrap_or(&chunk.path)
                                        .trim_start_matches('/');
                                    if !simple_glob_match(glob, relative_path) {
                                        continue;
                                    }
                                }
                                if let Some((match_offset, snippet)) = match_line_for_literal(
                                    &chunk.content,
                                    &effective_query,
                                    snippet_regex.as_ref(),
                                ) {
                                    let match_line = chunk.start_line + match_offset;
                                    items.push(LiteralSearchResultItem {
                                        path: chunk.path,
                                        start_line: match_line,
                                        end_line: match_line,
                                        snippet,
                                        score: 0.0, // No BM25 score — scan-path results are unranked
                                        kind: if chunk.kind.is_empty() {
                                            None
                                        } else {
                                            Some(chunk.kind)
                                        },
                                        signature: chunk.signature.filter(|s| !s.is_empty()),
                                    });
                                    if items.len() >= limit {
                                        break;
                                    }
                                }
                            }
                            Ok(items)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(items) => items,
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error scanning chunks: {e:#}"
                        ))]));
                    }
                }
            }
        } else {
            // ── BM25 path ──────────────────────────────────────────────
            // Note: regex=true uses BM25 for candidates, then post-filters with the
            // actual regex on raw content (Tantivy's RegexQuery only works on individual
            // tokens, not raw text — underscores/punctuation cause empty results).
            //
            // When regex is enabled, strip metacharacters from the BM25 query so
            // Tantivy gets clean tokens (e.g. "class Cache" instead of "class \w+Cache\b").
            let bm25_query = if regex_enabled {
                let cleaned = extract_bm25_query_from_regex(&effective_query);
                if cleaned.is_empty() {
                    effective_query.clone()
                } else {
                    cleaned
                }
            } else {
                effective_query.clone()
            };
            let fts_results = if let Some(ref sv) = ctx.stores_vec {
                let sa = ctx.store_aliases.as_ref().unwrap();
                let outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| {
                            if request.phrase.unwrap_or(false) {
                                fts_store.search_phrase(&bm25_query, limit * 3)
                            } else {
                                fts_store.search(&bm25_query, limit * 3, None)
                            }
                        },
                        sv.clone(),
                        sa,
                    )
                    .await
                    .unwrap_or_default();
                for (alias, err) in &outcome.failures {
                    let msg = format!("repo '{alias}' literal search failed: {err}");
                    tracing::error!("MCP: {}", msg);
                    literal_warnings.push(msg);
                }
                outcome.results
            } else {
                match self
                    .with_fts_store_read_for(
                        |fts_store| {
                            if request.phrase.unwrap_or(false) {
                                fts_store.search_phrase(&bm25_query, limit * 3)
                            } else {
                                fts_store.search(&bm25_query, limit * 3, None)
                            }
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error searching: {e:#}"
                        ))]));
                    }
                }
            };

            // Resolve chunk metadata and apply post-filters
            if let Some(ref sv) = ctx.stores_vec {
                // Multi-store: resolve chunks from all stores
                let mut items: Vec<LiteralSearchResultItem> = Vec::new();
                'outer: for fts_result in &fts_results {
                    let sa = ctx.store_aliases.as_ref().unwrap();
                    for (idx, store_arc) in sv.iter().enumerate() {
                        let store = store_arc.vector_store.read().await;
                        let looked_up = store.get_chunk(fts_result.chunk_id);
                        if let Err(ref e) = looked_up {
                            note_store_failure(&mut literal_warnings, sa, idx, "chunk lookup", e);
                        }
                        if let Some(chunk) = looked_up.ok().flatten() {
                            if let Some(ref lang) = lang_filter {
                                let file_lang =
                                    Language::from_path(std::path::Path::new(&chunk.path));
                                if file_lang.name() != lang {
                                    continue;
                                }
                            }
                            if let Some(ref glob) = glob_filter {
                                let relative_path = chunk
                                    .path
                                    .strip_prefix(&project_root_normalized)
                                    .unwrap_or(&chunk.path)
                                    .trim_start_matches('/');
                                if !simple_glob_match(glob, relative_path) {
                                    continue;
                                }
                            }
                            let match_info = match_line_for_literal(
                                &chunk.content,
                                &effective_query,
                                snippet_regex.as_ref(),
                            );
                            if regex_enabled && match_info.is_none() {
                                continue;
                            }
                            let (match_offset, snippet) = match_info.unwrap_or_else(|| {
                                (0, chunk.content.lines().next().unwrap_or("").to_string())
                            });
                            let match_line = chunk.start_line + match_offset;
                            items.push(LiteralSearchResultItem {
                                path: chunk.path,
                                start_line: match_line,
                                end_line: match_line,
                                snippet,
                                score: fts_result.score,
                                kind: if chunk.kind.is_empty() {
                                    None
                                } else {
                                    Some(chunk.kind)
                                },
                                signature: chunk.signature.filter(|s| !s.is_empty()),
                            });
                            if items.len() >= limit {
                                break 'outer;
                            }
                            break; // Found in this store
                        }
                    }
                }
                items
            } else {
                match self
                    .with_vector_store_read_for(
                        |store| {
                            // Resolve chunk metadata first so a store `Err`
                            // propagates to the error arm below ("Error
                            // resolving search results") instead of silently
                            // dropping the hit — `Ok(None)` alone is a true
                            // miss ("chunk not in this store").
                            let resolved: anyhow::Result<Vec<_>> = fts_results
                                .iter()
                                .map(|fts_result| {
                                    let chunk = store.get_chunk(fts_result.chunk_id)?;
                                    Ok((chunk, fts_result.score))
                                })
                                .collect();
                            let items: Vec<LiteralSearchResultItem> = resolved?
                                .into_iter()
                                .filter_map(|(looked_up, score)| {
                                    let chunk = looked_up?;
                                    Some((chunk, score))
                                })
                                .filter(|(chunk, _)| {
                                    if let Some(ref lang) = lang_filter {
                                        let file_lang =
                                            Language::from_path(std::path::Path::new(&chunk.path));
                                        if file_lang.name() != lang {
                                            return false;
                                        }
                                    }
                                    if let Some(ref glob) = glob_filter {
                                        let relative_path = chunk
                                            .path
                                            .strip_prefix(&project_root_normalized)
                                            .unwrap_or(&chunk.path)
                                            .trim_start_matches('/');
                                        if !simple_glob_match(glob, relative_path) {
                                            return false;
                                        }
                                    }
                                    true
                                })
                                .take(limit)
                                .filter_map(|(chunk, score)| {
                                    let match_info = match_line_for_literal(
                                        &chunk.content,
                                        &effective_query,
                                        snippet_regex.as_ref(),
                                    );
                                    if regex_enabled && match_info.is_none() {
                                        return None;
                                    }
                                    let (match_offset, snippet) = match_info.unwrap_or_else(|| {
                                        (0, chunk.content.lines().next().unwrap_or("").to_string())
                                    });
                                    let match_line = chunk.start_line + match_offset;
                                    Some(LiteralSearchResultItem {
                                        path: chunk.path,
                                        start_line: match_line,
                                        end_line: match_line,
                                        snippet,
                                        score,
                                        kind: if chunk.kind.is_empty() {
                                            None
                                        } else {
                                            Some(chunk.kind)
                                        },
                                        signature: chunk.signature.filter(|s| !s.is_empty()),
                                    })
                                })
                                .collect();
                            Ok(items)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(items) => items,
                    Err(e) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Error resolving search results: {e:#}"
                        ))]));
                    }
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Compute low-confidence signal
        let top_score = items.first().map(|i| i.score);
        let (low_confidence, suggested_tool) =
            compute_literal_low_confidence(top_score, &request.query);

        // Build note
        let note = if auto_promoted {
            Some(format!(
                "Query auto-promoted to regex mode (original: '{}', effective: '{}'). \
                 The query contained code-like punctuation that BM25 would tokenize incorrectly.",
                request.query, effective_query
            ))
        } else if low_confidence == Some(true) {
            suggested_tool.as_ref().map(|tool| {
                format!(
                    "Top result has weak BM25 score; consider using `{}` for better matches.",
                    tool
                )
            })
        } else {
            None
        };

        let response = LiteralSearchResponse {
            results: items,
            auto_promoted_to_regex: if auto_promoted { Some(true) } else { None },
            note,
            low_confidence,
            suggested_tool: if low_confidence == Some(true) {
                suggested_tool
            } else {
                None
            },
            warnings: if literal_warnings.is_empty() {
                None
            } else {
                Some(literal_warnings)
            },
        };

        // Instrument BM25 score for threshold calibration
        if let Some(top) = response.results.first() {
            tracing::debug!(
                target: "codesearch::literal_confidence",
                query = %request.query,
                top_bm25_score = top.score,
                result_count = response.results.len(),
                "literal_search score sample"
            );
        }

        // Format output
        let output = if output_format == "grep" {
            let mut lines: Vec<String> = Vec::new();
            if response.auto_promoted_to_regex == Some(true) {
                lines.push(
                    "# auto-promoted to regex mode (query contained code-like punctuation)"
                        .to_string(),
                );
            }
            if response.low_confidence == Some(true) {
                if let Some(ref hint) = response.suggested_tool {
                    lines.push(format!("# low confidence — consider: {}", hint));
                }
            }
            for item in &response.results {
                lines.push(format!(
                    "{}:{}:{}",
                    item.path, item.start_line, item.snippet
                ));
            }
            lines.join("\n")
        } else {
            serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string())
        };

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}
