//! Unit tests for the federation merge/parse/convert helpers. The
//! FederationClient HTTP layer + resolve_group_targets are covered
//! separately (federation/mod.rs and db_discovery/repos.rs respectively).
use super::{convert_remote_item, merge_ranked_lists, parse_search_items_from_call_result};
use crate::federation::RemoteSearchItem;
use crate::mcp::types::SearchResultItem;
use rmcp::model::{CallToolResult, Content};

fn local_item(chunk_id: u32, score: f32) -> SearchResultItem {
    SearchResultItem {
        chunk_id: Some(chunk_id),
        path: format!("local/{chunk_id}.rs"),
        start_line: 1,
        end_line: 2,
        kind: "Function".to_string(),
        score,
        signature: None,
        content: None,
        context_prev: None,
        context_next: None,
        source: None,
        chunk_ref: None,
    }
}

#[test]
fn merge_interleaves_disjoint_lists_by_rank() {
    // Two disjoint ranked lists. RRF must interleave by rank, not by raw
    // score (scores aren't comparable across systems).
    let local = vec![
        local_item(1, 0.99),
        local_item(2, 0.50),
        local_item(3, 0.10),
    ];
    let remote = vec![local_item(10, 0.88), local_item(11, 0.60)];
    let merged = merge_ranked_lists(vec![local, remote], 20.0, 10);

    // Top of each list should rank highest; order alternates by rank.
    assert_eq!(merged.len(), 5);
    // Rank-0 of each list: score 1/(20+0+1) = 1/21 ≈ 0.0476 — both rank 0
    // tiebreak on insertion order (local list first).
    let top_ids: Vec<Option<u32>> = merged.iter().map(|i| i.chunk_id).collect();
    assert_eq!(top_ids, vec![Some(1), Some(10), Some(2), Some(11), Some(3)]);
    // Scores must be reassigned to the RRF value.
    assert!((merged[0].score - 1.0 / 21.0).abs() < 1e-6);
}

#[test]
fn merge_respects_limit() {
    let a = vec![local_item(1, 0.9), local_item(2, 0.8), local_item(3, 0.7)];
    let merged = merge_ranked_lists(vec![a], 20.0, 2);
    assert_eq!(merged.len(), 2);
}

#[test]
fn convert_tags_source_and_namespaced_chunk_ref() {
    let remote = RemoteSearchItem {
        chunk_id: Some(42),
        path: "cloud/kb.md".to_string(),
        start_line: 5,
        end_line: 9,
        kind: Some("Section".to_string()),
        score: 0.7,
        signature: None,
        content: Some("body".to_string()),
        snippet: None,
        context_prev: None,
        context_next: None,
    };
    let item = convert_remote_item("cloud", "inriver", remote);
    assert_eq!(item.source.as_deref(), Some("cloud/inriver"));
    assert_eq!(item.chunk_ref.as_deref(), Some("cloud/inriver:42"));
    assert_eq!(item.chunk_id, Some(42)); // local id preserved for rendering
    assert_eq!(item.path, "cloud/kb.md");
}

#[test]
fn convert_falls_back_to_snippet_as_content() {
    // Literal-mode remote hits have `snippet` but no `content`.
    let remote = RemoteSearchItem {
        chunk_id: None,
        path: "x".to_string(),
        start_line: 0,
        end_line: 0,
        kind: None,
        score: 0.1,
        signature: None,
        content: None,
        snippet: Some("matched line".to_string()),
        context_prev: None,
        context_next: None,
    };
    let item = convert_remote_item("peer", "someproj", remote);
    assert_eq!(item.content.as_deref(), Some("matched line"));
    assert!(item.chunk_ref.is_none(), "no chunk_ref without chunk_id");
    // The fabricated-id regression pin: a literal hit must carry NO chunk_id,
    // not a default 0. `0` here would let a caller hand-build a bogus
    // "<peer>/<alias>:0" ref that get_chunk resolves to an unrelated chunk.
    assert_eq!(
        item.chunk_id, None,
        "literal hit must not render a chunk_id"
    );
    // And the omission must be a true JSON omission, not an explicit null
    // (serde_json::json! renders None as null; the struct's
    // skip_serializing_if must drop the key entirely).
    let json = serde_json::to_value(&item).unwrap();
    assert!(
        json.get("chunk_id").is_none(),
        "chunk_id key must be absent, got: {json}"
    );
}

