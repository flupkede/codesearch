use super::types::SearchResultItem;
use rmcp::model::CallToolResult;

// ════════════════════════════════════════════════════════════════
// Federation helpers (module-scope) — merge / parse / convert.
// ════════════════════════════════════════════════════════════════

/// RRF-interleave several disjoint ranked lists into one ranked list.
///
/// Each list is assumed already ranked best-first and disjoint from the others
/// (local repos vs. distinct remote peers). An item's merged score is
/// `1/(k + rank_in_own_list + 1)` (classic Reciprocal Rank Fusion with a `+1`
/// so the top hit never exceeds `1/k`). The union is sorted by score desc with a
/// stable source-order tiebreak, then truncated to `limit`.
pub(crate) fn merge_ranked_lists(
    lists: Vec<Vec<SearchResultItem>>,
    k: f32,
    limit: usize,
) -> Vec<SearchResultItem> {
    let mut merged: Vec<(f32, usize, SearchResultItem)> = Vec::new();
    let mut order = 0usize;
    for list in lists {
        for (rank, item) in list.into_iter().enumerate() {
            let score = 1.0 / (k + rank as f32 + 1.0);
            merged.push((score, order, item));
            order += 1;
        }
    }
    // Sort by score desc; tiebreak on insertion order for stable, predictable
    // output (local list first, then remotes in config order).
    merged.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    merged
        .into_iter()
        .take(limit)
        .map(|(score, _, mut it)| {
            it.score = score;
            it
        })
        .collect()
}

/// Extract the rendered tool payload from a `CallToolResult` and re-parse it as
/// local `SearchResultItem`s. Works for both semantic and literal modes — the
/// rendered JSON always has a top-level `results` array.
pub(crate) fn parse_search_items_from_call_result(
    result: &CallToolResult,
    mode: &str,
) -> Vec<SearchResultItem> {
    let text = extract_call_tool_text(result);
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let results = match value.get("results").and_then(|r| r.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    match mode {
        "semantic" => results
            .iter()
            .filter_map(|v| serde_json::from_value::<SearchResultItem>(v.clone()).ok())
            .collect(),
        // Literal items lack `chunk_id`; map their `snippet` into `content` so
        // the merged list renders uniformly. The absent id stays `None` —
        // fabricating `0` here once let callers build a bogus
        // `<peer>/<alias>:0` chunk_ref that `get_chunk` silently resolved to
        // an unrelated chunk.
        _ => results
            .iter()
            .map(|v| SearchResultItem {
                chunk_id: v.get("chunk_id").and_then(|c| c.as_u64()).map(|c| c as u32),
                path: v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string(),
                start_line: v.get("start_line").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
                end_line: v.get("end_line").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
                kind: v
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("")
                    .to_string(),
                score: v.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0) as f32,
                signature: v
                    .get("signature")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string()),
                content: v
                    .get("snippet")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string()),
                context_prev: None,
                context_next: None,
                source: None,
                chunk_ref: None,
            })
            .collect(),
    }
}

/// Convert a remote search hit into a local `SearchResultItem`, tagging it with
/// its origin (`source`) and a project-namespaced `chunk_ref` for later
/// retrieval.
///
/// The `chunk_ref` is `"<peer>/<remote_alias>:<chunk_id>"`. The `remote_alias`
/// segment is essential: the peer is itself multi-repo and chunk_ids are only
/// unique *within* a single index, so `federated_get_chunk` must forward the
/// alias as a `project=` scope to disambiguate. Omitting it (the old
/// `"<peer>:<id>"` shape) made every remote `get_chunk` fail with
/// `ambiguous_chunk_id` whenever the peer hosted more than one project.
///
/// Literal hits carry no chunk id: both `chunk_id` and `chunk_ref` stay
/// `None` (the fields are omitted from the rendered JSON). Never substitute
/// a default here — a fabricated `chunk_id: 0` invites callers to hand-build
/// a `"<peer>/<alias>:0"` ref that resolves to an unrelated chunk.
pub(crate) fn convert_remote_item(
    peer_name: &str,
    remote_alias: &str,
    item: crate::federation::RemoteSearchItem,
) -> SearchResultItem {
    let chunk_ref = item
        .chunk_id
        .map(|id| format!("{peer_name}/{remote_alias}:{id}"));
    SearchResultItem {
        chunk_id: item.chunk_id,
        path: item.path,
        start_line: item.start_line,
        end_line: item.end_line,
        kind: item.kind.unwrap_or_default(),
        score: item.score,
        signature: item.signature,
        content: item.content.or(item.snippet),
        context_prev: item.context_prev,
        context_next: item.context_next,
        source: Some(format!("{peer_name}/{remote_alias}")),
        chunk_ref,
    }
}

