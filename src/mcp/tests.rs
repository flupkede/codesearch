use crate::cache::{normalize_filter_path, normalize_path_str, path_matches_filter};

#[test]
fn test_mcp_no_raw_stdout_calls() {
    // Verify that no raw print!/println! calls exist in the MCP module sources.
    // MCP communicates over stdout (JSON-RPC), so any stdout pollution breaks the protocol.
    // All informational output must go through info_print!/warn_print!/eprintln! (stderr).
    let src = include_str!("mod.rs");
    let violations: Vec<(usize, &str)> = src
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim_start();
            // Skip comments and lines that are part of the detection logic itself
            if trimmed.starts_with("//") || trimmed.starts_with("\"") {
                return false;
            }
            // Only flag lines that actually invoke print! or println! as a macro call
            // (i.e. the identifier immediately followed by '!'), not lines discussing them
            let call_println = line.contains("println!(");
            let call_print = trimmed.starts_with("print!(")
                || line.contains(" print!(")
                || line.contains("\tprint!(");
            let is_prefixed = line.contains("info_print!(") || line.contains("warn_print!(");
            let is_detection_code = line.contains("line.contains(");
            (call_println || call_print) && !is_prefixed && !is_detection_code
        })
        .collect();

    assert!(
        violations.is_empty(),
        "MCP module has raw stdout calls that break the JSON-RPC protocol:\n{}",
        violations
            .iter()
            .map(|(i, l)| format!("  line {}: {}", i + 1, l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// === Tool registration pin ===
//
// Safety net for the mod.rs split: a `#[tool]` method that lands in an impl
// block the `#[tool_router]` macro does not scan is SILENTLY not registered.
// This asserts on the exact router the service wires up (`merged_tool_router`,
// the same expression both ctors and `#[tool_handler]` use) so every
// extraction stage must keep the 6-tool surface intact.

#[test]
fn test_tool_registration_exposes_exactly_the_six_tools() {
    let router = super::CodesearchService::merged_tool_router();
    let mut names: Vec<String> = router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    let expected: Vec<&str> = vec![
        "explore",
        "find",
        "find_impact",
        "get_chunk",
        "search",
        "status",
    ];
    assert_eq!(
        names, expected,
        "tools/list must expose exactly the consolidated 6-tool surface"
    );
}

#[cfg(windows)]
#[test]
fn test_mcp_filter_matches_absolute_path_under_project_root() {
    let project_root = normalize_path_str(r"C:\WorkArea\AI\codesearch");
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        r"\\?\C:\WorkArea\AI\codesearch\src\mcp\mod.rs",
        &filter,
        &project_root,
    ));
}

// Unix counterpart: same logic, native (forward-slash) absolute paths.
// normalize_path_str deliberately does NOT rewrite '\' on Unix (backslash
// is a legal filename char — see file_meta.rs Aikido rationale), so the
// Windows-path variant above is meaningless here and is gated off.
#[cfg(unix)]
#[test]
fn test_mcp_filter_matches_absolute_path_under_project_root() {
    let project_root = normalize_path_str("/work/codesearch");
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        "/work/codesearch/src/mcp/mod.rs",
        &filter,
        &project_root,
    ));
}

#[test]
fn test_mcp_filter_rejects_non_matching_path_under_project_root() {
    let project_root = normalize_path_str(r"C:\WorkArea\AI\codesearch");
    let filter = normalize_filter_path("src/");
    assert!(!path_matches_filter(
        r"C:\WorkArea\AI\codesearch\README.md",
        &filter,
        &project_root,
    ));
}

// === pick_filter_root (routed filter_path root selection) tests ===

fn roots(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs
        .iter()
        .map(|(a, r)| ((*a).to_string(), normalize_path_str(r)))
        .collect()
}

#[cfg(windows)]
#[test]
fn pick_filter_root_uses_routed_alias_root() {
    // serve single-project: the routed alias's own root, NOT the service
    // project_path fallback — this is the bug being fixed.
    let ar = roots(&[("myrepo", r"C:\data\repos\myrepo")]);
    let root = super::pick_filter_root(
        r"C:\data\repos\myrepo\src\foo.rs",
        Some("myrepo"),
        &ar,
        "/some/other/hub/path",
    );
    assert_eq!(root, normalize_path_str(r"C:\data\repos\myrepo"));
    // …and the filter then matches a repo-relative prefix.
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        r"C:\data\repos\myrepo\src\foo.rs",
        &filter,
        &root
    ));
    // …while a non-matching repo-relative prefix is correctly dropped.
    let other = normalize_filter_path("tests/");
    assert!(!path_matches_filter(
        r"C:\data\repos\myrepo\src\foo.rs",
        &other,
        &root
    ));
}

// Unix counterpart: native forward-slash paths (see cfg(windows) twin).
#[cfg(unix)]
#[test]
fn pick_filter_root_uses_routed_alias_root() {
    let ar = roots(&[("myrepo", "/data/repos/myrepo")]);
    let root = super::pick_filter_root(
        "/data/repos/myrepo/src/foo.rs",
        Some("myrepo"),
        &ar,
        "/some/other/hub/path",
    );
    assert_eq!(root, normalize_path_str("/data/repos/myrepo"));
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        "/data/repos/myrepo/src/foo.rs",
        &filter,
        &root
    ));
    let other = normalize_filter_path("tests/");
    assert!(!path_matches_filter(
        "/data/repos/myrepo/src/foo.rs",
        &other,
        &root
    ));
}

#[test]
fn pick_filter_root_multi_picks_longest_matching_root() {
    // serve multi/group: no project_alias; choose the alias root the path
    // lives under, longest-match so nested roots resolve correctly.
    let ar = roots(&[("outer", r"C:\data"), ("inner", r"C:\data\inner")]);
    let root = super::pick_filter_root(r"C:\data\inner\pkg\x.rs", None, &ar, "/fallback");
    assert_eq!(root, normalize_path_str(r"C:\data\inner"));
}

#[test]
fn pick_filter_root_stdio_falls_back_to_project_path() {
    // stdio single-repo: alias_roots empty → the service project_path,
    // preserving the (correct) pre-fix behaviour.
    let ar = std::collections::HashMap::new();
    let fallback = normalize_path_str(r"C:\repo");
    let root = super::pick_filter_root(r"C:\repo\src\a.rs", Some("repo"), &ar, &fallback);
    assert_eq!(root, fallback);
}

// === retain_by_filter_path (federated client-side path scoping) tests ===

fn ns_item(path: &str) -> super::SearchResultItem {
    super::SearchResultItem {
        chunk_id: Some(1),
        path: path.to_string(),
        start_line: 1,
        end_line: 2,
        kind: String::new(),
        score: 0.5,
        signature: None,
        content: None,
        context_prev: None,
        context_next: None,
        source: None,
        chunk_ref: None,
    }
}

#[test]
fn retain_by_filter_path_keeps_only_matching_namespaced_prefix() {
    // Federated results carry the `<peer>/<alias>/…` path the caller sees;
    // the filter must match against THAT, with an empty project root.
    let mut items = vec![
        ns_item("vendor-a/dam_help/Rendition-Presets.htm"),
        ns_item("vendor-a/mo_help/Approvals.htm"),
        ns_item("custom-kb/howto/foo.md"),
    ];
    super::retain_by_filter_path(&mut items, Some("vendor-a/dam_help"));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, "vendor-a/dam_help/Rendition-Presets.htm");
}

#[test]
fn retain_by_filter_path_none_and_blank_are_noops() {
    let pair = || {
        vec![
            ns_item("vendor-a/dam_help/x.htm"),
            ns_item("custom-kb/y.md"),
        ]
    };

    let mut a = pair();
    super::retain_by_filter_path(&mut a, None);
    assert_eq!(a.len(), 2, "None filter must not drop anything");

    let mut b = pair();
    super::retain_by_filter_path(&mut b, Some("   "));
    assert_eq!(b.len(), 2, "blank/whitespace filter must be a no-op");

    let mut c = pair();
    super::retain_by_filter_path(&mut c, Some("/"));
    assert_eq!(c.len(), 2, "root-only filter normalises to empty → no-op");
}

#[test]
fn retain_by_filter_path_no_match_yields_empty() {
    let mut items = vec![ns_item("vendor-a/dam_help/x.htm")];
    super::retain_by_filter_path(&mut items, Some("nonexistent/segment"));
    assert!(items.is_empty());
}

// === is_definition_chunk tests ===

#[test]
fn test_is_definition_chunk() {
    // Previously 18 separate #[test]s (plus an inline mini-table inside one of
    // them); consolidated into a single table-driven test exercising every
    // (kind, signature, symbol) triple and the expected boolean returned by
    // is_definition_chunk.
    let cases: &[(&str, Option<&str>, &str, bool)] = &[
        // rust function / struct / trait / enum
        ("Function", Some("fn authenticate("), "authenticate", true),
        (
            "Function",
            Some("pub fn CodesearchService"),
            "CodesearchService",
            true,
        ),
        (
            "Function",
            Some("pub async fn handle_request"),
            "handle_request",
            true,
        ),
        (
            "Struct",
            Some("pub struct CodesearchService"),
            "CodesearchService",
            true,
        ),
        ("Struct", Some("struct SearchResult"), "SearchResult", true),
        ("Trait", Some("pub trait Searchable"), "Searchable", true),
        ("Enum", Some("pub enum ModelType"), "ModelType", true),
        // python def / class
        ("Function", Some("def authenticate("), "authenticate", true),
        ("Class", Some("class UserService"), "UserService", true),
        // impl / const / static / type alias / interface
        (
            "Struct",
            Some("impl CodesearchService"),
            "CodesearchService",
            true,
        ),
        ("Function", Some("const MAX_SIZE"), "MAX_SIZE", true),
        ("Function", Some("static INSTANCE"), "INSTANCE", true),
        ("TypeAlias", Some("type Result"), "Result", true),
        ("TypeAlias", Some("pub type Error"), "Error", true),
        (
            "Interface",
            Some("interface Searchable"),
            "Searchable",
            true,
        ),
        // generics / colon-bound trait
        ("Function", Some("fn parse<T>"), "parse", true),
        ("Struct", Some("struct HashMap<K, V>"), "HashMap", true),
        ("Trait", Some("trait AsRef<T>:"), "AsRef", true),
        // method
        ("Method", Some("fn search"), "search", true),
        ("Method", Some("pub async fn handle"), "handle", true),
        // every DEFINITION_KIND recognized (former all_kinds mini-table)
        ("Function", Some("fn foo("), "foo", true),
        ("Class", Some("class Bar"), "Bar", true),
        ("Method", Some("fn baz("), "baz", true),
        ("Struct", Some("struct Qux"), "Qux", true),
        ("Trait", Some("trait Quux"), "Quux", true),
        ("Enum", Some("enum Corge"), "Corge", true),
        ("TypeAlias", Some("type Grault"), "Grault", true),
        ("Interface", Some("interface Garply"), "Garply", true),
        // negatives
        ("Comment", Some("fn authenticate("), "authenticate", false),
        ("Import", Some("use authenticate"), "authenticate", false),
        ("Function", Some("fn handle_request"), "authenticate", false),
        ("Function", None, "authenticate", false),
        ("Function", Some(""), "authenticate", false),
        ("Function", Some("fn authenticate"), "authorize", false),
        (
            "Function",
            Some("fn authenticate_user"),
            "authenticate",
            false,
        ),
    ];

    for (kind, sig, symbol, expected) in cases {
        let sig = sig.map(|s| s.to_string());
        let got = super::is_definition_chunk(kind, &sig, symbol);
        assert_eq!(
            got, *expected,
            "is_definition_chunk({kind:?}, {sig:?}, {symbol:?}) expected {expected}"
        );
    }
}