#[test]
fn parse_federated_chunk_ref_namespaced() {
    // Current shape: "<peer>/<alias>:<id>" → alias forwarded as project scope.
    let (peer, alias, id) = super::parse_federated_chunk_ref("cloud/inriver:390").unwrap();
    assert_eq!(peer, "cloud");
    assert_eq!(alias, Some("inriver"));
    assert_eq!(id, 390);
}

#[test]
fn parse_federated_chunk_ref_legacy_no_alias() {
    // Backward compat: bare "<peer>:<id>" → no alias, group-scoped fallback.
    let (peer, alias, id) = super::parse_federated_chunk_ref("cloud:42").unwrap();
    assert_eq!(peer, "cloud");
    assert_eq!(alias, None);
    assert_eq!(id, 42);
}

#[test]
fn parse_federated_chunk_ref_id_after_last_colon() {
    // The id is taken after the LAST ':' so a colon in the alias is safe.
    let (peer, alias, id) = super::parse_federated_chunk_ref("cloud/a:b:7").unwrap();
    assert_eq!(peer, "cloud");
    assert_eq!(alias, Some("a:b"));
    assert_eq!(id, 7);
}

#[test]
fn parse_federated_chunk_ref_rejects_garbage() {
    assert!(super::parse_federated_chunk_ref("no-colon-here").is_none());
    assert!(super::parse_federated_chunk_ref("cloud:notanumber").is_none());
}

// === parse_search_items_from_call_result (serve-delegation re-parse) ===

fn call_result_with_json(json: &str) -> CallToolResult {
    CallToolResult::success(vec![Content::text(json.to_string())])
}

#[test]
fn parse_literal_items_carry_no_chunk_id() {
    // The production literal shape: LiteralSearchResponse items have no
    // chunk_id at all. Re-parsing must keep the id absent — the fabricated
    // `0` this test replaces once let callers build a bogus
    // "<peer>/<alias>:0" chunk_ref that get_chunk resolved to an
    // unrelated chunk.
    let payload = serde_json::json!({
        "results": [
            {
                "path": "custom-kb/troubleshoot/example.md",
                "start_line": 3,
                "end_line": 3,
                "snippet": "tags: classification-importer",
                "score": 0.42,
                "kind": "Section"
            }
        ]
    })
    .to_string();
    let items = parse_search_items_from_call_result(&call_result_with_json(&payload), "literal");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].chunk_id, None, "literal hit must not gain an id");
    // Absent from the re-serialized JSON too — a null would look like a
    // real (if invalid) id to a caller combining fields by hand.
    let json = serde_json::to_value(&items[0]).unwrap();
    assert!(
        json.get("chunk_id").is_none(),
        "chunk_id key must be absent, got: {json}"
    );
    // The snippet must still map into content (uniform merged rendering).
    assert_eq!(
        items[0].content.as_deref(),
        Some("tags: classification-importer")
    );
}

#[test]
fn parse_literal_item_preserves_an_explicit_chunk_id() {
    // Defensive tolerance: if a peer ever includes a chunk_id in its
    // literal payload, it is a real id and must survive the re-parse
    // (Some), not be forced to None.
    let payload = serde_json::json!({
        "results": [
            { "path": "a.md", "start_line": 1, "end_line": 1, "snippet": "hit", "score": 0.1, "chunk_id": 7 }
        ]
    })
    .to_string();
    let items = parse_search_items_from_call_result(&call_result_with_json(&payload), "literal");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].chunk_id, Some(7));
}

#[test]
fn parse_semantic_items_keep_real_ids() {
    let payload = serde_json::json!({
        "results": [
            {
                "chunk_id": 321,
                "path": "custom-kb/howto/example.md",
                "start_line": 1,
                "end_line": 12,
                "kind": "Section",
                "score": 0.9
            }
        ]
    })
    .to_string();
    let items = parse_search_items_from_call_result(&call_result_with_json(&payload), "semantic");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].chunk_id, Some(321));
    assert_eq!(items[0].path, "custom-kb/howto/example.md");
}

#[test]
fn parse_unparseable_payload_yields_empty_list() {
    // Documents the pre-existing degrade for malformed delegation JSON:
    // empty list, not an error. Pinned as-is so a future tightening is a
    // deliberate change, not an accident.
    assert!(parse_search_items_from_call_result(
        &call_result_with_json("not json at all"),
        "literal"
    )
    .is_empty());
    assert!(parse_search_items_from_call_result(
        &call_result_with_json("{\"no_results_key\": true}"),
        "literal"
    )
    .is_empty());
}