/// Apply a `filter_path` prefix filter to federated results **client-side**,
/// on the namespaced paths the caller actually sees.
///
/// Federated `filter_path` cannot be forwarded to the peer: the peer matches
/// against its own un-namespaced store paths (and, in serve mode, against the
/// wrong project root), so a server-side match returns nothing for any value.
/// Here we match against the `<peer>/<alias>/…` path carried on each converted
/// item, with an empty project root (the namespaced path is already relative),
/// so the filter means exactly what the caller reads back in the results.
///
/// A blank/whitespace filter is a no-op. Returns immediately when `filter_path`
/// is `None`, so the non-filtered fast path pays nothing.
pub(crate) fn retain_by_filter_path(items: &mut Vec<SearchResultItem>, filter_path: Option<&str>) {
    let Some(raw) = filter_path else { return };
    if raw.trim().is_empty() {
        return;
    }
    let normalized = crate::cache::normalize_filter_path(raw);
    if normalized.is_empty() {
        return;
    }
    items.retain(|it| crate::cache::path_matches_filter(&it.path, &normalized, ""));
}

/// True when `filter_path` carries a meaningful prefix (non-blank, non-empty
/// after normalization) — the single predicate the federated search paths use
/// to decide whether to over-fetch and post-filter. Mirrors the no-op guards in
/// [`retain_by_filter_path`] so `has_filter` and the retain stay in lockstep.
pub(crate) fn is_meaningful_filter(filter_path: Option<&str>) -> bool {
    filter_path
        .map(|f| !f.trim().is_empty() && !crate::cache::normalize_filter_path(f).is_empty())
        .unwrap_or(false)
}

/// Parse a federated `chunk_ref` into its `(peer, remote_alias, chunk_id)`
/// parts.
///
/// Accepts the current project-namespaced shape `"<peer>/<alias>:<id>"` and,
/// for backward compatibility, the legacy `"<peer>:<id>"` shape (no alias →
/// `None`, which falls back to group-scoped lookup on the peer).
///
/// The `chunk_id` is taken after the *last* `':'` so peer/alias segments that
/// themselves contain a colon are not misparsed; the peer/alias split is on the
/// *first* `'/'`.
pub(crate) fn parse_federated_chunk_ref(chunk_ref: &str) -> Option<(&str, Option<&str>, u32)> {
    let (left, id_str) = chunk_ref.rsplit_once(':')?;
    let chunk_id: u32 = id_str.parse().ok()?;
    match left.split_once('/') {
        Some((peer, alias)) if !peer.is_empty() && !alias.is_empty() => {
            Some((peer, Some(alias), chunk_id))
        }
        _ => Some((left, None, chunk_id)),
    }
}

/// Best-effort extraction of the concatenated text content of a
/// `CallToolResult`. Resilient to rmcp's internal content enum shape.
pub(crate) fn extract_call_tool_text(result: &CallToolResult) -> String {
    serde_json::to_value(result)
        .ok()
        .and_then(|v| {
            v.get("content").and_then(|c| c.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "federation_helpers_tests.rs"]
mod federation_helpers_tests;