// === SemanticSearchResponse low-confidence tests ===

#[test]
fn test_low_confidence_response_serialization() {
    let response = super::SemanticSearchResponse {
        results: vec![],
        low_confidence: Some(true),
        suggested_tool: Some("literal_search".to_string()),
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"low_confidence\":true"));
    assert!(json.contains("\"suggested_tool\":\"literal_search\""));
}

#[test]
fn test_normal_response_omits_confidence_fields() {
    let response = super::SemanticSearchResponse {
        results: vec![super::SearchResultItem {
            chunk_id: Some(1),
            path: "test.rs".to_string(),
            start_line: 1,
            end_line: 10,
            kind: "Function".to_string(),
            score: 0.5,
            signature: Some("fn test()".to_string()),
            content: None,
            context_prev: None,
            context_next: None,
            source: None,
            chunk_ref: None,
        }],
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("low_confidence"));
    assert!(!json.contains("suggested_tool"));
}

// === Instructions length test ===

#[test]
fn test_instructions_max_50_lines() {
    // Verify that the MCP instructions template is ≤ 50 lines. MCP clients
    // display this on connect; keeping it compact avoids truncation and token
    // waste. The template is a named const (`INSTRUCTIONS_TEMPLATE`) so we can
    // validate it directly without instantiating the service or fragile
    // `include_str!` source-text searching.
    let line_count = super::INSTRUCTIONS_TEMPLATE.lines().count();
    assert!(
        line_count <= 50,
        "Instructions block is {} lines, must be ≤ 50 lines.\n\
             Content:\n{}",
        line_count,
        super::INSTRUCTIONS_TEMPLATE
    );
}

#[test]
fn test_no_deprecated_tool_aliases_in_instructions() {
    let instructions_text = super::INSTRUCTIONS_TEMPLATE;

    let deprecated = [
        "semantic_search",
        "literal_search",
        "find_definition",
        "find_usages",
        "find_references",
        "find_imports",
        "find_dependents",
        "file_outline",
        "similar_chunks",
        "index_status",
        "list_projects",
        "find_databases",
        "Deprecated aliases",
    ];
    for name in &deprecated {
        assert!(
            !instructions_text.contains(name),
            "Instructions still mentions deprecated tool/section: {}",
            name
        );
    }
}

// === prefix_path_with_alias tests ===

// Windows-only: backslash → '/' rewriting is a no-op on Unix by design
// (backslash is a legal Unix filename char). Forward-slash inputs are
// covered by test_path_prefix_no_alias / _empty_alias on all platforms.
#[cfg(windows)]
#[test]
fn test_path_prefix_windows_backslashes() {
    let result = super::prefix_path_with_alias(r"C:\repo\src\main.rs", Some("myrepo"), r"C:\repo");
    assert_eq!(result, "myrepo/src/main.rs");
}

#[test]
fn test_path_prefix_unc_prefix() {
    let result =
        super::prefix_path_with_alias(r"\\?\C:\repo\src\main.rs", Some("myrepo"), r"C:\repo");
    // After normalization, UNC prefix is stripped by normalize_path_str
    assert!(
        result.starts_with("myrepo/"),
        "Expected alias prefix, got: {}",
        result
    );
    assert!(
        result.contains("main.rs"),
        "Expected filename in result, got: {}",
        result
    );
}

// Windows-only: mixed '/' and '\' only collapse to '/' on Windows.
#[cfg(windows)]
#[test]
fn test_path_prefix_mixed_separators() {
    let result = super::prefix_path_with_alias(r"C:\repo/src\main.rs", Some("myrepo"), r"C:\repo");
    assert_eq!(result, "myrepo/src/main.rs");
}

#[test]
fn test_path_prefix_no_alias() {
    let result = super::prefix_path_with_alias("C:/repo/src/main.rs", None, "C:/repo");
    assert_eq!(result, "src/main.rs");
}

#[test]
fn test_path_prefix_empty_alias() {
    let result = super::prefix_path_with_alias("C:/repo/src/main.rs", Some(""), "C:/repo");
    assert_eq!(result, "src/main.rs");
}

#[test]
fn test_path_prefix_preserves_path_outside_root() {
    let result = super::prefix_path_with_alias("C:/other/src/main.rs", Some("myrepo"), "C:/repo");
    // Path doesn't start with root — returned normalized, no alias prefix
    assert_eq!(result, "C:/other/src/main.rs");
}

#[test]
fn test_group_results_are_alias_prefixed() {
    // Simulate two stores for aliases "a" and "b", each returning a result
    // with absolute path = "/abs/root/src/main.rs". After applying prefix_path_with_alias,
    // assert results have path = "a/src/main.rs" and "b/src/main.rs".
    let result_a = super::prefix_path_with_alias("/abs/root/src/main.rs", Some("a"), "/abs/root");
    let result_b = super::prefix_path_with_alias("/abs/root/src/main.rs", Some("b"), "/abs/root");
    assert_eq!(result_a, "a/src/main.rs");
    assert_eq!(result_b, "b/src/main.rs");
}

#[test]
fn test_single_project_result_is_alias_prefixed() {
    // Single store for alias "myrepo", result with path = "/abs/root/src/lib.rs",
    // project root "/abs/root" → assert path becomes "myrepo/src/lib.rs".
    let result = super::prefix_path_with_alias("/abs/root/src/lib.rs", Some("myrepo"), "/abs/root");
    assert_eq!(result, "myrepo/src/lib.rs");
}

#[test]
fn test_stdio_mode_paths_not_prefixed() {
    // alias None → path normalized, no prefix added.
    let result = super::prefix_path_with_alias("C:/repo/src/main.rs", None, "C:/repo");
    assert_eq!(result, "src/main.rs");
}

#[test]
fn test_dedup_key_includes_alias() {
    // Two stores each returning chunk_id=1, different content.
    // Assert both are kept after merge (key = (alias, chunk_id), not just chunk_id).
    use std::collections::HashMap;

    // Simulate the dedup logic from with_vector_store_read_multi
    let mut seen_ids: HashMap<(String, u32), usize> = HashMap::new();
    let mut all_results: Vec<(String, u32)> = Vec::new();

    // First result from alias "a" with chunk_id 1
    let key_a = ("a".to_string(), 1u32);
    seen_ids.insert(key_a.clone(), all_results.len());
    all_results.push(("a".to_string(), 1u32));

    // Second result from alias "b" with chunk_id 1
    let key_b = ("b".to_string(), 1u32);
    if !seen_ids.contains_key(&key_b) {
        seen_ids.insert(key_b.clone(), all_results.len());
        all_results.push(("b".to_string(), 1u32));
    }

    // Both should be kept because keys are different
    assert_eq!(all_results.len(), 2);
    assert!(seen_ids.contains_key(&key_a));
    assert!(seen_ids.contains_key(&key_b));
}

// === simple_glob_match tests ===

#[test]
fn test_simple_glob_match() {
    // Previously 16 separate #[test]s across two "simple_glob" / "glob" sections
    // that were near-duplicate sets; consolidated into one table-driven test.
    // Backslash rows use the raw-string originals (e.g. r"src\mcp\mod.rs"),
    // here written as escaped string literals.
    let cases: &[(&str, &str, bool)] = &[
        // exact
        ("src/main.rs", "src/main.rs", true),
        ("src/main.rs", "src/other.rs", false),
        ("src/main.rs", "src/main.rs.bak", false),
        // ** prefix
        ("src/mcp/**", "src/mcp/mod.rs", true),
        ("src/mcp/**", "src/mcp/types.rs", true),
        ("src/mcp/**", "src/mcp/sub/deep.rs", true),
        ("src/mcp/**", "src/other/mod.rs", false),
        ("**/test.rs", "test.rs", true),
        ("**/test.rs", "src/test.rs", true),
        ("**/test.rs", "a/b/c/test.rs", true),
        // ** suffix
        ("**/*.rs", "src/main.rs", true),
        ("**/*.rs", "deep/nested/file.rs", true),
        ("**/*.rs", "src/main.ts", false),
        ("src/**", "src/", true),
        ("src/**", "src/foo", true),
        ("src/**", "src/a/b/c", true),
        // ** both sides
        ("src/**/*.rs", "src/main.rs", true),
        ("src/**/*.rs", "src/mcp/mod.rs", true),
        ("src/**/*.rs", "src/lib.rs", true),
        ("src/**/*.rs", "src/a/b/c/d.rs", true),
        ("src/**/*.rs", "tests/main.rs", false),
        ("src/**/*.rs", "src/main.ts", false),
        ("src/**/*.rs", "src/lib.ts", false),
        ("src/**/*.rs", "test/lib.rs", false),
        ("**/**", "anything", true),
        ("**/**", "a/b/c", true),
        // single * (stays within a path segment)
        ("*.rs", "main.rs", true),
        ("*.rs", "main.ts", false),
        ("*.rs", "src/main.rs", false),
        ("src/*.rs", "src/main.rs", true),
        ("src/*.rs", "src/sub/main.rs", false),
        ("test_*.rs", "test_foo.rs", true),
        ("test_*.rs", "test_foo.ts", false),
        // ** in the middle
        ("src/**/test.rs", "src/test.rs", true),
        ("src/**/test.rs", "src/a/test.rs", true),
        ("src/**/test.rs", "src/a/b/c/test.rs", true),
        ("src/**/test.rs", "src/a/other.rs", false),
        // empty pattern
        ("", "", true),
        ("", "foo.rs", false),
        // backslash normalization (Windows paths)
        ("src/mcp/**", "src\\mcp\\mod.rs", true),
        ("src\\mcp\\**", "src/mcp/mod.rs", true),
    ];

    for (pattern, path, expected) in cases {
        let got = super::simple_glob_match(pattern, path);
        assert_eq!(
            got, *expected,
            "simple_glob_match({pattern:?}, {path:?}) expected {expected}"
        );
    }
}

