//! Unit tests for the federation merge/parse/convert helpers. The
//! FederationClient HTTP layer + resolve_group_targets are covered
//! separately (federation/mod.rs and db_discovery/repos.rs respectively).
use super::{convert_remote_item, merge_ranked_lists};
use crate::federation::RemoteSearchItem;
use crate::mcp::types::SearchResultItem;

fn local_item(chunk_id: u32, score: f32) -> SearchResultItem {
    SearchResultItem {
        chunk_id,
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
    let top_ids: Vec<u32> = merged.iter().map(|i| i.chunk_id).collect();
    assert_eq!(top_ids, vec![1, 10, 2, 11, 3]);
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
    assert_eq!(item.chunk_id, 42); // local id preserved for rendering
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