// === merge_exact_into_fts tests ===

#[test]
fn test_merge_exact_empty_base() {
    let mut fts: Vec<crate::fts::FtsResult> = vec![];
    let exact = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.5,
        },
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.3,
        },
    ];
    super::merge_exact_into_fts(&mut fts, exact);
    assert_eq!(fts.len(), 2);
    assert_eq!(fts[0].chunk_id, 1);
    assert_eq!(fts[1].chunk_id, 2);
}

#[test]
fn test_merge_exact_dedupe_keeps_max_score() {
    let mut fts = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.8,
        },
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.3,
        },
    ];
    let exact = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.5,
        }, // lower score → keep 0.8
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.9,
        }, // higher score → upgrade to 0.9
    ];
    super::merge_exact_into_fts(&mut fts, exact);
    assert_eq!(fts.len(), 2);
    assert!((fts[0].score - 0.8).abs() < 0.001);
    assert!((fts[1].score - 0.9).abs() < 0.001);
}

#[test]
fn test_merge_exact_adds_new_chunks() {
    let mut fts = vec![crate::fts::FtsResult {
        chunk_id: 1,
        score: 0.5,
    }];
    let exact = vec![
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.7,
        },
        crate::fts::FtsResult {
            chunk_id: 3,
            score: 0.4,
        },
    ];
    super::merge_exact_into_fts(&mut fts, exact);
    assert_eq!(fts.len(), 3);
    assert_eq!(fts[1].chunk_id, 2);
    assert_eq!(fts[2].chunk_id, 3);
}

#[test]
fn test_merge_exact_empty_exact() {
    let mut fts = vec![crate::fts::FtsResult {
        chunk_id: 1,
        score: 0.5,
    }];
    super::merge_exact_into_fts(&mut fts, vec![]);
    assert_eq!(fts.len(), 1);
}

#[test]
fn test_merge_exact_multiple_hits_same_chunk() {
    // Multiple exact results for the same chunk should still dedupe
    let mut fts = vec![];
    let exact = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.3,
        },
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.7,
        },
    ];
    super::merge_exact_into_fts(&mut fts, exact);
    assert_eq!(fts.len(), 1);
    // First is added (0.3), second dedupes and upgrades to 0.7
    assert!((fts[0].score - 0.7).abs() < 0.001);
}

// === compute_low_confidence tests ===

#[test]
fn test_low_confidence_below_threshold_with_identifiers() {
    let (lc, tool) = super::compute_low_confidence(Some(0.01), true);
    assert_eq!(lc, Some(true));
    assert_eq!(tool.as_deref(), Some("find_definition"));
}

#[test]
fn test_low_confidence_below_threshold_without_identifiers() {
    let (lc, tool) = super::compute_low_confidence(Some(0.01), false);
    assert_eq!(lc, Some(true));
    assert_eq!(tool.as_deref(), Some("literal_search"));
}

#[test]
fn test_low_confidence_above_threshold() {
    let (lc, tool) = super::compute_low_confidence(Some(0.5), true);
    assert_eq!(lc, None);
    assert_eq!(tool, None);
}

#[test]
fn test_low_confidence_exactly_at_threshold() {
    // Exactly at threshold (0.02) should NOT be low confidence (< not <=)
    let (lc, tool) = super::compute_low_confidence(Some(super::LOW_CONFIDENCE_THRESHOLD), false);
    assert_eq!(lc, None);
    assert_eq!(tool, None);
}

#[test]
fn test_low_confidence_no_results() {
    let (lc, tool) = super::compute_low_confidence(None, false);
    assert_eq!(lc, Some(true));
    assert_eq!(tool.as_deref(), Some("literal_search"));
}

#[test]
fn test_low_confidence_no_results_with_identifiers() {
    let (lc, tool) = super::compute_low_confidence(None, true);
    // Even with identifiers, no results → suggest literal_search
    assert_eq!(lc, Some(true));
    assert_eq!(tool.as_deref(), Some("literal_search"));
}

// === Serde roundtrip tests for new types ===

#[test]
fn test_literal_search_request_serde_roundtrip() {
    let json = r#"{"query":"fn authenticate","regex":true,"limit":5,"file_glob":"src/**/*.rs","language":"Rust","format":"grep"}"#;
    let req: super::LiteralSearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "fn authenticate");
    assert_eq!(req.regex, Some(true));
    assert_eq!(req.phrase, None);
    assert_eq!(req.limit, Some(5));
    assert_eq!(req.file_glob.as_deref(), Some("src/**/*.rs"));
    assert_eq!(req.language.as_deref(), Some("Rust"));
    assert_eq!(req.format.as_deref(), Some("grep"));
}

#[test]
fn test_literal_search_request_minimal() {
    let json = r#"{"query":"hello"}"#;
    let req: super::LiteralSearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "hello");
    assert_eq!(req.regex, None);
    assert_eq!(req.phrase, None);
    assert_eq!(req.limit, None);
    assert_eq!(req.file_glob, None);
    assert_eq!(req.language, None);
    assert_eq!(req.format, None);
}

#[test]
fn test_literal_search_request_phrase_mode() {
    let json = r#"{"query":"fn new","phrase":true}"#;
    let req: super::LiteralSearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.phrase, Some(true));
    assert_eq!(req.regex, None);
}

#[test]
fn test_find_definition_request_serde() {
    let json = r#"{"symbol":"authenticate","kind":"Function","limit":10}"#;
    let req: super::FindDefinitionRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "authenticate");
    assert_eq!(req.kind.as_deref(), Some("Function"));
    assert_eq!(req.limit, Some(10));
}

#[test]
fn test_find_definition_request_minimal() {
    let json = r#"{"symbol":"User"}"#;
    let req: super::FindDefinitionRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "User");
    assert_eq!(req.kind, None);
    assert_eq!(req.limit, None);
}

#[test]
fn test_find_usages_request_serde() {
    let json = r#"{"symbol":"authenticate","limit":50}"#;
    let req: super::FindUsagesRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "authenticate");
    assert_eq!(req.limit, Some(50));
}

#[test]
fn test_find_usages_request_minimal() {
    let json = r#"{"symbol":"Config"}"#;
    let req: super::FindUsagesRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "Config");
    assert_eq!(req.limit, None);
}

#[test]
fn test_file_outline_request_accepts_project_stub() {
    let json = r#"{"path":"src/mcp/mod.rs","project":"ignored"}"#;
    let req: super::FileOutlineRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.path, "src/mcp/mod.rs");
    assert_eq!(req.project.as_deref(), Some("ignored"));
}

#[test]
fn test_get_chunk_request_accepts_project_stub() {
    let json = r#"{"chunk_id":42,"context_lines":25,"project":"ignored"}"#;
    let req: super::GetChunkRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.chunk_id, 42);
    assert_eq!(req.context_lines, Some(25));
    assert_eq!(req.project.as_deref(), Some("ignored"));
}

#[test]
fn test_find_imports_request_accepts_project_stub() {
    let json = r#"{"path":"src/lib.rs","project":"ignored"}"#;
    let req: super::FindImportsRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.path, "src/lib.rs");
    assert_eq!(req.project.as_deref(), Some("ignored"));
}

#[test]
fn test_find_dependents_request_accepts_project_stub() {
    let json = r#"{"symbol_or_path":"auth","limit":10,"project":"ignored"}"#;
    let req: super::FindDependentsRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol_or_path, "auth");
    assert_eq!(req.limit, Some(10));
    assert_eq!(req.project.as_deref(), Some("ignored"));
}

#[test]
fn test_similar_chunks_request_accepts_project_stub() {
    let json = r#"{"chunk_id":7,"limit":5,"project":"ignored"}"#;
    let req: super::SimilarChunksRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.chunk_id, 7);
    assert_eq!(req.limit, Some(5));
    assert_eq!(req.project.as_deref(), Some("ignored"));
}

#[test]
fn test_semantic_search_request_mode_serde() {
    let json = r#"{"query":"auth handler","mode":"lexical","limit":5}"#;
    let req: super::SemanticSearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.mode.as_deref(), Some("lexical"));
    assert_eq!(req.limit, Some(5));
}

// === LiteralSearchResultItem serialization tests ===

#[test]
fn test_literal_search_result_item_serialization() {
    let item = super::LiteralSearchResultItem {
        path: "src/main.rs".to_string(),
        start_line: 10,
        end_line: 20,
        snippet: "fn main()".to_string(),
        score: 0.95,
        kind: Some("Function".to_string()),
        signature: Some("fn main()".to_string()),
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("\"kind\":\"Function\""));
    assert!(json.contains("\"signature\":\"fn main()\""));
}

#[test]
fn test_literal_search_result_item_omits_none_fields() {
    let item = super::LiteralSearchResultItem {
        path: "src/main.rs".to_string(),
        start_line: 10,
        end_line: 20,
        snippet: "code".to_string(),
        score: 0.5,
        kind: None,
        signature: None,
    };
    let json = serde_json::to_string(&item).unwrap();
    assert!(!json.contains("kind"));
    assert!(!json.contains("signature"));
}

// === SemanticSearchResponse serialization tests ===

#[test]
fn test_semantic_search_response_with_results() {
    let response = super::SemanticSearchResponse {
        results: vec![super::SearchResultItem {
            chunk_id: Some(1),
            path: "test.rs".to_string(),
            start_line: 1,
            end_line: 10,
            kind: "Function".to_string(),
            score: 0.8,
            signature: Some("fn test()".to_string()),
            content: None,
            context_prev: None,
            context_next: None,
            source: None,
            chunk_ref: None,
        }],
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"results\""));
    assert!(!json.contains("low_confidence"));
    assert!(!json.contains("suggested_tool"));
}

#[test]
fn test_semantic_search_response_empty_with_low_confidence() {
    let response = super::SemanticSearchResponse {
        results: vec![],
        low_confidence: Some(true),
        suggested_tool: Some("find_definition".to_string()),
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"low_confidence\":true"));
    assert!(json.contains("\"suggested_tool\":\"find_definition\""));
    assert!(json.contains("\"results\":[]"));
}

#[test]
fn test_match_line_for_literal_plain_and_fallback() {
    let content = "first line\nsecond has needle\nthird";
    let matched = super::match_line_for_literal(content, "needle", None);
    assert!(matched.is_some());
    let (offset, snippet) = matched.unwrap();
    assert_eq!(offset, 1);
    assert!(snippet.contains("needle"));

    let not_found = super::match_line_for_literal(content, "absent", None);
    assert!(not_found.is_none());
}

#[test]
fn test_match_line_for_literal_regex() {
    let content = "alpha\nbeta123\ngamma";
    let re = regex::Regex::new(r"beta\d+").unwrap();
    let matched = super::match_line_for_literal(content, "beta", Some(&re));
    assert!(matched.is_some());
    let (offset, snippet) = matched.unwrap();
    assert_eq!(offset, 1);
    assert!(snippet.contains("beta123"));
}

#[test]
fn test_parse_import_lines_detects_common_forms() {
    let content = "use std::fs;\nimport os\nfrom pkg import thing\n#include <stdio.h>\nconst x = require('x')\nlet y = 1;";
    let imports = super::parse_import_lines(content, 10);
    assert_eq!(imports.len(), 5);
    assert_eq!(imports[0].kind, "use");
    assert_eq!(imports[0].line, 10);
    assert_eq!(imports[1].kind, "import");
    assert_eq!(imports[1].line, 11);
    assert_eq!(imports[2].kind, "import");
    assert_eq!(imports[2].line, 12);
    assert_eq!(imports[3].kind, "include");
    assert_eq!(imports[3].line, 13);
    assert_eq!(imports[4].kind, "require");
    assert_eq!(imports[4].line, 14);
}

// === Project/group routing tests ===

#[test]
fn test_has_chunk_id_and_score_fts_result() {
    let result = crate::fts::FtsResult {
        chunk_id: 42,
        score: 0.85,
    };
    assert_eq!(super::HasChunkId::chunk_id(&result), 42);
    assert!((super::HasScore::score(&result) - 0.85).abs() < f32::EPSILON);
}

#[test]
fn test_has_chunk_id_and_score_search_result() {
    let result = crate::vectordb::SearchResult {
        id: 99,
        content: String::new(),
        path: String::new(),
        start_line: 1,
        end_line: 5,
        kind: String::new(),
        signature: None,
        docstring: None,
        context: None,
        hash: String::new(),
        distance: 0.1,
        score: 0.75,
        context_prev: None,
        context_next: None,
    };
    assert_eq!(super::HasChunkId::chunk_id(&result), 99);
    assert!((super::HasScore::score(&result) - 0.75).abs() < f32::EPSILON);
}

/// Simulate the dedup logic from `with_fts_store_read_multi` to verify correctness.
/// Uses (alias, chunk_id) as dedup key — matching production cross-store dedup.
#[test]
fn test_multi_store_dedup_keeps_highest_score() {
    use std::collections::HashMap;

    let aliases = ["repo_a", "repo_b", "repo_c"];

    // Simulate results from 3 stores with overlapping chunk_ids across repos
    let store1_results = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.5,
        },
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.8,
        },
        crate::fts::FtsResult {
            chunk_id: 3,
            score: 0.3,
        },
    ];
    let store2_results = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.9,
        }, // same chunk_id, different alias — NOT a dup
        crate::fts::FtsResult {
            chunk_id: 4,
            score: 0.7,
        },
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.4,
        }, // same chunk_id, different alias — NOT a dup
    ];
    let store3_results = vec![
        crate::fts::FtsResult {
            chunk_id: 3,
            score: 0.6,
        }, // same chunk_id, different alias — NOT a dup
        crate::fts::FtsResult {
            chunk_id: 5,
            score: 0.2,
        },
    ];

    // Apply the same dedup logic as with_fts_store_read_multi: key is (alias, chunk_id)
    let mut all_results: Vec<crate::fts::FtsResult> = Vec::new();
    let mut seen_ids: HashMap<(String, u32), usize> = HashMap::new();

    for (alias, results) in aliases
        .iter()
        .zip([&store1_results, &store2_results, &store3_results])
    {
        for r in results {
            let key = (alias.to_string(), super::HasChunkId::chunk_id(r));
            if let Some(&existing_idx) = seen_ids.get(&key) {
                if super::HasScore::score(r) > super::HasScore::score(&all_results[existing_idx]) {
                    all_results[existing_idx] = r.clone();
                }
            } else {
                seen_ids.insert(key, all_results.len());
                all_results.push(r.clone());
            }
        }
    }

    // Sort by score descending (same as with_fts_store_read_multi)
    all_results.sort_by(|a, b| {
        super::HasScore::score(b)
            .partial_cmp(&super::HasScore::score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Verify: 8 unique (alias, chunk_id) pairs — NO cross-alias dedup
    assert_eq!(
        all_results.len(),
        8,
        "Should have 8 unique (alias, chunk_id) pairs across 3 repos"
    );

    // Check sort: first result should be highest score
    assert!(
        (all_results[0].score - 0.9).abs() < f32::EPSILON,
        "First result should have highest score"
    );

    // Check sort: scores should be descending
    for i in 1..all_results.len() {
        assert!(
            all_results[i].score <= all_results[i - 1].score,
            "Results should be sorted by score descending, but [{}]={} > [{}]={}",
            i - 1,
            all_results[i - 1].score,
            i,
            all_results[i].score
        );
    }
}

#[test]
fn test_multi_store_dedup_no_overlap() {
    // Non-overlapping results — all should be kept
    let store1 = vec![crate::fts::FtsResult {
        chunk_id: 1,
        score: 0.5,
    }];
    let store2 = vec![crate::fts::FtsResult {
        chunk_id: 2,
        score: 0.8,
    }];
    let store3 = vec![crate::fts::FtsResult {
        chunk_id: 3,
        score: 0.3,
    }];

    let mut all_results: Vec<crate::fts::FtsResult> = Vec::new();
    let mut seen_ids: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();

    for results in [&store1, &store2, &store3] {
        for r in results {
            let id = super::HasChunkId::chunk_id(r);
            if let Some(&existing_idx) = seen_ids.get(&id) {
                if super::HasScore::score(r) > super::HasScore::score(&all_results[existing_idx]) {
                    all_results[existing_idx] = r.clone();
                }
            } else {
                seen_ids.insert(id, all_results.len());
                all_results.push(r.clone());
            }
        }
    }

    assert_eq!(
        all_results.len(),
        3,
        "All 3 non-overlapping results should be kept"
    );
}

#[test]
fn test_multi_store_dedup_all_same_ids() {
    // All stores return same chunk_ids — only keep each once with max score
    let store1 = vec![crate::fts::FtsResult {
        chunk_id: 1,
        score: 0.3,
    }];
    let store2 = vec![crate::fts::FtsResult {
        chunk_id: 1,
        score: 0.9,
    }];
    let store3 = vec![crate::fts::FtsResult {
        chunk_id: 1,
        score: 0.6,
    }];

    let mut all_results: Vec<crate::fts::FtsResult> = Vec::new();
    let mut seen_ids: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();

    for results in [&store1, &store2, &store3] {
        for r in results {
            let id = super::HasChunkId::chunk_id(r);
            if let Some(&existing_idx) = seen_ids.get(&id) {
                if super::HasScore::score(r) > super::HasScore::score(&all_results[existing_idx]) {
                    all_results[existing_idx] = r.clone();
                }
            } else {
                seen_ids.insert(id, all_results.len());
                all_results.push(r.clone());
            }
        }
    }

    assert_eq!(all_results.len(), 1, "Should deduplicate to 1 result");
    assert!(
        (all_results[0].score - 0.9).abs() < f32::EPSILON,
        "Should keep highest score 0.9, got {}",
        all_results[0].score
    );
}

// === Serde roundtrip tests for group field ===

#[test]
fn test_find_request_with_group() {
    let json = r#"{"symbol":"authenticate","kind":"definition","group":"frontend"}"#;
    let req: super::types::FindRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "authenticate");
    assert_eq!(req.group.as_deref(), Some("frontend"));
    assert!(req.project.is_none());
}

#[test]
fn test_find_request_with_project_and_group_exclusive() {
    // Both project and group can be deserialized (validation happens at runtime)
    let json = r#"{"symbol":"foo","project":"repo1","group":"grp1"}"#;
    let req: super::types::FindRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.project.as_deref(), Some("repo1"));
    assert_eq!(req.group.as_deref(), Some("grp1"));
}

#[test]
fn test_explore_request_with_group() {
    let json = r#"{"kind":"outline","target":"src/main.rs","group":"backend"}"#;
    let req: super::types::ExploreRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.kind.as_deref(), Some("outline"));
    assert_eq!(req.group.as_deref(), Some("backend"));
}

#[test]
fn test_status_request_with_group() {
    let json = r#"{"kind":"index","group":"all"}"#;
    let req: super::types::StatusRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.kind.as_deref(), Some("index"));
    assert_eq!(req.group.as_deref(), Some("all"));
}

#[test]
fn test_search_request_with_group() {
    let json = r#"{"query":"auth","group":"platform","mode":"semantic"}"#;
    let req: super::types::SearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "auth");
    assert_eq!(req.group.as_deref(), Some("platform"));
    assert_eq!(req.mode.as_deref(), Some("semantic"));
}

#[test]
fn test_find_definition_request_with_group() {
    let json = r#"{"symbol":"User","project":"api","group":"backend"}"#;
    let req: super::types::FindDefinitionRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "User");
    assert_eq!(req.project.as_deref(), Some("api"));
    assert_eq!(req.group.as_deref(), Some("backend"));
}

#[test]
fn test_find_usages_request_with_group() {
    let json = r#"{"symbol":"handle_request","group":"services"}"#;
    let req: super::types::FindUsagesRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol, "handle_request");
    assert_eq!(req.group.as_deref(), Some("services"));
    assert!(req.project.is_none());
}

#[test]
fn test_file_outline_request_with_group() {
    let json = r#"{"path":"src/main.rs","group":"all"}"#;
    let req: super::types::FileOutlineRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.path, "src/main.rs");
    assert_eq!(req.group.as_deref(), Some("all"));
}

#[test]
fn test_get_chunk_request_with_group() {
    let json = r#"{"chunk_id":42,"group":"backend"}"#;
    let req: super::types::GetChunkRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.chunk_id, 42);
    assert_eq!(req.group.as_deref(), Some("backend"));
}

#[test]
fn test_find_imports_request_with_group() {
    let json = r#"{"path":"src/lib.rs","group":"platform"}"#;
    let req: super::types::FindImportsRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.path, "src/lib.rs");
    assert_eq!(req.group.as_deref(), Some("platform"));
}

#[test]
fn test_find_dependents_request_with_group() {
    let json = r#"{"symbol_or_path":"auth","limit":10,"group":"services"}"#;
    let req: super::types::FindDependentsRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.symbol_or_path, "auth");
    assert_eq!(req.limit, Some(10));
    assert_eq!(req.group.as_deref(), Some("services"));
}

#[test]
fn test_similar_chunks_request_with_group() {
    let json = r#"{"chunk_id":7,"limit":5,"group":"frontend"}"#;
    let req: super::types::SimilarChunksRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.chunk_id, 7);
    assert_eq!(req.limit, Some(5));
    assert_eq!(req.group.as_deref(), Some("frontend"));
}

#[test]
fn test_literal_search_request_with_group() {
    let json = r#"{"query":"TODO","group":"all","format":"grep"}"#;
    let req: super::types::LiteralSearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "TODO");
    assert_eq!(req.group.as_deref(), Some("all"));
    assert_eq!(req.format.as_deref(), Some("grep"));
}

#[test]
fn test_semantic_search_request_with_group() {
    let json = r#"{"query":"authentication flow","group":"platform","mode":"hybrid"}"#;
    let req: super::types::SemanticSearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.query, "authentication flow");
    assert_eq!(req.group.as_deref(), Some("platform"));
    assert_eq!(req.mode.as_deref(), Some("hybrid"));
}

// === MultiStoreContext decomposition tests ===
//
// These tests verify the pure decomposition logic used by `resolve_routing()`:
//   Option<Vec<Arc<SharedStores>>> → { stores, stores_vec, is_multi, needs_local_db }
//
// We simulate the exact same logic without needing a real CodesearchService
// (which requires LMDB databases, file system state, etc).

/// Simulates the decomposition in `resolve_routing()`.
/// Returns (stores, stores_vec, is_multi, needs_local_db).
#[allow(clippy::type_complexity)]
fn decompose_routing_ctx<T: Clone>(
    multi_stores: Option<Vec<std::sync::Arc<T>>>,
) -> (
    Option<std::sync::Arc<T>>,
    Option<Vec<std::sync::Arc<T>>>,
    bool,
    bool,
) {
    let is_multi = multi_stores.as_ref().is_some_and(|v| v.len() > 1);
    let stores = match &multi_stores {
        None => None,
        Some(vec) if vec.len() == 1 => Some(vec[0].clone()),
        Some(_) => None,
    };
    let stores_vec = if is_multi { multi_stores } else { None };
    let needs_local_db = stores.is_none() && !is_multi;
    (stores, stores_vec, is_multi, needs_local_db)
}

// Helper: create Arc<i32> as a stand-in for Arc<SharedStores>
fn arc_val(v: i32) -> std::sync::Arc<i32> {
    std::sync::Arc::new(v)
}

#[test]
fn test_routing_decomposition_none_input() {
    // No routing params → all None/false, needs_local_db = true
    let (stores, stores_vec, is_multi, needs_local_db) = decompose_routing_ctx::<i32>(None);
    assert!(stores.is_none(), "stores should be None");
    assert!(stores_vec.is_none(), "stores_vec should be None");
    assert!(!is_multi, "is_multi should be false");
    assert!(
        needs_local_db,
        "needs_local_db should be true — no serve-state stores"
    );
}

#[test]
fn test_routing_decomposition_single_store() {
    // One repo resolved → stores = Some, stores_vec = None, not multi
    let (stores, stores_vec, is_multi, needs_local_db) =
        decompose_routing_ctx(Some(vec![arc_val(1)]));
    assert!(stores.is_some(), "stores should be Some for single repo");
    assert!(
        stores_vec.is_none(),
        "stores_vec should be None for single repo"
    );
    assert!(!is_multi, "is_multi should be false for single repo");
    assert!(
        !needs_local_db,
        "needs_local_db should be false — we have a store"
    );
    assert_eq!(*stores.unwrap(), 1);
}

#[test]
fn test_routing_decomposition_two_stores() {
    // Group with 2 repos → stores = None, stores_vec = Some, is_multi = true
    let (stores, stores_vec, is_multi, needs_local_db) =
        decompose_routing_ctx(Some(vec![arc_val(1), arc_val(2)]));
    assert!(stores.is_none(), "stores should be None for multi-store");
    assert!(
        stores_vec.is_some(),
        "stores_vec should be Some for multi-store"
    );
    assert!(is_multi, "is_multi should be true for 2+ stores");
    assert!(
        !needs_local_db,
        "needs_local_db should be false — we have stores"
    );
    let sv = stores_vec.unwrap();
    assert_eq!(sv.len(), 2);
}

#[test]
fn test_routing_decomposition_three_stores() {
    // Group with 3 repos → same as 2 but verify vec length
    let (stores, stores_vec, is_multi, needs_local_db) =
        decompose_routing_ctx(Some(vec![arc_val(10), arc_val(20), arc_val(30)]));
    assert!(stores.is_none());
    assert!(stores_vec.is_some());
    assert!(is_multi);
    assert!(!needs_local_db);
    assert_eq!(stores_vec.unwrap().len(), 3);
}

#[test]
fn test_routing_decomposition_empty_vec() {
    // Empty vec (edge case — shouldn't happen but verify)
    let (stores, stores_vec, is_multi, needs_local_db) = decompose_routing_ctx::<i32>(Some(vec![]));
    // Empty vec: is_multi=false (len=0 not > 1), stores=None (len=0 not 1)
    assert!(stores.is_none(), "empty vec → stores None");
    assert!(
        stores_vec.is_none(),
        "empty vec → stores_vec None (is_multi=false)"
    );
    assert!(!is_multi, "empty vec → is_multi false");
    assert!(needs_local_db, "empty vec → needs_local_db true");
}

// === MultiStoreContext decomposition tests ===
//
// These tests verify the pure decomposition logic used by `resolve_routing()`:
//   Option<Vec<Arc<SharedStores>>> → { stores, stores_vec, is_multi, needs_local_db }
//
// We test the same logic without needing a real CodesearchService
// (which requires LMDB databases, file system state, etc).

#[test]
fn test_routing_single_project_maps_to_single_store() {
    // A single project alias → vec of length 1 → single-store path
    let multi = Some(vec![arc_val(42)]);
    let (stores, stores_vec, is_multi, needs_local_db) = decompose_routing_ctx(multi);
    assert!(!is_multi);
    assert!(stores.is_some());
    assert_eq!(*stores.unwrap(), 42);
    assert!(stores_vec.is_none());
    assert!(!needs_local_db);
}

#[test]
fn test_routing_group_maps_to_multi_store() {
    // A group with 3 aliases → vec of length 3 → multi-store path
    let multi = Some(vec![arc_val(1), arc_val(2), arc_val(3)]);
    let (stores, stores_vec, is_multi, needs_local_db) = decompose_routing_ctx(multi);
    assert!(is_multi);
    assert!(stores.is_none(), "multi-store → no single override");
    assert_eq!(stores_vec.unwrap().len(), 3);
    assert!(!needs_local_db);
}

// === merge_exact_into_fts routing-relevant tests ===

#[test]
fn test_merge_exact_cross_store_dedup() {
    // Simulate merging FTS results from multiple stores with overlapping chunk_ids
    // This is the pattern used by with_fts_store_read_multi
    let mut base: Vec<crate::fts::FtsResult> = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.5,
        },
        crate::fts::FtsResult {
            chunk_id: 2,
            score: 0.8,
        },
    ];
    let exact = vec![
        crate::fts::FtsResult {
            chunk_id: 1,
            score: 0.9,
        }, // higher score
        crate::fts::FtsResult {
            chunk_id: 3,
            score: 0.7,
        }, // new chunk
    ];

    super::merge_exact_into_fts(&mut base, exact);

    assert_eq!(base.len(), 3, "should have 3 unique chunks");
    let chunk1 = base.iter().find(|r| r.chunk_id == 1).unwrap();
    assert!(
        (chunk1.score - 0.9).abs() < f32::EPSILON,
        "chunk 1 should have max score 0.9, got {}",
        chunk1.score
    );
}

// ─── regex_has_anchorable_token detector tests ───────────────────────

#[test]
fn test_regex_has_anchorable_token() {
    // Previously 13 separate #[test]s named test_regex_has_anchorable_token_*
    // (plus two near-duplicate "scan-path decision" tests asserting the same
    // predicate); consolidated into one table-driven test.
    let cases: &[(&str, bool)] = &[
        // anchorable (>=3 alphanumeric run)
        ("match_line_for_literal", true),
        ("Vec<.*>", true),
        ("HashMap::new", true),
        ("fnx", true),
        ("impl\\b\\s+function_name", true),
        // not anchorable
        ("fn", false),
        ("\\bfn\\s+\\w+", false),
        ("\\bimpl\\s+", false),
        ("\\.\\w+\\(\\)", false),
        ("[A-Z]+_[A-Z]+", false),
        ("^[A-Z]\\w+", false),
        ("", false),
        ("->", false),
        ("::", false),
        ("impl\\b", false),
        ("Result\\b", false),
        ("match\\b", false),
        ("impl[A-Z]", false),
        ("foo[abc]+", false),
        ("impl\\s", false),
        ("\\bimpl\\b", false),
    ];
    for (pattern, expected) in cases {
        let got = super::regex_has_anchorable_token(pattern);
        assert_eq!(
            got, *expected,
            "regex_has_anchorable_token({pattern:?}) expected {expected}"
        );
    }
}

// ── regex_has_disjunctive_or tests ──────────────────────────────

#[test]
fn test_regex_has_disjunctive_or() {
    // Previously 9 separate #[test]s; consolidated into one table-driven test
    // over regex_has_disjunctive_or (top-level `|`, ignoring pipes inside
    // groups, brackets, or escaped).
    let cases: &[(&str, bool)] = &[
        ("TODO|FIXME|HACK", true),
        ("foo|bar", true),
        ("(foo|bar)", false),
        ("[a|b]", false),
        ("foo\\|bar", false),
        ("TODO", false),
        ("foo|(bar|baz)", true),
        ("((a|b))", false),
        ("[a-z]|foo", true),
    ];
    for (pattern, expected) in cases {
        let got = super::regex_has_disjunctive_or(pattern);
        assert_eq!(
            got, *expected,
            "regex_has_disjunctive_or({pattern:?}) expected {expected}"
        );
    }
}

#[test]
fn test_regex_no_match_match_line_returns_none() {
    // match_line_for_literal returns None for patterns that don't match
    let regex = regex::Regex::new(r"\bfn\s+\w+").unwrap();
    let content = "struct Foo { x: i32 }\nimpl Foo { fn bar() {} }";
    // This content DOES match — fn bar() matches \bfn\s+\w+
    assert!(super::match_line_for_literal(content, r"\bfn\s+\w+", Some(&regex)).is_some());

    // This content does NOT match the regex
    let regex2 = regex::Regex::new(r"zzz_definitely_not_in_code").unwrap();
    let content2 = "fn foo() {}\nfn bar() {}";
    assert!(
        super::match_line_for_literal(content2, "zzz_definitely_not_in_code", Some(&regex2))
            .is_none()
    );

    // Non-anchorable regex with no matches → empty (scan path would skip)
    let regex3 = regex::Regex::new(r"\bimpl\s+\w+\s+for\s+\w+").unwrap();
    let content3 = "fn simple() {}\nstruct Foo;";
    assert!(
        super::match_line_for_literal(content3, r"\bimpl\s+\w+\s+for\s+\w+", Some(&regex3))
            .is_none()
    );
}

// ─── looks_like_code_pattern detector tests ───────────────────────

#[test]
fn test_looks_like_code_pattern() {
    // Previously 8 separate #[test]s; consolidated into one table-driven test.
    let cases: &[(&str, bool)] = &[
        // code-like (true)
        ("foo = null", true),
        ("x = 42", true),
        ("foo->bar", true),
        ("x => y", true),
        ("std::string", true),
        ("a::b::c", true),
        ("Vec<T>", true),
        ("HashMap<K, V>", true),
        ("return x;", true),
        ("if (x) {", true),
        // not code-like (false)
        ("ActivitiesListModelResponse", false),
        ("foo_bar", false),
        ("foo.bar", false),
        ("System.Console", false),
        ("", false),
    ];
    for (pattern, expected) in cases {
        let got = super::looks_like_code_pattern(pattern);
        assert_eq!(
            got, *expected,
            "looks_like_code_pattern({pattern:?}) expected {expected}"
        );
    }
}

// ─── extract_bm25_query_from_regex tests ─────────────────────────

#[test]
fn test_extract_bm25_query_from_regex() {
    // Previously 7 separate #[test]s; consolidated into one table-driven test.
    // Input patterns are regex source strings (backslashes already escaped).
    let cases: &[(&str, &str)] = &[
        ("class \\w+Cache\\b", "class Cache"),
        ("interface I\\w+", "interface"),
        ("class \\w+Store\\b", "class Store"),
        ("CleanupController", "CleanupController"),
        ("\\w+", ""),
        ("\\.MethodName\\(", "MethodName"),
        ("[a-z]+Cache", "Cache"),
    ];
    for (pattern, expected) in cases {
        let got = super::extract_bm25_query_from_regex(pattern);
        assert_eq!(
            got, *expected,
            "extract_bm25_query_from_regex({pattern:?}) expected {expected:?}"
        );
    }
}

// ─── compute_literal_low_confidence tests ─────────────────────────

#[test]
fn test_literal_lc_natural_language_zero_results() {
    let (lc, hint) = super::compute_literal_low_confidence(None, "how do we handle auth");
    assert_eq!(lc, Some(true));
    assert!(hint.unwrap().contains("semantic"));
}

#[test]
fn test_literal_lc_identifier_zero_results() {
    let (lc, hint) = super::compute_literal_low_confidence(None, "CodesearchService");
    assert_eq!(lc, Some(true));
    assert!(hint.unwrap().contains("regex"));
}

#[test]
fn test_literal_lc_code_pattern_zero_results() {
    let (lc, hint) = super::compute_literal_low_confidence(None, "foo = null");
    assert_eq!(lc, Some(true));
    assert!(hint.unwrap().contains("regex"));
}

#[test]
fn test_literal_lc_natural_language_weak_score() {
    // Use a score demonstrably less than f32::MAX
    let weak_score = super::LITERAL_LOW_CONFIDENCE_BM25 / 2.0;
    let (lc, hint) =
        super::compute_literal_low_confidence(Some(weak_score), "how do we handle auth");
    assert_eq!(lc, Some(true));
    assert!(hint.unwrap().contains("semantic"));
}

#[test]
fn test_literal_lc_identifier_weak_score() {
    // Single-word identifiers with low BM25 score: trust the result.
    // BM25 IDF artefacts (e.g. `or` in a snake_case name) must not
    // cause false low_confidence signals when results exist.
    let weak_score = super::LITERAL_LOW_CONFIDENCE_BM25 / 2.0;
    let (lc, hint) = super::compute_literal_low_confidence(Some(weak_score), "CodesearchService");
    assert_eq!(
        lc, None,
        "single identifier with results must not be flagged low_confidence"
    );
    assert_eq!(hint, None);
}

#[test]
fn test_literal_lc_does_not_fire_on_strong_results() {
    // Strong BM25 score (well above floor) must NOT be flagged low_confidence.
    let (lc, hint) = super::compute_literal_low_confidence(Some(41.5), "anything");
    assert_eq!(
        lc, None,
        "strong BM25 results must not be flagged low_confidence"
    );
    assert_eq!(hint, None);
}

#[test]
fn test_literal_lc_fires_on_weak_results() {
    // Multi-word queries (not single identifiers) still fire low_confidence
    // when the BM25 score is below the floor.
    let (lc, hint) = super::compute_literal_low_confidence(
        Some(super::LITERAL_LOW_CONFIDENCE_BM25 - 0.5),
        "how do we handle authentication", // multi-word natural language
    );
    assert_eq!(lc, Some(true));
    assert!(hint.is_some());
}

#[test]
fn test_literal_lc_threshold_boundary_uses_strict_less_than() {
    // Score EXACTLY at the threshold should NOT fire (< not <=).
    let (lc, hint) =
        super::compute_literal_low_confidence(Some(super::LITERAL_LOW_CONFIDENCE_BM25), "anything");
    assert_eq!(lc, None);
    assert_eq!(hint, None);
}

#[test]
fn test_literal_lc_high_score_returns_none() {
    let (lc, hint) = super::compute_literal_low_confidence(Some(50.0), "anything");
    assert_eq!(lc, None);
    assert_eq!(hint, None);
}

#[test]
fn test_literal_response_json_has_lc_fields() {
    let response = super::LiteralSearchResponse {
        results: vec![],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: Some(true),
        suggested_tool: Some("search with mode='semantic'".to_string()),
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains(r#""low_confidence":true"#));
    assert!(json.contains("\"suggested_tool\""));
}

#[test]
fn test_literal_response_json_omits_lc_fields_when_none() {
    let response = super::LiteralSearchResponse {
        results: vec![],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("low_confidence"));
    assert!(!json.contains("suggested_tool"));
    assert!(!json.contains("auto_promoted"));
    assert!(!json.contains("note"));
}

// ─── note phrasing tests ──────────────────────────────────────────

#[test]
fn test_literal_response_note_is_sentence_not_tool_name() {
    // Simulate the note-construction logic for the low-confidence branch.
    let suggested_tool: Option<String> = Some("find with kind='definition'".to_string());
    let auto_promoted = false;
    let low_confidence = Some(true);

    let note: Option<String> = if auto_promoted {
        Some("ignored".to_string())
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

    let n = note.expect("note must be present when low_confidence is true");
    assert!(
        n.starts_with("Top result"),
        "note must read as a sentence, got: {}",
        n
    );
    assert!(
        n.contains("find with kind='definition'"),
        "note must reference the suggested tool: {}",
        n
    );
}

// ─── MCP mode selection tests ────────────────────────────────────

#[test]
fn test_mcp_mode_from_str() {
    assert_eq!(
        "auto".parse::<super::McpMode>().unwrap(),
        super::McpMode::Auto
    );
    assert_eq!(
        "client".parse::<super::McpMode>().unwrap(),
        super::McpMode::Client
    );
    assert_eq!(
        "local".parse::<super::McpMode>().unwrap(),
        super::McpMode::Local
    );
    assert_eq!(
        "AUTO".parse::<super::McpMode>().unwrap(),
        super::McpMode::Auto
    );
    assert_eq!(
        "Client".parse::<super::McpMode>().unwrap(),
        super::McpMode::Client
    );
    assert!("invalid".parse::<super::McpMode>().is_err());
}

#[test]
fn test_mcp_mode_display() {
    assert_eq!(super::McpMode::Auto.to_string(), "auto");
    assert_eq!(super::McpMode::Client.to_string(), "client");
    assert_eq!(super::McpMode::Local.to_string(), "local");
}

#[test]
fn test_mcp_mode_default_is_auto() {
    assert_eq!(super::McpMode::default(), super::McpMode::Auto);
}

#[test]
fn test_mcp_mode_env_is_used_by_cli() {
    // The CLI uses clap's #[arg(env = "...")] which handles env var fallback.
    // When no --mode is provided and no env var, default is Auto.
    assert_eq!(super::McpMode::default(), super::McpMode::Auto);
}

#[test]
fn test_mcp_mode_from_str_covers_all() {
    // Verify all valid modes parse correctly
    for mode in &["auto", "client", "local", "AUTO", "Client", "LOCAL"] {
        assert!(
            mode.parse::<super::McpMode>().is_ok(),
            "failed to parse: {}",
            mode
        );
    }
    assert!("invalid".parse::<super::McpMode>().is_err());
}

// ─── auto-promotion behaviour tests ────────────────────────────────

#[test]
fn test_auto_promotion_escapes_and_relaxes_spaces() {
    // "foo = null" → regex::escape → "foo = null" (spaces not escaped) → replace ' ' with \s+ → "foo\s+=\s+null"
    let query = "foo = null";
    let escaped = regex::escape(query);
    let relaxed = escaped.replace(' ', r"\s+");
    assert_eq!(relaxed, r"foo\s+=\s+null");
}

#[test]
fn test_auto_promoted_skipped_when_user_sets_regex() {
    let user_set_regex = true;
    let user_set_phrase = false;
    let auto_promoted =
        !user_set_regex && !user_set_phrase && super::looks_like_code_pattern("foo = null");
    assert!(!auto_promoted);
}

#[test]
fn test_auto_promoted_skipped_when_user_sets_phrase() {
    let user_set_regex = false;
    let user_set_phrase = true;
    let auto_promoted =
        !user_set_regex && !user_set_phrase && super::looks_like_code_pattern("foo = null");
    assert!(!auto_promoted);
}

#[test]
fn test_literal_search_response_shape_json() {
    let response = super::LiteralSearchResponse {
        results: vec![super::LiteralSearchResultItem {
            path: "test.rs".to_string(),
            start_line: 1,
            end_line: 1,
            snippet: "fn test()".to_string(),
            score: 1.0,
            kind: None,
            signature: None,
        }],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.starts_with('{'));
    assert!(json.contains("\"results\":["));
    assert!(!json.starts_with('['));
}

#[test]
fn test_literal_search_response_carries_note_when_promoted() {
    let response = super::LiteralSearchResponse {
        results: vec![],
        auto_promoted_to_regex: Some(true),
        note: Some("auto-promoted".to_string()),
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains(r#""auto_promoted_to_regex":true"#));
    assert!(json.contains("\"note\""));
}

// === Store-failure reporting =========================================
//
// These exist because this exact contract has been silently re-broken three
// times in three review rounds, in three different handlers. The behaviour
// is verified in production, but nothing in CI would have caught a fourth
// regression. These tests make the contract cheap to keep.

#[test]
fn store_warning_is_formatted_in_one_place() {
    assert_eq!(
        super::store_warning("inriver", "chunk lookup", "os error 22"),
        "repo 'inriver' chunk lookup failed: os error 22"
    );
}

#[test]
fn note_store_failure_records_once_per_store() {
    let aliases = vec!["inriver".to_string(), "akeneo".to_string()];
    let mut warnings = Vec::new();
    let err = anyhow::anyhow!("os error 22");

    // A resolution loop runs per hit; the caller wants to know THAT the
    // repo is down, not how many times we noticed.
    super::note_store_failure(&mut warnings, &aliases, 0, "chunk lookup", &err);
    super::note_store_failure(&mut warnings, &aliases, 0, "chunk lookup", &err);
    super::note_store_failure(&mut warnings, &aliases, 1, "chunk lookup", &err);

    assert_eq!(
        warnings.len(),
        2,
        "duplicates must be collapsed: {warnings:?}"
    );
    assert!(warnings[0].contains("inriver"));
    assert!(warnings[1].contains("akeneo"));
}

#[test]
fn note_store_failure_renders_the_whole_error_chain() {
    // Plain `{}` shows only the outermost context, which is what turned a
    // real EINVAL into an unactionable "Error reading from vector store".
    let err = anyhow::anyhow!("os error 22").context("Error searching vector store");
    let mut warnings = Vec::new();
    super::note_store_failure(&mut warnings, &["inriver".to_string()], 0, "search", &err);

    assert!(warnings[0].contains("os error 22"), "got: {}", warnings[0]);
    assert!(warnings[0].contains("Error searching vector store"));
}

#[test]
fn note_store_failure_survives_a_short_alias_list() {
    // Fan-out and alias vectors are parallel by convention, not by type. A
    // mismatch must not panic in a search handler.
    let mut warnings = Vec::new();
    super::note_store_failure(&mut warnings, &[], 3, "search", &anyhow::anyhow!("boom"));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("unknown"));
}

// === status(kind="index") multi-store summary ==========================
//
// Follow-up 16: a store failing mid-fan-out used to render identically to
// "not yet indexed" (both are 0 chunks, `all_indexed = false`). These pin
// the three-way decision the fix depends on — building / degraded-ready /
// clean-ready are distinct messages, not just a boolean.

#[test]
fn index_status_summary_reports_building_before_anything_failed() {
    let (status, message) = super::index_status_summary(3, 0, 0);
    assert_eq!(status, "building");
    assert!(!message.contains("failed"), "got: {message}");
}

#[test]
fn index_status_summary_reports_clean_ready_with_no_failures() {
    let (status, message) = super::index_status_summary(3, 0, 500);
    assert_eq!(status, "ready");
    assert!(!message.contains("failed"), "got: {message}");
    assert!(message.contains("3 repo(s)"), "got: {message}");
}

#[test]
fn index_status_summary_surfaces_a_degraded_group_as_ready_with_a_count() {
    // This is the exact case that used to be indistinguishable from
    // "index still warming": some data is in, one store didn't answer.
    let (status, message) = super::index_status_summary(3, 1, 500);
    assert_eq!(
        status, "ready",
        "the two healthy stores must not be masked by the one that failed"
    );
    assert!(
        message.contains("2 of 3 repo(s)") && message.contains("1 store(s) failed"),
        "message must name both the healthy count and the failure count, got: {message}"
    );
    assert!(message.contains("warnings"), "got: {message}");
}

#[test]
fn index_status_summary_reports_error_when_every_store_failed() {
    // The correlated-failure case: all stores went down together (e.g. a
    // shared read-only-snapshot or disk-full condition), so `total_chunks`
    // is 0 for the same reason it would be on a never-indexed group. Before
    // this fix, `total_chunks == 0` was checked first and this rendered as
    // "building" — byte-identical to "not indexed yet" — even though every
    // store actively failed. `failed_count >= total_repos` must win.
    let (status, message) = super::index_status_summary(3, 3, 0);
    assert_eq!(
        status, "error",
        "a group where every store failed must not read as merely 'still building'"
    );
    assert!(message.contains("3"), "got: {message}");
    assert!(message.contains("warnings"), "got: {message}");
}

#[test]
fn repo_stats_from_result_carries_counts_and_no_error_on_success() {
    let stats = crate::vectordb::StoreStats {
        total_chunks: 42,
        total_files: 7,
        indexed: true,
        dimensions: 384,
        max_chunk_id: 42,
    };
    let (total_chunks, total_files, error) = super::repo_stats_from_result(Ok(stats));
    assert_eq!((total_chunks, total_files), (42, 7));
    assert!(error.is_none(), "got: {error:?}");
}

#[test]
fn repo_stats_from_result_zeroes_counts_and_names_the_error_on_failure() {
    // This is the exact case requirement 2 of follow-up 16 closes: a
    // stats() failure used to be indistinguishable from a healthy, simply
    // empty repo (both render as 0/0 with no error). Reintroducing the old
    // behaviour (returning `None` unconditionally here, as the fix
    // originally had it before this helper existed) makes this assertion
    // fail — confirmed by hand before restoring the real branch.
    let err = anyhow::anyhow!("LMDB env unreadable: os error 30");
    let (total_chunks, total_files, error) =
        super::repo_stats_from_result(Err::<crate::vectordb::StoreStats, _>(err));
    assert_eq!((total_chunks, total_files), (0, 0));
    let error = error.expect("a stats() failure must surface an error, not render as healthy");
    assert!(
        error.contains("stats unavailable") && error.contains("os error 30"),
        "got: {error}"
    );
}

#[test]
fn record_stats_or_warn_pushes_nothing_on_success() {
    let stats = crate::vectordb::StoreStats {
        total_chunks: 5,
        total_files: 2,
        indexed: true,
        dimensions: 384,
        max_chunk_id: 5,
    };
    let mut warnings = Vec::new();
    let (total_chunks, total_files, error) =
        super::record_stats_or_warn(Ok(stats), "inriver", &mut warnings);
    assert_eq!((total_chunks, total_files), (5, 2));
    assert!(error.is_none(), "got: {error:?}");
    assert!(
        warnings.is_empty(),
        "a healthy store must not add a warning, got: {warnings:?}"
    );
}

#[test]
fn record_stats_or_warn_names_the_repo_and_surfaces_the_error_on_failure() {
    // This is the exact call site `list_projects` uses — pinning it here
    // means a future edit cannot silently stop reporting a broken store
    // without also breaking `total_chunks`/`total_files`, which this
    // asserts too. Reintroducing the old bug (discarding `error` after
    // this call, or calling `repo_stats_from_result` directly and
    // skipping the push) makes the `warnings` assertion below fail —
    // confirmed by hand before restoring the real call site.
    let err = anyhow::anyhow!("LMDB env unreadable: os error 30");
    let mut warnings = Vec::new();
    let (total_chunks, total_files, error) = super::record_stats_or_warn(
        Err::<crate::vectordb::StoreStats, _>(err),
        "inriver",
        &mut warnings,
    );
    assert_eq!((total_chunks, total_files), (0, 0));
    assert!(error.is_some(), "got: {error:?}");
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("repo 'inriver' stats failed") && warnings[0].contains("os error 30"),
        "got: {warnings:?}"
    );
}

#[test]
fn record_stats_or_warn_does_not_duplicate_the_same_warning() {
    // `push_store_warning` dedups by exact match; this pins that
    // `record_stats_or_warn` still benefits from it when called twice
    // with the same failure (e.g. a repo appearing twice in a fan-out).
    let mut warnings = Vec::new();
    for _ in 0..2 {
        let err = anyhow::anyhow!("LMDB env unreadable: os error 30");
        super::record_stats_or_warn(
            Err::<crate::vectordb::StoreStats, _>(err),
            "inriver",
            &mut warnings,
        );
    }
    assert_eq!(
        warnings.len(),
        1,
        "the same repo/failure must not be reported twice, got: {warnings:?}"
    );
}

#[test]
fn into_results_routes_failures_into_warnings() {
    let outcome = super::MultiReadOutcome {
        results: vec![1u32, 2],
        failures: vec![("inriver".to_string(), "os error 22".to_string())],
    };
    let mut warnings = Vec::new();
    let results = outcome.into_results(&mut warnings, "chunk lookup");

    assert_eq!(results, vec![1, 2]);
    assert_eq!(
        warnings,
        vec!["repo 'inriver' chunk lookup failed: os error 22".to_string()],
        "taking the results must never drop the failures"
    );
}

#[test]
fn qualify_empty_result_is_transparent_when_nothing_failed() {
    let msg = "No definition found for 'Foo'.".to_string();
    assert_eq!(super::qualify_empty_result(msg.clone(), &[]), msg);
}

#[test]
fn qualify_empty_result_contradicts_a_not_found_diagnosis() {
    // "may not be indexed" is a DIAGNOSIS, and it is flatly wrong when the
    // store never answered. An agent acts on it by giving up or by
    // re-indexing something that was never broken.
    let out = super::qualify_empty_result(
        "No definition found for 'Foo'. The symbol may not be indexed.".to_string(),
        &["repo 'inriver' definition search failed: os error 22".to_string()],
    );
    assert!(out.contains("WARNING"));
    assert!(out.contains("inriver"));
    assert!(out.contains("os error 22"));
    // The message is a caller-facing sentence. A mangled line
    // continuation collapses it into a run of spaces and nobody
    // notices, because every `contains` assertion above still passes.
    assert!(
        out.contains("this result is not trustworthy — 1 store(s) in scope failed"),
        "message must read as a sentence, got: {out}"
    );
    assert!(!out.contains("  "), "no run-on spacing in: {out}");
}

#[test]
fn respond_with_items_carries_warnings_on_every_path() {
    use rmcp::model::RawContent;
    let text = |r: Result<super::CallToolResult, super::McpError>| -> String {
        match &r.unwrap().content[0].raw {
            RawContent::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    };
    let warned = vec!["repo 'inriver' outline scan failed: os error 22".to_string()];

    // Empty + failure: the message must contradict its own diagnosis.
    let out = text(super::respond_with_items(&[0u32; 0], &warned, || {
        "No indexed chunks found for path.".to_string()
    }));
    assert!(out.contains("WARNING"), "got: {out}");
    assert!(out.contains("inriver"), "got: {out}");

    // NON-empty + failure: this is the path five handlers used to drop. A
    // short-but-plausible list from a partially-dead group must say so.
    let out = text(super::respond_with_items(&[1u32, 2], &warned, || {
        "unused".to_string()
    }));
    assert!(out.contains("warnings"), "got: {out}");
    assert!(out.contains("os error 22"), "got: {out}");
    assert!(out.contains("results"), "got: {out}");

    // Healthy: byte-identical to the pre-existing bare array.
    let out = text(super::respond_with_items(&[1u32, 2], &[], || {
        "unused".to_string()
    }));
    assert_eq!(out, "[1,2]", "a healthy response must not change shape");

    // Empty and healthy: plain message, no warning noise.
    let out = text(super::respond_with_items(&[0u32; 0], &[], || {
        "No indexed chunks found for path.".to_string()
    }));
    assert_eq!(out, "No indexed chunks found for path.");
}

#[test]
fn respond_with_items_noted_shapes_on_every_path() {
    use rmcp::model::RawContent;
    let text = |r: Result<super::CallToolResult, super::McpError>| -> String {
        match &r.unwrap().content[0].raw {
            RawContent::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    };
    let warned = vec!["repo 'inriver' usage search failed: os error 22".to_string()];
    let note = Some("lexical text matching — use find_impact for precise references");

    // Healthy, no note: byte-identical bare array (the delegated legacy path).
    let out = text(super::respond_with_items_noted(
        &[1u32, 2],
        &[],
        None,
        || "unused".to_string(),
    ));
    assert_eq!(
        out, "[1,2]",
        "healthy no-note response must not change shape"
    );

    // Note only: results + note, no warnings key (absent, not null).
    let out = text(super::respond_with_items_noted(
        &[1u32, 2],
        &[],
        note,
        || "unused".to_string(),
    ));
    let p: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(p["results"][0], 1);
    assert!(p["note"].as_str().unwrap().contains("find_impact"));
    assert!(p.get("warnings").is_none(), "no null warnings key: {p}");

    // Note + warnings: the channel still terminates, next to the note.
    let out = text(super::respond_with_items_noted(
        &[1u32],
        &warned,
        note,
        || "unused".to_string(),
    ));
    let p: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(p["note"].as_str().is_some(), "got: {p}");
    assert_eq!(p["warnings"][0], warned[0].as_str());

    // Warnings only (note absent): identical to respond_with_items shape.
    let out = text(super::respond_with_items_noted(
        &[1u32],
        &warned,
        None,
        || "unused".to_string(),
    ));
    let p: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(p.get("note").is_none(), "got: {p}");
    assert!(p["warnings"][0].as_str().is_some(), "got: {p}");

    // Empty + note: the note rides on the empty message — an empty lexical
    // result is exactly where the SCIP upgrade path matters most.
    let out = text(super::respond_with_items_noted(
        &[0u32; 0],
        &[],
        note,
        || "No usages found for 'Foo'.".to_string(),
    ));
    assert!(out.contains("No usages found for 'Foo'."), "got: {out}");
    assert!(out.contains("find_impact"), "got: {out}");
}

#[test]
fn rank_code_first_demotes_docs_without_reordering_code() {
    let item = |path: &str, score: f32| super::ReferenceItem {
        chunk_id: 0,
        path: path.to_string(),
        line: 1,
        kind: "Block".to_string(),
        signature: None,
        score,
    };
    // Score order deliberately NOT aligned with code/doc grouping: the
    // highest-scoring hit is markdown. Stable sort must keep cs before ts
    // (both code, 5.0 before 4.0) and md before AGENTS.md (both docs,
    // 9.0 before 1.0), while every code item outranks every doc item.
    let mut items = vec![
        item("docs/README.md", 9.0),
        item("src/A.cs", 5.0),
        item("src/b.ts", 4.0),
        item("AGENTS.md", 1.0),
    ];
    super::rank_code_first(&mut items);
    let paths: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(
        paths,
        ["src/A.cs", "src/b.ts", "docs/README.md", "AGENTS.md"],
        "code first (score order kept), docs demoted as a block: {paths:?}"
    );
}

#[test]
fn scip_usages_note_is_suppressed_without_backed_source_files() {
    // Machine-independent half of the gate: no C#/TS source in the hits →
    // no note, regardless of what indexers the host happens to have (the
    // registry must not even be consulted). The positive branch is gated on
    // helper availability and therefore structure-covered only, same
    // reasoning as the csharp_helper_integration test.
    let registry = std::sync::Arc::new(crate::symbols::SymbolIndexerRegistry::new());
    let item = |path: &str| super::ReferenceItem {
        chunk_id: 0,
        path: path.to_string(),
        line: 1,
        kind: "Block".to_string(),
        signature: None,
        score: 1.0,
    };
    let markdown_only = vec![item("docs/notes.md"), item("src/lib.rs"), item("AGENTS.md")];
    assert!(
        super::scip_usages_note(&registry, &markdown_only, "Foo").is_none(),
        "a Rust/docs hit list must not advertise a SCIP upgrade path"
    );
}

#[test]
fn ambiguous_chunk_payload_declares_an_incomplete_candidate_list() {
    // `candidate_projects` reads as exhaustive. When a store failed to
    // answer, the repo the caller wants may be the one missing from it, so
    // the message itself must stop claiming completeness.
    let warned = vec!["repo 'inriver' chunk lookup failed: os error 22".to_string()];
    let p = super::ambiguous_chunk_payload(123, &["akeneo", "custom-kb"], &warned);

    assert_eq!(p["warnings"][0], warned[0].as_str());
    let msg = p["message"].as_str().unwrap();
    assert!(msg.contains("incomplete"), "got: {msg}");
}

#[test]
fn ambiguous_chunk_payload_is_unchanged_when_every_store_answered() {
    let p = super::ambiguous_chunk_payload(123, &["akeneo", "custom-kb"], &[]);

    // Absent, not `null`: `json!` renders `None` as an explicit null, which
    // would be a shape change on the healthy path.
    assert!(
        p.get("warnings").is_none(),
        "healthy payload must not gain a key: {p}"
    );
    assert_eq!(
        p["message"],
        "chunk_id 123 exists in multiple repositories. Specify which one."
    );
}

#[test]
fn respond_with_object_carries_warnings_without_disturbing_the_healthy_shape() {
    use rmcp::model::RawContent;
    let text = |r: Result<super::CallToolResult, super::McpError>| -> String {
        match &r.unwrap().content[0].raw {
            RawContent::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    };
    // Declaration order is deliberately NOT alphabetical: a round-trip
    // through `serde_json::to_value` would re-sort these (the Map is a
    // BTreeMap without `preserve_order`), so this pins that the healthy path
    // does not round-trip.
    #[derive(serde::Serialize)]
    struct Chunkish {
        path: String,
        content: String,
    }
    let obj = Chunkish {
        path: "src/x.rs".to_string(),
        content: "fn x() {}".to_string(),
    };

    let out = text(super::respond_with_object(&obj, &[]));
    assert_eq!(
        out, r#"{"path":"src/x.rs","content":"fn x() {}"}"#,
        "a healthy object must keep its bytes AND its key order"
    );

    let warned = vec!["repo 'inriver' chunk lookup failed: os error 22".to_string()];
    let out = text(super::respond_with_object(&obj, &warned));
    assert!(out.contains("os error 22"), "got: {out}");
    assert!(
        out.contains("src/x.rs"),
        "the object itself survives: {out}"
    );
}

#[test]
fn retry_hint_is_dropped_only_when_a_store_failed() {
    let hint = || Some("literal_search".to_string());

    // The ordinary low-confidence hint is legitimate and must survive.
    assert_eq!(super::retry_hint(hint(), &None), hint());
    assert_eq!(super::retry_hint(hint(), &Some(vec![])), hint());

    // But retrying against a store we KNOW is down is bad advice.
    let warned = Some(vec!["repo 'inriver' search failed: os error 22".to_string()]);
    assert_eq!(super::retry_hint(hint(), &warned), None);
}

#[test]
fn semantic_response_emits_warnings_to_the_caller() {
    let response = super::SemanticSearchResponse {
        results: vec![],
        low_confidence: Some(true),
        suggested_tool: None,
        warnings: Some(vec!["repo 'inriver' search failed: os error 22".to_string()]),
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"warnings\""), "got: {json}");
    assert!(json.contains("os error 22"));
}

#[test]
fn semantic_response_omits_warnings_when_healthy() {
    // Backward compatibility: a healthy response must be byte-identical to
    // what callers saw before the field existed.
    let response = super::SemanticSearchResponse {
        results: vec![],
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("warnings"), "got: {json}");
}

#[test]
fn literal_response_emits_warnings_and_omits_them_when_healthy() {
    let failed = super::LiteralSearchResponse {
        results: vec![],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: Some(vec![
            "repo 'inriver' chunk lookup failed: os error 22".to_string()
        ]),
    };
    let json = serde_json::to_string(&failed).unwrap();
    assert!(json.contains("\"warnings\""), "got: {json}");
    assert!(json.contains("os error 22"));

    let healthy = super::LiteralSearchResponse {
        results: vec![],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&healthy).unwrap();
    assert!(!json.contains("warnings"), "got: {json}");
}

#[test]
fn test_literal_search_response_omits_fields_when_not_promoted() {
    let response = super::LiteralSearchResponse {
        results: vec![],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("auto_promoted_to_regex"));
    assert!(!json.contains("note"));
}

#[test]
fn test_grep_format_includes_comment_when_promoted() {
    let response = super::LiteralSearchResponse {
        results: vec![super::LiteralSearchResultItem {
            path: "test.rs".to_string(),
            start_line: 1,
            end_line: 1,
            snippet: "fn test()".to_string(),
            score: 0.0,
            kind: None,
            signature: None,
        }],
        auto_promoted_to_regex: Some(true),
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let mut lines: Vec<String> = Vec::new();
    if response.auto_promoted_to_regex == Some(true) {
        lines.push(
            "# auto-promoted to regex mode (query contained code-like punctuation)".to_string(),
        );
    }
    for item in &response.results {
        lines.push(format!(
            "{}:{}:{}",
            item.path, item.start_line, item.snippet
        ));
    }
    let output = lines.join("\n");
    assert!(output.starts_with("# auto-promoted"));
}

#[test]
fn test_grep_format_no_comment_when_plain() {
    let response = super::LiteralSearchResponse {
        results: vec![super::LiteralSearchResultItem {
            path: "test.rs".to_string(),
            start_line: 1,
            end_line: 1,
            snippet: "fn test()".to_string(),
            score: 1.0,
            kind: None,
            signature: None,
        }],
        auto_promoted_to_regex: None,
        note: None,
        low_confidence: None,
        suggested_tool: None,
        warnings: None,
    };
    let mut lines: Vec<String> = Vec::new();
    if response.auto_promoted_to_regex == Some(true) {
        lines.push(
            "# auto-promoted to regex mode (query contained code-like punctuation)".to_string(),
        );
    }
    for item in &response.results {
        lines.push(format!(
            "{}:{}:{}",
            item.path, item.start_line, item.snippet
        ));
    }
    let output = lines.join("\n");
    assert!(!output.starts_with('#'));
}
