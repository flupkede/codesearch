//! MCP (Model Context Protocol) server for Claude Code integration
//!
//! Exposes codesearch's semantic search capabilities via the MCP protocol,
//! allowing AI assistants like Claude to search codebases during conversations.
//!
//! # Important: No Stdout Output
//!
//! The MCP module MUST NOT use `print!` or `println!` macros anywhere in its code.
//! All non-JSON output must go to stderr via `info_print!`, `warn_print!`, or `eprintln!`.
//! This is critical because the MCP protocol communicates over stdout via JSON-RPC,
//! and any stdout pollution will break the protocol.

#[cfg(test)]
mod tests {
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
            chunk_id: 1,
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
    fn test_is_definition_chunk_rust_function() {
        assert!(super::is_definition_chunk(
            "Function",
            &Some("fn authenticate(".to_string()),
            "authenticate"
        ));
        assert!(super::is_definition_chunk(
            "Function",
            &Some("pub fn CodesearchService".to_string()),
            "CodesearchService"
        ));
        assert!(super::is_definition_chunk(
            "Function",
            &Some("pub async fn handle_request".to_string()),
            "handle_request"
        ));
    }

    #[test]
    fn test_is_definition_chunk_rust_struct() {
        assert!(super::is_definition_chunk(
            "Struct",
            &Some("pub struct CodesearchService".to_string()),
            "CodesearchService"
        ));
        assert!(super::is_definition_chunk(
            "Struct",
            &Some("struct SearchResult".to_string()),
            "SearchResult"
        ));
    }

    #[test]
    fn test_is_definition_chunk_rust_trait() {
        assert!(super::is_definition_chunk(
            "Trait",
            &Some("pub trait Searchable".to_string()),
            "Searchable"
        ));
    }

    #[test]
    fn test_is_definition_chunk_rust_enum() {
        assert!(super::is_definition_chunk(
            "Enum",
            &Some("pub enum ModelType".to_string()),
            "ModelType"
        ));
    }

    #[test]
    fn test_is_definition_chunk_non_definition_kind() {
        // A Comment or Import kind should never be treated as a definition
        assert!(!super::is_definition_chunk(
            "Comment",
            &Some("fn authenticate(".to_string()),
            "authenticate"
        ));
        assert!(!super::is_definition_chunk(
            "Import",
            &Some("use authenticate".to_string()),
            "authenticate"
        ));
    }

    #[test]
    fn test_is_definition_chunk_usage_not_definition() {
        // A function chunk where the signature mentions the symbol but isn't its definition
        // should NOT be filtered out
        assert!(!super::is_definition_chunk(
            "Function",
            &Some("fn handle_request".to_string()),
            "authenticate"
        ));
    }

    #[test]
    fn test_is_definition_chunk_no_signature() {
        // No signature = can't determine if it's a definition
        assert!(!super::is_definition_chunk(
            "Function",
            &None,
            "authenticate"
        ));
        assert!(!super::is_definition_chunk(
            "Function",
            &Some(String::new()),
            "authenticate"
        ));
    }

    #[test]
    fn test_is_definition_chunk_python() {
        assert!(super::is_definition_chunk(
            "Function",
            &Some("def authenticate(".to_string()),
            "authenticate"
        ));
        assert!(super::is_definition_chunk(
            "Class",
            &Some("class UserService".to_string()),
            "UserService"
        ));
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
                chunk_id: 1,
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
        let result =
            super::prefix_path_with_alias(r"C:\repo\src\main.rs", Some("myrepo"), r"C:\repo");
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
        let result =
            super::prefix_path_with_alias(r"C:\repo/src\main.rs", Some("myrepo"), r"C:\repo");
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
        let result =
            super::prefix_path_with_alias("C:/other/src/main.rs", Some("myrepo"), "C:/repo");
        // Path doesn't start with root — returned normalized, no alias prefix
        assert_eq!(result, "C:/other/src/main.rs");
    }

    #[test]
    fn test_group_results_are_alias_prefixed() {
        // Simulate two stores for aliases "a" and "b", each returning a result
        // with absolute path = "/abs/root/src/main.rs". After applying prefix_path_with_alias,
        // assert results have path = "a/src/main.rs" and "b/src/main.rs".
        let result_a =
            super::prefix_path_with_alias("/abs/root/src/main.rs", Some("a"), "/abs/root");
        let result_b =
            super::prefix_path_with_alias("/abs/root/src/main.rs", Some("b"), "/abs/root");
        assert_eq!(result_a, "a/src/main.rs");
        assert_eq!(result_b, "b/src/main.rs");
    }

    #[test]
    fn test_single_project_result_is_alias_prefixed() {
        // Single store for alias "myrepo", result with path = "/abs/root/src/lib.rs",
        // project root "/abs/root" → assert path becomes "myrepo/src/lib.rs".
        let result =
            super::prefix_path_with_alias("/abs/root/src/lib.rs", Some("myrepo"), "/abs/root");
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
    fn test_simple_glob_match_exact() {
        assert!(super::simple_glob_match("src/main.rs", "src/main.rs"));
        assert!(!super::simple_glob_match("src/main.rs", "src/other.rs"));
    }

    #[test]
    fn test_simple_glob_match_double_star_prefix() {
        assert!(super::simple_glob_match("src/mcp/**", "src/mcp/mod.rs"));
        assert!(super::simple_glob_match("src/mcp/**", "src/mcp/types.rs"));
        assert!(super::simple_glob_match(
            "src/mcp/**",
            "src/mcp/sub/deep.rs"
        ));
        assert!(!super::simple_glob_match("src/mcp/**", "src/other/mod.rs"));
    }

    #[test]
    fn test_simple_glob_match_double_star_suffix() {
        assert!(super::simple_glob_match("**/*.rs", "src/main.rs"));
        assert!(super::simple_glob_match("**/*.rs", "deep/nested/file.rs"));
        assert!(!super::simple_glob_match("**/*.rs", "src/main.ts"));
    }

    #[test]
    fn test_simple_glob_match_double_star_both() {
        assert!(super::simple_glob_match("src/**/*.rs", "src/main.rs"));
        assert!(super::simple_glob_match("src/**/*.rs", "src/mcp/mod.rs"));
        assert!(!super::simple_glob_match("src/**/*.rs", "tests/main.rs"));
        assert!(!super::simple_glob_match("src/**/*.rs", "src/main.ts"));
    }

    #[test]
    fn test_simple_glob_match_single_star() {
        assert!(super::simple_glob_match("*.rs", "main.rs"));
        assert!(!super::simple_glob_match("*.rs", "main.ts"));
        assert!(super::simple_glob_match("src/*.rs", "src/main.rs"));
        assert!(!super::simple_glob_match("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn test_simple_glob_match_backslash_normalization() {
        assert!(super::simple_glob_match("src/mcp/**", r"src\mcp\mod.rs"));
        assert!(super::simple_glob_match(r"src\mcp\**", "src/mcp/mod.rs"));
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
        let (lc, tool) =
            super::compute_low_confidence(Some(super::LOW_CONFIDENCE_THRESHOLD), false);
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

    // === Extended is_definition_chunk tests ===

    #[test]
    fn test_is_definition_chunk_impl_block() {
        // impl blocks should match
        assert!(super::is_definition_chunk(
            "Struct",
            &Some("impl CodesearchService".to_string()),
            "CodesearchService"
        ));
    }

    #[test]
    fn test_is_definition_chunk_const() {
        assert!(super::is_definition_chunk(
            "Function",
            &Some("const MAX_SIZE".to_string()),
            "MAX_SIZE"
        ));
        assert!(super::is_definition_chunk(
            "Function",
            &Some("static INSTANCE".to_string()),
            "INSTANCE"
        ));
    }

    #[test]
    fn test_is_definition_chunk_type_alias() {
        assert!(super::is_definition_chunk(
            "TypeAlias",
            &Some("type Result".to_string()),
            "Result"
        ));
        assert!(super::is_definition_chunk(
            "TypeAlias",
            &Some("pub type Error".to_string()),
            "Error"
        ));
    }

    #[test]
    fn test_is_definition_chunk_interface() {
        assert!(super::is_definition_chunk(
            "Interface",
            &Some("interface Searchable".to_string()),
            "Searchable"
        ));
    }

    #[test]
    fn test_is_definition_chunk_with_generics() {
        // fn with generics — symbol is just the name before <
        assert!(super::is_definition_chunk(
            "Function",
            &Some("fn parse<T>".to_string()),
            "parse"
        ));
        assert!(super::is_definition_chunk(
            "Struct",
            &Some("struct HashMap<K, V>".to_string()),
            "HashMap"
        ));
    }

    #[test]
    fn test_is_definition_chunk_with_colon() {
        // trait with colon (Rust trait bounds)
        assert!(super::is_definition_chunk(
            "Trait",
            &Some("trait AsRef<T>:".to_string()),
            "AsRef"
        ));
    }

    #[test]
    fn test_is_definition_chunk_wrong_symbol() {
        // Correct prefix but symbol name doesn't follow
        assert!(!super::is_definition_chunk(
            "Function",
            &Some("fn authenticate".to_string()),
            "authorize" // different symbol
        ));
    }

    #[test]
    fn test_is_definition_chunk_symbol_as_prefix_of_other() {
        // Symbol is a prefix of the actual name — should NOT match
        assert!(!super::is_definition_chunk(
            "Function",
            &Some("fn authenticate_user".to_string()),
            "authenticate" // missing boundary check
        ));
    }

    #[test]
    fn test_is_definition_chunk_method() {
        assert!(super::is_definition_chunk(
            "Method",
            &Some("fn search".to_string()),
            "search"
        ));
        assert!(super::is_definition_chunk(
            "Method",
            &Some("pub async fn handle".to_string()),
            "handle"
        ));
    }

    #[test]
    fn test_is_definition_chunk_all_kinds() {
        // Verify all DEFINITION_KINDS are recognized
        let test_cases = [
            ("Function", "fn foo(", "foo"),
            ("Class", "class Bar", "Bar"),
            ("Method", "fn baz(", "baz"),
            ("Struct", "struct Qux", "Qux"),
            ("Trait", "trait Quux", "Quux"),
            ("Enum", "enum Corge", "Corge"),
            ("TypeAlias", "type Grault", "Grault"),
            ("Interface", "interface Garply", "Garply"),
        ];
        for (kind, sig, symbol) in &test_cases {
            assert!(
                super::is_definition_chunk(kind, &Some(sig.to_string()), symbol),
                "is_definition_chunk({kind}, {sig}, {symbol}) should be true"
            );
        }
    }

    // === Extended simple_glob_match tests ===

    #[test]
    fn test_glob_exact_match_no_star() {
        assert!(super::simple_glob_match("src/main.rs", "src/main.rs"));
        assert!(!super::simple_glob_match("src/main.rs", "src/other.rs"));
        assert!(!super::simple_glob_match("src/main.rs", "src/main.rs.bak"));
    }

    #[test]
    fn test_glob_double_star_prefix_empty() {
        // ** at start matches any prefix
        assert!(super::simple_glob_match("**/test.rs", "test.rs"));
        assert!(super::simple_glob_match("**/test.rs", "src/test.rs"));
        assert!(super::simple_glob_match("**/test.rs", "a/b/c/test.rs"));
    }

    #[test]
    fn test_glob_double_star_suffix_empty() {
        // ** at end matches any suffix
        assert!(super::simple_glob_match("src/**", "src/"));
        assert!(super::simple_glob_match("src/**", "src/foo"));
        assert!(super::simple_glob_match("src/**", "src/a/b/c"));
    }

    #[test]
    fn test_glob_both_double_stars() {
        assert!(super::simple_glob_match("**/**", "anything"));
        assert!(super::simple_glob_match("**/**", "a/b/c"));
    }

    #[test]
    fn test_glob_nested_double_star() {
        // src/**/*.rs — must have src/ prefix and .rs extension
        assert!(super::simple_glob_match("src/**/*.rs", "src/lib.rs"));
        assert!(super::simple_glob_match("src/**/*.rs", "src/mcp/mod.rs"));
        assert!(super::simple_glob_match("src/**/*.rs", "src/a/b/c/d.rs"));
        assert!(!super::simple_glob_match("src/**/*.rs", "test/lib.rs"));
        assert!(!super::simple_glob_match("src/**/*.rs", "src/lib.ts"));
    }

    #[test]
    fn test_glob_single_star_multiple() {
        // Multiple single stars in pattern
        assert!(super::simple_glob_match("test_*.rs", "test_foo.rs"));
        assert!(!super::simple_glob_match("test_*.rs", "test_foo.ts"));
    }

    #[test]
    fn test_glob_single_star_stays_in_segment() {
        // * should NOT cross /
        assert!(!super::simple_glob_match("*.rs", "src/main.rs"));
        assert!(!super::simple_glob_match("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn test_glob_empty_pattern() {
        assert!(super::simple_glob_match("", ""));
        assert!(!super::simple_glob_match("", "foo.rs"));
    }

    #[test]
    fn test_glob_trailing_slash_in_prefix() {
        // src/mcp/** with trailing slash in path
        assert!(super::simple_glob_match("src/mcp/**", "src/mcp/mod.rs"));
    }

    #[test]
    fn test_glob_double_star_middle() {
        // Pattern: src/**/test.rs
        assert!(super::simple_glob_match("src/**/test.rs", "src/test.rs"));
        assert!(super::simple_glob_match("src/**/test.rs", "src/a/test.rs"));
        assert!(super::simple_glob_match(
            "src/**/test.rs",
            "src/a/b/c/test.rs"
        ));
        assert!(!super::simple_glob_match(
            "src/**/test.rs",
            "src/a/other.rs"
        ));
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
                chunk_id: 1,
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

        for (alias, results) in
            aliases
                .iter()
                .zip([&store1_results, &store2_results, &store3_results])
        {
            for r in results {
                let key = (alias.to_string(), super::HasChunkId::chunk_id(r));
                if let Some(&existing_idx) = seen_ids.get(&key) {
                    if super::HasScore::score(r)
                        > super::HasScore::score(&all_results[existing_idx])
                    {
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
                    if super::HasScore::score(r)
                        > super::HasScore::score(&all_results[existing_idx])
                    {
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
                    if super::HasScore::score(r)
                        > super::HasScore::score(&all_results[existing_idx])
                    {
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
        let (stores, stores_vec, is_multi, needs_local_db) =
            decompose_routing_ctx::<i32>(Some(vec![]));
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
    fn test_regex_has_anchorable_token_plain_identifier() {
        assert!(super::regex_has_anchorable_token("match_line_for_literal"));
    }

    #[test]
    fn test_regex_has_anchorable_token_generic_with_word() {
        assert!(super::regex_has_anchorable_token("Vec<.*>"));
        assert!(super::regex_has_anchorable_token("HashMap::new"));
    }

    #[test]
    fn test_regex_has_anchorable_token_short_word_below_threshold() {
        // "fn" alone is only 2 chars — not enough.
        assert!(!super::regex_has_anchorable_token("fn"));
        assert!(super::regex_has_anchorable_token("fnx")); // 3 chars triggers
    }

    #[test]
    fn test_regex_has_anchorable_token_word_boundary_pattern() {
        assert!(!super::regex_has_anchorable_token(r"\bfn\s+\w+"));
        assert!(!super::regex_has_anchorable_token(r"\bimpl\s+"));
    }

    #[test]
    fn test_regex_has_anchorable_token_method_call_pattern() {
        assert!(!super::regex_has_anchorable_token(r"\.\w+\(\)"));
    }

    #[test]
    fn test_regex_has_anchorable_token_character_classes_dont_count() {
        // [A-Z] and [a-z] inside brackets must NOT be counted as runs.
        assert!(!super::regex_has_anchorable_token(r"[A-Z]+_[A-Z]+"));
        assert!(!super::regex_has_anchorable_token(r"^[A-Z]\w+"));
    }

    #[test]
    fn test_regex_has_anchorable_token_empty() {
        assert!(!super::regex_has_anchorable_token(""));
    }

    #[test]
    fn test_regex_has_anchorable_token_pure_punctuation() {
        assert!(!super::regex_has_anchorable_token(r"->"));
        assert!(!super::regex_has_anchorable_token(r"::"));
    }

    // ─── Scan-path decision logic tests ──────────────────────────────────
    //
    // Full integration tests for literal_search require a CodesearchService
    // with a working DB/FTS index — no such harness exists yet. These tests
    // validate the critical decision logic: which queries take the BM25 path
    // vs the scan path.

    #[test]
    fn test_regex_anchorable_queries_detected_correctly() {
        // Queries with ≥3 alphanumeric runs → anchorable → BM25 path
        assert!(super::regex_has_anchorable_token("match_line_for_literal"));
        assert!(super::regex_has_anchorable_token("HashMap::new"));
        assert!(super::regex_has_anchorable_token("Vec<.*>"));
        assert!(super::regex_has_anchorable_token("fnx"));
    }

    #[test]
    fn test_regex_tokenless_queries_detected_correctly() {
        // Tokenless regex patterns → not anchorable → scan path
        assert!(!super::regex_has_anchorable_token(r"\bfn\s+\w+"));
        assert!(!super::regex_has_anchorable_token(r"\bimpl\s+"));
        assert!(!super::regex_has_anchorable_token(r"\.\w+\(\)"));
        assert!(!super::regex_has_anchorable_token(r"[A-Z]+_[A-Z]+"));
        assert!(!super::regex_has_anchorable_token(r"^[A-Z]\w+"));
    }

    // ─── Trailing-escape detector tests ──────────────────────────────

    #[test]
    fn test_regex_has_anchorable_token_trailing_word_boundary() {
        assert!(!super::regex_has_anchorable_token(r"impl\b"));
        assert!(!super::regex_has_anchorable_token(r"Result\b"));
        assert!(!super::regex_has_anchorable_token(r"match\b"));
    }

    #[test]
    fn test_regex_has_anchorable_token_trailing_class() {
        assert!(!super::regex_has_anchorable_token(r"impl[A-Z]"));
        assert!(!super::regex_has_anchorable_token(r"foo[abc]+"));
    }

    #[test]
    fn test_regex_has_anchorable_token_trailing_escape_with_clean_run_after() {
        // After the merged trailing escape, if there's a clean run later, that
        // later run can still anchor.
        assert!(super::regex_has_anchorable_token(r"impl\b\s+function_name"));
        //                                              ^^^^^^^^^^^^^ anchorable
    }

    #[test]
    fn test_regex_has_anchorable_token_trailing_escape_at_end_only() {
        // Run, then escape, then EOF — not anchorable.
        assert!(!super::regex_has_anchorable_token(r"impl\s"));
    }

    #[test]
    fn test_regex_has_anchorable_token_both_sides_escaped() {
        // \bimpl\b — leading escape already disqualifies "impl"; trailing
        // doesn't change the answer.
        assert!(!super::regex_has_anchorable_token(r"\bimpl\b"));
    }

    // ── regex_has_disjunctive_or tests ──────────────────────────────

    #[test]
    fn test_disjunctive_or_simple_alternation() {
        assert!(super::regex_has_disjunctive_or("TODO|FIXME|HACK"));
    }

    #[test]
    fn test_disjunctive_or_two_alternatives() {
        assert!(super::regex_has_disjunctive_or("foo|bar"));
    }

    #[test]
    fn test_disjunctive_or_pipe_inside_group_not_counted() {
        // (foo|bar) is inside parens — not top-level
        assert!(!super::regex_has_disjunctive_or("(foo|bar)"));
    }

    #[test]
    fn test_disjunctive_or_pipe_inside_bracket_not_counted() {
        // [|] is inside character class
        assert!(!super::regex_has_disjunctive_or("[a|b]"));
    }

    #[test]
    fn test_disjunctive_or_escaped_pipe_not_counted() {
        assert!(!super::regex_has_disjunctive_or(r"foo\|bar"));
    }

    #[test]
    fn test_disjunctive_or_no_pipe() {
        assert!(!super::regex_has_disjunctive_or("TODO"));
    }

    #[test]
    fn test_disjunctive_or_mixed_top_level_and_group() {
        // foo|(bar|baz) — the first | is top-level
        assert!(super::regex_has_disjunctive_or("foo|(bar|baz)"));
    }

    #[test]
    fn test_disjunctive_or_nested_groups() {
        // ((a|b)) — pipe inside double parens
        assert!(!super::regex_has_disjunctive_or("((a|b))"));
    }

    #[test]
    fn test_disjunctive_or_mixed_top_level_and_bracket() {
        // [a-z]|foo — pipe after bracket is top-level
        assert!(super::regex_has_disjunctive_or("[a-z]|foo"));
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
        assert!(super::match_line_for_literal(
            content2,
            "zzz_definitely_not_in_code",
            Some(&regex2)
        )
        .is_none());

        // Non-anchorable regex with no matches → empty (scan path would skip)
        let regex3 = regex::Regex::new(r"\bimpl\s+\w+\s+for\s+\w+").unwrap();
        let content3 = "fn simple() {}\nstruct Foo;";
        assert!(super::match_line_for_literal(
            content3,
            r"\bimpl\s+\w+\s+for\s+\w+",
            Some(&regex3)
        )
        .is_none());
    }

    // ─── looks_like_code_pattern detector tests ───────────────────────

    #[test]
    fn test_looks_like_code_pattern_assignment() {
        assert!(super::looks_like_code_pattern("foo = null"));
        assert!(super::looks_like_code_pattern("x = 42"));
    }

    #[test]
    fn test_looks_like_code_pattern_arrow() {
        assert!(super::looks_like_code_pattern("foo->bar"));
        assert!(super::looks_like_code_pattern("x => y"));
    }

    #[test]
    fn test_looks_like_code_pattern_namespace() {
        assert!(super::looks_like_code_pattern("std::string"));
        assert!(super::looks_like_code_pattern("a::b::c"));
    }

    #[test]
    fn test_looks_like_code_pattern_generics() {
        assert!(super::looks_like_code_pattern("Vec<T>"));
        assert!(super::looks_like_code_pattern("HashMap<K, V>"));
    }

    #[test]
    fn test_looks_like_code_pattern_statement_end() {
        assert!(super::looks_like_code_pattern("return x;"));
        assert!(super::looks_like_code_pattern("if (x) {"));
    }

    #[test]
    fn test_looks_like_code_pattern_plain_identifier_false() {
        assert!(!super::looks_like_code_pattern(
            "ActivitiesListModelResponse"
        ));
        assert!(!super::looks_like_code_pattern("foo_bar"));
    }

    #[test]
    fn test_looks_like_code_pattern_dotted_path_false() {
        assert!(!super::looks_like_code_pattern("foo.bar"));
        assert!(!super::looks_like_code_pattern("System.Console"));
    }

    #[test]
    fn test_looks_like_code_pattern_empty_false() {
        assert!(!super::looks_like_code_pattern(""));
    }

    // ─── extract_bm25_query_from_regex tests ─────────────────────────

    #[test]
    fn test_extract_bm25_query_from_regex_class_word_cache() {
        // "class \w+Cache\b" → should extract "class Cache"
        assert_eq!(
            super::extract_bm25_query_from_regex("class \\w+Cache\\b"),
            "class Cache"
        );
    }

    #[test]
    fn test_extract_bm25_query_from_regex_interface() {
        // "interface I\w+" → should extract "interface"
        assert_eq!(
            super::extract_bm25_query_from_regex("interface I\\w+"),
            "interface"
        );
    }

    #[test]
    fn test_extract_bm25_query_from_regex_class_word_store() {
        // "class \w+Store\b" → should extract "class Store"
        assert_eq!(
            super::extract_bm25_query_from_regex("class \\w+Store\\b"),
            "class Store"
        );
    }

    #[test]
    fn test_extract_bm25_query_from_regex_plain() {
        // Plain identifier → unchanged
        assert_eq!(
            super::extract_bm25_query_from_regex("CleanupController"),
            "CleanupController"
        );
    }

    #[test]
    fn test_extract_bm25_query_from_regex_all_escapes() {
        // Pure escape classes → empty
        assert_eq!(super::extract_bm25_query_from_regex("\\w+"), "");
    }

    #[test]
    fn test_extract_bm25_query_from_regex_method_call() {
        // "\.MethodName\(" → "MethodName"
        assert_eq!(
            super::extract_bm25_query_from_regex("\\.MethodName\\("),
            "MethodName"
        );
    }

    #[test]
    fn test_extract_bm25_query_from_regex_bracket_class() {
        // "[a-z]+Cache" → "Cache" (bracket class stripped)
        assert_eq!(super::extract_bm25_query_from_regex("[a-z]+Cache"), "Cache");
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
        let (lc, hint) =
            super::compute_literal_low_confidence(Some(weak_score), "CodesearchService");
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
        let (lc, hint) = super::compute_literal_low_confidence(
            Some(super::LITERAL_LOW_CONFIDENCE_BM25),
            "anything",
        );
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
            warnings[0].contains("repo 'inriver' stats failed")
                && warnings[0].contains("os error 30"),
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
}

pub mod types;

/// Resolve the serve base URL from env or default host/port.
fn serve_url_from_env() -> String {
    use crate::constants::resolve_serve_host;
    let host = resolve_serve_host();
    let port = std::env::var(crate::constants::SERVE_PORT_ENV)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(crate::constants::DEFAULT_SERVE_PORT);
    format!("http://{}:{}", host, port)
}

use crate::db_discovery::{find_best_database, load_repos_config};
use crate::embed::{EmbeddingService, ModelType};
use crate::file::Language;
use crate::fts::FtsStore;
use crate::index::{IndexManager, SharedStores};
use crate::rerank::{rrf_fusion, rrf_fusion_with_exact, vector_only, EXACT_MATCH_RRF_K};
use crate::search::{adapt_rrf_k, boost_kind, detect_identifiers, detect_structural_intent};
use crate::symbols::SymbolIndexerRegistry;
use crate::vectordb::VectorStore;
use anyhow::{Context, Result};
use regex::Regex;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleClient, RoleServer, ServerHandler,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// Re-export types
pub use types::*;

// ═══════════════════════════════════════════════════════════════════
// MCP Proxy Service  (--mode client / --mode auto with serve detected)
// ═══════════════════════════════════════════════════════════════════

/// Transparent stdio↔HTTP proxy with automatic reconnect.
///
/// When `codesearch mcp --mode client` is started by Claude Desktop:
/// - Claude Desktop sends MCP requests over stdio
/// - `McpProxyService` forwards every request to the running `codesearch serve` hub via HTTP
/// - Responses flow back unchanged
///
/// This is the correct architecture for Claude Desktop: it has no repo context of its own
/// and therefore cannot use `--mode local`. With `--mode client` it always connects to
/// the serve hub, gaining access to all registered repos.
///
/// Only tool operations (`list_tools`, `call_tool`) are forwarded. Prompts, resources,
/// and completion are not proxied — the serve hub does not expose them.
///
/// ## Reconnect
///
/// The peer is wrapped in `Arc<RwLock<Option<Peer>>>` so it can be hot-swapped when the
/// serve connection drops and reconnects. During reconnection, tool calls return a
/// descriptive "reconnecting" error so Claude Desktop can retry.
///
/// ## Idle disconnect / connect on demand
///
/// The peer is also `None` while the proxy is *deliberately* disconnected: after
/// `CODESEARCH_MCP_PROXY_IDLE_DISCONNECT_SECS` (default
/// `DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS`, `0` disables) without a successful
/// forwarded request, the idle-checker in `run_mcp_client` closes the HTTP MCP
/// session so a scale-to-zero remote can suspend its replica. Every successful
/// `list_tools` / `call_tool` stamps `last_activity`, which resets that window.
///
/// Because a closed session is indistinguishable from a dead one at the peer
/// slot, `call_tool` / `list_tools` signal `connect_request_tx` on their first
/// attempt whenever the slot is `None`, asking the main loop to connect *now*
/// instead of waiting for the failure-path reconnect cadence. The existing
/// bounded retry-with-backoff remains the fallback if that connect does not land
/// within the retry budget.
struct McpProxyService {
    /// Shared peer handle — hot-swapped on reconnect.
    /// `None` means we're reconnecting to serve; tool calls return a retry-able error.
    peer: std::sync::Arc<tokio::sync::RwLock<Option<rmcp::service::Peer<RoleClient>>>>,
    /// Signal to the main loop in `run_mcp_client` that the current peer is dead
    /// and a fresh `connect_to_serve` should be attempted. Sent from `call_tool` /
    /// `list_tools` when rmcp returns a transport-level error so we can recover
    /// from server restarts and TCP keep-alive failures without bubbling the error
    /// up to Claude Desktop.
    disconnect_tx: tokio::sync::mpsc::Sender<()>,
    /// Ask the main loop to run `connect_to_serve` immediately (capacity-1
    /// channel — duplicate requests coalesce, "connect now" is idempotent).
    /// Sent when a request arrives while the peer slot is empty.
    connect_request_tx: tokio::sync::mpsc::Sender<()>,
    /// When the last request was successfully forwarded to serve. Shared with the
    /// idle-checker in `run_mcp_client`, which closes the connection once this is
    /// older than the configured idle-disconnect window.
    last_activity: Arc<Mutex<std::time::Instant>>,
    /// Number of requests currently being forwarded. `last_activity` only advances
    /// on completion, so without this a request that runs longer than the idle
    /// window (a big search, a cold symbol rebuild) would have its own transport
    /// closed underneath it. The idle-checker never disconnects while this is > 0.
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// Notified by the main loop's `connect_request_rx` arm whenever an on-demand
    /// `connect_to_serve` attempt returns `Err` — i.e. serve refused the
    /// connection outright, as opposed to still being slow to accept one. Lets
    /// `await_peer` stop waiting immediately on a definitive failure instead of
    /// polling out the rest of `PROXY_CONNECT_WAIT_MS` (previously ~20s per call
    /// even when serve was known to be down within the first few milliseconds).
    /// A slow-but-eventually-successful wake never touches this: it resolves by
    /// the peer slot filling in, which `await_peer`'s own poll already catches.
    connect_failed: Arc<tokio::sync::Notify>,
}

/// Keeps `McpProxyService::in_flight` incremented for its lifetime. A guard rather
/// than paired add/sub calls because the forwarding loop has several early returns.
struct InFlightGuard(Arc<std::sync::atomic::AtomicUsize>);

impl InFlightGuard {
    fn new(counter: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl McpProxyService {
    #[allow(dead_code)]
    fn new(peer: rmcp::service::Peer<RoleClient>) -> Self {
        // Direct constructor used by tests / single-shot scenarios.
        // No reconnect plumbing — the dummy channels are never read.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (connect_tx, _connect_rx) = tokio::sync::mpsc::channel(1);
        Self {
            peer: std::sync::Arc::new(tokio::sync::RwLock::new(Some(peer))),
            disconnect_tx: tx,
            connect_request_tx: connect_tx,
            last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            connect_failed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Stamp "real traffic just flowed", resetting the idle-disconnect window.
    fn mark_activity(&self) {
        mark_proxy_activity(&self.last_activity);
    }

    /// Best-effort nudge to the main loop: connect to serve now. A full channel
    /// already means "a connect is pending", a closed one means the loop is gone;
    /// both are fine to ignore — the caller's own retry/backoff covers it.
    fn request_connect(&self) {
        let _ = self.connect_request_tx.try_send(());
    }

    /// Wait — bounded by `PROXY_CONNECT_WAIT_MS` — for the peer slot to be filled
    /// after `request_connect`. Returns true as soon as a peer is available.
    ///
    /// Without this, a request arriving after an idle-close would burn its whole
    /// retry budget (~1s) while the on-demand connect is still waking a
    /// scaled-to-zero remote, and fail with "reconnecting" every single time.
    async fn await_peer(&self) -> bool {
        self.await_peer_bounded(PROXY_CONNECT_WAIT_MS).await
    }

    /// Core of `await_peer`, parameterized on the wait budget so it is unit
    /// testable without actually waiting out `PROXY_CONNECT_WAIT_MS` (~20s).
    /// Uses the production refusal-grace window; see
    /// `await_peer_bounded_with_grace` for what that means and why it is its
    /// own parameter.
    async fn await_peer_bounded(&self, wait_ms: u64) -> bool {
        self.await_peer_bounded_with_grace(wait_ms, CONNECT_REFUSAL_GRACE)
            .await
    }

    /// Core of `await_peer_bounded`, additionally parameterized on the
    /// refusal-grace window so *that* is unit testable without waiting out
    /// `reconnect::INTERVAL_SECS` (~3s) for real.
    ///
    /// Polls the peer slot on `PROXY_RETRY_BACKOFF_MS` cadence, but also races
    /// each poll against `connect_failed` so a definitive on-demand connect
    /// failure (serve refused the connection, not merely slow to accept one)
    /// clamps the remaining wait down to `refusal_grace` instead of polling
    /// out the rest of `wait_ms`. A slow-but-still-in-progress wake never
    /// fires `connect_failed` — it is only notified from an `Err` return of
    /// `connect_to_serve` — so this does not shorten the legitimate
    /// scale-to-zero wake path, only the case where serve is already known to
    /// have refused this attempt.
    ///
    /// The clamp is deliberately *not* an immediate return: a refusal only
    /// means this one on-demand attempt was refused, not that serve won't
    /// recover — `run_mcp_client`'s own disconnect/reconnect cycle
    /// (`reconnect::INTERVAL_SECS` later) can still land within the original
    /// budget, e.g. when serve is mid-restart rather than genuinely down.
    /// Returning immediately turned that case — previously transparent to the
    /// caller, since the pre-fix full-budget poll caught the reconnect — into
    /// a visible "reconnecting" error on the very first request after a
    /// restart. Clamping to `refusal_grace` keeps most of the original fix's
    /// win (a hard-down serve is still bounded well under the full `wait_ms`)
    /// while still giving that recovery cycle room to land.
    async fn await_peer_bounded_with_grace(
        &self,
        wait_ms: u64,
        refusal_grace: std::time::Duration,
    ) -> bool {
        let mut deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        loop {
            // Register for the next failure notification BEFORE checking the
            // peer slot, so a failure landing between this check and the
            // `select!` below cannot be missed (the standard tokio::sync::Notify
            // idiom: create the `Notified` future first, await it second).
            let failed = self.connect_failed.notified();
            if self.peer.read().await.is_some() {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let backoff = std::time::Duration::from_millis(PROXY_RETRY_BACKOFF_MS)
                .min(deadline.saturating_duration_since(now));
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = failed => {
                    // A concurrent successful connect could still have landed in
                    // the instant before this notification; one last check keeps
                    // that case correct instead of reporting a false failure.
                    if self.peer.read().await.is_some() {
                        return true;
                    }
                    deadline = deadline.min(now + refusal_grace);
                }
            }
        }
    }

    /// On an empty peer slot (a deliberate idle-close or a real outage), ask the
    /// main loop to connect *now* instead of waiting for the failure-path
    /// reconnect cadence to notice, then give that connect a bounded window to
    /// land. Returns true if a peer became available and the caller should
    /// retry its forwarded call immediately.
    ///
    /// Only meaningful on the caller's first attempt (`attempt == 0`) — a
    /// second empty slot means the on-demand connect already ran and fell
    /// through to the ordinary retry/backoff path. Pulled out of `list_tools`/
    /// `call_tool` because the two copies had already started to drift (see
    /// review remarks on the commit that added this).
    async fn try_on_demand_connect(&self) -> bool {
        self.request_connect();
        self.await_peer().await
    }

    /// Force a reconnect: clear the shared peer and signal the main loop in
    /// `run_mcp_client` to call `connect_to_serve` again. Brief sleep gives
    /// the main loop time to actually reconnect before the caller retries.
    async fn force_reconnect(&self) {
        *self.peer.write().await = None;
        let _ = self.disconnect_tx.send(()).await;
        tokio::time::sleep(std::time::Duration::from_millis(
            crate::mcp::PROXY_RETRY_BACKOFF_MS,
        ))
        .await;
    }
}

/// Maximum number of attempts when forwarding a request to serve.
/// Each retry includes a forced reconnect, so this also bounds reconnect attempts
/// per individual tool call.
const PROXY_MAX_RETRY_ATTEMPTS: u32 = 3;

/// Backoff between proxy retries, also used as the post-reconnect settle delay.
const PROXY_RETRY_BACKOFF_MS: u64 = 500;

/// How long a request may wait for an on-demand connect (after an idle-close, or
/// while serve is still starting) before falling back to the retry/backoff path.
///
/// Sized for a scale-to-zero host: the remote's ingress *holds* the request while
/// it activates a suspended replica, so the connect itself can legitimately take
/// several seconds. Waiting here is strictly better than returning "reconnecting"
/// on the first call after every idle period.
const PROXY_CONNECT_WAIT_MS: u64 = 20_000;

/// How long `await_peer_bounded` still waits after a definitive on-demand
/// connect refusal, instead of returning immediately or polling out the rest
/// of `PROXY_CONNECT_WAIT_MS`.
///
/// Sized to cover `run_mcp_client`'s own disconnect/reconnect cycle
/// (`reconnect::INTERVAL_SECS`, ~3s) plus margin for the ~100ms synthetic-
/// disconnect delay and `connect_to_serve`'s own latency — so a serve that is
/// merely mid-restart still recovers transparently within this window,
/// exactly as it did before the refusal short-circuit existed, while a
/// genuinely-down serve is still bounded well under the full ~20s budget.
const CONNECT_REFUSAL_GRACE: std::time::Duration =
    std::time::Duration::from_millis(reconnect::INTERVAL_SECS * 1_000 + 1_000);

/// Record a definitive on-demand connect refusal: wake any `await_peer_bounded`
/// callers immediately (via `connect_failed`) instead of leaving them to poll
/// out their full budget for a refusal that is already known, then seed a
/// synthetic disconnect so `run_mcp_client`'s own disconnect/reconnect cycle
/// picks it up. A genuinely slow wake never reaches this function — it
/// resolves via the `Ok` branch in the caller once the peer slot fills in —
/// so this does not shorten a legitimate scale-to-zero wake, only a refusal.
///
/// Pulled out of `run_mcp_client`'s `connect_request_rx` arm so the one line
/// that makes `await_peer_bounded`'s refusal short-circuit real in production
/// is covered by a test that calls this function directly, not only by tests
/// that call `connect_failed.notify_waiters()` themselves in isolation —
/// those pin how `await_peer_bounded` *reacts* to a notification, but nothing
/// previously pinned that this call site still *fires* one: deleting this
/// function's body left the full suite green.
fn note_connect_failure(
    connect_failed: &tokio::sync::Notify,
    disconnect_tx: &tokio::sync::mpsc::Sender<()>,
) {
    connect_failed.notify_waiters();
    let tx = disconnect_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = tx.send(()).await;
    });
}

/// Heuristic: does this error message describe a transport-level failure
/// (broken TCP, server gone, stale keep-alive, stale session) that warrants
/// a forced reconnect + retry, as opposed to a real tool-level error that
/// the caller should see?
fn is_transport_error_msg(msg: &str) -> bool {
    msg.contains("Transport send error")
        || msg.contains("error sending request")
        || msg.contains("Transport error")
        || msg.contains("connection closed")
        || msg.contains("error decoding response body")
        || msg.contains("Session not found")
        || msg.contains("404")
}

/// Reconnect-related constants for the MCP proxy.
mod reconnect {
    /// How long to wait between reconnect attempts.
    pub const INTERVAL_SECS: u64 = 3;
    /// Maximum total time to spend trying to reconnect before giving up.
    pub const MAX_DURATION_SECS: u64 = 300; // 5 minutes
}

/// Record the current instant as the proxy's most recent activity.
fn mark_proxy_activity(last_activity: &Arc<Mutex<std::time::Instant>>) {
    if let Ok(mut slot) = last_activity.lock() {
        *slot = std::time::Instant::now();
    }
}

/// Resolve the MCP proxy idle-disconnect window: explicit value → env var →
/// `DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS`. Mirrors how `run_serve` resolves its
/// own `idle_suspend_secs`. `0` means "never idle-disconnect".
fn resolve_proxy_idle_disconnect_secs(explicit: Option<u64>) -> u64 {
    explicit
        .or_else(|| {
            std::env::var(crate::constants::MCP_PROXY_IDLE_DISCONNECT_SECS_ENV)
                .ok()
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(crate::constants::DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS)
}

/// Has the proxy been idle long enough to close its connection to serve?
///
/// `threshold_secs == 0` disables idle-disconnect, so this always returns false.
/// `now` is a parameter (rather than read from the clock) purely so this is unit
/// testable without sleeping.
fn is_idle(
    last_activity: std::time::Instant,
    threshold_secs: u64,
    now: std::time::Instant,
) -> bool {
    if threshold_secs == 0 {
        return false;
    }
    now.saturating_duration_since(last_activity).as_secs() >= threshold_secs
}

#[cfg(test)]
mod proxy_idle_tests {
    use super::{is_idle, resolve_proxy_idle_disconnect_secs};
    use crate::constants::DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS;
    use std::time::{Duration, Instant};

    #[test]
    fn not_idle_before_the_threshold_elapses() {
        let last = Instant::now();
        let now = last + Duration::from_secs(59);
        assert!(!is_idle(last, 60, now));
    }

    #[test]
    fn idle_once_the_threshold_is_reached() {
        let last = Instant::now();
        assert!(is_idle(last, 60, last + Duration::from_secs(60)));
        assert!(is_idle(last, 60, last + Duration::from_secs(600)));
    }

    #[test]
    fn zero_threshold_disables_idle_disconnect() {
        let last = Instant::now();
        // Even an absurdly long idle period must not trigger a close.
        assert!(!is_idle(last, 0, last + Duration::from_secs(86_400)));
    }

    #[test]
    fn clock_going_backwards_is_not_idle() {
        // saturating_duration_since floors at zero instead of panicking.
        let last = Instant::now() + Duration::from_secs(10);
        assert!(!is_idle(last, 60, Instant::now()));
    }

    #[test]
    fn explicit_value_wins_over_env_and_default() {
        // Explicit takes precedence without consulting the environment, so this
        // stays correct regardless of what other tests set.
        assert_eq!(resolve_proxy_idle_disconnect_secs(Some(5)), 5);
        assert_eq!(resolve_proxy_idle_disconnect_secs(Some(0)), 0);
    }

    #[test]
    fn falls_back_to_the_documented_default() {
        // No explicit value and (in the normal test environment) no env override.
        if std::env::var(crate::constants::MCP_PROXY_IDLE_DISCONNECT_SECS_ENV).is_err() {
            assert_eq!(
                resolve_proxy_idle_disconnect_secs(None),
                DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS
            );
        }
    }
}

/// Unit tests for `await_peer_bounded`'s refusal clamp and the
/// `note_connect_failure` call site that fires it in production, isolated
/// from the full `run_mcp_client` loop by parameterizing the wait budget (and,
/// for the clamp itself, the refusal-grace window) so these run in
/// milliseconds instead of the real `PROXY_CONNECT_WAIT_MS` (~20s) or
/// `reconnect::INTERVAL_SECS` (~3s).
#[cfg(test)]
mod await_peer_tests {
    use super::McpProxyService;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A `McpProxyService` whose peer slot never fills on its own — no reconnect
    /// plumbing behind it, matching the "single-shot" spirit of the existing
    /// `McpProxyService::new` test constructor but with an *empty* peer slot,
    /// which is the case `await_peer_bounded` actually has to wait through.
    fn empty_peer_service() -> McpProxyService {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (connect_tx, _connect_rx) = tokio::sync::mpsc::channel(1);
        McpProxyService {
            peer: Arc::new(tokio::sync::RwLock::new(None)),
            disconnect_tx: tx,
            connect_request_tx: connect_tx,
            last_activity: Arc::new(Mutex::new(Instant::now())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            connect_failed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[tokio::test]
    async fn times_out_when_the_peer_slot_never_fills_and_nothing_is_notified() {
        let svc = empty_peer_service();
        let start = Instant::now();
        let ok = svc.await_peer_bounded(150).await;
        assert!(!ok);
        // Baseline: with no signal at all this genuinely waits out the budget,
        // rather than returning early for some unrelated reason — which is what
        // makes the next test's early return meaningful.
        assert!(
            start.elapsed() >= Duration::from_millis(150),
            "expected the full wait budget to elapse, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn a_connect_failure_notification_clamps_the_wait_to_the_refusal_grace() {
        // Uses `await_peer_bounded_with_grace` directly (not the production
        // `await_peer_bounded`, which hardcodes `CONNECT_REFUSAL_GRACE` at
        // ~4s) so the clamp itself is exercised with a millisecond-scale
        // grace instead of actually waiting out `reconnect::INTERVAL_SECS`.
        let svc = empty_peer_service();
        let connect_failed = svc.connect_failed.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            connect_failed.notify_waiters();
        });

        let start = Instant::now();
        // Budget is 5s, grace is 100ms — if the clamp did not fire, this call
        // would take the full 5s instead of ~20ms (notification) + ~100ms
        // (grace) for the peer slot to (not) fill in.
        let ok = svc
            .await_peer_bounded_with_grace(5_000, Duration::from_millis(100))
            .await;
        let elapsed = start.elapsed();

        assert!(
            !ok,
            "peer slot stayed empty — this was a refusal, not a success"
        );
        assert!(
            elapsed < Duration::from_millis(1_000),
            "expected the failure notification to clamp the 5s wait down near the \
             grace window, took {:?}",
            elapsed
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "expected the clamp to still honor the refusal-grace window rather than \
             returning immediately, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn note_connect_failure_wakes_a_parked_waiter_and_schedules_a_disconnect() {
        // Pins the exact production call site (`run_mcp_client`'s
        // `connect_request_rx` Err arm) rather than re-testing
        // `await_peer_bounded`'s reaction to a hand-fired notification: this
        // is the one line that makes that short-circuit real, and nothing
        // previously covered it — deleting `note_connect_failure`'s body left
        // the whole suite green.
        let connect_failed = Arc::new(tokio::sync::Notify::new());
        let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::channel::<()>(1);

        let waiter_failed = connect_failed.clone();
        let waiter = tokio::spawn(async move {
            waiter_failed.notified().await;
        });
        // Give the spawned task a moment to actually park in `.notified()`
        // before firing, so this proves a live waiter is woken — not merely
        // that a notification lands somewhere.
        tokio::time::sleep(Duration::from_millis(20)).await;

        super::note_connect_failure(&connect_failed, &disconnect_tx);

        tokio::time::timeout(Duration::from_millis(500), waiter)
            .await
            .expect("note_connect_failure did not wake the parked waiter in time")
            .expect("waiter task panicked");

        let got = tokio::time::timeout(Duration::from_millis(500), disconnect_rx.recv())
            .await
            .expect("note_connect_failure did not schedule the synthetic disconnect in time");
        assert!(
            got.is_some(),
            "expected the disconnect channel to receive a message"
        );
    }

    #[tokio::test]
    async fn a_stale_notification_before_anyone_is_waiting_does_not_leak_forward() {
        // Notify::notify_waiters() only wakes tasks already parked in
        // .notified() — it stores no permit for a future waiter (unlike
        // notify_one()). Pinned explicitly because `await_peer_bounded`'s
        // correctness depends on this: a failure from an unrelated, already-
        // finished wait must not falsely short-circuit the next one.
        let svc = empty_peer_service();
        svc.connect_failed.notify_waiters(); // no one is waiting yet

        let start = Instant::now();
        let ok = svc.await_peer_bounded(150).await;
        assert!(!ok);
        assert!(
            start.elapsed() >= Duration::from_millis(150),
            "a pre-existing notification must not shorten a later, unrelated wait, took {:?}",
            start.elapsed()
        );
    }
}

impl ServerHandler for McpProxyService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("codesearch", env!("CARGO_PKG_VERSION"))
                    .with_title("codesearch (serve proxy)"),
            )
            .with_instructions(
                "Proxy to a running codesearch serve hub. All tool calls are forwarded to the hub.",
            )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let _in_flight = InFlightGuard::new(&self.in_flight);
        let mut last_err: Option<String> = None;
        for attempt in 0..PROXY_MAX_RETRY_ATTEMPTS {
            let peer = self.peer.read().await.clone();
            match peer {
                Some(p) => match p.list_tools(request.clone()).await {
                    Ok(r) => {
                        self.mark_activity();
                        return Ok(r);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if !is_transport_error_msg(&msg) || attempt >= PROXY_MAX_RETRY_ATTEMPTS - 1
                        {
                            return Err(McpError::internal_error(msg, None));
                        }
                        tracing::warn!(
                            "list_tools attempt {}/{} failed (transport): {} — forcing reconnect",
                            attempt + 1,
                            PROXY_MAX_RETRY_ATTEMPTS,
                            msg
                        );
                        last_err = Some(msg);
                        self.force_reconnect().await;
                    }
                },
                None => {
                    // Empty peer slot: either a deliberate idle-close or a real
                    // outage. `try_on_demand_connect` asks the main loop to
                    // connect *now* rather than waiting for the failure-path
                    // reconnect cadence to notice, bounded so we still fall back
                    // to the ordinary retry/backoff below if it doesn't land.
                    if attempt == 0 && self.try_on_demand_connect().await {
                        continue;
                    }
                    if attempt < PROXY_MAX_RETRY_ATTEMPTS - 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            PROXY_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }
                    return Err(McpError::internal_error(
                        "codesearch serve is reconnecting — please retry in a moment".to_string(),
                        None,
                    ));
                }
            }
        }
        Err(McpError::internal_error(
            last_err.unwrap_or_else(|| "transport error after retries".to_string()),
            None,
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _cx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let _in_flight = InFlightGuard::new(&self.in_flight);
        let mut last_err: Option<String> = None;
        for attempt in 0..PROXY_MAX_RETRY_ATTEMPTS {
            let peer = self.peer.read().await.clone();
            match peer {
                Some(p) => match p.call_tool(request.clone()).await {
                    Ok(r) => {
                        self.mark_activity();
                        return Ok(r);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if !is_transport_error_msg(&msg) || attempt >= PROXY_MAX_RETRY_ATTEMPTS - 1
                        {
                            return Err(McpError::internal_error(msg, None));
                        }
                        tracing::warn!(
                            "call_tool('{}') attempt {}/{} failed (transport): {} — forcing reconnect",
                            request.name,
                            attempt + 1,
                            PROXY_MAX_RETRY_ATTEMPTS,
                            msg
                        );
                        last_err = Some(msg);
                        self.force_reconnect().await;
                    }
                },
                None => {
                    // Empty peer slot: either a deliberate idle-close or a real
                    // outage. `try_on_demand_connect` asks the main loop to
                    // connect *now* rather than waiting for the failure-path
                    // reconnect cadence to notice, bounded so we still fall back
                    // to the ordinary retry/backoff below if it doesn't land.
                    if attempt == 0 && self.try_on_demand_connect().await {
                        continue;
                    }
                    if attempt < PROXY_MAX_RETRY_ATTEMPTS - 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            PROXY_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }
                    return Err(McpError::internal_error(
                        "codesearch serve is reconnecting — please retry in a moment".to_string(),
                        None,
                    ));
                }
            }
        }
        Err(McpError::internal_error(
            last_err.unwrap_or_else(|| "transport error after retries".to_string()),
            None,
        ))
    }
}

/// Read model short-name and dimensions from a database's `metadata.json`.
/// Returns `(model_name, dimensions)`, defaulting to `("unknown", DEFAULT_EMBEDDING_DIMENSIONS)`.
fn read_model_metadata(db_path: &Path) -> (String, usize) {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let model_name = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let dims = json.get("dimensions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            // If metadata has explicit dimensions, use those; otherwise infer from model name.
            let dims = if dims > 0 {
                dims
            } else {
                ModelType::parse(&model_name)
                    .map(|m| m.dimensions())
                    .unwrap_or(crate::constants::DEFAULT_EMBEDDING_DIMENSIONS)
            };
            return (model_name, dims);
        }
    }
    (
        "unknown".to_string(),
        crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
    )
}

/// Read chunk/file counts from metadata.json (written after each indexing operation).
/// Returns `(total_chunks, total_files)` defaulting to `(0, 0)`.
///
/// When metadata.json reports `total_chunks == 0` but the LMDB database exists,
/// falls back to opening the store read-only and counting live chunks.
/// This catches the case where a metadata writer clobbered the stats fields
/// (see `merge_metadata_atomic` for the definitive fix). The fallback is lazy —
/// only triggered when metadata reports zero — so it does not unnecessarily
/// open databases for repos that already have correct metadata.
fn read_metadata_stats(db_path: &Path) -> (usize, usize) {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let total_chunks = json
                .get("total_chunks")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let total_files = json
                .get("total_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            if total_chunks > 0 {
                return (total_chunks, total_files);
            }

            // Metadata says 0 chunks — try live LMDB count as fallback.
            // This is safe in serve context: `read_metadata_stats` is only called
            // for repos NOT yet opened in SharedStores (opened repos use vs.stats()
            // directly), so no double-open risk.
            if let Some((live_chunks, live_files)) = live_chunk_count(db_path) {
                tracing::info!(
                    "metadata.json reports 0 chunks for {}, but LMDB has {} chunks / {} files — using live count",
                    db_path.display(), live_chunks, live_files
                );
                return (live_chunks, live_files);
            }

            return (total_chunks, total_files);
        }
    }
    (0, 0)
}

/// Open the LMDB read-only and count chunks/files.
/// Returns `None` if the database cannot be opened (missing, corrupt, or
/// already locked by another handle).
///
/// # Safety (LMDB double-open)
///
/// This function is only called when `get_opened_stores(alias)` returned `None`,
/// meaning no `SharedStores` handle exists for this repo. There is a theoretical
/// race window between that check and this `open_readonly` call where another task
/// could open the repo via `get_or_open_stores`. In practice this is safe because:
/// 1. The tokio runtime uses a single thread for non-spawned futures.
/// 2. Even if the race occurs, `open_readonly` returns `Err` (TrackedEnv blocks it),
///    and we return `None` — no crash, no corruption.
fn live_chunk_count(db_path: &Path) -> Option<(usize, usize)> {
    let (model_name, dims) = read_model_metadata(db_path);
    if model_name == "unknown" {
        return None;
    }
    match VectorStore::open_readonly(db_path, dims) {
        Ok(store) => match store.stats() {
            Ok(stats) if stats.total_chunks > 0 => Some((stats.total_chunks, stats.total_files)),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(
                    "live_chunk_count: stats() failed for {}: {}",
                    db_path.display(),
                    e
                );
                None
            }
        },
        Err(e) => {
            tracing::debug!(
                "live_chunk_count: open_readonly failed for {}: {}",
                db_path.display(),
                e
            );
            None
        }
    }
}

/// RRF score threshold below which results are considered low-confidence.
/// When the top result's RRF score falls below this, the response includes
/// a `low_confidence` flag and a `suggested_tool` hint.
const LOW_CONFIDENCE_THRESHOLD: f32 = 0.02;

/// Chunk kinds that represent symbol definitions (not usages/comments/etc.)
const DEFINITION_KINDS: &[&str] = &[
    "Function",
    "Class",
    "Method",
    "Struct",
    "Trait",
    "Enum",
    "TypeAlias",
    "Interface",
];

/// Codesearch MCP service
pub struct CodesearchService {
    #[allow(dead_code)]
    tool_router: ToolRouter<CodesearchService>,
    db_path: PathBuf,
    project_path: PathBuf,
    model_type: ModelType,
    dimensions: usize,
    // Lazily initialized on first search
    embedding_service: Arc<Mutex<Option<EmbeddingService>>>,
    // Shared stores for concurrent access (optional - only set when running with IndexManager)
    shared_stores: Option<Arc<SharedStores>>,
    // Serve-mode state (set when running inside `codesearch serve`)
    serve_state: Option<Arc<crate::serve::ServeState>>,
    // Shared symbol indexer registry — reused across MCP sessions to preserve
    // helper-detection cache. In serve mode, cloned from ServeState; in
    // standalone mode, a locally owned Arc.
    symbol_registry: Arc<SymbolIndexerRegistry>,
    // True ONLY for services created by the serve-mode MCP session factory,
    // which pairs `session_connected()` (on create) with `session_disconnected()`
    // (in Drop). Per-request REST services built via `make_service` leave this
    // false so `Drop` does NOT decrement `active_sessions` — otherwise that
    // AtomicU64 underflows (0 - 1 wraps to u64::MAX) on every REST request,
    // corrupting the `/status` health signal.
    tracks_session: bool,
}

impl std::fmt::Debug for CodesearchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodesearchService")
            .field("db_path", &self.db_path)
            .field("model_type", &self.model_type)
            .field("dimensions", &self.dimensions)
            .field("has_shared_stores", &self.shared_stores.is_some())
            .field("serve_mode", &self.serve_state.is_some())
            .finish()
    }
}

impl Drop for CodesearchService {
    fn drop(&mut self) {
        // Only genuine MCP sessions (created by the serve factory, which calls
        // session_connected() + mark_session_tracked()) balance the counter.
        // Per-request REST services (make_service → new_for_serve) never
        // increment it, so must NOT decrement here — otherwise active_sessions
        // underflows (the AtomicU64 wraps to u64::MAX) on every REST request.
        if self.tracks_session {
            if let Some(ref serve_state) = self.serve_state {
                serve_state.session_disconnected();
            }
        }
    }
}

// === Multi-store fan-out traits ===

/// Trait for types that have a chunk ID (used for deduplication in group fan-out).
trait HasChunkId {
    fn chunk_id(&self) -> u32;
}

/// Trait for types that have a relevance score (used for sorting in group fan-out).
trait HasScore {
    fn score(&self) -> f32;
}

impl HasChunkId for crate::vectordb::SearchResult {
    fn chunk_id(&self) -> u32 {
        self.id
    }
}

impl HasScore for crate::vectordb::SearchResult {
    fn score(&self) -> f32 {
        self.score
    }
}

impl HasChunkId for crate::fts::FtsResult {
    fn chunk_id(&self) -> u32 {
        self.chunk_id
    }
}

impl HasScore for crate::fts::FtsResult {
    fn score(&self) -> f32 {
        self.score
    }
}

// === Simple Glob Matcher ===
// v1: supports prefix/suffix patterns with `*` and `**` only.
/// Merge exact FTS results into the main result set, deduplicating by chunk_id
/// and keeping the max score for duplicates.
///
/// This is the pure logic extracted from `semantic_search_lexical` for testability.
fn merge_exact_into_fts(
    fts_results: &mut Vec<crate::fts::FtsResult>,
    exact: Vec<crate::fts::FtsResult>,
) {
    let mut positions: std::collections::HashMap<u32, usize> = fts_results
        .iter()
        .enumerate()
        .map(|(idx, r)| (r.chunk_id, idx))
        .collect();

    for r in exact {
        if let Some(&existing_idx) = positions.get(&r.chunk_id) {
            fts_results[existing_idx].score = fts_results[existing_idx].score.max(r.score);
        } else {
            positions.insert(r.chunk_id, fts_results.len());
            fts_results.push(r);
        }
    }
}

/// Compute low-confidence signaling based on the top result's score.
///
/// Returns `(low_confidence, suggested_tool)` where both are `None` when
/// confidence is high (score >= threshold).
fn compute_low_confidence(
    top_score: Option<f32>,
    has_identifiers: bool,
) -> (Option<bool>, Option<String>) {
    match top_score {
        Some(score) if score < LOW_CONFIDENCE_THRESHOLD => {
            let suggestion = if has_identifiers {
                "find_definition"
            } else {
                "literal_search"
            };
            (Some(true), Some(suggestion.to_string()))
        }
        Some(_) => (None, None),
        None => (Some(true), Some("literal_search".to_string())),
    }
}

// Full glob syntax deferred to avoid adding new dependencies.

/// Match a file path against a simple glob pattern.
///
/// Supported patterns:
/// - `src/mcp/**` → any path starting with `src/mcp/`
/// - `**/*.rs` → any path ending with `.rs`
/// - `src/**/*.rs` → path starting with `src/` and ending with `.rs`
/// - `*.rs` → any path ending with `.rs` (single `*` within a segment)
/// - `foo.rs` → exact match
fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");

    if !pattern.contains('*') {
        // Exact match
        return path == pattern;
    }

    if pattern.contains("**") {
        // Split on first ** only
        let parts: Vec<&str> = pattern.splitn(2, "**").collect();
        let prefix = parts[0];
        // Strip leading / from suffix since ** already matches the separator
        let suffix = parts
            .get(1)
            .map(|s| s.strip_prefix('/').unwrap_or(s))
            .unwrap_or("");

        let mut p = path.as_str();
        if !prefix.is_empty() && !p.starts_with(prefix) {
            return false;
        }
        if !prefix.is_empty() {
            p = &p[prefix.len()..];
        }
        // Strip leading / from remaining path (since ** can match empty + /)
        if p.starts_with('/') {
            p = &p[1..];
        }
        if suffix.is_empty() {
            return true;
        }
        // The suffix may contain single * — match against the tail of the path.
        // After **, the suffix describes constraints on the end of the path.
        // For `**/*.rs`, the `*.rs` should match the last segment.
        if suffix.contains('*') {
            // Match suffix against the end of the path using segment-aware logic
            return match_suffix_with_star(suffix, p);
        }
        p.ends_with(suffix)
    } else {
        // Pure single-star pattern (no **)
        simple_glob_match_single_star(&pattern, &path)
    }
}

/// Match a suffix pattern (containing `*`) against the end of a path.
/// The `*` matches within a single segment only.
///
/// E.g., suffix `*.rs` matches `src/main.rs` because the last segment `main.rs` ends with `.rs`.
fn match_suffix_with_star(suffix: &str, path: &str) -> bool {
    // Find the segments in the suffix (split by /)
    let suffix_parts: Vec<&str> = suffix.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();

    // The suffix must match the last N segments of the path
    if suffix_parts.len() > path_segments.len() {
        return false;
    }

    let path_tail = &path_segments[path_segments.len() - suffix_parts.len()..];

    for (sp, pp) in suffix_parts.iter().zip(path_tail.iter()) {
        if sp.contains('*') {
            if !single_segment_match(sp, pp) {
                return false;
            }
        } else if *sp != *pp {
            return false;
        }
    }
    true
}

/// Match a single segment pattern against a single segment path part.
/// `*` matches any characters within the segment.
fn single_segment_match(pattern: &str, segment: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut s = segment;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !s.starts_with(part) {
                return false;
            }
            s = &s[part.len()..];
        } else if i == parts.len() - 1 {
            if !s.ends_with(part) {
                return false;
            }
        } else if let Some(pos) = s.find(part) {
            s = &s[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// Match a single-star glob pattern where `*` matches any characters except `/`.
fn simple_glob_match_single_star(pattern: &str, path: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut p = path;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First part must be a prefix
            if !p.starts_with(part) {
                return false;
            }
            p = &p[part.len()..];
        } else if i == parts.len() - 1 {
            // Last part must be a suffix of the CURRENT segment (after *)
            // * does not cross /, so find the end of the current segment
            let seg_end = p.find('/').unwrap_or(p.len());
            let segment = &p[..seg_end];
            if !segment.ends_with(part) {
                return false;
            }
        } else {
            // Middle parts: find within remaining path but NOT across /
            if let Some(pos) = p.find(part) {
                let before = &p[..pos];
                if before.contains('/') {
                    return false;
                }
                p = &p[pos + part.len()..];
            } else {
                return false;
            }
        }
    }
    true
}

fn normalize_tool_path(path: &str, project_root: &Path) -> String {
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    crate::cache::normalize_path_str(resolved.to_string_lossy().as_ref())
}

/// Strip a project-alias prefix from a tool path.
///
/// In serve mode, tools like explore receive `target = "ALIAS/src/foo.rs"` with
/// `project = "ALIAS"`.  The alias prefix must be stripped before calling
/// `chunks_for_file`, which expects a path relative to the project root.
fn strip_alias_prefix(path: &str, alias: Option<&String>) -> String {
    if let Some(a) = alias {
        let prefix = format!("{}/", a);
        match path.strip_prefix(&prefix) {
            Some(rest) => rest.to_string(),
            None => path.to_string(),
        }
    } else {
        path.to_string()
    }
}

/// Prefix a result path with its repo alias for group queries, normalizing
/// Windows backslashes to forward slashes in the process. When `alias` is
/// None or empty, the path is still normalized (useful for stdio mode).
pub(crate) fn prefix_path_with_alias(
    path: &str,
    alias: Option<&str>,
    project_root: &str,
) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    let normalized_root = crate::cache::normalize_path_str(project_root)
        .trim_end_matches('/')
        .to_string();
    match normalized.strip_prefix(&normalized_root) {
        Some(rest) => {
            let relative = rest.trim_start_matches('/');
            match alias {
                Some(a) if !a.is_empty() => format!("{}/{}", a, relative),
                _ => relative.to_string(),
            }
        }
        None => normalized,
    }
}

/// Prefix a result path with the matching repo alias from a set of aliases and their roots.
/// Used by handlers that have alias/root info but not a full `MultiStoreContext`.
fn prefix_path_multi(
    path: &str,
    aliases: &[String],
    alias_roots: &std::collections::HashMap<String, String>,
) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    for alias in aliases {
        if let Some(root) = alias_roots.get(alias) {
            if normalized.starts_with(root.as_str()) {
                return prefix_path_with_alias(path, Some(alias), root);
            }
        }
    }
    normalized
}

/// Pick the project root to relativise a result path against for a `filter_path`
/// prefix match, so `filter_path` is interpreted **relative to the repo root**
/// in every routing mode:
/// - serve single-project routing → the routed alias's root (`alias_roots[alias]`);
/// - serve multi/group → the longest alias root the (absolute) path lives under;
/// - stdio single-repo (no alias roots) → the service's own `project_path`
///   (`fallback_root`).
///
/// Before this, the filter always used the service's `project_path`, which for a
/// serve-routed project is NOT the routed repo's root — so the absolute stored
/// path never relativised and every hit was dropped. The federated paths solve
/// the same class of bug client-side (see `retain_by_filter_path`); this covers
/// the local (non-federated) serve/multi case.
fn pick_filter_root(
    path: &str,
    project_alias: Option<&str>,
    alias_roots: &std::collections::HashMap<String, String>,
    fallback_root: &str,
) -> String {
    if let Some(alias) = project_alias {
        if let Some(root) = alias_roots.get(alias) {
            return root.clone();
        }
    }
    if !alias_roots.is_empty() {
        let normalized = crate::cache::normalize_path_str(path);
        if let Some(root) = alias_roots
            .values()
            .filter(|r| normalized.starts_with(r.as_str()))
            .max_by_key(|r| r.len())
        {
            return root.clone();
        }
    }
    fallback_root.to_string()
}

fn is_import_kind(kind: &str) -> bool {
    matches!(kind, "Import" | "Use" | "Require" | "Include" | "Imports")
}

/// Common import-keyword literals used by the FTS fallback when no import-kind
/// chunks are found via vector-store lookup.
const IMPORT_FTS_KEYWORDS: &[&str] = &["import", "use", "using", "from", "require", "include"];

fn truncate_line_around_match(line: &str, match_start_byte: usize, max_chars: usize) -> String {
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

fn match_line_for_literal(
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

/// Returns true when a regex pattern contains at least one run of three or more
/// alphanumerics-or-underscore characters. Such a run is enough for Tantivy's
/// analyzer to produce a real BM25 token, which means the BM25 candidate path
/// will work for this query.
///
/// When this returns false, the regex is "tokenless" — it consists only of
/// regex syntax (\b, \s, \w, ^, $, character classes, anchors). BM25 has
/// nothing to match on, so the caller must fall back to a full chunk scan.
///
/// Conservative direction: false positives ("looks anchorable, isn't really")
/// are safe because the BM25 path will return empty candidates and the regex
/// post-filter will return empty results — same outcome as the scan path
/// would on a corpus with no matches. False negatives ("looks tokenless,
/// actually has tokens") are unsafe because they trigger an unnecessary scan.
/// We bias toward false positives.
fn regex_has_anchorable_token(pattern: &str) -> bool {
    let mut run: usize = 0;
    let mut need_separator = false;
    let mut i = 0;
    let bytes = pattern.as_bytes();
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip the next char after a backslash — it's an escape, not content.
        // This prevents \w, \s, \b, \d etc. from contributing to the run count.
        if c == '\\' && i + 1 < bytes.len() {
            run = 0;
            need_separator = true; // chars after escape are merged by BM25 tokenizer
            i += 2;
            continue;
        }
        // Character classes [abc] don't anchor BM25 either — the tokens inside
        // are alternatives, not a contiguous string. Skip the whole class.
        if c == '[' {
            run = 0;
            need_separator = true;
            // Find matching ]; tolerate \] inside.
            let mut j = i + 1;
            while j < bytes.len() {
                let cj = bytes[j] as char;
                if cj == '\\' && j + 1 < bytes.len() {
                    j += 2;
                    continue;
                }
                if cj == ']' {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            if need_separator {
                // After \X or [...], BM25 merges the next alphanumeric run with
                // the escape/class content (e.g. \bimpl → "bimpl", not "impl").
                // So we skip these chars — they're not independent tokens.
                i += 1;
                continue;
            }
            run += 1;
            if run >= 3 {
                // Only peek when the run might be ending: check if the next byte
                // is NOT alphanumeric. If it IS, keep building the run.
                let next_idx = i + 1;
                let run_continues = next_idx < bytes.len() && {
                    let nc = bytes[next_idx] as char;
                    nc.is_alphanumeric() || nc == '_'
                };
                if !run_continues {
                    // Run has ended. Check if next byte merges (escape or class).
                    if next_idx < bytes.len() {
                        let next_c = bytes[next_idx] as char;
                        if next_c == '\\' || next_c == '[' {
                            run = 0;
                            need_separator = true;
                            i += 1;
                            continue;
                        }
                    }
                    // Run ended naturally (EOF or non-merge separator) → anchorable
                    return true;
                }
                // Run continues — keep building in next iteration
            }
        } else {
            run = 0;
            need_separator = false;
        }
        i += 1;
    }
    false
}

/// Extracts a clean BM25 query string from a regex pattern.
///
/// When `regex=true` and the BM25 path is used, we can't pass the raw regex
/// (e.g. `class \w+Cache\b`) to Tantivy — it tokenizes poorly on backslashes
/// and metacharacters, producing useless candidates. Instead, this function
/// extracts only the literal alphanumeric runs from the pattern and joins them
/// with spaces, producing a clean BM25 query (e.g. `class Cache`).
///
/// The regex post-filter (`match_line_for_literal`) then correctly filters
/// the BM25 candidates against the actual regex pattern.
fn extract_bm25_query_from_regex(pattern: &str) -> String {
    let mut tokens: Vec<&str> = Vec::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;
    let mut run_start: Option<usize> = None;

    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip escaped characters (\w, \b, \d, etc.)
        if c == '\\' && i + 1 < bytes.len() {
            if run_start.is_some() {
                let token = &pattern[run_start.unwrap()..i];
                if token.len() >= 2 {
                    tokens.push(token);
                }
                run_start = None;
            }
            i += 2;
            continue;
        }
        // Skip character classes [abc]
        if c == '[' {
            if run_start.is_some() {
                let token = &pattern[run_start.unwrap()..i];
                if token.len() >= 2 {
                    tokens.push(token);
                }
                run_start = None;
            }
            let mut j = i + 1;
            while j < bytes.len() {
                let cj = bytes[j] as char;
                if cj == '\\' && j + 1 < bytes.len() {
                    j += 2;
                    continue;
                }
                if cj == ']' {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if run_start.is_some() {
            let token = &pattern[run_start.unwrap()..i];
            if token.len() >= 2 {
                tokens.push(token);
            }
            run_start = None;
        }
        i += 1;
    }
    // Flush trailing run
    if let Some(start) = run_start {
        let token = &pattern[start..];
        if token.len() >= 2 {
            tokens.push(token);
        }
    }
    tokens.join(" ")
}

/// Returns true when a regex pattern contains a top-level alternation (`|`)
/// that is NOT inside a group `(...)` or character class `[...]`.
///
/// BM25 treats a query like `TODO|FIXME|HACK` as a conjunction of all tokens
/// (`TODO AND FIXME AND HACK`), which returns 0 results because no single chunk
/// contains all three. The regex post-filter would then discard everything.
/// Detecting top-level `|` lets us fall back to the scan path, which applies the
/// regex correctly (matching any alternative per chunk).
///
/// Escaped pipes (`\|`) are ignored.
fn regex_has_disjunctive_or(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut depth_paren = 0u32; // nesting depth of (...)
    let mut in_bracket = false; // inside [...]
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        // Skip escaped char
        if c == '\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if in_bracket {
            if c == ']' {
                in_bracket = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => {
                in_bracket = true;
            }
            '(' => {
                depth_paren += 1;
            }
            ')' => {
                depth_paren = depth_paren.saturating_sub(1);
            }
            '|' => return depth_paren == 0,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Returns true when a literal-search query looks like a code pattern whose
/// punctuation would be destroyed by BM25 tokenization.
///
/// Triggers on:
/// - Multi-char operators: ->, =>, ::, !=, ==, <=, >=, &&, ||, <<, >>
/// - Space-surrounded single operators: " = ", " < ", " > "
/// - Statement endings: trailing `;` or `{`
/// - ≥ 2 angle/square bracket characters: `Vec<T>`, `[0]`
///
/// Does NOT trigger on:
/// - Plain identifiers: "ActivitiesListModelResponse", "foo_bar"
/// - Dotted paths: "foo.bar", "System.Console"
/// - Single parens alone: "(error)" — parens are not in the bracket set
fn looks_like_code_pattern(query: &str) -> bool {
    const MULTI_OPS: &[&str] = &[
        "->", "=>", "::", "!=", "==", "<=", ">=", "&&", "||", "<<", ">>",
    ];
    if MULTI_OPS.iter().any(|op| query.contains(op)) {
        return true;
    }
    const SPACED_OPS: &[&str] = &[" = ", " < ", " > "];
    if SPACED_OPS.iter().any(|op| query.contains(op)) {
        return true;
    }
    let trimmed = query.trim();
    if trimmed.ends_with(';') || trimmed.ends_with('{') {
        return true;
    }
    let bracket_count = query
        .chars()
        .filter(|c| matches!(c, '<' | '>' | '[' | ']'))
        .count();
    bracket_count >= 2
}

/// BM25 score threshold for low-confidence signalling in literal search.
///
/// Scores **below** this threshold trigger `low_confidence: true` in the
/// response. Tantivy BM25 scores in the codesearch corpus typically range
/// from ~5 (weak match) to ~50+ (strong match), so 5.0 is a conservative
/// initial floor — below this, results are likely noise rather than real hits.
///
/// To recalibrate: enable `RUST_LOG=codesearch::literal_confidence=debug`,
/// collect query/score samples, set this to roughly the 25th percentile of
/// real query scores.
const LITERAL_LOW_CONFIDENCE_BM25: f32 = 5.0;

fn compute_literal_low_confidence(
    top_score: Option<f32>,
    query: &str,
) -> (Option<bool>, Option<String>) {
    let word_count = query.split_whitespace().count();
    let has_code_chars = query.chars().any(|c| "{}[]<>=|;:".contains(c));
    let is_natural_language = word_count >= 3 && !has_code_chars;
    // A single identifier with no spaces: trust results even when BM25 score is low.
    // BM25 scores are unreliable for identifiers that tokenise into common sub-words
    // (e.g. `regex_has_disjunctive_or` → `or` has near-zero IDF and drags the score
    // below the floor even when the match is correct).
    let is_single_identifier = word_count == 1 && !has_code_chars;

    let suggest_semantic = "search with mode='semantic'";
    let suggest_regex = "search with mode='literal' and regex=true";
    let suggest_find = "find with kind='definition' or kind='usages'";

    match top_score {
        Some(score) if score < LITERAL_LOW_CONFIDENCE_BM25 => {
            if is_single_identifier {
                // Results exist for a single-word identifier: low BM25 score is an
                // IDF artefact, not a quality signal. Trust the results.
                return (None, None);
            }
            let hint = if is_natural_language {
                suggest_semantic
            } else {
                suggest_find
            };
            (Some(true), Some(hint.to_string()))
        }
        None => {
            let hint = if is_natural_language {
                suggest_semantic
            } else {
                suggest_regex
            };
            (Some(true), Some(hint.to_string()))
        }
        Some(_) => (None, None),
    }
}

/// Parse individual import statements from chunk content.
///
/// Handles: `use`, `import`, `from ... import`, `#include`, `require(...)`.
/// Limitation: multi-line imports (e.g. Python `from X import (\n  a,\n  b\n)`)
/// are only partially captured — the first line is matched, continuation lines
/// are missed. Acceptable for v1; a proper AST-based approach would require
/// changes to the chunker.
fn parse_import_lines(content: &str, start_line: usize) -> Vec<ImportItem> {
    let mut items = Vec::new();

    for (offset, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed = if let Some(rest) = trimmed.strip_prefix("use ") {
            Some((
                "use".to_string(),
                rest.trim().trim_end_matches(';').to_string(),
            ))
        } else if let Some(rest) = trimmed.strip_prefix("using ") {
            // C# using directive — skip `using (...)` statements and `using var` declarations
            if rest.starts_with('(') || rest.starts_with("var ") {
                None
            } else {
                Some((
                    "using".to_string(),
                    rest.trim().trim_end_matches(';').to_string(),
                ))
            }
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            Some((
                "import".to_string(),
                rest.trim().trim_end_matches(';').to_string(),
            ))
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            Some((
                "import".to_string(),
                rest.trim().trim_end_matches(';').to_string(),
            ))
        } else if trimmed.starts_with("#include") {
            Some((
                "include".to_string(),
                trimmed
                    .trim_start_matches("#include")
                    .trim()
                    .trim_end_matches(';')
                    .to_string(),
            ))
        } else if trimmed.contains("require(") {
            Some(("require".to_string(), trimmed.to_string()))
        } else {
            None
        };

        if let Some((kind, imported)) = parsed {
            items.push(ImportItem {
                imported,
                line: start_line + offset,
                kind,
            });
        }
    }

    items
}

// === Multi-Store Routing Context ===

/// Pre-computed routing context for a tool handler.
///
/// Created by `CodesearchService::resolve_routing()`, this struct encapsulates
/// all the decisions a handler needs: which store to use, whether to fan out,
/// and whether to call `ensure_database_exists()`.
/// Outcome of a fan-out read across several repos.
///
/// Exists so an empty `results` is never ambiguous. A group query that hits a
/// broken store used to come back as a successful search with zero hits, which
/// is the most misleading signal this system can emit — it reads as "the corpus
/// does not contain that", and it is what sent an earlier round of this
/// investigation chasing an indexing problem that did not exist.
#[must_use]
struct MultiReadOutcome<R> {
    /// Merged, deduplicated, score-sorted results from the stores that worked.
    results: Vec<R>,
    /// `(alias, full error chain)` for every store that failed. Empty on a
    /// clean run.
    failures: Vec<(String, String)>,
}

/// Decide the `status`/`status_message` pair for a multi-store
/// `status(kind="index"|"projects")` response.
///
/// Pulled out of the handler so the four-way call — every store down, still
/// building, ready but one or more stores failed to report their stats, or
/// fully ready — is testable without opening a single store. `failed_count`
/// is checked before declaring "building" or "ready" precisely so a store
/// that came back `Err` cannot render identically to one that returned
/// healthy zero-valued stats. The all-failed case is checked first: a
/// correlated failure (e.g. every store hits the same read-only-snapshot or
/// disk-full condition at once) also has `total_chunks == 0`, and without
/// this ordering it fell through to "building" — byte-identical to a group
/// that simply has not been indexed yet, which is the exact indistinguishable
/// case this fix exists to close. See AGENTS.md's fan-out warnings-channel
/// rule.
fn index_status_summary(
    total_repos: usize,
    failed_count: usize,
    total_chunks: usize,
) -> (String, String) {
    if total_repos > 0 && failed_count >= total_repos {
        (
            "error".to_string(),
            format!(
                "All {total_repos} repo(s) failed to report status — every store errored, see `warnings`."
            ),
        )
    } else if total_chunks == 0 {
        (
            "building".to_string(),
            format!(
                "Index is being built across {total_repos} repo(s). Searches may fail until indexing completes."
            ),
        )
    } else if failed_count > 0 {
        (
            "ready".to_string(),
            format!(
                "Index is ready for searching across {} of {total_repos} repo(s) — {failed_count} store(s) failed to report status, see `warnings`.",
                total_repos.saturating_sub(failed_count),
            ),
        )
    } else {
        (
            "ready".to_string(),
            format!("Index is ready for searching across {total_repos} repo(s)."),
        )
    }
}

/// Turn a store's `stats()` result into the `(total_chunks, total_files,
/// error)` triple `list_projects` reports per repo.
///
/// Pulled out of the handler, mirroring `index_status_summary` just above, so
/// the fix's actual claim — a `stats()` failure surfaces as `error: Some(..)`
/// with zero-valued counts, instead of silently rendering as a healthy-looking
/// empty repo — is unit-testable without opening a real `VectorStore` or
/// `ServeState`. The two calls to `serve_state.repo_lock_status()` in
/// `list_projects` don't vary by outcome, so they stay in the handler; this
/// covers only the part that does.
fn repo_stats_from_result(
    stats: anyhow::Result<crate::vectordb::StoreStats>,
) -> (usize, usize, Option<String>) {
    match stats {
        Ok(s) => (s.total_chunks, s.total_files, None),
        Err(ref e) => (0, 0, Some(format!("stats unavailable: {e:#}"))),
    }
}

/// `repo_stats_from_result` plus recording the failure as a caller-facing
/// warning, in one call.
///
/// `list_projects` used to inline `repo_stats_from_result` and then decide
/// separately whether to push a warning — two steps a future edit could
/// silently pull apart (drop the second one, keep the first) without
/// affecting `total_chunks`/`total_files` at all, so nothing would look
/// wrong at the call site. Folding both into one call means a regression
/// that drops the warning has to delete this call entirely, which also
/// deletes the counts — no longer a silent edit. This is also the seam a
/// test can drive without opening a real `VectorStore`/`ServeState`: it
/// exercises the exact composition `list_projects` calls, not a
/// re-implementation of it.
fn record_stats_or_warn(
    stats: anyhow::Result<crate::vectordb::StoreStats>,
    alias: &str,
    warnings: &mut Vec<String>,
) -> (usize, usize, Option<String>) {
    let (total_chunks, total_files, error) = repo_stats_from_result(stats);
    if let Some(ref msg) = error {
        push_store_warning(warnings, &store_warning(alias, "stats", msg));
    }
    (total_chunks, total_files, error)
}

/// Record a per-store failure as a caller-facing warning, once per store.
///
/// Resolution loops run per hit, so a single broken store would otherwise emit
/// one identical warning per result; the caller wants to know *that* the repo
/// is down, not how many times it noticed.
fn note_store_failure(
    warnings: &mut Vec<String>,
    aliases: &[String],
    idx: usize,
    what: &str,
    err: &anyhow::Error,
) {
    let alias = aliases.get(idx).map(|s| s.as_str()).unwrap_or("unknown");
    push_store_warning(warnings, &store_warning(alias, what, &format!("{err:#}")));
}

/// The one place a per-store warning line is formatted. Two copies used to
/// exist and could drift; a caller matching on this text would then silently
/// stop matching half of them.
fn store_warning(alias: &str, what: &str, err: &str) -> String {
    format!("repo '{alias}' {what} failed: {err}")
}

/// Append a warning unless it is already present, logging it once.
fn push_store_warning(warnings: &mut Vec<String>, msg: &str) {
    if !warnings.iter().any(|w| w == msg) {
        tracing::error!("MCP: {}", msg);
        warnings.push(msg.to_string());
    }
}

/// The single exit for a handler that returns a list of items plus a warnings
/// channel.
///
/// Five handlers previously read their channel ONLY on the empty path, so a
/// partially-failed group returned a plausible-looking short list with no
/// signal at all - the same false negative as an empty result, just harder to
/// notice. Routing every exit through here means the channel is carried
/// whether the list is empty or not, and there is no per-handler discipline
/// left to forget.
///
/// A healthy call is byte-identical to the previous behaviour (a bare JSON
/// array), so this is backward compatible.
fn respond_with_items<T: serde::Serialize>(
    items: &[T],
    warnings: &[String],
    empty_message: impl FnOnce() -> String,
) -> Result<CallToolResult, McpError> {
    if items.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            qualify_empty_result(empty_message(), warnings),
        )]));
    }
    if !warnings.is_empty() {
        let payload = serde_json::json!({ "results": items, "warnings": warnings });
        return Ok(CallToolResult::success(vec![Content::text(
            payload.to_string(),
        )]));
    }
    let json = serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string());
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// The object-shaped sibling of `respond_with_items`: one exit for handlers that
/// return a single struct rather than a list.
///
/// A `warnings` *field* on the response struct was the obvious fix and is the
/// weaker one — the handler is still free to populate it with `None`, and a test
/// that builds the struct itself cannot see that happen. Review round 8 proved
/// it: the round-7 defect was reintroduced at the `get_chunk` success path and
/// all 630 tests still passed.
///
/// **This is an improvement, not a guarantee.** Round 9 measured the difference:
/// passing `&[]` here is exactly as writable as `warnings: None` was, the suite
/// still cannot see it, and no lint fires (the channel stays "used" by the
/// ambiguous path). What it actually buys is narrower and real — no optional
/// field whose absence is invisible, no future construction site that can zero
/// it, and an audit that collapses from "check every response struct" to "check
/// the call sites of two functions", which is grep-answerable. The channel can
/// no longer be *forgotten*, only actively discarded.
///
/// Healthy path serializes the struct directly, so its key order and bytes are
/// unchanged. `serde_json::Map` is a `BTreeMap` here (no `preserve_order`
/// feature), so round-tripping through `to_value` would silently re-sort the
/// keys — which is only acceptable on the warning path, where the shape is new
/// anyway.
fn respond_with_object<T: serde::Serialize>(
    value: &T,
    warnings: &[String],
) -> Result<CallToolResult, McpError> {
    if !warnings.is_empty() {
        if let Ok(mut v) = serde_json::to_value(value) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("warnings".to_string(), serde_json::json!(warnings));
                return Ok(CallToolResult::success(vec![Content::text(v.to_string())]));
            }
        }
    }
    let json = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Build the `ambiguous_chunk_id` payload for `get_chunk`.
///
/// `candidate_projects` reads as the complete set of repos holding this
/// chunk_id, so a store that failed to answer must be declared: the repo the
/// caller actually wants may be the one missing from the list. Extracted so the
/// "is this list complete?" decision is testable without standing up stores.
///
/// `warnings` is *inserted* rather than emitted as `null`, so the healthy-path
/// shape is byte-identical to before — matching `skip_serializing_if` on every
/// other warnings-carrying response.
fn ambiguous_chunk_payload(
    chunk_id: u32,
    candidate_projects: &[&str],
    warnings: &[String],
) -> serde_json::Value {
    let mut message =
        format!("chunk_id {chunk_id} exists in multiple repositories. Specify which one.");
    if !warnings.is_empty() {
        message.push_str(" The candidate list is incomplete — see `warnings`.");
    }
    let mut payload = serde_json::json!({
        "error_code": "ambiguous_chunk_id",
        "message": message,
        "candidate_projects": candidate_projects,
        "hint_for_agent": "The chunk_id collision is a known limitation of multi-repo mode. Re-run get_chunk with one of the candidate_projects, or use search to identify the correct repository first."
    });
    if !warnings.is_empty() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("warnings".to_string(), serde_json::json!(warnings));
        }
    }
    payload
}

/// Decide whether to keep the "try another tool" hint.
///
/// A weak or empty result caused by a store that is DOWN is not a reason to
/// retry with a different tool — that just sends the agent back at the same
/// broken store. Extracted from `build_semantic_response` so the decision is
/// testable without standing up a service.
fn retry_hint(suggested: Option<String>, warnings: &Option<Vec<String>>) -> Option<String> {
    // `is_some()` alone is wrong: an empty `Some(vec![])` means nothing failed,
    // and suppressing a legitimate hint on it would be a silent regression the
    // moment a caller constructs the warnings vec eagerly.
    if warnings.as_ref().is_some_and(|w| !w.is_empty()) {
        return None;
    }
    suggested
}

/// Qualify a "nothing found" message when a store in scope actually failed.
///
/// This is the defect that keeps coming back in a new handler: "No definition
/// found — the symbol may not be indexed" is a *diagnosis*, and it is flatly
/// wrong when the store never answered. An agent acts on it by giving up or by
/// re-indexing something that was never broken.
fn qualify_empty_result(message: String, warnings: &[String]) -> String {
    if warnings.is_empty() {
        return message;
    }
    format!(
        "{message}\n\nWARNING: this result is not trustworthy — {count} store(s) in \
         scope failed, so \"not found\" may mean \"not searched\":\n{detail}",
        count = warnings.len(),
        detail = warnings.join("\n")
    )
}

// Hand-written rather than derived: `derive(Default)` would demand `R: Default`,
// which the result types do not implement and do not need to.
impl<R> Default for MultiReadOutcome<R> {
    fn default() -> Self {
        Self {
            results: Vec::new(),
            failures: Vec::new(),
        }
    }
}

impl<R> MultiReadOutcome<R> {
    /// Render failures as caller-facing warning lines.
    fn warnings(&self, what: &str) -> Vec<String> {
        self.failures
            .iter()
            .map(|(alias, err)| store_warning(alias, what, err))
            .collect()
    }

    /// Take the results, routing any failures into `warnings` on the way out.
    ///
    /// Deliberately the only ergonomic way to get at `results`: reaching for
    /// the field directly and dropping `failures` is `unwrap_or_default()`
    /// under a new name, and that is the bug this whole type exists to stop.
    fn into_results(self, warnings: &mut Vec<String>, what: &str) -> Vec<R> {
        for (alias, err) in &self.failures {
            push_store_warning(warnings, &store_warning(alias, what, err));
        }
        self.results
    }
}

struct MultiStoreContext {
    /// Single-store override (set when exactly 1 repo resolved, or None).
    /// Pass to `with_*_store_read_for()` methods.
    stores: Option<Arc<SharedStores>>,
    /// Multi-store vec for fan-out (set when 2+ repos resolved, or None).
    /// Use `if let Some(ref sv) = ctx.stores_vec { ... }` for the multi-store path.
    stores_vec: Option<Vec<Arc<SharedStores>>>,
    /// Alias for each store in `stores_vec` (parallel with stores_vec).
    /// Used for path prefixing and per-alias dedup.
    store_aliases: Option<Vec<String>>,
    /// Alias for single-project routing (set when project= is given).
    project_alias: Option<String>,
    /// Normalized project root for each alias (alias → root path).
    /// Used by `prefix_path` to strip absolute paths and add alias prefix.
    alias_roots: std::collections::HashMap<String, String>,
    /// True when `stores_vec` has 2+ entries (group fan-out).
    is_multi: bool,
    /// True when no serve-state stores resolved and local DB should be checked.
    needs_local_db: bool,
}

impl MultiStoreContext {
    /// Aliases parallel to `stores_vec`, or an empty slice when absent.
    ///
    /// Every fan-out that reports a per-store failure needs this, and hand-rolling
    /// `let empty = Vec::new(); ...unwrap_or(&empty)` at each site produced four
    /// copies of the same two lines — and one handler where the binding was out
    /// of scope, which is how a silent store read survived a round of review.
    fn aliases(&self) -> &[String] {
        self.store_aliases.as_deref().unwrap_or(&[])
    }

    /// Prefix a result path with its owning alias for multi-repo identification.
    ///
    /// Three dispatch modes:
    /// - Single-project (`project_alias = Some(...)`): prefix with that alias.
    /// - Group (`store_aliases = Some([...])`): detect alias by prefix-matching
    ///   the path against known project roots in `alias_roots`.
    /// - Stdio / no alias info: normalize only, no prefix.
    ///
    /// Emits a `tracing::debug!` event when an expected alias cannot be resolved.
    /// That usually indicates a config mismatch or a path from an unregistered source —
    /// the path is still normalized and returned, but diagnosis is easier with the log.
    fn prefix_result_path(&self, path: &str) -> String {
        if let Some(ref alias) = self.project_alias {
            if let Some(root) = self.alias_roots.get(alias) {
                return prefix_path_with_alias(path, Some(alias), root);
            }
            tracing::debug!(
                target: "codesearch::mcp::path_prefix",
                alias = %alias,
                path = %path,
                "project_alias has no entry in alias_roots"
            );
        }
        if let Some(ref aliases) = self.store_aliases {
            let normalized = crate::cache::normalize_path_str(path);
            for alias in aliases {
                if let Some(root) = self.alias_roots.get(alias) {
                    if normalized.starts_with(root.as_str()) {
                        return prefix_path_with_alias(path, Some(alias), root);
                    }
                }
            }
            tracing::debug!(
                target: "codesearch::mcp::path_prefix",
                aliases = ?aliases,
                path = %path,
                "no alias root matched path in group mode"
            );
        }
        crate::cache::normalize_path_str(path)
    }
}

// === Tool Router Implementation ===

#[tool_router]
impl CodesearchService {
    /// Create a new CodesearchService (standalone mode - opens its own VectorStore)
    #[allow(dead_code)] // Reserved for standalone MCP server mode
    pub fn new(requested_path: Option<PathBuf>) -> Result<Self> {
        Self::new_with_stores(requested_path, None)
    }

    /// Create a new CodesearchService with shared stores (for use with IndexManager)
    pub fn new_with_stores(
        requested_path: Option<PathBuf>,
        shared_stores: Option<Arc<SharedStores>>,
    ) -> Result<Self> {
        // Find the best database to use
        let db_info = find_best_database(requested_path.as_deref())?;

        if db_info.is_none() {
            return Err(anyhow::anyhow!(
                "No database found in current directory, parent directories, or globally tracked repositories. \
                 Run 'codesearch index' first to index the codebase."
            ));
        }

        let db_info = db_info.unwrap();
        let db_path = db_info.db_path;
        let project_path = db_info.project_path;

        // Read model metadata from database
        let metadata_path = db_path.join("metadata.json");
        let (model_type, dimensions) = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            let json: serde_json::Value = serde_json::from_str(&content)?;
            let model_name = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("minilm-l6");
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(crate::constants::DEFAULT_EMBEDDING_DIMENSIONS as u64)
                as usize;
            let mt = ModelType::parse(model_name).unwrap_or_default();
            (mt, dims)
        } else {
            (
                ModelType::default(),
                crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
            )
        };

        Ok(Self {
            tool_router: Self::tool_router(),
            db_path,
            project_path,
            model_type,
            dimensions,
            embedding_service: Arc::new(Mutex::new(None)),
            shared_stores,
            serve_state: None,
            symbol_registry: Arc::new(SymbolIndexerRegistry::new()),
            tracks_session: false,
        })
    }

    /// Create a CodesearchService for use inside `codesearch serve`.
    ///
    /// In serve mode, the service does not have a single local DB; instead
    /// it routes requests to the repo identified by `project`/`group`.
    pub(crate) fn new_for_serve(serve_state: Arc<crate::serve::ServeState>) -> Result<Self> {
        let symbol_registry = serve_state.symbol_registry();
        Ok(Self {
            tool_router: Self::tool_router(),
            db_path: PathBuf::from("serve://multi-repo"),
            project_path: PathBuf::from("serve://multi-repo"),
            model_type: ModelType::default(),
            dimensions: crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
            embedding_service: serve_state.embedding_service(),
            shared_stores: None,
            serve_state: Some(serve_state),
            symbol_registry,
            tracks_session: false,
        })
    }

    /// Mark this service as owning a session slot so `Drop` will balance the
    /// `session_connected()` the caller already made.
    ///
    /// Only the serve-mode MCP session factory (`run_serve`) should call this:
    /// it calls `session_connected()` and then this. Per-request REST services
    /// built via `make_service` must NOT — they never increment the counter, so
    /// decrementing it on drop would underflow `active_sessions` to `u64::MAX`.
    pub(crate) fn mark_session_tracked(&mut self) {
        self.tracks_session = true;
    }

    /// Get or initialize the embedding service
    fn get_embedding_service(&self) -> Result<std::sync::MutexGuard<'_, Option<EmbeddingService>>> {
        let mut guard = self.embedding_service.lock().unwrap();
        if guard.is_none() {
            let cache_dir = crate::constants::get_global_models_cache_dir()?;
            *guard = Some(EmbeddingService::with_cache_dir(
                self.model_type,
                Some(&cache_dir),
            )?);
        }
        Ok(guard)
    }

    /// Return the current MCP mode as a string for diagnostics.
    fn mcp_mode(&self) -> Option<String> {
        if self.serve_state.is_some() {
            Some("serve_hub".to_string())
        } else {
            Some("stdio".to_string())
        }
    }

    /// Check if database exists and return error if not
    fn ensure_database_exists(&self) -> Result<(), String> {
        if !self.db_path.exists() {
            return Err(format!(
                "❌ No index database found at: {}\n\n\
                 ⚠️  IMPORTANT: This MCP server cannot index the codebase itself. Indexing takes 30-60 seconds and must be done manually.\n\n\
                 To fix this, run the following command in your terminal:\n\
                 $ cd {}\n\
                 $ codesearch index\n\n\
                 For more information about database locations, use the `status` tool with `kind=\"projects\"`.",
                self.db_path.display(),
                self.project_path.display()
            ));
        }
        Ok(())
    }

    /// Resolve project/group parameters to a specific `Arc<SharedStores>`.
    ///
    /// For groups with multiple members, only the first store is returned.
    /// Use `resolve_repo_stores_multi` for full group fan-out.
    ///
    /// Returns:
    /// Resolve project/group parameters to all matching `Arc<SharedStores>`.
    ///
    /// For groups, returns stores for ALL group members (fan-out).
    /// For project, returns a single-element vec.
    ///
    /// Returns:
    /// - `Ok(None)` — no project/group specified, use default local stores
    /// - `Ok(Some(vec))` — one or more stores to query (fan out and merge)
    /// - `Err(msg)` — validation error
    async fn resolve_repo_stores_multi(
        &self,
        project: &Option<String>,
        group: &Option<String>,
        allow_unscoped: bool,
    ) -> std::result::Result<Option<(Vec<Arc<SharedStores>>, Vec<String>)>, String> {
        // No routing params → resolve based on repo count
        if project.is_none() && group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let cfg = serve_state.config_snapshot();
                let aliases: Vec<String> = cfg.repos.keys().cloned().collect();
                if aliases.len() > 1 && !allow_unscoped {
                    // Multi-repo: reject fan-out, require explicit scope
                    return Err(self.format_scope_error());
                }
                if !aliases.is_empty() {
                    let mut all_stores = Vec::with_capacity(aliases.len());
                    for alias in &aliases {
                        all_stores.push(serve_state.get_or_open_stores(alias, false).await?);
                    }
                    return Ok(Some((all_stores, aliases)));
                }
                // No repos configured — fall through to local DB
            }
            return Ok(None);
        }

        // Must have serve_state to route
        let serve_state = match self.serve_state.as_ref() {
            Some(ss) => ss,
            None => {
                // Local/stdio mode: only one DB available, project/group are meaningless.
                // Fall through to local DB instead of erroring.
                tracing::warn!(
                    "MCP: project/group ignored in local mode (no serve running). \
                     Using local database."
                );
                return Ok(None);
            }
        };

        // Validate params
        types::validate_project_group(project, group, true)?;

        if let Some(ref alias) = project {
            let stores = serve_state.get_or_open_stores(alias, true).await?;
            return Ok(Some((vec![stores], vec![alias.clone()])));
        }

        if let Some(ref group_name) = group {
            let aliases = serve_state.resolve_group_aliases(group_name)?;
            if aliases.is_empty() {
                return Err(format!("Group '{}' has no members.", group_name));
            }
            let mut all_stores = Vec::with_capacity(aliases.len());
            for alias in &aliases {
                all_stores.push(serve_state.get_or_open_stores(alias, false).await?);
            }
            return Ok(Some((all_stores, aliases)));
        }

        Ok(None)
    }

    /// Resolve project/group params into a ready-to-use routing context.
    ///
    /// Encapsulates the common pattern: resolve multi-stores, extract single override
    /// vs multi-store vec, and determine if local DB check is needed.
    /// Also records the tool call for dashboard tracking when serve_state is active.
    async fn resolve_routing(
        &self,
        project: &Option<String>,
        group: &Option<String>,
        allow_unscoped: bool,
        tool_name: &str,
    ) -> std::result::Result<MultiStoreContext, String> {
        let resolved = self
            .resolve_repo_stores_multi(project, group, allow_unscoped)
            .await?;
        let is_multi = resolved
            .as_ref()
            .is_some_and(|(stores, _)| stores.len() > 1);
        let (stores, stores_vec, store_aliases, project_alias) = match &resolved {
            None => (None, None, None, None),
            Some((store_vec, aliases)) if store_vec.len() == 1 => {
                let alias = aliases.first().cloned();
                (Some(store_vec[0].clone()), None, None, alias)
            }
            Some((store_vec, aliases)) => {
                (None, Some(store_vec.clone()), Some(aliases.clone()), None)
            }
        };

        // Build alias → normalized project root map for path prefixing
        let mut alias_roots = std::collections::HashMap::new();
        if let Some(ref serve_state) = self.serve_state {
            let cfg = serve_state.config_snapshot();
            let all_aliases = store_aliases.as_deref().unwrap_or(&[]);
            for alias in all_aliases.iter() {
                if let Some(path) = cfg.resolve(alias) {
                    let root = crate::cache::normalize_path_str(path.to_string_lossy().as_ref())
                        .trim_end_matches('/')
                        .to_string();
                    alias_roots.insert(alias.clone(), root);
                }
            }
            if let Some(ref alias) = project_alias {
                if let Some(path) = cfg.resolve(alias) {
                    let root = crate::cache::normalize_path_str(path.to_string_lossy().as_ref())
                        .trim_end_matches('/')
                        .to_string();
                    alias_roots.insert(alias.clone(), root);
                }
            }
        }

        let needs_local_db = stores.is_none() && !is_multi;

        // Record tool call for serve dashboard tracking.
        // Skip recording for unscoped multi-store fan-out (allow_unscoped=true means
        // get_chunk or status — get_chunk will record after candidate detection,
        // status doesn't need per-repo recording).
        if let Some(ref serve_state) = self.serve_state {
            if !allow_unscoped || !is_multi {
                if let Some(ref aliases) = store_aliases {
                    for alias in aliases {
                        serve_state.record_tool_call(alias, tool_name);
                        // Explicit multi-repo/group query: treat as access.
                        // (Unscoped multi fan-out is skipped by the outer condition.)
                        serve_state.touch_access(alias);
                    }
                }
                if let Some(ref alias) = project_alias {
                    serve_state.record_tool_call(alias, tool_name);
                }
            }
        }

        Ok(MultiStoreContext {
            stores,
            stores_vec,
            store_aliases,
            project_alias,
            alias_roots,
            is_multi,
            needs_local_db,
        })
    }

    /// Build a structured `scope_required` error JSON for multi-repo mode.
    ///
    /// Returns a JSON string containing `error_code`, `message`, `available_projects`,
    /// `available_groups`, `project_groups`, and `hint_for_agent` so that LLM
    /// agents can programmatically react to the scope requirement. `project_groups`
    /// maps each project to the named group(s) it belongs to, so an agent can tell
    /// that picking a single project would miss sibling repos in the same group
    /// (e.g. a separate config / import-data repo).
    fn format_scope_error(&self) -> String {
        let (projects, mut groups, project_groups) = if let Some(ref serve_state) = self.serve_state
        {
            let cfg = serve_state.config_snapshot();
            let mut projects: Vec<String> = cfg.repos.keys().cloned().collect();
            // Include opt-in mounted remote projects so an agent can discover and
            // route to them by name (they are first-class `project=` targets).
            projects.extend(
                cfg.mounted_remote_projects()
                    .into_iter()
                    .map(|(name, _)| name),
            );
            projects.sort();
            projects.dedup();
            let mut groups: Vec<String> = cfg.groups.keys().cloned().collect();
            groups.sort();
            (projects, groups, cfg.project_groups())
        } else {
            (vec![], vec![], std::collections::HashMap::new())
        };
        // The "all" virtual group is always available when there are projects to
        // search — advertise it so agents discover the cross-repo shortcut.
        // (scope_required only fires when >1 repo is registered, so this is safe.)
        let all = crate::constants::ALL_GROUP_NAME.to_string();
        if !projects.is_empty() && !groups.contains(&all) {
            groups.push(all);
            groups.sort();
        }

        let payload = serde_json::json!({
            "error_code": "scope_required",
            "message": "Specify project= for a single repository or group= for cross-repo search.",
            "available_projects": projects,
            "available_groups": groups,
            "project_groups": project_groups,
            "hint_for_agent": "If the user has not indicated which repository to search, ask them to choose. Show available_projects and available_groups as options. IMPORTANT: project_groups maps each project to the group(s) it belongs to — if the project you would pick is listed there, prefer group= over project= so related repos (e.g. a separate config or import-data repo) are searched too."
        });
        payload.to_string()
    }

    /// Execute a read-only action against the vector store with an explicit store override.
    ///
    /// If `store_override` is provided (from project/group routing), it takes precedence.
    async fn with_vector_store_read_for<R, F>(
        &self,
        mut action: F,
        store_override: Option<Arc<SharedStores>>,
    ) -> Result<R>
    where
        F: FnMut(&VectorStore) -> anyhow::Result<R>,
    {
        // Priority 1: explicit store override (from project/group routing)
        if let Some(stores) = store_override {
            let store = stores.vector_store.read().await;
            return action(&store).context("Error reading from project-routed vector store");
        }

        // Priority 2: shared stores (set during IndexManager init)
        if let Some(ref stores) = self.shared_stores {
            let store = stores.vector_store.read().await;
            match action(&store) {
                Ok(result) => return Ok(result),
                Err(shared_err) => {
                    tracing::error!("Shared vector store read failed: {:?}", shared_err);

                    // In serve mode, do NOT fall back to a standalone VectorStore —
                    // it would open a second LMDB handle on the same .codesearch.db,
                    // which LMDB rejects ("environment already opened with different options").
                    if self.serve_state.is_some() {
                        return Err(shared_err.context(
                            "Shared vector store read failed in serve mode; \
                             standalone fallback disabled to prevent LMDB double-open",
                        ));
                    }
                }
            }

            // Standalone readonly fallback (non-serve mode only).
            // In serve mode the guard above already returned.
            if stores.readonly {
                let ro_store = VectorStore::open_readonly(&self.db_path, self.dimensions)
                    .context("Error opening readonly database for read fallback")?;
                return action(&ro_store)
                    .context("Error reading from readonly fallback vector store");
            }
        }

        // Standalone fallback (non-serve mode only — CLI / stdio MCP).
        // In serve mode, either Priority 1/2 succeeded or the guard above returned Err.
        let store = VectorStore::new(&self.db_path, self.dimensions)
            .context("Error opening database for read fallback")?;
        action(&store).context("Error reading from vector store")
    }

    /// Execute a read-only action against the FTS store with an explicit store override.
    ///
    /// If `store_override` is provided (from project/group routing), it takes precedence.
    async fn with_fts_store_read_for<R, F>(
        &self,
        action: F,
        store_override: Option<Arc<SharedStores>>,
    ) -> Result<R>
    where
        F: Fn(&FtsStore) -> Result<R>,
    {
        // Priority 1: explicit store override (from project/group routing)
        if let Some(stores) = store_override {
            let fts = stores.fts_store.read().await;
            return action(&fts);
        }

        // Priority 2: shared stores
        if let Some(ref stores) = self.shared_stores {
            let fts = stores.fts_store.read().await;
            return action(&fts);
        }

        // Fallback: open a new FtsStore
        let fts_store = FtsStore::new(&self.db_path).context("Error opening FTS store")?;
        action(&fts_store)
    }

    /// Fan-out vector store read across multiple stores, merging results.
    ///
    /// Runs `action` against each store and merges all results into a single vec,
    /// deduplicating by (alias, chunk_id) (keeping highest score) and sorting by score descending.
    ///
    /// A per-store failure does NOT abort the fan-out — one broken repo should
    /// not blind a group query to the healthy ones — but it is reported back in
    /// [`MultiReadOutcome::failures`] so the caller can tell an genuinely empty
    /// result apart from a total failure.
    async fn with_vector_store_read_multi<R, F>(
        &self,
        mut action: F,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
    ) -> Result<MultiReadOutcome<R>>
    where
        F: FnMut(&VectorStore) -> anyhow::Result<Vec<R>>,
        R: Clone + HasChunkId + HasScore,
    {
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut all_results: Vec<R> = Vec::new();
        let mut seen_ids: std::collections::HashMap<(String, u32), usize> =
            std::collections::HashMap::new();

        for (idx, store_arc) in stores.iter().enumerate() {
            let alias = aliases.get(idx).map(|s| s.as_str()).unwrap_or("unknown");
            let store = store_arc.vector_store.read().await;
            match action(&store) {
                Ok(results) => {
                    for r in results {
                        let key = (alias.to_string(), r.chunk_id());
                        if let Some(&existing_idx) = seen_ids.get(&key) {
                            // Keep the one with higher score
                            if r.score() > all_results[existing_idx].score() {
                                all_results[existing_idx] = r;
                            }
                        } else {
                            seen_ids.insert(key, all_results.len());
                            all_results.push(r);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Vector store read failed for multi-store fan-out (alias {}): {:?}",
                        alias,
                        e
                    );
                    // Remembered, not just logged: a caller that only sees an
                    // empty Vec cannot tell "nothing matched" from "every store
                    // failed", and the second one must never be reported as a
                    // successful empty search.
                    failures.push((alias.to_string(), format!("{e:#}")));
                }
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(MultiReadOutcome {
            results: all_results,
            failures,
        })
    }

    /// Fan-out FTS store read across multiple stores, merging results.
    ///
    /// Runs `action` against each store and merges all results into a single vec,
    /// deduplicating by (alias, chunk_id) (keeping highest score) and sorting by score descending.
    ///
    /// Like the vector fan-out, a per-store failure does not abort the query but
    /// IS reported in [`MultiReadOutcome::failures`]. The literal path is not
    /// hypothetical here: during the cloud read-only incident every affected
    /// vendor returned 0 results for literal search too, and it looked clean.
    async fn with_fts_store_read_multi<R, F>(
        &self,
        mut action: F,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
    ) -> Result<MultiReadOutcome<R>>
    where
        F: FnMut(&FtsStore) -> Result<Vec<R>>,
        R: Clone + HasChunkId + HasScore,
    {
        let mut failures: Vec<(String, String)> = Vec::new();
        let mut all_results: Vec<R> = Vec::new();
        let mut seen_ids: std::collections::HashMap<(String, u32), usize> =
            std::collections::HashMap::new();

        for (idx, store_arc) in stores.iter().enumerate() {
            let alias = aliases.get(idx).map(|s| s.as_str()).unwrap_or("unknown");
            let fts = store_arc.fts_store.read().await;
            match action(&fts) {
                Ok(results) => {
                    for r in results {
                        let key = (alias.to_string(), r.chunk_id());
                        if let Some(&existing_idx) = seen_ids.get(&key) {
                            if r.score() > all_results[existing_idx].score() {
                                all_results[existing_idx] = r;
                            }
                        } else {
                            seen_ids.insert(key, all_results.len());
                            all_results.push(r);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "FTS store read failed for multi-store fan-out (alias {}): {:?}",
                        alias,
                        e
                    );
                    // Same contract as the vector fan-out: a swallowed failure
                    // must not reach the caller as an ordinary empty result.
                    failures.push((alias.to_string(), format!("{e:#}")));
                }
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(MultiReadOutcome {
            results: all_results,
            failures,
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Federation — cross-instance query merging (remote peers in a group).
    // ─────────────────────────────────────────────────────────────────

    /// Load the current repos config: from the live serve state when available,
    /// else straight from disk (stdio mode). A missing disk file yields the
    /// default (empty) config — federation simply has no peers to query.
    fn federation_config(&self) -> crate::db_discovery::repos::ReposConfig {
        if let Some(ref ss) = self.serve_state {
            return ss.config_snapshot();
        }
        crate::db_discovery::repos::ReposConfig::load().unwrap_or_default()
    }

    /// True when the given group fans out to at least one **mounted** remote
    /// project. A group that references `@peer` but where the user has mounted
    /// none of that peer's individual indexes is treated as local-only.
    fn group_has_remotes(cfg: &crate::db_discovery::repos::ReposConfig, group: &str) -> bool {
        !cfg.group_remote_projects(group).is_empty()
    }

    /// Merge local + remote search results for a group that has `@<peer>`
    /// members. Runs the local query (restricted to the group's local repos),
    /// fans out in parallel to each **mounted** remote project of the referenced
    /// peers (`<peer>/<alias>`, opt-in `remote_mounts`), then RRF-interleaves the
    /// disjoint ranked lists. One unreachable project becomes a `warning`, never
    /// a hard failure.
    /// Build the JSON body shipped to a remote peer for a federated search
    /// (group fan-out or single-project fan-out). Both call sites forward the
    /// same fields — `mode` and `limit_value` are passed explicitly because
    /// each caller computes them slightly differently (single lowercased
    /// `mode` string shared across a whole request; a per-call `limit_value`
    /// that may be over-fetched to compensate for client-side `filter_path`
    /// filtering). Extracted so the two bodies can't drift out of sync.
    fn build_remote_search_body(
        request: &SearchRequest,
        mode: &str,
        limit_value: Option<usize>,
    ) -> serde_json::Value {
        serde_json::json!({
            "query": request.query,
            "mode": mode,
            "compact": request.compact,
            "semantic_mode": request.semantic_mode,
            "regex": request.regex,
            "phrase": request.phrase,
            "file_glob": request.file_glob,
            "language": request.language,
            "format": request.format,
            "limit": limit_value,
        })
    }

    async fn federated_search(
        &self,
        request: &SearchRequest,
        cfg: &crate::db_discovery::repos::ReposConfig,
        remote_projects: Vec<(String, crate::db_discovery::repos::RemotePeer, String)>,
    ) -> Result<CallToolResult, McpError> {
        use crate::federation::{FederationClient, Outcome};
        use crate::rerank::DEFAULT_RRF_K;

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        let limit = request.limit.unwrap_or(10);
        let group = request.group.clone().unwrap_or_default();

        // `filter_path` is applied CLIENT-SIDE (retain_by_filter_path) on the
        // namespaced result paths for BOTH the local and remote lists, never
        // forwarded to a peer nor down into the local group search — matching
        // against a store's own paths (wrong project root in serve mode) drops
        // everything. Over-fetch when a filter is set so enough survives.
        let has_filter = is_meaningful_filter(request.filter_path.as_deref());
        let fetch_limit = if has_filter {
            Some(request.limit.map(|l| (l * 10).max(50)).unwrap_or(50))
        } else {
            request.limit
        };

        // 1) Local results — internal handlers ignore `@remote` group members
        //    (they aren't local aliases), so they search only the group's local
        //    repos. Skip entirely when the group has no local repos.
        let (locals, _) = cfg.split_group_targets(&group);
        let mut local_items: Vec<SearchResultItem> = Vec::new();
        if !locals.is_empty() {
            let local_result = match mode.as_str() {
                "semantic" => {
                    let req = SemanticSearchRequest {
                        query: request.query.clone(),
                        limit: fetch_limit,
                        compact: request.compact,
                        filter_path: None,
                        mode: request.semantic_mode.clone(),
                        project: None,
                        group: Some(group.clone()),
                    };
                    self.semantic_search(Parameters(req)).await?
                }
                "literal" => {
                    let req = LiteralSearchRequest {
                        query: request.query.clone(),
                        regex: request.regex,
                        phrase: request.phrase,
                        limit: fetch_limit,
                        file_glob: request.file_glob.clone(),
                        language: request.language.clone(),
                        format: request.format.clone(),
                        project: None,
                        group: Some(group.clone()),
                    };
                    self.literal_search(Parameters(req)).await?
                }
                _ => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Unknown search mode '{}'. Use `semantic` or `literal`.",
                        mode
                    ))]));
                }
            };
            local_items = parse_search_items_from_call_result(&local_result, &mode);
            retain_by_filter_path(&mut local_items, request.filter_path.as_deref());
        }

        // 2) Build the request body shipped to each remote (group forced to the
        //    peer's own scope + project stripped by the federation client).
        //    `filter_path` intentionally omitted — applied client-side below.
        let body = Self::build_remote_search_body(request, &mode, fetch_limit);

        let client = match FederationClient::new() {
            Ok(c) => c,
            Err(e) => {
                // Can't build the HTTP client at all — degrade to local-only.
                return Ok(self.build_federated_response(
                    local_items,
                    vec![format!("federation disabled (http client error): {e}")],
                ));
            }
        };

        // 3) Fan out to each mounted remote project concurrently. Each is a
        //    project-scoped query (`project=<remote_alias>`) to its peer, so a
        //    group only ever searches the indexes the user opted into.
        let mut join = tokio::task::JoinSet::new();
        for (peer_name, peer, remote_alias) in remote_projects.into_iter() {
            let body = body.clone();
            let client = client.clone();
            join.spawn(async move {
                let outcome = client.search_project(&peer, body, &remote_alias).await;
                (peer_name, remote_alias, outcome)
            });
        }

        let mut warnings: Vec<String> = Vec::new();
        let mut all_lists: Vec<Vec<SearchResultItem>> = vec![local_items];
        while let Some(res) = join.join_next().await {
            match res {
                Ok((peer_name, remote_alias, Outcome::Ok(items))) => {
                    let mut converted: Vec<SearchResultItem> = items
                        .into_iter()
                        .map(|it| convert_remote_item(&peer_name, &remote_alias, it))
                        .collect();
                    retain_by_filter_path(&mut converted, request.filter_path.as_deref());
                    all_lists.push(converted);
                }
                Ok((peer_name, remote_alias, Outcome::Unreachable(reason))) => {
                    warnings.push(format!(
                        "remote project '{}/{}' unreachable: {}",
                        peer_name, remote_alias, reason
                    ));
                }
                Err(joinerr) => {
                    warnings.push(format!("federation task failed: {joinerr}"));
                }
            }
        }

        // 4) RRF-interleave the disjoint ranked lists and render.
        let merged = merge_ranked_lists(all_lists, DEFAULT_RRF_K, limit);
        Ok(self.build_federated_response(merged, warnings))
    }

    /// Query a single mounted remote project (`project=<peer>/<alias>`).
    ///
    /// A 1-to-1 passthrough: the query is forwarded to `peer` scoped to its own
    /// `remote_alias` project, and the peer's results are re-namespaced so
    /// `chunk_ref`s route back through `federated_get_chunk`. There is no local
    /// list to merge, but results still pass through `merge_ranked_lists` (a
    /// single list), so item `score`s are the RRF rank score, keeping rendering
    /// identical to the group path. An unreachable peer yields a warning with
    /// zero results rather than a hard error.
    async fn federated_project_search(
        &self,
        request: &SearchRequest,
        peer_name: String,
        peer: crate::db_discovery::repos::RemotePeer,
        remote_alias: String,
    ) -> Result<CallToolResult, McpError> {
        use crate::federation::{FederationClient, Outcome};
        use crate::rerank::DEFAULT_RRF_K;

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        let limit = request.limit.unwrap_or(10);

        // `filter_path` is applied CLIENT-SIDE (see retain_by_filter_path) on the
        // namespaced result paths, NOT forwarded to the peer — a server-side
        // match against the peer's own store paths returns 0 for any value. When
        // a filter is set we over-fetch from the peer so enough survives the
        // post-filter to still fill `limit`.
        let has_filter = is_meaningful_filter(request.filter_path.as_deref());
        let peer_limit = if has_filter {
            Some(request.limit.map(|l| (l * 10).max(50)).unwrap_or(50))
        } else {
            request.limit
        };

        // Same shape as the group fan-out body; the federation client forces
        // `project=<remote_alias>` and strips `group`. `filter_path` is
        // intentionally omitted — applied client-side below.
        let body = Self::build_remote_search_body(request, &mode, peer_limit);

        let client = match FederationClient::new() {
            Ok(c) => c,
            Err(e) => {
                return Ok(self.build_federated_response(
                    vec![],
                    vec![format!("federation disabled (http client error): {e}")],
                ));
            }
        };

        let outcome = client.search_project(&peer, body, &remote_alias).await;
        let (mut items, warnings) = match outcome {
            Outcome::Ok(items) => (
                items
                    .into_iter()
                    .map(|it| convert_remote_item(&peer_name, &remote_alias, it))
                    .collect::<Vec<_>>(),
                Vec::new(),
            ),
            Outcome::Unreachable(reason) => (
                Vec::new(),
                vec![format!(
                    "remote project '{}/{}' unreachable: {}",
                    peer_name, remote_alias, reason
                )],
            ),
        };

        // Client-side path scoping on the namespaced result paths.
        retain_by_filter_path(&mut items, request.filter_path.as_deref());

        // Single ranked list — RRF here is order-preserving and just caps to
        // `limit`, keeping rendering identical to the group path.
        let merged = merge_ranked_lists(vec![items], DEFAULT_RRF_K, limit);
        Ok(self.build_federated_response(merged, warnings))
    }

    /// Fetch a chunk from a remote peer by its namespaced `chunk_ref`.
    async fn federated_get_chunk(
        &self,
        chunk_ref: &str,
        context_lines: Option<usize>,
    ) -> Result<CallToolResult, McpError> {
        use crate::federation::{FederationClient, Outcome};

        let (peer_name, remote_alias, chunk_id) = match parse_federated_chunk_ref(chunk_ref) {
            Some(parts) => parts,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Invalid chunk_ref '{}': expected '<peer>/<alias>:<chunk_id>'.",
                    chunk_ref
                ))]));
            }
        };
        let cfg = self.federation_config();
        let peer = match cfg.remotes.get(peer_name) {
            Some(p) => p.clone(),
            None => {
                let known: Vec<String> = cfg.remotes.keys().cloned().collect();
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Unknown remote peer '{}' in chunk_ref '{}'. Known remotes: {}",
                    peer_name,
                    chunk_ref,
                    known.join(", ")
                ))]));
            }
        };
        let client = match FederationClient::new() {
            Ok(c) => c,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "federation disabled (http client error): {e}"
                ))]));
            }
        };
        match client
            .get_chunk(&peer, remote_alias, chunk_id, context_lines)
            .await
        {
            Outcome::Ok(value) => Ok(CallToolResult::success(vec![Content::text(
                value.to_string(),
            )])),
            Outcome::Unreachable(reason) => {
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Could not fetch chunk from remote peer '{}': {}",
                    peer_name, reason
                ))]))
            }
        }
    }

    /// Render the merged federated results as a `SemanticSearchResponse` JSON.
    ///
    /// `low_confidence` is only flagged when the merged set is empty: RRF-fused
    /// scores (`1/(k+rank+1)`, max ≈ 0.048 for k=20) are NOT comparable to the
    /// single-source embedding/BM25 thresholds, so applying any score cutoff
    /// here would be meaningless. An empty result, however, is a genuine signal
    /// to the agent that federation yielded nothing and it should try a broader
    /// query or a different scope.
    fn build_federated_response(
        &self,
        items: Vec<SearchResultItem>,
        warnings: Vec<String>,
    ) -> CallToolResult {
        let response = SemanticSearchResponse {
            low_confidence: if items.is_empty() { Some(true) } else { None },
            results: items,
            suggested_tool: None,
            warnings: if warnings.is_empty() {
                None
            } else {
                Some(warnings)
            },
        };
        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        CallToolResult::success(vec![Content::text(json)])
    }

    // ─────────────────────────────────────────────────────────────────
    // Consolidated tools (the primary 5-tool surface)
    // ─────────────────────────────────────────────────────────────────

    /// Unified search tool — dispatches to semantic or literal search based on `mode`.
    #[tool(
        description = "Unified code search. Set `mode` to choose the backend:\n\n- `semantic` (default): vector embeddings + BM25 FTS + exact-identifier boosting, fused with RRF. Best for conceptual queries, identifier lookups, and mixed natural-language + symbol queries.\n- `literal`: pure FTS, no embeddings. Fast and works without an embedding model. Sub-mode selection:\n  * Queries with operators, brackets, or punctuation (`foo = null`, `Vec<T>`, `return x;`, `a::b`) -> set `regex=true` and write the query as a regex. BM25 tokenizes on punctuation otherwise, producing noisy results.\n  * Multi-word exact phrases -> set `phrase=true`.\n  * Plain identifier lookups (`CodesearchService`) -> leave both false.\n\nFor semantic mode, optionally set `semantic_mode`: \"auto\" (default) | \"semantic\" | \"lexical\" | \"hybrid\".\nReturns metadata only by default (`compact=true`). Use `get_chunk` to read full code. Prefer `search(mode=\"literal\", regex=true)` over external grep/ripgrep for code patterns.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 search(query={:?}, mode={:?}, project={:?}, group={:?})",
            request.query,
            request.mode,
            request.project,
            request.group,
        );

        // Federation: when the query targets a group that resolves to one or more
        // remote peers, merge local + remote results (RRF-interleave) instead of
        // searching local repos only. Only `group` federates; `project` stays
        // local because project aliases are instance-local.
        if let Some(group) = request.group.as_deref() {
            let cfg = self.federation_config();
            if Self::group_has_remotes(&cfg, group) {
                let remote_projects = cfg.group_remote_projects(group);
                return self.federated_search(&request, &cfg, remote_projects).await;
            }
        }

        // Project-level federation (mounted remote project): a `project` of the
        // form "<peer>/<alias>" transparently routes to that single peer's own
        // `<alias>` project — a 1-to-1 passthrough, as if the index were local.
        // Local repos ALWAYS win a name clash: only route remotely when the name
        // does not resolve to a local project.
        if let Some(proj) = request.project.as_deref() {
            let cfg = self.federation_config();
            if cfg.resolve(proj).is_none() {
                if let Some(crate::db_discovery::repos::Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                }) = cfg.resolve_remote_project(proj)
                {
                    return self
                        .federated_project_search(&request, peer_name, peer, remote_alias)
                        .await;
                }
            }
        }

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        match mode.as_str() {
            "semantic" => {
                // Delegate to the existing semantic_search implementation
                let semantic_req = SemanticSearchRequest {
                    query: request.query,
                    limit: request.limit,
                    compact: request.compact,
                    filter_path: request.filter_path,
                    mode: request.semantic_mode,
                    project: request.project,
                    group: request.group,
                };
                self.semantic_search(Parameters(semantic_req)).await
            }
            "literal" => {
                // Delegate to the existing literal_search implementation
                let literal_req = LiteralSearchRequest {
                    query: request.query,
                    regex: request.regex,
                    phrase: request.phrase,
                    limit: request.limit,
                    file_glob: request.file_glob,
                    language: request.language,
                    format: request.format,
                    project: request.project,
                    group: request.group,
                };
                self.literal_search(Parameters(literal_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown search mode '{}'. Use `semantic` or `literal`.",
                mode
            ))])),
        }
    }

    /// Unified symbol navigation — dispatches based on `kind`.
    #[tool(
        description = "Unified symbol navigation. Set `kind` to choose the action:\n\n- `definition` (default): locate where a symbol is defined (function, class, struct, etc.)\n- `usages`: find all call-sites and references to a symbol (lexical/text-based; for IDE-precise call-graphs prefer `find_impact`)\n- `imports`: list all imports/dependencies declared in a file (set `symbol` to the file path)\n- `dependents`: find all files that import or depend on a module, file, or symbol\n\nFor `imports`, set `symbol` to a file path. For other kinds, `symbol` is the symbol name.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    async fn find(
        &self,
        Parameters(request): Parameters<FindRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request
            .kind
            .as_deref()
            .unwrap_or("definition")
            .to_lowercase();
        tracing::info!(
            "📥 find(symbol={:?}, kind={}, project={:?}, group={:?})",
            request.symbol,
            kind,
            request.project,
            request.group,
        );
        match kind.as_str() {
            "definition" => {
                let def_req = FindDefinitionRequest {
                    symbol: request.symbol,
                    kind: request.definition_kind,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_definition(Parameters(def_req)).await
            }
            "usages" => {
                let usages_req = FindUsagesRequest {
                    symbol: request.symbol,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_usages(Parameters(usages_req)).await
            }
            "imports" => {
                let imports_req = FindImportsRequest {
                    path: request.symbol,
                    project: request.project,
                    group: request.group,
                };
                self.find_imports(Parameters(imports_req)).await
            }
            "dependents" => {
                let dep_req = FindDependentsRequest {
                    symbol_or_path: request.symbol,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_dependents(Parameters(dep_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown find kind '{}'. Use `definition`, `usages`, `imports`, or `dependents`.",
                kind
            ))])),
        }
    }

    /// Unified exploration tool — dispatches based on `kind`.
    #[tool(
        description = "Unified code exploration. Set `kind` to choose the action:\n\n- `outline` (default): list all indexed top-level symbols in a file — kind, signature, and line range. Set `target` to a file path.\n- `similar`: find chunks semantically similar to a given chunk by its ID. Set `target` to the chunk_id (as string).\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    async fn explore(
        &self,
        Parameters(request): Parameters<ExploreRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request.kind.as_deref().unwrap_or("outline").to_lowercase();
        tracing::info!(
            "📥 explore(target={:?}, kind={}, project={:?})",
            request.target,
            kind,
            request.project,
        );
        match kind.as_str() {
            "outline" => {
                let outline_req = FileOutlineRequest {
                    path: request.target,
                    project: request.project,
                    group: request.group,
                };
                self.file_outline(Parameters(outline_req)).await
            }
            "similar" => {
                let chunk_id = match request.target.parse::<u32>() {
                    Ok(id) => id,
                    Err(_) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "For similar mode, `target` must be a numeric chunk_id, got: '{}'",
                            request.target
                        ))]));
                    }
                };
                let similar_req = SimilarChunksRequest {
                    chunk_id,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.similar_chunks(Parameters(similar_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown explore kind '{}'. Use `outline` or `similar`.",
                kind
            ))])),
        }
    }

    /// Unified status tool — dispatches based on `kind`.
    #[tool(
        description = "Unified status/info tool. Set `kind` to choose the action:\n\n- `index` (default): get the status of the local search index (model info, chunk count, readiness)\n- `projects`: list all registered projects/repositories, groups, and their index status"
    )]
    async fn status(
        &self,
        Parameters(request): Parameters<StatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request.kind.as_deref().unwrap_or("index").to_lowercase();
        tracing::info!("📥 status(kind={})", kind);
        match kind.as_str() {
            "index" => self.index_status_impl(request.project, request.group).await,
            "projects" => self.list_projects().await,
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown status kind '{}'. Use `index` or `projects`.",
                kind
            ))])),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Internal implementations (called by consolidated tools above)
    // ─────────────────────────────────────────────────────────────────

    /// Internal: semantic/hybrid search implementation used by `search(mode="semantic")`.
    async fn semantic_search(
        &self,
        Parameters(request): Parameters<SemanticSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing (multi-store for group fan-out)
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "search")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        let limit = request.limit.unwrap_or(10);
        let compact = request.compact.unwrap_or(true);
        let mode = request.mode.as_deref().unwrap_or("auto");
        let identifiers = detect_identifiers(&request.query);
        let has_identifiers = !identifiers.is_empty();

        tracing::debug!(
            "MCP semantic_search: query='{}', limit={}, compact={}, mode='{}', multi={}",
            request.query,
            limit,
            compact,
            mode,
            ctx.is_multi
        );

        // Ensure database exists (skip if serve-mode with routed stores)
        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // === Multi-store group fan-out ===
        if ctx.is_multi {
            return self
                .semantic_search_multi(
                    &request,
                    &identifiers,
                    limit,
                    compact,
                    ctx.stores_vec.unwrap(),
                    ctx.store_aliases.as_ref().unwrap(),
                    &ctx.alias_roots,
                )
                .await;
        }

        // === Mode: "lexical" — FTS only, no embedding ===
        if mode == "lexical" {
            tracing::debug!("MCP: mode=lexical — skipping embedding service");
            return self
                .semantic_search_lexical(
                    &request,
                    &identifiers,
                    limit,
                    compact,
                    ctx.stores,
                    ctx.project_alias.as_deref(),
                    &ctx.alias_roots,
                )
                .await;
        }

        // === Modes: "semantic", "hybrid", "auto" — require embedding ===
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("MCP: Failed to get embedding service: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };

            let service = service_guard.as_mut().unwrap();
            tracing::debug!("MCP: Embedding query...");
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("MCP: Failed to embed query: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
            }
        };

        // Failures on this single-store path. The group fan-out has carried a
        // warnings channel since the read-only incident; without the same thing
        // here, `project=<alias>` — the form an agent uses most — still reports
        // a broken store as an ordinary empty result.
        let mut single_warnings: Vec<String> = Vec::new();

        // Search vector store
        let vector_results = match self
            .with_vector_store_read_for(
                |store| {
                    store
                        .search(&query_embedding, limit * 5)
                        .context("Error searching vector store")
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("MCP: Search failed: {:?}", e);
                // Only "semantic" has no second backend to fall back on. In
                // hybrid/auto the FTS half can still answer, so hard-failing
                // here would throw away good results — the same mistake this
                // branch already fixed once in the group fan-out.
                //
                // `{:#}` renders the whole anyhow chain. With plain `{}` the
                // caller only ever saw the outermost `.context(...)` wrapper
                // ("Error reading from project-routed vector store"), which
                // hides the actual fault and makes remote diagnosis guesswork.
                if mode == "semantic" {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching vector store: {:#}",
                        e
                    ))]));
                }
                single_warnings.push(format!("vector search failed: {e:#}"));
                Vec::new()
            }
        };

        tracing::debug!("MCP: Found {} vector results", vector_results.len());

        // === Mode: "semantic" — vector only, skip FTS fusion ===
        if mode == "semantic" {
            tracing::debug!("MCP: mode=semantic — using vector results only");
            let fused = vector_only(&vector_results);

            let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
                vector_results.iter().map(|r| (r.id, r)).collect();

            let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
            for f in fused.into_iter().take(limit) {
                if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                    let mut r = (*result).clone();
                    r.score = f.rrf_score;
                    results.push(r);
                }
            }
            return self.build_semantic_response(
                results,
                &request,
                compact,
                has_identifiers,
                ctx.project_alias.as_deref(),
                &ctx.alias_roots,
                &single_warnings,
            );
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let structural_intent = detect_structural_intent(&request.query);
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        tracing::debug!(
            "MCP: Query analysis - identifiers: {:?}, structural_intent: {:?}, rrf_k: ({}, {})",
            identifiers,
            structural_intent,
            vector_k,
            fts_k
        );

        // Perform FTS search and fusion
        let mut results = match self
            .with_fts_store_read_for(
                |fts_store| {
                    let fts_results = fts_store
                        .search(&request.query, limit * 5, structural_intent)
                        .context("Error searching FTS store")?;

                    let fused = if identifiers.is_empty() {
                        rrf_fusion(&vector_results, &fts_results, vector_k as f32)
                    } else {
                        let mut all_exact: Vec<crate::fts::FtsResult> = Vec::new();
                        for ident in &identifiers {
                            if let Ok(exact) =
                                fts_store.search_exact(ident, limit * 3, structural_intent)
                            {
                                for r in exact {
                                    if !all_exact.iter().any(|e| e.chunk_id == r.chunk_id) {
                                        all_exact.push(r);
                                    }
                                }
                            }
                        }

                        tracing::debug!(
                            "MCP: FTS found {} results, exact found {} results",
                            fts_results.len(),
                            all_exact.len()
                        );

                        rrf_fusion_with_exact(
                            &vector_results,
                            &fts_results,
                            &all_exact,
                            vector_k as f32,
                            fts_k as f32,
                            EXACT_MATCH_RRF_K,
                        )
                    };

                    Ok(fused)
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(fused) => {
                // Map FusedResult back to SearchResult
                let chunk_to_result: std::collections::HashMap<
                    u32,
                    &crate::vectordb::SearchResult,
                > = vector_results.iter().map(|r| (r.id, r)).collect();

                let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
                for f in fused.into_iter().take(limit) {
                    if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                        let mut r = (*result).clone();
                        r.score = f.rrf_score;
                        mapped.push(r);
                    }
                }
                mapped
            }
            Err(e) => {
                tracing::warn!("MCP: FTS store unavailable, using vector-only: {:?}", e);
                // Degrading to vector-only is correct, but it must be VISIBLE:
                // a caller that gets half a hybrid search with no signal cannot
                // tell it from a complete one.
                single_warnings.push(format!("lexical (FTS) search failed: {e:#}"));
                vector_results.into_iter().take(limit).collect()
            }
        };

        // Apply language boost
        if let Some((_, _, Some(primary_lang))) = crate::search::read_metadata(&self.db_path) {
            for result in &mut results {
                let file_lang = format!(
                    "{:?}",
                    Language::from_path(std::path::Path::new(&result.path))
                );
                if file_lang.to_lowercase() == primary_lang.to_lowercase() {
                    result.score *= 1.2;
                }
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        // Auto-fallback: if hybrid search returned very few results for a code-like query,
        // run literal FTS and merge missing chunks.
        if results.len() < 3 && has_identifiers {
            tracing::debug!(
                "Auto-fallback: semantic returned {} results, trying literal",
                results.len()
            );

            let literal_results = self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&request.query, limit, None),
                    ctx.stores.clone(),
                )
                .await
                .unwrap_or_default();

            let mut existing_ids: std::collections::HashSet<u32> =
                results.iter().map(|r| r.id).collect();

            for fts in literal_results {
                if results.len() >= limit {
                    break;
                }
                if existing_ids.contains(&fts.chunk_id) {
                    continue;
                }

                let maybe_resolved = self
                    .with_vector_store_read_for(
                        |store| {
                            if let Ok(Some(chunk)) = store.get_chunk(fts.chunk_id) {
                                Ok(Some(crate::vectordb::SearchResult {
                                    id: fts.chunk_id,
                                    content: chunk.content,
                                    path: chunk.path,
                                    start_line: chunk.start_line,
                                    end_line: chunk.end_line,
                                    kind: chunk.kind,
                                    signature: chunk.signature,
                                    docstring: chunk.docstring,
                                    context: chunk.context,
                                    hash: chunk.hash,
                                    distance: 0.0,
                                    score: fts.score,
                                    context_prev: chunk.context_prev,
                                    context_next: chunk.context_next,
                                }))
                            } else {
                                Ok(None)
                            }
                        },
                        ctx.stores.clone(),
                    )
                    .await
                    .ok()
                    .flatten();

                if let Some(resolved) = maybe_resolved {
                    existing_ids.insert(resolved.id);
                    results.push(resolved);
                }
            }
        }

        tracing::debug!("MCP: Final {} results after hybrid search", results.len());
        self.build_semantic_response(
            results,
            &request,
            compact,
            has_identifiers,
            ctx.project_alias.as_deref(),
            &ctx.alias_roots,
            &single_warnings,
        )
    }

    // === Helper methods (not exposed as tools) ===

    /// Multi-store semantic search: fan out across all stores, merge raw vector/FTS
    /// results, then apply RRF fusion.
    #[allow(clippy::too_many_arguments)]
    async fn semantic_search_multi(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        let mode = request.mode.as_deref().unwrap_or("auto");
        let structural_intent = detect_structural_intent(&request.query);

        // === Lexical mode: FTS only across all stores ===
        if mode == "lexical" {
            // Lexical has no second backend, so a failed store here is invisible
            // unless it is reported: the query simply looks like it found nothing.
            let mut lexical_warnings: Vec<String> = Vec::new();

            let outcome = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                    stores.clone(),
                    aliases,
                )
                .await
                .unwrap_or_default();
            if !outcome.failures.is_empty() {
                tracing::error!(
                    "MCP: lexical fan-out degraded — {} of {} repo(s) failed: {:?}",
                    outcome.failures.len(),
                    stores.len(),
                    outcome.failures
                );
                lexical_warnings.extend(outcome.warnings("literal search"));
            }
            let fts_results = outcome.results;

            // Also do exact search if identifiers detected
            let mut all_fts = fts_results;
            for ident in identifiers {
                let exact_outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                lexical_warnings.extend(exact_outcome.warnings("exact-identifier search"));
                merge_exact_into_fts(&mut all_fts, exact_outcome.results);
            }

            all_fts.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let results = self
                .resolve_fts_to_search_results_multi(
                    &all_fts,
                    limit,
                    &stores,
                    aliases,
                    &mut lexical_warnings,
                )
                .await;

            if let Some(target_kind) = structural_intent {
                // We need mutable results but we have them as vectordb::SearchResult
                let mut mutable_results = results;
                boost_kind(&mut mutable_results, target_kind);
                return self.build_semantic_response(
                    mutable_results,
                    request,
                    compact,
                    !identifiers.is_empty(),
                    None,
                    alias_roots,
                    &lexical_warnings,
                );
            }

            return self.build_semantic_response(
                results,
                request,
                compact,
                !identifiers.is_empty(),
                None,
                alias_roots,
                &lexical_warnings,
            );
        }

        // === Modes requiring embedding: "semantic", "hybrid", "auto" ===
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };
            let service = service_guard.as_mut().unwrap();
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
            }
        };

        // Search vector stores across all repos
        let outcome = self
            .with_vector_store_read_multi(
                |store| {
                    store
                        .search(&query_embedding, limit * 5)
                        .context("Error searching vector store")
                },
                stores.clone(),
                aliases,
            )
            .await;

        // Warnings raised by the fan-out, carried into the response so the
        // calling agent can tell "not in the corpus" from "that repo is down".
        let mut search_warnings: Vec<String> = Vec::new();

        let vector_results =
            match outcome {
                Ok(o) => {
                    if !o.failures.is_empty() {
                        tracing::error!(
                            "MCP: vector fan-out degraded — {} of {} repo(s) failed: {:?}",
                            o.failures.len(),
                            stores.len(),
                            o.failures
                        );
                        // Only "semantic" has no second backend to fall back on. In
                        // hybrid/auto/lexical the FTS half can still answer, so
                        // hard-failing here would throw away good results — the same
                        // reason one broken repo does not abort the whole fan-out.
                        if mode == "semantic" && o.results.is_empty() {
                            let detail = o
                                .failures
                                .iter()
                                .map(|(alias, err)| format!("  - {alias}: {err}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            return Ok(CallToolResult::success(vec![Content::text(format!(
                                "Error searching vector store: {} of {} repo(s) in scope failed \
                             and none returned results:\n{}",
                                o.failures.len(),
                                stores.len(),
                                detail
                            ))]));
                        }
                        search_warnings.extend(o.failures.iter().map(|(alias, err)| {
                            format!("repo '{alias}' vector search failed: {err}")
                        }));
                    }
                    o.results
                }
                Err(e) => {
                    tracing::error!("MCP: vector fan-out failed: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching vector store: {e:#}"
                    ))]));
                }
            };

        // === Mode: "semantic" — vector only ===
        if mode == "semantic" {
            let fused = vector_only(&vector_results);
            let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
                vector_results.iter().map(|r| (r.id, r)).collect();

            let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
            for f in fused.into_iter().take(limit) {
                if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                    let mut r = (*result).clone();
                    r.score = f.rrf_score;
                    results.push(r);
                }
            }
            return self.build_semantic_response(
                results,
                request,
                compact,
                !identifiers.is_empty(),
                None,
                alias_roots,
                &search_warnings,
            );
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        // FTS search across all stores. Its failures matter as much as the
        // vector half's: during the cloud read-only incident literal search
        // also returned 0 results for every affected vendor, and looked clean.
        let fts_outcome = self
            .with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                stores.clone(),
                aliases,
            )
            .await
            .unwrap_or_default();
        if !fts_outcome.failures.is_empty() {
            tracing::error!(
                "MCP: FTS fan-out degraded — {} of {} repo(s) failed: {:?}",
                fts_outcome.failures.len(),
                stores.len(),
                fts_outcome.failures
            );
            search_warnings.extend(fts_outcome.warnings("literal search"));
        }
        let fts_results = fts_outcome.results;

        // Exact identifier search across all stores
        let all_exact = if !identifiers.is_empty() {
            let mut exact_results: Vec<crate::fts::FtsResult> = Vec::new();
            for ident in identifiers {
                let exact_outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                search_warnings.extend(exact_outcome.warnings("exact-identifier search"));
                for r in exact_outcome.results {
                    if !exact_results.iter().any(|e| e.chunk_id == r.chunk_id) {
                        exact_results.push(r);
                    }
                }
            }
            exact_results
        } else {
            Vec::new()
        };

        // RRF fusion
        let fused = if identifiers.is_empty() {
            rrf_fusion(&vector_results, &fts_results, vector_k as f32)
        } else {
            rrf_fusion_with_exact(
                &vector_results,
                &fts_results,
                &all_exact,
                vector_k as f32,
                fts_k as f32,
                EXACT_MATCH_RRF_K,
            )
        };

        // Map FusedResult back to SearchResult via chunk lookup across all stores
        let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
            vector_results.iter().map(|r| (r.id, r)).collect();

        let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
        for f in fused.into_iter().take(limit) {
            if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                let mut r = (*result).clone();
                r.score = f.rrf_score;
                mapped.push(r);
            } else {
                // Chunk from FTS but not in vector results — resolve from stores
                if let Some(resolved) = self
                    .resolve_chunk_from_stores(
                        f.chunk_id,
                        f.rrf_score,
                        &stores,
                        aliases,
                        &mut search_warnings,
                    )
                    .await
                {
                    mapped.push(resolved);
                }
            }
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut mapped, target_kind);
        }

        self.build_semantic_response(
            mapped,
            request,
            compact,
            !identifiers.is_empty(),
            None,
            alias_roots,
            &search_warnings,
        )
    }

    /// Resolve a single chunk from multiple stores (used for FTS-only hits in multi-store fusion).
    async fn resolve_chunk_from_stores(
        &self,
        chunk_id: u32,
        score: f32,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        warnings: &mut Vec<String>,
    ) -> Option<crate::vectordb::SearchResult> {
        for (idx, store_arc) in stores.iter().enumerate() {
            let store = store_arc.vector_store.read().await;
            let looked_up = store.get_chunk(chunk_id);
            if let Err(ref e) = looked_up {
                note_store_failure(warnings, aliases, idx, "chunk lookup", e);
            }
            if let Ok(Some(chunk)) = looked_up {
                return Some(crate::vectordb::SearchResult {
                    id: chunk_id,
                    content: chunk.content,
                    path: chunk.path,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    kind: chunk.kind,
                    signature: chunk.signature,
                    docstring: chunk.docstring,
                    context: chunk.context,
                    hash: chunk.hash,
                    distance: 0.0,
                    score,
                    context_prev: chunk.context_prev,
                    context_next: chunk.context_next,
                });
            }
        }
        None
    }

    /// Resolve FTS results to SearchResult using multiple stores.
    async fn resolve_fts_to_search_results_multi(
        &self,
        fts_results: &[crate::fts::FtsResult],
        limit: usize,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let mut results = Vec::new();
        for fts in fts_results.iter().take(limit) {
            for (idx, store_arc) in stores.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                let looked_up = store.get_chunk(fts.chunk_id);
                if let Err(ref e) = looked_up {
                    // `Ok(None)` means "this store does not hold that chunk" and
                    // is normal during fan-out; `Err` means the store is broken.
                    // Collapsing the two is how a dead vector store renders as
                    // an empty literal search — the exact shape of the step-8
                    // incident, which tantivy-side checks cannot detect.
                    note_store_failure(warnings, aliases, idx, "chunk lookup", e);
                }
                if let Ok(Some(chunk)) = looked_up {
                    results.push(crate::vectordb::SearchResult {
                        id: fts.chunk_id,
                        content: chunk.content,
                        path: chunk.path,
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                        kind: chunk.kind,
                        signature: chunk.signature,
                        docstring: chunk.docstring,
                        context: chunk.context,
                        hash: chunk.hash,
                        distance: 0.0,
                        score: fts.score,
                        context_prev: chunk.context_prev,
                        context_next: chunk.context_next,
                    });
                    break; // Found in this store, skip remaining stores
                }
            }
        }
        results
    }

    /// Lexical-only search: FTS without embedding service.
    #[allow(clippy::too_many_arguments)]
    async fn semantic_search_lexical(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
        stores: Option<Arc<SharedStores>>,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        let structural_intent = detect_structural_intent(&request.query);

        // `project=`-scoped queries route here, not through the fan-out
        // (`is_multi` requires >1 store), so this path needs the same failure
        // reporting — it is at least as common as a group query.
        let mut lexical_warnings: Vec<String> = Vec::new();

        let mut fts_results = match self
            .with_fts_store_read_for(
                |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("literal search failed: {e:#}");
                tracing::error!("MCP: {}", msg);
                lexical_warnings.push(msg);
                Vec::new()
            }
        };

        // Also do exact search if identifiers detected
        for ident in identifiers {
            let exact = match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                    stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("exact-identifier search for '{ident}' failed: {e:#}");
                    tracing::error!("MCP: {}", msg);
                    lexical_warnings.push(msg);
                    continue;
                }
            };
            merge_exact_into_fts(&mut fts_results, exact);
        }

        fts_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Resolve FTS results to chunk metadata
        let mut results = self
            .resolve_fts_to_search_results(&fts_results, limit, stores, &mut lexical_warnings)
            .await;

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        self.build_semantic_response(
            results,
            request,
            compact,
            !identifiers.is_empty(),
            project_alias,
            alias_roots,
            &lexical_warnings,
        )
    }

    /// Build the final SemanticSearchResponse with low-confidence signaling.
    // Eight parameters, one over clippy's threshold. Bundling them into a
    // `ResponseContext` struct is the right end state and is recorded as a
    // follow-up; doing it in an incident fix would touch all seven call sites
    // for no behavioural gain. The alternative — dropping `warnings` — is not
    // acceptable: without it a failed repo is silently reported as "no match".
    #[allow(clippy::too_many_arguments)]
    fn build_semantic_response(
        &self,
        results: Vec<crate::vectordb::SearchResult>,
        request: &SemanticSearchRequest,
        compact: bool,
        has_identifiers: bool,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
        // Repos that failed during a fan-out. MUST reach the caller: the
        // consumer of this tool is a remote agent that never sees the server
        // log, so a silently omitted repo reads as "no match there" — a false
        // negative. The federated path already does this (`warnings` on the
        // remote-project fan-out); the local path never could.
        warnings: &[String],
    ) -> Result<CallToolResult, McpError> {
        let warnings = if warnings.is_empty() {
            None
        } else {
            Some(warnings.to_vec())
        };
        if results.is_empty() {
            let response = SemanticSearchResponse {
                results: vec![],
                low_confidence: Some(true),
                suggested_tool: retry_hint(Some("literal_search".to_string()), &warnings),
                warnings,
            };
            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Pre-compute normalized project root for stripping absolute paths
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };

        let mut items: Vec<SearchResultItem> = results
            .into_iter()
            .filter(|r| {
                if let Some(ref fp) = request.filter_path {
                    let normalized_filter = crate::cache::normalize_filter_path(fp);
                    if normalized_filter.is_empty() {
                        return true;
                    }
                    // Relativise against the ROUTED project's root, not the
                    // service's own project_path — otherwise a serve-routed
                    // absolute path never strips and every hit is dropped.
                    let filter_root = pick_filter_root(
                        &r.path,
                        project_alias,
                        alias_roots,
                        &project_root_normalized,
                    );
                    crate::cache::path_matches_filter(&r.path, &normalized_filter, &filter_root)
                } else {
                    true
                }
            })
            .map(|r| SearchResultItem {
                chunk_id: r.id,
                path: r.path,
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind,
                score: r.score,
                signature: r.signature,
                content: if compact { None } else { Some(r.content) },
                context_prev: if compact { None } else { r.context_prev },
                context_next: if compact { None } else { r.context_next },
                source: None,
                chunk_ref: None,
            })
            .collect();

        // Prefix paths with alias for multi-repo / single-project identification
        for item in &mut items {
            if let Some(alias) = project_alias {
                if let Some(root) = alias_roots.get(alias) {
                    item.path = prefix_path_with_alias(&item.path, Some(alias), root);
                } else {
                    item.path = crate::cache::normalize_path_str(&item.path);
                }
            } else if !alias_roots.is_empty() {
                item.path = prefix_path_multi(&item.path, &[], alias_roots);
            }
        }

        // Check low-confidence: top result's RRF score below threshold
        let top_score = items.first().map(|r| r.score);
        let (low_confidence, suggested_tool) = compute_low_confidence(top_score, has_identifiers);
        let suggested_tool = retry_hint(suggested_tool, &warnings);

        let response = SemanticSearchResponse {
            results: items,
            low_confidence,
            suggested_tool,
            warnings,
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Resolve FTS results to SearchResult by looking up chunk metadata.
    async fn resolve_fts_to_search_results(
        &self,
        fts_results: &[crate::fts::FtsResult],
        limit: usize,
        stores: Option<Arc<SharedStores>>,
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let outcome = self
            .with_vector_store_read_for(
                |store| {
                    let mut results = Vec::new();
                    for fts in fts_results.iter().take(limit) {
                        // A failed lookup is not an absent chunk. Propagating the
                        // error keeps a broken vector store from rendering as an
                        // ordinary empty literal search.
                        let chunk = store
                            .get_chunk(fts.chunk_id)
                            .context("Error resolving FTS hit to chunk metadata")?;
                        if let Some(chunk) = chunk {
                            results.push(crate::vectordb::SearchResult {
                                id: fts.chunk_id,
                                content: chunk.content,
                                path: chunk.path,
                                start_line: chunk.start_line,
                                end_line: chunk.end_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                docstring: chunk.docstring,
                                context: chunk.context,
                                hash: chunk.hash,
                                distance: 0.0,
                                score: fts.score,
                                context_prev: chunk.context_prev,
                                context_next: chunk.context_next,
                            });
                        }
                    }
                    Ok(results)
                },
                stores,
            )
            .await;
        match outcome {
            Ok(results) => results,
            Err(e) => {
                let msg = format!("literal search could not read the index: {e:#}");
                tracing::error!("MCP: {}", msg);
                if !warnings.contains(&msg) {
                    warnings.push(msg);
                }
                Vec::new()
            }
        }
    }

    // === find_definition internal ===

    /// Internal: find symbol definitions, used by `find(kind="definition")`.
    async fn find_definition(
        &self,
        Parameters(request): Parameters<FindDefinitionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(20);

        tracing::debug!(
            "MCP find_definition: symbol='{}', kind={:?}, limit={}",
            request.symbol,
            request.kind,
            limit
        );

        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // Stores that failed during this lookup. Without this, "the symbol may
        // not be indexed" below is emitted as a confident diagnosis even when
        // no store ever answered.
        let mut find_warnings: Vec<String> = Vec::new();

        // FTS search — multi-store or single
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.symbol, limit * 3, None),
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
            .into_results(&mut find_warnings, "definition search")
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&request.symbol, limit * 3, None),
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

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                qualify_empty_result(
                    format!(
                        "No definition found for '{}'. The symbol may not be indexed.",
                        request.symbol
                    ),
                    &find_warnings,
                ),
            )]));
        }

        // Resolve chunk metadata and filter by definition kinds
        let requested_kind = request.kind.clone();
        let mut items: Vec<ReferenceItem> = if let Some(ref sv) = ctx.stores_vec {
            let mut items: Vec<ReferenceItem> = Vec::new();
            'outer: for fts_result in &fts_results {
                for store_arc in sv {
                    let store = store_arc.vector_store.read().await;
                    if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                        // Skip non-definition kinds — try next FTS result, not next store
                        if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                            continue 'outer;
                        }
                        if let Some(ref rk) = requested_kind {
                            if chunk.kind != *rk {
                                continue 'outer;
                            }
                        }
                        items.push(ReferenceItem {
                            chunk_id: fts_result.chunk_id,
                            path: chunk.path,
                            line: chunk.start_line,
                            kind: chunk.kind,
                            signature: chunk.signature,
                            score: fts_result.score,
                        });
                        if items.len() >= limit {
                            break 'outer;
                        }
                        break; // Found in this store — move to next FTS result
                    }
                }
                // If we get here, chunk wasn't found in any store — just skip it
            }
            items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let items = fts_results
                            .iter()
                            .filter_map(|fts_result| {
                                if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                                    if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                                        return None;
                                    }
                                    if let Some(ref requested_kind) = requested_kind {
                                        if chunk.kind != *requested_kind {
                                            return None;
                                        }
                                    }
                                    Some(ReferenceItem {
                                        chunk_id: fts_result.chunk_id,
                                        path: chunk.path,
                                        line: chunk.start_line,
                                        kind: chunk.kind,
                                        signature: chunk.signature,
                                        score: fts_result.score,
                                    })
                                } else {
                                    None
                                }
                            })
                            .take(limit)
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
                        "Error opening database: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        respond_with_items(&items, &find_warnings, || {
            format!(
                "No definition found for '{}'. Try find_usages() to find references, \
                 or broaden your search.",
                request.symbol
            )
        })
    }

    // === find_usages tool ===

    async fn find_usages(
        &self,
        Parameters(request): Parameters<FindUsagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.find_usages_impl(
            request.symbol.clone(),
            request.limit.unwrap_or(20),
            request.project,
            request.group,
        )
        .await
    }

    /// Shared implementation for find_usages (used by `find(kind="usages")`).
    async fn find_usages_impl(
        &self,
        symbol: String,
        limit: usize,
        project: Option<String>,
        group: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        tracing::debug!("MCP find_usages: symbol='{}', limit={}", symbol, limit);

        // Resolve project/group routing
        let ctx = match self.resolve_routing(&project, &group, false, "find").await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // See `find_definition`: an empty result and a dead store must not
        // produce the same sentence.
        let mut find_warnings: Vec<String> = Vec::new();

        // FTS search — multi-store or single
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| fts_store.search(&symbol, limit * 2, None),
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
            .into_results(&mut find_warnings, "usage search")
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&symbol, limit * 2, None),
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

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                qualify_empty_result(
                    format!("No usages found for '{symbol}'. The symbol may not be indexed."),
                    &find_warnings,
                ),
            )]));
        }

        // Resolve chunks and exclude definition chunks
        let mut items: Vec<ReferenceItem> = if let Some(ref sv) = ctx.stores_vec {
            let mut items: Vec<ReferenceItem> = Vec::new();
            for fts_result in &fts_results {
                for store_arc in sv {
                    let store = store_arc.vector_store.read().await;
                    if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                        if !is_definition_chunk(&chunk.kind, &chunk.signature, &symbol) {
                            items.push(ReferenceItem {
                                chunk_id: fts_result.chunk_id,
                                path: chunk.path,
                                line: chunk.start_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                score: fts_result.score,
                            });
                        }
                        break;
                    }
                }
                if items.len() >= limit {
                    break;
                }
            }
            items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let items = fts_results
                            .iter()
                            .filter_map(|fts_result| {
                                if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                                    if is_definition_chunk(&chunk.kind, &chunk.signature, &symbol) {
                                        return None;
                                    }
                                    Some(ReferenceItem {
                                        chunk_id: fts_result.chunk_id,
                                        path: chunk.path,
                                        line: chunk.start_line,
                                        kind: chunk.kind,
                                        signature: chunk.signature,
                                        score: fts_result.score,
                                    })
                                } else {
                                    None
                                }
                            })
                            .take(limit)
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
                        "Error opening database: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        respond_with_items(&items, &find_warnings, || {
            format!(
                "No usages found for '{symbol}' (only definitions were found). Try \
                 find_definition() to locate the declaration."
            )
        })
    }

    /// Fetch outline items for an already-normalised absolute path.
    ///
    /// Returns `Ok(vec![])` when no chunks match.
    /// In multi-store mode, per-store I/O failures are recorded in `warnings` and
    /// skipped (never `Err`) so one broken repo cannot blank the whole outline.
    /// In single-store mode, I/O failures are returned as `Err`.
    ///
    /// `warnings` is not optional: without it a failed store is indistinguishable
    /// from a file with no indexed chunks, and the caller is told the file is not
    /// indexed — a diagnosis, and a wrong one.
    async fn outline_items_for_normalized(
        &self,
        normalized: &str,
        ctx: &MultiStoreContext,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<Vec<FileOutlineItem>> {
        if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut all_items: Vec<FileOutlineItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(normalized) {
                    Ok(metas) => {
                        for c in metas {
                            if seen_ids.insert(c.id) {
                                all_items.push(FileOutlineItem {
                                    chunk_id: c.id,
                                    kind: c.kind,
                                    signature: c.signature,
                                    start_line: c.start_line,
                                    end_line: c.end_line,
                                });
                            }
                        }
                    }
                    Err(ref e) => {
                        note_store_failure(warnings, aliases, store_idx, "outline scan", e);
                    }
                }
            }
            all_items.sort_by_key(|i| i.start_line);
            Ok(all_items)
        } else {
            let normalized_owned = normalized.to_string();
            self.with_vector_store_read_for(
                move |store| {
                    let mut out: Vec<FileOutlineItem> = store
                        .chunks_for_file(&normalized_owned)?
                        .into_iter()
                        .map(|c| FileOutlineItem {
                            chunk_id: c.id,
                            kind: c.kind,
                            signature: c.signature,
                            start_line: c.start_line,
                            end_line: c.end_line,
                        })
                        .collect();
                    out.sort_by_key(|i| i.start_line);
                    Ok(out)
                },
                ctx.stores.clone(),
            )
            .await
        }
    }

    async fn file_outline(
        &self,
        Parameters(request): Parameters<FileOutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "explore")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Outline operates on a single repo — reject group fan-out
        if ctx.is_multi {
            return Ok(CallToolResult::success(vec![Content::text(
                "Tool 'explore' operates on a single repo. Use 'project' instead of 'group'."
                    .to_string(),
            )]));
        }

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // In serve mode, use the resolved project root from alias_roots;
        // self.project_path is "serve://multi-repo" which doesn't resolve.
        let project_root = if let Some(ref alias) = ctx.project_alias {
            ctx.alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone())
        } else {
            self.project_path.clone()
        };
        // Strip project-alias prefix from target path if present.
        // E.g. "ExampleRepo/src/foo.cs" with project="ExampleRepo" → "src/foo.cs"
        let stripped_path = strip_alias_prefix(&request.path, ctx.project_alias.as_ref());
        let normalized = normalize_tool_path(&stripped_path, &project_root);

        let mut outline_warnings: Vec<String> = Vec::new();
        let mut items = match self
            .outline_items_for_normalized(&normalized, &ctx, &mut outline_warnings)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error reading outline: {e:#}"
                ))]));
            }
        };

        // Two-pass fallback: if alias-stripping changed the path and yielded no results,
        // try the original un-stripped path. Handles the case where the project alias
        // matches a package subdirectory name (e.g. project "my_pkg" with target
        // "my_pkg/config.py" → after strip becomes "config.py" which is wrong;
        // the correct relative path is "my_pkg/config.py").
        if items.is_empty() && stripped_path != request.path {
            let normalized_orig = normalize_tool_path(&request.path, &project_root);
            if normalized_orig != normalized {
                tracing::debug!(
                    "file_outline: primary '{}' empty, trying fallback '{}'",
                    normalized,
                    normalized_orig
                );
                items = match self
                    .outline_items_for_normalized(&normalized_orig, &ctx, &mut outline_warnings)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "file_outline: fallback '{}' also failed: {:?}",
                            normalized_orig,
                            e
                        );
                        push_store_warning(
                            &mut outline_warnings,
                            &store_warning(
                                ctx.project_alias.as_deref().unwrap_or("unknown"),
                                "outline scan",
                                &format!("{e:#}"),
                            ),
                        );
                        Vec::new()
                    }
                };
            }
        }

        respond_with_items(&items, &outline_warnings, || {
            "No indexed chunks found for path. Verify the file is within the \
             project root and the index is up to date."
                .to_string()
        })
    }

    #[tool(
        description = "Retrieve the full content of a specific chunk by its ID, plus optional surrounding lines for context.\nUse this after search or explore to read the actual code without loading the whole file.\n\nUSE FOR: reading a specific function/class body after finding it via search.\nSet context_lines (default 0, max 20) to include lines before and after the chunk.\n\nIMPORTANT (multi-repo): chunk_ids are local to each repository and are NOT globally unique.\nWhen `project` is omitted in multi-repo mode, the tool scans all repositories for the chunk_id.\nIf found in exactly one repo, it is returned automatically. If found in multiple repos, an `ambiguous_chunk_id` error lists the candidates so you can retry with `project`."
    )]
    async fn get_chunk(
        &self,
        Parameters(request): Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 get_chunk(chunk_id={}, project={:?})",
            request.chunk_id,
            request.project,
        );

        // Federation: a `chunk_ref` of the form "<peer>/<remote_alias>:<chunk_id>"
        // (returned by a federated search result) fetches the chunk from a remote
        // peer rather than the local index. The alias scopes the fetch to a single
        // remote project so the multi-repo peer can disambiguate the chunk_id.
        if let Some(chunk_ref) = request.chunk_ref.as_deref() {
            return self
                .federated_get_chunk(chunk_ref, request.context_lines)
                .await;
        }

        // In multi-repo serve mode, require explicit project or group scope.
        // Unscoped get_chunk would fan-out over all repos, opening all DBs unnecessarily.
        // Consistent with search/find/explore which also require scope.
        if request.project.is_none() && request.group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let config = serve_state.config_snapshot();
                if config.repos.len() > 1 {
                    return Ok(CallToolResult::success(vec![Content::text(
                        self.format_scope_error(),
                    )]));
                }
            }
        }

        // Resolve project/group routing — allow unscoped only for single-repo mode
        let ctx = match self
            .resolve_routing(&request.project, &request.group, true, "get_chunk")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let mut clamped = false;
        let mut context_lines = request.context_lines.unwrap_or(0);
        if context_lines > 20 {
            context_lines = 20;
            clamped = true;
        }

        // Stores that failed while looking up this chunk. get_chunk previously
        // collapsed every `Err` into "not found", so during the read-only
        // incident it would have reported every chunk in every vendor repo as
        // missing — a confident, wrong answer.
        let mut chunk_warnings: Vec<String> = Vec::new();

        // Look up chunk — multi-store: smart candidate detection for chunk_id collision.
        // chunk_ids are local per database, not globally unique. When no project is specified
        // and multiple stores are active, scan all stores to find which ones have this chunk_id.
        let chunk = if let Some(ref sv) = ctx.stores_vec {
            if sv.len() > 1 && request.project.is_none() {
                // Smart candidate detection: find which stores actually contain this chunk_id
                let mut candidates: Vec<(&Arc<SharedStores>, String)> = Vec::new();
                let aliases = ctx.aliases();
                for (i, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(request.chunk_id) {
                        Ok(Some(_)) => {
                            // A store that HAS the chunk stays a candidate even if
                            // its alias is missing. `resolve_repo_stores_multi`
                            // keeps stores and aliases the same length, so this is
                            // unreachable today — but gating the push on
                            // `aliases.get(i)` meant a future break of that
                            // invariant would degrade to a silent auto-route rather
                            // than a loud one. The placeholder is per-index so two
                            // aliasless candidates stay distinguishable in
                            // `candidate_projects`.
                            let alias = aliases
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("<unnamed store #{i}>"));
                            candidates.push((store_arc, alias));
                        }
                        Ok(None) => continue,
                        Err(ref e) => {
                            note_store_failure(&mut chunk_warnings, aliases, i, "chunk lookup", e);
                            continue;
                        }
                    }
                }
                match candidates.len() {
                    0 => {
                        return Ok(CallToolResult::success(vec![Content::text(
                            qualify_empty_result(
                                format!(
                                    "Chunk {} not found in any repository. Verify the \
                                     chunk_id and index state.",
                                    request.chunk_id
                                ),
                                &chunk_warnings,
                            ),
                        )]));
                    }
                    1 => {
                        // Exactly one store has this chunk_id — auto-route
                        let (store_arc, ref alias) = candidates[0];
                        // Record tool call for the specific repo that served this chunk
                        if let Some(ref serve_state) = self.serve_state {
                            serve_state.record_tool_call(alias, "get_chunk");
                            serve_state.touch_access(alias);
                        }
                        let store = store_arc.vector_store.read().await;
                        match store.get_chunk(request.chunk_id) {
                            Ok(c) => c,
                            Err(ref e) => {
                                push_store_warning(
                                    &mut chunk_warnings,
                                    &store_warning(alias, "chunk lookup", &format!("{e:#}")),
                                );
                                None
                            }
                        }
                    }
                    _ => {
                        // Multiple stores have this chunk_id — ambiguous.
                        //
                        // `candidate_projects` reads as the complete list, so a
                        // store that failed to answer has to be declared: the
                        // right repo may be the one missing from it.
                        let candidate_names: Vec<&str> =
                            candidates.iter().map(|(_, a)| a.as_str()).collect();
                        let payload = ambiguous_chunk_payload(
                            request.chunk_id,
                            &candidate_names,
                            &chunk_warnings,
                        );
                        return Ok(CallToolResult::success(vec![Content::text(
                            payload.to_string(),
                        )]));
                    }
                }
            } else {
                // Single store or project specified — direct lookup
                let aliases = ctx.aliases();
                let mut found = None;
                for (i, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(request.chunk_id) {
                        Ok(Some(c)) => {
                            found = Some(c);
                            break;
                        }
                        Ok(None) => continue,
                        // Do NOT abandon the remaining stores: one broken store
                        // says nothing about the others, and the chunk may well
                        // live in a healthy one.
                        Err(ref e) => {
                            note_store_failure(&mut chunk_warnings, aliases, i, "chunk lookup", e);
                            continue;
                        }
                    }
                }
                found
            }
        } else {
            match self
                .with_vector_store_read_for(
                    |store| store.get_chunk(request.chunk_id),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    push_store_warning(
                        &mut chunk_warnings,
                        &store_warning(
                            ctx.project_alias.as_deref().unwrap_or("unknown"),
                            "chunk lookup",
                            &format!("{e:#}"),
                        ),
                    );
                    None
                }
            }
        };

        let mut chunk = match chunk {
            Some(c) => c,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(
                    qualify_empty_result(
                        format!(
                            "Chunk {} not found. Verify the chunk_id and index state.",
                            request.chunk_id
                        ),
                        &chunk_warnings,
                    ),
                )]));
            }
        };

        // Prefix path with alias for multi-repo identification
        chunk.path = ctx.prefix_result_path(&chunk.path);

        let mut context_before = None;
        let mut context_after = None;
        let mut note = None;

        if context_lines > 0 {
            // Resolve relative chunk paths against project root (not process CWD).
            let source_path = if Path::new(&chunk.path).is_absolute() {
                PathBuf::from(&chunk.path)
            } else {
                self.project_path.join(&chunk.path)
            };
            match tokio::fs::read_to_string(&source_path).await {
                Ok(src) => {
                    let lines: Vec<&str> = src.lines().collect();
                    if !lines.is_empty() {
                        let before_start = chunk.start_line.saturating_sub(context_lines);
                        let before_end = chunk.start_line.min(lines.len());
                        if before_start < before_end {
                            context_before = Some(lines[before_start..before_end].join("\n"));
                        }

                        let after_start = chunk.end_line.min(lines.len());
                        let after_end = (chunk.end_line + context_lines).min(lines.len());
                        if after_start < after_end {
                            context_after = Some(lines[after_start..after_end].join("\n"));
                        }
                    }
                }
                Err(_) => {
                    note = Some(
                        "source file not readable, returning indexed content only".to_string(),
                    );
                }
            }
        }

        let response = GetChunkResponse {
            chunk_id: request.chunk_id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            kind: chunk.kind,
            signature: chunk.signature,
            content: chunk.content,
            context_before,
            context_after,
            context_lines_clamped: if clamped { Some(true) } else { None },
            note,
        };

        // The success path is the one that used to drop this, and it is the
        // dangerous one: a confidently-returned chunk from a group where a store
        // failed to answer looks exactly like a chunk from a healthy group. Same
        // false negative as an empty result, harder to notice.
        respond_with_object(&response, &chunk_warnings)
    }

    /// Symbol impact analysis — returns transitive call-sites of a symbol with file/line precision.
    ///
    /// The recommended tool for "who calls X?" / "what breaks if I rename X?". Uses
    /// language-specific semantic analysis (SCIP) to find all references, enabling agents
    /// to plan refactors with IDE-class accuracy instead of text-matching grep heuristics.
    /// Precision backends ship per language: C# (bundled `scip-csharp` helper,
    /// `-with-csharp` releases) and TypeScript (`scip-typescript`, resolved via `npx`
    /// or `CODESEARCH_SCIP_TYPESCRIPT`). If no backend is installed for the target
    /// language, the response reports it — fall back to `find` with `kind="usages"`
    /// (lexical) only then.
    #[tool(
        description = "Symbol impact analysis — find all references to a symbol with IDE-class precision (SCIP).\n\nThe right tool for \"who calls X?\" / \"what breaks if I rename X?\". Returns transitive call-sites with file/line precision, enabling agents to plan refactors without missing a caller. More accurate than text-based `find kind=\"usages\"` because it understands language semantics.\n\nInput variants:\n- By name: `{ \"symbol_name\": \"FieldDefinition.Validate\", \"project\": \"myrepo\" }`\n- By position: `{ \"file\": \"src/Validation/FieldDefinition.cs\", \"line\": 42, \"project\": \"myrepo\" }`\n\nPrecision backends (SCIP) ship per language; C# (bundled `scip-csharp` helper, `-with-csharp` releases) and TypeScript (via `npx` or `CODESEARCH_SCIP_TYPESCRIPT`) are available today. For Rust/Python/Go/etc., use `find` with `kind=\"usages\"` as a text-based fallback until SCIP backends for those languages ship.\n\nIMPORTANT (multi-repo): always specify `project` (single repo). Omitting `project` in multi-repo mode returns a `scope_required` error."
    )]
    async fn find_impact(
        &self,
        Parameters(request): Parameters<FindImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 find_impact(symbol_name={:?}, file={:?}, line={:?}, language={:?}, project={:?})",
            request.symbol_name,
            request.file,
            request.line,
            request.language,
            request.project,
        );

        // Validate input: must provide either symbol_name or file+line
        let has_name = request
            .symbol_name
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_position = request.file.is_some() && request.line.is_some();
        if !has_name && !has_position {
            return Ok(CallToolResult::success(vec![Content::text(
                "Must provide either `symbol_name` or both `file` and `line` for position-based lookup.".to_string(),
            )]));
        }

        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find_impact")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Determine project root and db_path for the symbol index
        let (project_root, db_path) = if let Some(ref alias) = ctx.project_alias {
            let root = ctx
                .alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone());
            // The symbol index DB lives alongside the vector DB
            let db = root.join(crate::constants::DB_DIR_NAME);
            (root, db)
        } else {
            // Single-repo / stdio mode: use the service's own paths
            (self.project_path.clone(), self.db_path.clone())
        };

        // Use the shared symbol indexer registry
        let registry = &self.symbol_registry;

        // Determine which language to use
        let language = request.language.clone().or_else(|| {
            // Auto-detect from file extension
            request.file.as_ref().and_then(|f| {
                let ext = Path::new(f).extension()?.to_str()?.to_lowercase();
                match ext.as_str() {
                    "cs" => Some(crate::constants::LANG_CSHARP.to_string()),
                    "ts" | "tsx" | "mts" | "cts" => {
                        Some(crate::constants::LANG_TYPESCRIPT.to_string())
                    }
                    _ => None,
                }
            })
        });

        let indexer: &dyn crate::symbols::SymbolIndexer = match language {
            Some(ref lang) => match registry.get(lang) {
                Some(i) => i,
                None => {
                    let available = registry.available_languages();
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "No symbol indexer for language '{}'. Available languages: {:?}",
                        lang, available
                    ))]));
                }
            },
            None => {
                // No language specified and couldn't auto-detect — try all installed
                let installed = registry.installed_languages();
                if installed.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No symbol indexers installed. Install the `scip-csharp` helper for C# support, or `scip-typescript` (via npx) for TypeScript support.".to_string(),
                    )]));
                }
                // Use the first installed language (MVP: C# or TypeScript)
                match registry.get(&installed[0]) {
                    Some(i) => i,
                    None => {
                        unreachable!("installed_languages() returned a language with no indexer")
                    }
                }
            }
        };

        // Check if the helper is available
        if !indexer.is_available() {
            let error = crate::symbols::SymbolIndexError {
                error: format!(
                    "Symbol indexer for '{}' is not available. The helper binary is not installed.",
                    indexer.language()
                ),
                available_languages: registry.available_languages(),
                hint_for_agent: format!(
                    "Install the `-with-csharp` release variant, or set {} to the helper path.",
                    crate::constants::SCIP_CSHARP_HELPER_ENV
                ),
            };
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&error).unwrap_or_else(|_| error.error.clone()),
            )]));
        }

        // Perform the lookup.
        //
        // `find_references` may invoke `scip-csharp find-refs` on a cache miss
        // (lazy Opt-2 reference resolution). That subprocess can take several minutes
        // on a large solution, so we use `block_in_place` to avoid blocking the async
        // executor thread. `block_in_place` is safe here: we are inside a
        // multi-threaded tokio runtime and do not hold any async locks.
        let file_for_pos = if !has_name {
            Some(self.normalize_symbol_query_path(
                &project_root,
                Path::new(request.file.as_ref().unwrap()),
            ))
        } else {
            None
        };
        let symbol_name_for_lookup = request.symbol_name.clone();
        let line_for_lookup = request.line;
        let result = tokio::task::block_in_place(|| {
            if has_name {
                indexer.find_references(&db_path, symbol_name_for_lookup.as_ref().unwrap())
            } else {
                indexer.find_references_by_position(
                    &db_path,
                    &file_for_pos.unwrap(),
                    line_for_lookup.unwrap(),
                )
            }
        });

        match result {
            Ok(references) => {
                let age = indexer.index_age(&db_path);
                let impact = crate::symbols::FindImpactResult {
                    symbol: request.symbol_name.clone().unwrap_or_else(|| {
                        format!(
                            "{}:{}",
                            request.file.as_deref().unwrap_or("?"),
                            request.line.unwrap_or(0)
                        )
                    }),
                    references,
                    index_age_seconds: age,
                    language: indexer.language().to_string(),
                    scope: ctx
                        .project_alias
                        .map(|a| format!("project:{}", a))
                        .unwrap_or_else(|| "local".to_string()),
                };
                let json = serde_json::to_string(&impact).unwrap_or_else(|_| "{}".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Symbol lookup failed: {e:#}"
            ))])),
        }
    }

    fn normalize_symbol_query_path(&self, project_root: &Path, file: &Path) -> PathBuf {
        if file.is_absolute() {
            if let Ok(relative) = file.strip_prefix(project_root) {
                return PathBuf::from(relative.to_string_lossy().replace('\\', "/"));
            }
        }

        PathBuf::from(file.to_string_lossy().replace('\\', "/"))
    }

    async fn find_imports(
        &self,
        Parameters(request): Parameters<FindImportsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // In serve mode, use the resolved project root from alias_roots
        let project_root = if let Some(ref alias) = ctx.project_alias {
            ctx.alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone())
        } else {
            self.project_path.clone()
        };
        // Strip project-alias prefix from target path if present.
        let stripped_path = strip_alias_prefix(&request.path, ctx.project_alias.as_ref());
        let normalized = normalize_tool_path(&stripped_path, &project_root);

        // Stores that failed during this lookup, so "no imports found" is never
        // reported as fact when a store never answered.
        let mut import_warnings: Vec<String> = Vec::new();

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store group fan-out: collect import items from all stores
            let import_aliases = ctx.aliases();
            let mut all_items: Vec<ImportItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(&normalized) {
                    Ok(metas) => {
                        for meta in metas {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if seen_ids.insert(meta.id) {
                                match store.get_chunk(meta.id) {
                                    Ok(Some(chunk)) => all_items.extend(parse_import_lines(
                                        &chunk.content,
                                        chunk.start_line,
                                    )),
                                    Ok(None) => {}
                                    Err(ref e) => note_store_failure(
                                        &mut import_warnings,
                                        import_aliases,
                                        store_idx,
                                        "chunk lookup",
                                        e,
                                    ),
                                }
                            }
                        }
                    }
                    Err(ref e) => {
                        note_store_failure(
                            &mut import_warnings,
                            import_aliases,
                            store_idx,
                            "imports scan",
                            e,
                        );
                    }
                }
            }
            all_items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut out = Vec::new();
                        for meta in store.chunks_for_file(&normalized)? {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if let Some(chunk) = store.get_chunk(meta.id)? {
                                out.extend(parse_import_lines(&chunk.content, chunk.start_line));
                            }
                        }
                        Ok(out)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error reading imports: {e:#}"
                    ))]));
                }
            }
        };

        if items.is_empty() {
            // Fallback: no import-kind chunks found for this file. Broaden the
            // search to common import keywords and filter to the target path.
            // Limitation: this only finds chunks containing these literal words;
            // language-specific import forms that lack these keywords will be missed.
            let fallback_limit = 40usize;
            let mut all_hits: Vec<(u32, f32)> = Vec::new();
            let mut seen_fts_ids: HashSet<u32> = HashSet::new();

            if let Some(ref sv) = ctx.stores_vec {
                let import_aliases = ctx.aliases();
                // Multi-store FTS fallback
                for keyword in IMPORT_FTS_KEYWORDS {
                    let hits = self
                        .with_fts_store_read_multi(
                            |fts_store| fts_store.search_exact(keyword, fallback_limit, None),
                            sv.clone(),
                            ctx.store_aliases.as_ref().unwrap(),
                        )
                        .await
                        .unwrap_or_default()
                        .into_results(&mut import_warnings, "imports search");
                    for h in hits {
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                // Resolve FTS hits via vector stores
                let mut resolved: Vec<ImportItem> = Vec::new();
                for (chunk_id, _) in &all_hits {
                    for (store_idx, store_arc) in sv.iter().enumerate() {
                        let store = store_arc.vector_store.read().await;
                        match store.get_chunk(*chunk_id) {
                            Ok(Some(chunk)) => {
                                if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                    resolved.extend(parse_import_lines(
                                        &chunk.content,
                                        chunk.start_line,
                                    ));
                                }
                                break;
                            }
                            Ok(None) => continue,
                            Err(ref e) => {
                                note_store_failure(
                                    &mut import_warnings,
                                    import_aliases,
                                    store_idx,
                                    "chunk lookup",
                                    e,
                                );
                                continue;
                            }
                        }
                    }
                }
                items = resolved;
            } else {
                // Single-store FTS fallback
                for keyword in IMPORT_FTS_KEYWORDS {
                    let hits = match self
                        .with_fts_store_read_for(
                            |fts_store| fts_store.search_exact(keyword, fallback_limit, None),
                            ctx.stores.clone(),
                        )
                        .await
                    {
                        Ok(h) => h,
                        Err(e) => {
                            push_store_warning(
                                &mut import_warnings,
                                &store_warning(
                                    ctx.project_alias.as_deref().unwrap_or("unknown"),
                                    "imports search",
                                    &format!("{e:#}"),
                                ),
                            );
                            Vec::new()
                        }
                    };
                    for h in hits {
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                items = self
                    .with_vector_store_read_for(
                        |store| {
                            let mut out = Vec::new();
                            for (chunk_id, _) in &all_hits {
                                if let Some(chunk) = store.get_chunk(*chunk_id)? {
                                    if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                        out.extend(parse_import_lines(
                                            &chunk.content,
                                            chunk.start_line,
                                        ));
                                    }
                                }
                            }
                            Ok(out)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        push_store_warning(
                            &mut import_warnings,
                            &store_warning(
                                ctx.project_alias.as_deref().unwrap_or("unknown"),
                                "chunk lookup",
                                &format!("{e:#}"),
                            ),
                        );
                        Vec::new()
                    });
            }
        }

        items.sort_by_key(|i| i.line);
        respond_with_items(&items, &import_warnings, || {
            "No import chunks found. The index may not include import statements \
             for this language, or the file has no imports."
                .to_string()
        })
    }

    async fn find_dependents(
        &self,
        Parameters(request): Parameters<FindDependentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(20).min(200);
        let high_limit = (limit * 10).max(200); // generous budget for filtering

        // Stores that failed during this lookup, so "no dependents" is never
        // reported as fact when a store never answered.
        let mut dep_warnings: Vec<String> = Vec::new();

        // Extract a meaningful search term from path-like inputs.
        // Import chunks contain module references like `use crate::constants::X`
        // but the tool receives file paths like `src/constants.rs`.
        // We extract the file stem to match against module names in imports.
        let search_term = if request.symbol_or_path.contains('/')
            || request.symbol_or_path.contains('\\')
            || request.symbol_or_path.contains('.')
        {
            std::path::Path::new(&request.symbol_or_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&request.symbol_or_path)
                .to_string()
        } else {
            request.symbol_or_path.clone()
        };

        let import_kind = Some(crate::chunker::ChunkKind::Imports);

        // Two-phase search strategy:
        // 1. `search_exact` — precise term match on signature+content with
        //    MUST filter for Import kind. Strictly limits results to import chunks.
        // 2. If that yields no import-kind results, fall back to `search`
        //    (QueryParser, broader tokenization) with kind boost for imports.
        //
        // Limitation: the chunker does not emit per-statement AST import chunks;
        // imports are gap-classified as `Imports` kind. Chunks whose kind doesn't
        // match `is_import_kind()` will be missed regardless of search method.
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            // Multi-store FTS search
            let exact_hits = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search_exact(&search_term, high_limit, import_kind),
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
                .into_results(&mut dep_warnings, "dependents search");

            if exact_hits.is_empty() {
                self.with_fts_store_read_multi(
                    |fts_store| fts_store.search(&search_term, high_limit, import_kind),
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
                .into_results(&mut dep_warnings, "dependents search")
            } else {
                exact_hits
            }
        } else {
            // Single-store FTS search
            let alias = ctx.project_alias.as_deref().unwrap_or("unknown");
            let mut run = |r: anyhow::Result<Vec<crate::fts::FtsResult>>| match r {
                Ok(hits) => hits,
                Err(e) => {
                    push_store_warning(
                        &mut dep_warnings,
                        &store_warning(alias, "dependents search", &format!("{e:#}")),
                    );
                    Vec::new()
                }
            };
            let exact_hits = run(self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search_exact(&search_term, high_limit, import_kind),
                    ctx.stores.clone(),
                )
                .await);

            if exact_hits.is_empty() {
                run(self
                    .with_fts_store_read_for(
                        |fts_store| fts_store.search(&search_term, high_limit, import_kind),
                        ctx.stores.clone(),
                    )
                    .await)
            } else {
                exact_hits
            }
        };

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: resolve chunks across all stores
            let dep_aliases = ctx.aliases();
            let mut seen_paths = HashSet::new();
            let mut out = Vec::new();
            for f in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(f.chunk_id) {
                        Ok(Some(chunk)) => {
                            if !is_import_kind(&chunk.kind) {
                                break; // try next FTS result
                            }

                            let norm = crate::cache::normalize_path_str(&chunk.path);
                            if !seen_paths.insert(norm) {
                                break;
                            }

                            let term_lower = search_term.to_lowercase();
                            let import_statement =
                                if chunk.content.to_lowercase().contains(&term_lower) {
                                    chunk
                                        .content
                                        .lines()
                                        .find(|l| l.to_lowercase().contains(&term_lower))
                                        .unwrap_or("")
                                        .to_string()
                                } else {
                                    chunk.signature.filter(|s| !s.is_empty()).unwrap_or(
                                        chunk.content.lines().next().unwrap_or("").to_string(),
                                    )
                                };

                            out.push(DependentItem {
                                path: chunk.path,
                                line: chunk.start_line,
                                import_statement,
                            });

                            break; // found in this store, move to next FTS result
                        }
                        Ok(None) => {} // try next store
                        // One broken store says nothing about the others; a
                        // `break` here silently drops a chunk that lives in a
                        // healthy store later in the list.
                        Err(ref e) => {
                            note_store_failure(
                                &mut dep_warnings,
                                dep_aliases,
                                store_idx,
                                "chunk lookup",
                                e,
                            );
                            continue;
                        }
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
            out
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut seen_paths = HashSet::new();
                        let mut out = Vec::new();
                        let term_lower = search_term.to_lowercase();
                        for f in &fts_results {
                            if let Some(chunk) = store.get_chunk(f.chunk_id)? {
                                if !is_import_kind(&chunk.kind) {
                                    continue;
                                }

                                let norm = crate::cache::normalize_path_str(&chunk.path);
                                if !seen_paths.insert(norm) {
                                    continue;
                                }

                                // Extract the specific import line(s) that mention the
                                // module name, rather than returning the entire chunk content.
                                let import_statement =
                                    if chunk.content.to_lowercase().contains(&term_lower) {
                                        chunk
                                            .content
                                            .lines()
                                            .find(|l| l.to_lowercase().contains(&term_lower))
                                            .unwrap_or("")
                                            .to_string()
                                    } else {
                                        chunk.signature.filter(|s| !s.is_empty()).unwrap_or(
                                            chunk.content.lines().next().unwrap_or("").to_string(),
                                        )
                                    };

                                out.push(DependentItem {
                                    path: chunk.path,
                                    line: chunk.start_line,
                                    import_statement,
                                });

                                if out.len() >= limit {
                                    break;
                                }
                            }
                        }
                        Ok(out)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error resolving dependents: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        items.sort_by(|a, b| a.path.cmp(&b.path));
        respond_with_items(&items, &dep_warnings, || {
            format!("No dependent files found for '{}'.", request.symbol_or_path)
        })
    }

    /// Internal: find similar chunks, used by `explore(kind="similar")`.
    async fn similar_chunks(
        &self,
        Parameters(request): Parameters<SimilarChunksRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "explore")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(5).min(20);

        // Stores that failed while resolving the source embedding. `if let
        // Ok(Some(..))` used to discard the error, so a dead store produced
        // "embedding not found" — a wrong diagnosis, not a missing chunk.
        let mut similar_warnings: Vec<String> = Vec::new();

        let mut results = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: find the embedding in whichever store has it,
            // then search across all stores for similar chunks.
            let aliases = ctx.aliases();
            let mut embedding: Option<Vec<f32>> = None;
            for (i, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.get_embedding(request.chunk_id) {
                    Ok(Some(emb)) => {
                        embedding = Some(emb);
                        break;
                    }
                    Ok(None) => continue,
                    Err(ref e) => {
                        note_store_failure(
                            &mut similar_warnings,
                            aliases,
                            i,
                            "embedding lookup",
                            e,
                        );
                        continue;
                    }
                }
            }

            let embedding = match embedding {
                Some(e) => e,
                None => {
                    return Ok(CallToolResult::success(vec![Content::text(
                        qualify_empty_result(
                            format!(
                                "Embedding not found for chunk_id {} in any store.",
                                request.chunk_id
                            ),
                            &similar_warnings,
                        ),
                    )]));
                }
            };

            // Search across all stores with the found embedding
            let mut all_results: Vec<SearchResultItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.search(&embedding, limit + 1) {
                    Ok(mut neighbors) => {
                        neighbors.retain(|r| r.id != request.chunk_id);
                        for r in neighbors {
                            if seen_ids.insert(r.id) {
                                all_results.push(SearchResultItem {
                                    chunk_id: r.id,
                                    path: r.path,
                                    start_line: r.start_line,
                                    end_line: r.end_line,
                                    kind: r.kind,
                                    score: r.score,
                                    signature: r.signature,
                                    content: None,
                                    context_prev: None,
                                    context_next: None,
                                    source: None,
                                    chunk_ref: None,
                                });
                            }
                        }
                    }
                    Err(ref e) => {
                        // The embedding was found, so the handler returns results
                        // either way; without this, a group query silently omits
                        // every neighbour from the broken repo.
                        note_store_failure(
                            &mut similar_warnings,
                            aliases,
                            store_idx,
                            "similarity search",
                            e,
                        );
                    }
                }
            }

            all_results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            all_results.truncate(limit);
            all_results
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let embedding =
                            store.get_embedding(request.chunk_id)?.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "embedding not found for chunk_id {}",
                                    request.chunk_id
                                )
                            })?;

                        let mut neighbors = store.search(&embedding, limit + 1)?;
                        neighbors.retain(|r| r.id != request.chunk_id);
                        neighbors.truncate(limit);

                        let items = neighbors
                            .into_iter()
                            .map(|r| SearchResultItem {
                                chunk_id: r.id,
                                path: r.path,
                                start_line: r.start_line,
                                end_line: r.end_line,
                                kind: r.kind,
                                score: r.score,
                                signature: r.signature,
                                content: None,
                                context_prev: None,
                                context_next: None,
                                source: None,
                                chunk_ref: None,
                            })
                            .collect::<Vec<_>>();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error finding similar chunks: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut results {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Every exit carries the channel: the earlier read sat in an
        // early-return arm, so once an embedding was found, every failure
        // recorded afterwards (the whole neighbour fan-out) was discarded.
        respond_with_items(&results, &similar_warnings, || {
            format!("No similar chunks found for chunk_id {}.", request.chunk_id)
        })
    }

    async fn literal_search(
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
                            let items: Vec<LiteralSearchResultItem> = fts_results
                                .iter()
                                .filter_map(|fts_result| {
                                    let chunk = store.get_chunk(fts_result.chunk_id).ok()??;
                                    Some((chunk, fts_result.score))
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

    /// Internal implementation for index_status with optional project/group routing.
    async fn index_status_impl(
        &self,
        project: Option<String>,
        group: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        // When no project/group specified in serve mode, return lightweight aggregated
        // status WITHOUT opening any databases. Only a specific project/group request
        // should trigger DB activation.
        if project.is_none() && group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let config = serve_state.config_snapshot();
                let repo_count = config.repos.len();
                // Count the virtual "all" group when repos are registered, so the
                // summary doesn't read "0 group(s)" while `all` is actually available.
                let group_count = config.groups.len() + if config.repos.is_empty() { 0 } else { 1 };
                let statuses = serve_state.repo_statuses_lightweight();
                let open_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Open))
                    .count();
                let warm_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Warm))
                    .count();
                let closed_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Closed))
                    .count();

                let status = if open_count + warm_count > 0 {
                    "ready".to_string()
                } else if repo_count > 0 {
                    "idle".to_string()
                } else {
                    "no_repos".to_string()
                };

                let status_message = format!(
                    "{} repo(s) registered, {} group(s). Open: {}, Warm: {}, Closed: {}.",
                    repo_count, group_count, open_count, warm_count, closed_count
                );

                let response = IndexStatusResponse {
                    indexed: open_count + warm_count > 0,
                    status,
                    status_message,
                    total_chunks: 0, // Not available without opening DBs
                    total_files: 0,
                    model: self.model_type.short_name().to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: format!("({} repos)", repo_count),
                    project_path: format!("serve mode — {} repo(s)", repo_count),
                    error_message: None,
                    mode: self.mcp_mode(),
                };

                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        }

        // Resolve project/group routing — status is scope-free, allow unscoped fan-out
        let ctx = match self.resolve_routing(&project, &group, true, "status").await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            let indexed = self.db_path.exists();

            if !indexed {
                let response = IndexStatusResponse {
                    indexed: false,
                    status: "not_indexed".to_string(),
                    status_message: "No index found. Run 'codesearch index' or start with --create-index=true to automatically create one.".to_string(),
                    total_chunks: 0,
                    total_files: 0,
                    model: "none".to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: self.db_path.display().to_string(),
                    project_path: self.project_path.display().to_string(),
                    error_message: None,
                    mode: self.mcp_mode(),
                };
                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        }

        if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: aggregate stats across all group members
            let mut total_chunks = 0usize;
            let mut total_files = 0usize;
            let mut max_chunk_id = 0u32;
            let mut dimensions = 0usize;
            let mut all_indexed = true;
            let aliases = ctx.aliases();
            let mut stats_warnings: Vec<String> = Vec::new();
            let mut failed_count = 0usize;

            for (i, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.stats() {
                    Ok(stats) => {
                        total_chunks += stats.total_chunks;
                        total_files += stats.total_files;
                        if stats.max_chunk_id > max_chunk_id {
                            max_chunk_id = stats.max_chunk_id;
                        }
                        if stats.dimensions > 0 {
                            dimensions = stats.dimensions;
                        }
                        if !stats.indexed {
                            all_indexed = false;
                        }
                    }
                    // `all_indexed = false` alone renders identically to "still
                    // warming" — the caller has no way to tell "wait" from "this
                    // store is down". This is the tool whose job is reporting index
                    // health, so it must not stay silent on the one signal that
                    // matters here: bind the error, carry it, never `Err(_)`.
                    Err(ref e) => {
                        all_indexed = false;
                        failed_count += 1;
                        note_store_failure(&mut stats_warnings, aliases, i, "stats", e);
                    }
                }
            }

            let (status, status_message) =
                index_status_summary(sv.len(), failed_count, total_chunks);

            let response = IndexStatusResponse {
                indexed: all_indexed,
                status,
                status_message,
                total_chunks,
                total_files,
                model: self.model_type.short_name().to_string(),
                dimensions,
                max_chunk_id,
                db_path: format!("({} repos)", sv.len()),
                project_path: format!("group with {} repo(s)", sv.len()),
                error_message: None,
                mode: self.mcp_mode(),
            };

            return respond_with_object(&response, &stats_warnings);
        }

        // Single-store path
        let stats = match self
            .with_vector_store_read_for(
                |store| store.stats().context("Error getting index stats"),
                ctx.stores.clone(),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let response = IndexStatusResponse {
                    indexed: false,
                    status: "error".to_string(),
                    status_message: format!("{}", e),
                    total_chunks: 0,
                    total_files: 0,
                    model: self.model_type.short_name().to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: self.db_path.display().to_string(),
                    project_path: self.project_path.display().to_string(),
                    error_message: Some(format!("{}", e)),
                    mode: self.mcp_mode(),
                };
                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        };

        // Determine status based on database state
        let (status, status_message) = if stats.total_chunks == 0 {
            (
                "building".to_string(),
                "Index is being built in the background. Searches may fail until indexing completes. Please check back in a few minutes.".to_string(),
            )
        } else {
            (
                "ready".to_string(),
                "Index is ready for searching.".to_string(),
            )
        };

        let response = IndexStatusResponse {
            indexed: stats.indexed,
            status,
            status_message,
            total_chunks: stats.total_chunks,
            total_files: stats.total_files,
            model: self.model_type.short_name().to_string(),
            dimensions: stats.dimensions,
            max_chunk_id: stats.max_chunk_id,
            db_path: self.db_path.display().to_string(),
            project_path: self.project_path.display().to_string(),
            error_message: None,
            mode: self.mcp_mode(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// List all registered projects and groups. Called by `status(kind="projects")`.
    /// Build the `remote_projects` listing (opt-in mounts) for `list_projects`.
    fn remote_projects_listing(
        config: &crate::db_discovery::repos::ReposConfig,
    ) -> Vec<RemoteProjectInfo> {
        config
            .mounted_remote_projects()
            .into_iter()
            .filter_map(|(name, target)| match target {
                crate::db_discovery::repos::Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                } => Some(RemoteProjectInfo {
                    name,
                    peer: peer_name,
                    remote_alias,
                    peer_url: peer.url,
                }),
                _ => None,
            })
            .collect()
    }

    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let serve_active = self.serve_state.is_some();
        let serve_url = if serve_active {
            Some(serve_url_from_env())
        } else {
            None
        };

        // When serve is active, use ServeState as source of truth for lock status
        if let Some(ref serve_state) = self.serve_state {
            let config = serve_state.config_snapshot();
            let project_groups = config.project_groups();
            let mut repos_info = Vec::new();
            let mut list_warnings: Vec<String> = Vec::new();

            for (alias, path) in &config.repos {
                let db_path = path.join(crate::constants::DB_DIR_NAME);

                let (total_chunks, total_files, model, lock_status, error) = if db_path.exists() {
                    let (model_name, _dims) = read_model_metadata(&db_path);

                    // For repos already opened in DashMap, use the live SharedStores for stats
                    // WITHOUT opening a new VectorStore connection.
                    // For unopened repos, just report metadata — do NOT open the DB.
                    if let Some(stores) = serve_state.get_opened_stores(alias) {
                        let stats_result = {
                            let vs = stores.vector_store.read().await;
                            vs.stats()
                        };
                        // `0 chunks` alone reads exactly like "not indexed yet" — the
                        // repo may in fact be full and simply failing to answer (the
                        // read-only-incident shape this branch exists for). Attribute
                        // the failure to THIS repo rather than a top-level channel:
                        // list_projects returns one entry per repo, so per-item is the
                        // shape that actually matches the fan-out.
                        // `repo_stats_from_result` carries only the part of this
                        // decision that varies by Ok/Err — see its doc comment.
                        // `record_stats_or_warn` wraps it so this call site cannot
                        // silently drop the warning half without also breaking the
                        // counts it returns — see its own doc comment.
                        let (total_chunks, total_files, error) =
                            record_stats_or_warn(stats_result, alias, &mut list_warnings);
                        (
                            total_chunks,
                            total_files,
                            model_name,
                            serve_state
                                .repo_lock_status(alias)
                                .unwrap_or("unknown")
                                .to_string(),
                            error,
                        )
                    } else {
                        // Repo NOT opened — read persisted stats from metadata.json
                        let (md_chunks, md_files) = read_metadata_stats(&db_path);
                        let lock_status = if crate::index::is_database_locked(&db_path) {
                            "locked-externally".to_string()
                        } else {
                            "available".to_string()
                        };
                        (md_chunks, md_files, model_name, lock_status, None)
                    }
                } else {
                    (0, 0, "not indexed".to_string(), "unknown".to_string(), None)
                };

                repos_info.push(RepoInfo {
                    alias: alias.clone(),
                    project_path: path.display().to_string(),
                    database_path: db_path.display().to_string(),
                    total_chunks,
                    total_files,
                    model,
                    lock_status,
                    groups: project_groups.get(alias).cloned().unwrap_or_default(),
                    error,
                });
            }

            let response = ListProjectsResponse {
                repos: repos_info,
                groups: config.groups_with_virtual_all(),
                remote_projects: Self::remote_projects_listing(&config),
                serve_active,
                serve_url,
                current_directory: current_dir.display().to_string(),
            };

            return respond_with_object(&response, &list_warnings);
        }

        // Stdio mode: fall back to disk-based lock detection
        let config = load_repos_config().unwrap_or_default();
        let project_groups = config.project_groups();
        let mut repos_info = Vec::new();
        for (alias, path) in &config.repos {
            let db_path = path.join(crate::constants::DB_DIR_NAME);

            // Get stats
            let (total_chunks, total_files, model, lock_status) = if db_path.exists() {
                let (model_name, dims) = read_model_metadata(&db_path);

                let lock = if crate::index::is_database_locked(&db_path) {
                    "conflicted"
                } else {
                    "available"
                };

                if let Ok(store) = VectorStore::new(&db_path, dims) {
                    if let Ok(stats) = store.stats() {
                        (
                            stats.total_chunks,
                            stats.total_files,
                            model_name,
                            lock.to_string(),
                        )
                    } else {
                        (0, 0, model_name, lock.to_string())
                    }
                } else {
                    (0, 0, model_name, "readonly".to_string())
                }
            } else {
                (0, 0, "not indexed".to_string(), "unknown".to_string())
            };

            // Stdio mode is single-repo-at-a-time CLI usage, not the live multi-repo
            // federation this fan-out fix targets — a stats() failure here is out of
            // scope for this fix (VectorStore::new/stats failing locally is a different
            // shape than a store going down mid-request in a shared serve process).
            repos_info.push(RepoInfo {
                alias: alias.clone(),
                project_path: path.display().to_string(),
                database_path: db_path.display().to_string(),
                total_chunks,
                total_files,
                model,
                lock_status,
                groups: project_groups.get(alias).cloned().unwrap_or_default(),
                error: None,
            });
        }

        let response = ListProjectsResponse {
            repos: repos_info,
            groups: config.groups_with_virtual_all(),
            remote_projects: Self::remote_projects_listing(&config),
            serve_active,
            serve_url,
            current_directory: current_dir.display().to_string(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

// === Server Handler Implementation ===

/// Check if a chunk is a definition of the given symbol.
///
/// Best-effort heuristic for v1: a chunk is considered a definition if:
/// 1. Its kind is a definition kind (Function, Struct, Class, etc.)
/// 2. Its signature starts with a common definition pattern containing the symbol name
///
/// Limitation: this uses simple substring matching on the signature field.
/// False positives/negatives are possible for symbols that appear in signatures
/// of chunks that are not their definitions.
fn is_definition_chunk(kind: &str, signature: &Option<String>, symbol: &str) -> bool {
    // Only check definition kinds
    if !DEFINITION_KINDS.contains(&kind) {
        return false;
    }

    let sig = match signature {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };

    // Common definition prefixes across languages.
    // Keep this allocation-free in hot paths by using &str prefixes and boundary checks.
    const PREFIXES: &[&str] = &[
        "fn ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "type ",
        "interface ",
        "impl ",
        "pub fn ",
        "pub async fn ",
        "pub struct ",
        "pub enum ",
        "pub trait ",
        "pub type ",
        "async fn ",
        "const ",
        "static ",
    ];

    let prefix_match = PREFIXES.iter().any(|prefix| {
        if !sig.starts_with(prefix) {
            return false;
        }

        let rest = &sig[prefix.len()..];
        if !rest.starts_with(symbol) {
            return false;
        }

        let next = rest[symbol.len()..].chars().next();
        matches!(next, None | Some('(' | '<' | ':' | ' ' | '\t'))
    });

    if prefix_match {
        return true;
    }

    // Fallback for languages with verbose signatures (C#, Java):
    // signatures include access modifiers and return types before the symbol name,
    // e.g. "public async Task<string> UploadFileAsync(...)" or "protected override void Update(...)".
    // Search for the symbol as a whole word anywhere in the signature.
    contains_symbol_as_word(sig, symbol)
}

/// Check whether `symbol` appears as a whole word in `sig`.
/// A word boundary requires the character before to be a space/tab (or start-of-string)
/// and the character after to be `(`, `<`, `:`, space, tab, or end-of-string.
/// This is intentionally conservative to avoid matching parameter type names.
fn contains_symbol_as_word(sig: &str, symbol: &str) -> bool {
    let sig_bytes = sig.as_bytes();
    let sym_len = symbol.len();
    let mut start = 0usize;
    while start + sym_len <= sig.len() {
        if let Some(rel) = sig[start..].find(symbol) {
            let abs = start + rel;
            let before_ok = abs == 0
                || matches!(
                    sig_bytes.get(abs - 1),
                    Some(&b' ') | Some(&b'\t') | Some(&b'\n')
                );
            let after_char = sig[abs + sym_len..].chars().next();
            let after_ok = matches!(after_char, None | Some('(' | '<' | ':' | ' ' | '\t'));
            if before_ok && after_ok {
                return true;
            }
            start = abs + 1;
        } else {
            break;
        }
    }
    false
}

/// MCP server instructions template (pre-substitution). Kept as a named const so
/// the line-count and deprecated-alias tests can validate it directly without
/// fragile `include_str!` source-text searching or instantiating the service.
/// Substitution uses `str::replace` (not `format!`) because `format!` requires a
/// literal; the placeholders are unique tokens that don't appear in the prose.
/// See `test_instructions_max_50_lines` / `test_no_deprecated_tool_aliases`.
const INSTRUCTIONS_TEMPLATE: &str = r#"codesearch — semantic code search + symbol impact analysis.

WHEN TO USE codesearch (prefer over grep/glob):
  Good for: semantic or cross-file lookup, unknown file paths, symbol navigation,
    "where is X implemented", "find usages of Y", "how does Z flow through the code"
  Not for: a single known file (just read it), trivial one-line edits,
    exact literal patterns where plain grep is faster

SERVICE-MODE NOTES (codesearch serve, esp. on another host):
  - Paths come from the SERVER's filesystem. Use get_chunk to read content;
    don't try to open returned paths locally.
  - Not every directory is indexed (e.g. .venv, node_modules, build/). If a
    search returns nothing, the dir may be unindexed — ask, don't grep blindly.

PICK THE RIGHT TOOL FOR THE TASK:
  "who calls X?" / "what breaks if I rename X?"
    → find_impact (precise SCIP call-graph; if no backend for the language, it says so → then use find kind="usages")
  "find code about X" / "how does X work" / "show me X"
    → search(mode="semantic") — concepts + synonyms + identifiers
  exact syntax like Vec<T> / foo = null / a::b
    → search(mode="literal", regex=true) — patterns semantic can't match
  "where is X defined?" / "what does file X import?"
    → find(kind="definition" | "imports")
  "show all symbols in file X" / "code like chunk Y"
    → explore(kind="outline" | "similar")
  read chunk content → get_chunk(chunk_id)
  index health / repo list → status

RULES:
  - search(semantic) is the DEFAULT for code lookup. Don't skip it.
  - For "who calls X" / impact analysis, try find_impact first; fall back to find(kind="usages") only if find_impact reports no backend.
  - NEVER use literal as first search unless you need exact syntax.
  - project or group is REQUIRED in multi-repo mode.

Mode: {mode}
Project: {project}
Database: {db} ({exists})
Model: {model} ({dims}d)
"#;

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
fn merge_ranked_lists(
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
fn parse_search_items_from_call_result(
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
        // the merged list renders uniformly.
        _ => results
            .iter()
            .map(|v| SearchResultItem {
                chunk_id: v.get("chunk_id").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
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
fn convert_remote_item(
    peer_name: &str,
    remote_alias: &str,
    item: crate::federation::RemoteSearchItem,
) -> SearchResultItem {
    let chunk_ref = item
        .chunk_id
        .map(|id| format!("{peer_name}/{remote_alias}:{id}"));
    SearchResultItem {
        chunk_id: item.chunk_id.unwrap_or(0),
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
fn retain_by_filter_path(items: &mut Vec<SearchResultItem>, filter_path: Option<&str>) {
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
fn is_meaningful_filter(filter_path: Option<&str>) -> bool {
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
fn parse_federated_chunk_ref(chunk_ref: &str) -> Option<(&str, Option<&str>, u32)> {
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
fn extract_call_tool_text(result: &CallToolResult) -> String {
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

// ════════════════════════════════════════════════════════════════
// REST API handlers (federation-friendly HTTP+JSON mirror of MCP tools).
//
// Expose the same logic over plain HTTP so a remote codesearch serve can be
// queried for federation WITHOUT an MCP session. Each handler constructs a
// throwaway `CodesearchService` bound to the live `ServeState`, invokes the
// existing `#[tool]` method, and returns the tool's JSON payload unwrapped
// from `CallToolResult`. Protected by serve's `require_auth_for_network`
// layer (same as /status, /mcp) — no separate auth code needed.
// ════════════════════════════════════════════════════════════════
use axum::extract::{Path as AxumPath, Query as AxumQuery, State as AxumState};
use axum::http::StatusCode;
use axum::response::Json as AxumJson;

type RestResponse = AxumJson<serde_json::Value>;
type RestError = (StatusCode, AxumJson<serde_json::Value>);

/// Unwrap a `CallToolResult` into the JSON a federation client wants.
///
/// `CallToolResult` carries its payload as `Content::text(json_string)`. The
/// normal case for search/find/explore/get_chunk is a single text item whose
/// value parses as JSON, so we parse it back and return the structured value
/// (clients get clean objects instead of a JSON-in-string). When the tool set
/// `is_error` (e.g. a `scope_required` error) we still parse the body but mark
/// it with `"_mcp_is_error": true` so a federation caller can distinguish tool
/// errors from HTTP errors. Non-JSON payloads fall back to `{content, is_error}`.
pub(crate) fn call_tool_result_to_json(result: CallToolResult) -> serde_json::Value {
    let is_error = result.is_error.unwrap_or(false);
    let val = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({}));
    let text = val
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut parsed) => {
            if is_error {
                if let Some(obj) = parsed.as_object_mut() {
                    obj.insert("_mcp_is_error".into(), serde_json::json!(true));
                }
            }
            parsed
        }
        Err(_) => serde_json::json!({ "content": text, "is_error": is_error }),
    }
}

/// Build a per-request `CodesearchService` bound to the live serve state.
fn make_service(
    state: &std::sync::Arc<crate::serve::ServeState>,
) -> Result<CodesearchService, RestError> {
    CodesearchService::new_for_serve(state.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            AxumJson(serde_json::json!({"error": format!("failed to build service: {e}")})),
        )
    })
}

/// Map an MCP-layer error to an HTTP 500. `McpError` (= `rmcp::ErrorData`)
/// derives `Serialize`, so round-trip it through a JSON value for the body.
fn mcp_err_to_http(e: McpError) -> RestError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        AxumJson(serde_json::json!({
            "error": serde_json::to_value(&e).unwrap_or(serde_json::Value::Null)
        })),
    )
}

pub(crate) async fn rest_search_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumJson(req): AxumJson<SearchRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .search(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_find_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumJson(req): AxumJson<FindRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .find(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_explore_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumJson(req): AxumJson<ExploreRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .explore(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_get_chunk_handler(
    AxumState(state): AxumState<std::sync::Arc<crate::serve::ServeState>>,
    AxumPath(chunk_id): AxumPath<u32>,
    AxumQuery(params): AxumQuery<std::collections::HashMap<String, String>>,
) -> Result<RestResponse, RestError> {
    let req = GetChunkRequest {
        chunk_id,
        chunk_ref: params.get("chunk_ref").cloned(),
        context_lines: params.get("context_lines").and_then(|s| s.parse().ok()),
        project: params.get("project").cloned(),
        group: params.get("group").cloned(),
    };
    let service = make_service(&state)?;
    let result = service
        .get_chunk(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

#[tool_handler]
impl ServerHandler for CodesearchService {
    fn get_info(&self) -> ServerInfo {
        let db_exists = self.db_path.exists();
        let mode = if self.serve_state.is_some() {
            "serve hub (direct)"
        } else {
            "self-contained (stdio)"
        };

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codesearch", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                INSTRUCTIONS_TEMPLATE
                    .replace("{mode}", mode)
                    .replace("{project}", &self.project_path.display().to_string())
                    .replace("{db}", &self.db_path.display().to_string())
                    .replace("{exists}", if db_exists { "ready" } else { "not found" })
                    .replace("{model}", self.model_type.short_name())
                    .replace("{dims}", &self.dimensions.to_string()),
            )
    }
}

// === Server Entry Point ===

/// Run the MCP server using stdio transport with file watching for live index updates.
///
/// MCP server mode: how `codesearch mcp` connects to the index backend.
///
/// - **Auto** — If `codesearch serve` is running, connect as an HTTP client;
///   otherwise fall back to local stdio mode.
/// - **Client** — Always connect to `codesearch serve` via HTTP; fail if not running.
/// - **Local** — Always use local DB in stdio mode (classic behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    /// Connect to serve if available, otherwise local.
    #[default]
    Auto,
    /// Always connect to serve; fail if unreachable.
    Client,
    /// Always use local DB (stdio).
    Local,
}

impl std::fmt::Display for McpMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpMode::Auto => write!(f, "auto"),
            McpMode::Client => write!(f, "client"),
            McpMode::Local => write!(f, "local"),
        }
    }
}

impl std::str::FromStr for McpMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(McpMode::Auto),
            "client" => Ok(McpMode::Client),
            "local" => Ok(McpMode::Local),
            other => Err(format!(
                "invalid MCP mode '{}': must be 'auto', 'client', or 'local'",
                other
            )),
        }
    }
}

/// Probe the serve health endpoint. Returns Ok(serve_url) if serve is alive.
async fn probe_serve_health(serve_url: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(
            crate::constants::MCP_HEALTH_PROBE_TIMEOUT_MS,
        ))
        .build();
    let Ok(client) = client else { return false };
    let url = format!("{}{}", serve_url, crate::constants::HEALTH_PATH);
    client.get(&url).send().await.is_ok()
}

/// Run `codesearch mcp` as an HTTP client connecting to a running serve instance.
///
/// Uses rmcp's `StreamableHttpClientWorker` with `reqwest::Client` to speak
/// MCP Streamable HTTP to the serve hub. The MCP client (e.g. Claude Code)
/// talks JSON-RPC over stdio to us, and rmcp relays to the serve HTTP endpoint.
/// Run `codesearch mcp` as a transparent stdio↔HTTP proxy to `codesearch serve`.
///
/// Architecture:
///   Claude Desktop ──(stdio JSON-RPC)──▶ McpProxyService ──(HTTP Streamable)──▶ codesearch serve
///
/// Every MCP request from Claude Desktop is forwarded verbatim to the serve hub and the
/// response is returned unchanged. This allows Claude Desktop — which has no repo context
/// of its own — to reach all repos managed by `codesearch serve`.
///
/// ## Reconnect behaviour
///
/// When `codesearch serve` goes away (restart, crash, network blip), the proxy does NOT
/// exit. Instead it:
/// 1. Keeps the stdio connection to Claude Desktop alive
/// 2. Returns "reconnecting" errors for any incoming tool calls
/// 3. Retries the HTTP connection every 3 seconds for up to 5 minutes
/// 4. On success, hot-swaps the peer — tool calls resume immediately
/// 5. After 5 minutes of failure, exits cleanly (Claude Desktop detects the disconnect)
///
/// ## Idle disconnect behaviour
///
/// One HTTP MCP session held open for the lifetime of the proxy keeps a request
/// permanently registered at the remote's ingress, so a scale-to-zero host never
/// sees 0 concurrent requests and never suspends the replica. To avoid that, the
/// connection is only held while it is actually being used:
///
/// - An idle-checker ticks every `MCP_PROXY_IDLE_CHECK_INTERVAL_SECS`. Once no
///   request has been forwarded for `CODESEARCH_MCP_PROXY_IDLE_DISCONNECT_SECS`
///   (default `DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS`; `0` disables and restores
///   the always-connected behaviour), it clears the peer and cancels the
///   `RunningService`, closing the transport.
/// - That is a *planned* close, not an outage: it does not open a failure window
///   and does not count against `reconnect::MAX_DURATION_SECS`. The monitor task's
///   resulting `disconnect_tx` signal is recognised (via `voluntary_disconnect`)
///   and does not trigger an eager reconnect — reconnecting immediately would
///   defeat the purpose.
/// - The next `list_tools` / `call_tool` finds an empty peer slot and signals
///   `connect_request_tx`, which reconnects on demand. Failure-path reconnects
///   are unaffected and still run on their own cadence.
async fn run_mcp_client(serve_url: &str, cancel_token: CancellationToken) -> Result<()> {
    use rmcp::{transport::stdio, ServiceExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let mcp_url = format!("{}{}", serve_url, crate::constants::MCP_ENDPOINT_PATH);
    tracing::info!("🔗 Connecting to codesearch serve at {}", mcp_url);

    // Channels: spawned monitor tasks notify us when their connection drops.
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (stdio_close_tx, mut stdio_close_rx) = tokio::sync::mpsc::channel::<()>(1);
    // Capacity 1: coalescing duplicate "connect now" requests is correct.
    let (connect_request_tx, mut connect_request_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Shared peer state — hot-swapped on reconnect.
    let peer_state: std::sync::Arc<tokio::sync::RwLock<Option<rmcp::service::Peer<RoleClient>>>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(None));

    // Idle-disconnect state, shared with the proxy service.
    let last_activity: Arc<Mutex<std::time::Instant>> =
        Arc::new(Mutex::new(std::time::Instant::now()));
    let in_flight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    // Notified whenever an on-demand `connect_to_serve` attempt (below, in the
    // `connect_request_rx` arm) comes back `Err` — lets `await_peer` stop waiting
    // on a definitive refusal instead of polling out the rest of its window.
    let connect_failed: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
    // Cancellation handle for the *current* connection. `RunningServiceCancellationToken`
    // is not Clone and its `cancel()` consumes self, so it lives in an Option slot
    // that the idle-checker `take()`s.
    let conn_cancel: Arc<
        tokio::sync::Mutex<Option<rmcp::service::RunningServiceCancellationToken>>,
    > = Arc::new(tokio::sync::Mutex::new(None));
    // Set just before we cancel a connection ourselves, so the disconnect signal it
    // produces is not mistaken for an outage.
    let voluntary_disconnect = Arc::new(AtomicBool::new(false));
    let idle_disconnect_secs = resolve_proxy_idle_disconnect_secs(None);
    if idle_disconnect_secs == 0 {
        tracing::info!("idle-disconnect disabled — holding the serve connection open");
    } else {
        tracing::info!(
            "💤 idle-disconnect enabled: closing the serve connection after {}s without traffic (checked every {}s)",
            idle_disconnect_secs,
            crate::constants::MCP_PROXY_IDLE_CHECK_INTERVAL_SECS
        );
    }

    // Step 1: Start stdio proxy for Claude Desktop.
    // This must happen first so Claude Desktop has something to talk to,
    // even before the serve connection is established.
    let proxy = McpProxyService {
        peer: peer_state.clone(),
        disconnect_tx: disconnect_tx.clone(),
        connect_request_tx: connect_request_tx.clone(),
        last_activity: last_activity.clone(),
        in_flight: in_flight.clone(),
        connect_failed: connect_failed.clone(),
    };
    let server = proxy
        .serve(stdio())
        .await
        .context("Failed to start proxy stdio server")?;

    // Spawn a task that watches the stdio connection (takes ownership of server).
    tokio::spawn(async move {
        let _ = server.waiting().await;
        let _ = stdio_close_tx.send(()).await;
    });

    // Step 2: Initial connection to serve (tolerant — may not be running yet).
    let mut serve_down_since: Option<std::time::Instant> = None;
    match connect_to_serve(
        &mcp_url,
        &peer_state,
        disconnect_tx.clone(),
        &conn_cancel,
        &last_activity,
    )
    .await
    {
        Ok(()) => {
            tracing::info!("🚀 MCP proxy ready — forwarding Claude Desktop ↔ codesearch serve");
        }
        Err(e) => {
            serve_down_since = Some(std::time::Instant::now());
            tracing::warn!(
                "codesearch serve not yet available ({}). Proxy is up, will retry every {}s.",
                e,
                reconnect::INTERVAL_SECS
            );
            // Seed a synthetic disconnect so the main loop starts reconnecting.
            let tx = disconnect_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = tx.send(()).await;
            });
        }
    }

    // Step 3: Main loop — wait for stdio close, serve disconnect, an on-demand
    // connect request, an idle timeout, or cancel.

    let mut idle_ticker = tokio::time::interval(std::time::Duration::from_secs(
        crate::constants::MCP_PROXY_IDLE_CHECK_INTERVAL_SECS,
    ));
    // The first tick of a tokio interval completes immediately; skip it so a
    // freshly started proxy is not evaluated for idleness before it can be used.
    idle_ticker.tick().await;

    loop {
        tokio::select! {
            biased; // Prefer clean shutdown paths over reconnect

            // Claude Desktop closed stdio — we're done.
            _ = stdio_close_rx.recv() => {
                tracing::info!("MCP proxy transport closed");
                return Ok(());
            }

            // External cancel signal (e.g. process termination).
            _ = cancel_token.cancelled() => {
                tracing::info!("🛑 Shutdown signal received, stopping MCP proxy...");
                return Ok(());
            }

            // A request arrived while the peer slot was empty — connect now rather
            // than waiting for the failure-path cadence. Ordered before the
            // disconnect branch so a pending 3s backoff cannot starve it.
            _ = connect_request_rx.recv() => {
                if peer_state.read().await.is_some() {
                    continue; // Someone else already reconnected.
                }
                match connect_to_serve(&mcp_url, &peer_state, disconnect_tx.clone(), &conn_cancel, &last_activity).await {
                    Ok(()) => {
                        tracing::info!("🔗 Reconnected to codesearch serve on demand");
                        serve_down_since = None;
                    }
                    Err(e) => {
                        // Serve is genuinely unreachable (or still waking). Hand over
                        // to the existing failure loop, which retries on its own
                        // cadence and eventually gives up.
                        tracing::debug!("On-demand connect failed: {}", e);
                        note_connect_failure(&connect_failed, &disconnect_tx);
                    }
                }
            }

            // Serve disconnected — enter reconnect loop.
            _ = disconnect_rx.recv() => {
                // Clear peer so tool calls get "reconnecting" error.
                {
                    let mut p = peer_state.write().await;
                    *p = None;
                }

                // A disconnect we caused on purpose (idle-close) is not an outage:
                // no failure window, no eager reconnect — the next request will ask
                // for one via connect_request_tx.
                if voluntary_disconnect.swap(false, Ordering::SeqCst) {
                    tracing::debug!(
                        "serve connection closed after idle — will reconnect on the next request"
                    );
                    continue;
                }

                if serve_down_since.is_none() {
                    serve_down_since = Some(std::time::Instant::now());
                    tracing::warn!(
                        "codesearch serve disconnected — will attempt reconnect every {}s for up to {}s",
                        reconnect::INTERVAL_SECS,
                        reconnect::MAX_DURATION_SECS,
                    );
                }

                let elapsed = serve_down_since.unwrap().elapsed();
                if elapsed.as_secs() > reconnect::MAX_DURATION_SECS {
                    tracing::error!(
                        "❌ Could not reconnect to serve after {}s — giving up",
                        reconnect::MAX_DURATION_SECS
                    );
                    return Ok(()); // Clean exit so Claude Desktop gets graceful EOF
                }

                // Wait before retrying.
                tokio::time::sleep(std::time::Duration::from_secs(reconnect::INTERVAL_SECS)).await;

                match connect_to_serve(&mcp_url, &peer_state, disconnect_tx.clone(), &conn_cancel, &last_activity).await {
                    Ok(()) => {
                        tracing::info!(
                            "✅ Reconnected to codesearch serve (was down for {:.0}s)",
                            serve_down_since.unwrap().elapsed().as_secs()
                        );
                        serve_down_since = None;
                    }
                    Err(e) => {
                        tracing::debug!("Reconnect attempt failed: {}", e);
                        // Re-trigger ourselves: the disconnect_tx from the failed
                        // connect_to_serve was never used, so we send a synthetic
                        // disconnect to keep the loop going.
                        let tx = disconnect_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            let _ = tx.send(()).await;
                        });
                    }
                }
            }

            // Idle check — close the connection so a scale-to-zero remote can suspend.
            _ = idle_ticker.tick() => {
                if idle_disconnect_secs == 0 {
                    continue; // Idle-disconnect disabled.
                }
                let last = match last_activity.lock() {
                    Ok(guard) => *guard,
                    Err(_) => continue, // Poisoned: never tear down on a bookkeeping error.
                };
                if !is_idle(last, idle_disconnect_secs, std::time::Instant::now()) {
                    continue;
                }

                // Take the peer slot's write lock *before* checking `in_flight`, and
                // hold it through the clear below, instead of checking `in_flight`
                // first and taking the write lock afterwards.
                //
                // Every forwarding call does `InFlightGuard::new` (increments
                // `in_flight`) BEFORE `self.peer.read().await` (list_tools/call_tool
                // above) — so a call that has already obtained `Some(peer)` to
                // forward through has necessarily already incremented the counter,
                // and a call that hasn't reached the read yet will block on it once
                // we hold the write lock. Reading `in_flight` first (the previous
                // version) left a gap between that read and taking the write lock in
                // which such a call could still slip past, get `Some(peer)`, and have
                // its transport cancelled out from under it mid-request — the
                // in-flight count and the peer-slot teardown were two separate
                // operations pretending to be one guard. See AGENTS.md
                // "counter-then-teardown races".
                let mut p = peer_state.write().await;
                if p.is_none() {
                    continue; // Already disconnected.
                }
                if in_flight.load(Ordering::SeqCst) > 0 {
                    continue; // A request is still being forwarded over this transport.
                }

                tracing::info!(
                    "💤 Idle for {}s — closing MCP proxy connection to codesearch serve (will reconnect on next request)",
                    idle_disconnect_secs
                );
                // Flag first, so the disconnect signal from the dying monitor task is
                // recognised as planned no matter how fast it arrives.
                voluntary_disconnect.store(true, Ordering::SeqCst);
                *p = None;
                drop(p);
                if let Some(token) = conn_cancel.lock().await.take() {
                    token.cancel();
                } else {
                    // Nothing to cancel — don't leave the flag set for a later,
                    // genuine disconnect to misread.
                    voluntary_disconnect.store(false, Ordering::SeqCst);
                }
            }
        }
    }
}

/// Establish (or re-establish) an HTTP MCP client connection to the serve hub.
///
/// On success, updates `peer_state` with the new peer and spawns a background task
/// that monitors the connection and sends a message on `disconnect_tx` when it drops.
///
/// Also parks the connection's cancellation handle in `conn_cancel` (so the
/// idle-checker can close it) and stamps `last_activity`, so a connection opened
/// just before an idle tick is not immediately judged idle.
async fn connect_to_serve(
    mcp_url: &str,
    peer_state: &std::sync::Arc<tokio::sync::RwLock<Option<rmcp::service::Peer<RoleClient>>>>,
    disconnect_tx: tokio::sync::mpsc::Sender<()>,
    conn_cancel: &Arc<tokio::sync::Mutex<Option<rmcp::service::RunningServiceCancellationToken>>>,
    last_activity: &Arc<Mutex<std::time::Instant>>,
) -> Result<()> {
    use rmcp::ServiceExt;

    let transport = {
        use rmcp::transport::streamable_http_client::{
            StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
        };
        let config =
            StreamableHttpClientTransportConfig::with_uri(mcp_url).reinit_on_expired_session(true);
        StreamableHttpClientWorker::new(reqwest::Client::new(), config)
    };

    let http_client: rmcp::service::RunningService<RoleClient, ()> =
        ().serve(transport).await.map_err(|e| {
            anyhow::anyhow!(
                "Failed to connect to codesearch serve at {}.\n\
                 Error: {}\n\
                 Is `codesearch serve` running?",
                mcp_url,
                e
            )
        })?;

    // Grab the cancellation handle before the RunningService is moved into the
    // monitor task below — that's the only way to close this transport later
    // (idle-disconnect). Cancelling it makes the monitor's `waiting()` resolve, so
    // the normal disconnect path still runs; nothing needs special-casing.
    {
        let mut slot = conn_cancel.lock().await;
        *slot = Some(http_client.cancellation_token());
    }

    // Update the shared peer.
    let peer = http_client.peer().clone();
    {
        let mut p = peer_state.write().await;
        *p = Some(peer);
    }

    // A fresh connection counts as activity: without this, an idle tick firing
    // right after a reconnect would immediately close it again.
    mark_proxy_activity(last_activity);

    // Spawn a monitor task that detects when the connection drops.
    tokio::spawn(async move {
        let _ = http_client.waiting().await;
        // Connection lost — notify main loop.
        let _ = disconnect_tx.send(()).await;
    });

    Ok(())
}

/// # Multi-instance Support
///
/// When another instance is already running with write access to the same database,
/// this server will automatically start in **readonly mode**:
/// - Searches work normally
/// - No file watching (index won't auto-update)
/// - No incremental refresh
///
/// This allows multiple terminal windows to use codesearch simultaneously.
pub async fn run_mcp_server(
    path: Option<PathBuf>,
    create_index: bool,
    log_level: crate::logger::LogLevel,
    quiet: bool,
    mode: McpMode,
    cancel_token: CancellationToken,
) -> Result<()> {
    let serve_url = serve_url_from_env();

    // Set FASTEMBED_CACHE_DIR early (before any embedding work) to ensure fastembed
    // downloads and caches models to ~/.codesearch/models instead of creating
    // .fastembed_cache in the current working directory. Do this once for all modes.
    match crate::constants::get_global_models_cache_dir() {
        Ok(models_dir) => {
            std::env::set_var("FASTEMBED_CACHE_DIR", &models_dir);
        }
        Err(e) => {
            tracing::warn!("Could not set FASTEMBED_CACHE_DIR: {}", e);
        }
    }

    match mode {
        McpMode::Client => {
            // Client mode: init logger using global cache dir (no local DB needed)
            if let Err(e) = crate::logger::init_logger(
                &crate::constants::get_global_cache_dir(),
                log_level,
                quiet,
            ) {
                tracing::warn!("Failed to initialize file logger: {}", e);
            }
            tracing::info!("📡 MCP mode: client — connecting to serve at {}", serve_url);
            if !probe_serve_health(&serve_url).await {
                return Err(anyhow::anyhow!(
                    "codesearch serve is not running at {}. \
                     Start it with `codesearch serve` or use --mode auto/local.",
                    serve_url
                ));
            }
            return run_mcp_client(&serve_url, cancel_token).await;
        }
        McpMode::Auto => {
            // Auto mode: init logger early for probe logging
            if let Err(e) = crate::logger::init_logger(
                &crate::constants::get_global_cache_dir(),
                log_level,
                quiet,
            ) {
                tracing::warn!("Failed to initialize file logger: {}", e);
            }
            if probe_serve_health(&serve_url).await {
                tracing::info!(
                    "📡 MCP mode: auto — serve detected at {}, connecting as client",
                    serve_url
                );
                return run_mcp_client(&serve_url, cancel_token).await;
            }
            tracing::info!("📡 MCP mode: auto — no serve detected, falling back to local stdio");
            // Fall through to local mode
        }
        McpMode::Local => {
            tracing::info!("📡 MCP mode: local — using local DB (stdio)");
            // Fall through to local mode
        }
    }

    // ── Local stdio mode (original behavior) ──────────────────────────
    use rmcp::{transport::stdio, ServiceExt};

    tracing::info!("🚀 Starting codesearch MCP server");

    // Use database discovery to find the best database
    let db_info = find_best_database(path.as_deref())?;

    let (project_path, db_path) = if let Some(info) = db_info {
        (info.project_path, info.db_path)
    } else {
        // No database found
        if !create_index {
            return Err(anyhow::anyhow!(
                "No database found in current directory, parent directories, or globally tracked repositories. \
                 Run 'codesearch index' first to index the codebase, or use --create-index=true flag to automatically create it."
            ));
        }

        // Create minimal database structure to allow server to start immediately
        let effective_path = path.as_ref().cloned().unwrap_or(std::env::current_dir()?);

        // Use git root detection to place database in the correct location
        let db_root =
            crate::index::find_git_root(&effective_path)?.unwrap_or_else(|| effective_path.clone());
        let db_path = db_root.join(".codesearch.db");

        tracing::info!(
            "📁 Creating minimal database structure at {}",
            db_path.display()
        );

        // Create directory
        std::fs::create_dir_all(&db_path)?;

        // Get model info
        let model_type = ModelType::default();
        let model_short_name = model_type.short_name().to_string();
        let dimensions = model_type.dimensions();

        // Create minimal metadata.json (atomic read-modify-write, matching format
        // used by build_index). Routes through the single-source-of-truth stamp so
        // model_name matches the other index paths (previously wrote the Debug
        // variant name here, e.g. "AllMiniLML6V2Q", instead of the model name).
        crate::vectordb::merge_metadata_atomic(&db_path, |obj| {
            model_type.write_metadata_fields(obj);
            obj.insert(
                "indexed_at".to_string(),
                serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
            );
        })?;

        // Create minimal file_meta.json (matching FileMetaStore format)
        let file_meta = crate::cache::FileMetaStore::new(model_short_name.clone(), dimensions);
        file_meta.save(&db_path)?;

        // Create FTS directory
        let fts_path = db_path.join("fts");
        std::fs::create_dir_all(&fts_path)?;

        // Create LMDB file by opening VectorStore (creates minimal structure)
        let _store = crate::vectordb::VectorStore::new(&db_path, dimensions)?;

        tracing::info!("✅ Minimal database created successfully");
        tracing::info!("🔄 Background indexing will begin shortly via incremental refresh");

        (effective_path, db_path)
    };

    // Initialize file logger now that db_path is known (works for both existing and auto-created DB)
    // NOTE: For MCP, tracing is NOT initialized in main.rs — this is the only init call
    if let Err(e) = crate::logger::init_logger(&db_path, log_level, quiet) {
        tracing::warn!("Failed to initialize file logger: {}", e);
    }

    tracing::info!("📂 Project: {}", project_path.display());
    tracing::info!("💾 Database: {}", db_path.display());

    // Read model metadata to get dimensions (fallback to 384 if missing/corrupt)
    let metadata_path = db_path.join("metadata.json");
    let dimensions = if metadata_path.exists() {
        match std::fs::read_to_string(&metadata_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|j| j.get("dimensions").and_then(|v| v.as_u64()))
        {
            Some(d) => d as usize,
            None => {
                tracing::warn!(
                    "⚠️  Could not parse dimensions from metadata.json, using default {}",
                    crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
                );
                crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
            }
        }
    } else {
        tracing::warn!(
            "⚠️  metadata.json not found, using default dimensions {}",
            crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
        );
        crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
    };

    // Create shared stores - try write mode first, fall back to readonly if locked
    // This enables multiple terminal windows to use the same database
    tracing::info!("📦 Creating shared stores...");
    let (shared_stores, is_readonly) = SharedStores::new_or_readonly(&db_path, dimensions)?;
    let shared_stores = Arc::new(shared_stores);

    if is_readonly {
        tracing::warn!("🔒 Running in READONLY mode (another instance has write access)");
        tracing::warn!("   ↳ Searches work normally, but index won't auto-update");
        tracing::warn!("   ↳ Close the other instance to enable write mode");
    }

    // Create MCP service with shared stores (ready immediately)
    let service = CodesearchService::new_with_stores(
        Some(project_path.clone()),
        Some(shared_stores.clone()),
    )?;

    tracing::info!("🧠 Model: {}", service.model_type.name());

    // START MCP SERVER NOW - fixes timeout!
    tracing::info!(
        "🚀 Starting MCP server{}...",
        if is_readonly { " (readonly)" } else { "" }
    );
    let server = service.serve(stdio()).await?;

    tracing::info!("MCP server ready. Waiting for requests...");

    // Only run background tasks if we have write access
    if !is_readonly {
        // Create IndexManager with shared stores (skip initial refresh - do in background)
        tracing::info!("🔍 Initializing index manager...");
        let index_manager =
            IndexManager::new_without_refresh(&project_path, shared_stores.clone()).await?;

        // Background: refresh FIRST, then file watcher (sequential, not concurrent)
        // Both write to SharedStores, so they must not run concurrently
        let project_path_clone = project_path.clone();
        let db_path_clone = db_path.clone();
        let shared_stores_clone = shared_stores.clone();
        let index_manager_arc = Arc::new(index_manager);
        let bg_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            // Step 0: Pre-start FSW to collect file change events during refresh
            // This ensures changes made while the refresh is running are not missed
            if let Err(e) = index_manager_arc.start_watching().await {
                tracing::warn!("⚠️ Could not pre-start file watcher: {}", e);
            }

            // Step 1: Run initial refresh (writes to stores)
            tracing::info!("🔄 Starting background incremental refresh...");
            match IndexManager::perform_incremental_refresh_with_stores(
                &project_path_clone,
                &db_path_clone,
                &shared_stores_clone,
                &bg_cancel_token,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!("✅ Background incremental refresh completed");

                    // Check if shutdown was requested during refresh
                    if bg_cancel_token.is_cancelled() {
                        tracing::info!("🛑 Shutdown requested, skipping file watcher startup");
                        return;
                    }

                    // Step 2: AFTER refresh completes, start file watcher (also writes to stores)
                    tracing::info!("👀 Starting file watcher...");
                    if let Err(e) = index_manager_arc
                        .start_file_watcher(bg_cancel_token, None, None)
                        .await
                    {
                        tracing::error!("❌ Failed to start file watcher: {}", e);
                    } else {
                        tracing::info!(
                            "✅ File watcher active - index will auto-update on file changes"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("❌ Background incremental refresh failed: {}", e);
                }
            }
        });

        // Start periodic log cleanup task
        let db_path_for_cleanup = db_path.clone();
        let cleanup_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            use crate::logger::{cleanup_old_logs, LogRotationConfig};

            // Run initial cleanup on startup
            let rotation_config = LogRotationConfig::from_env();
            tracing::info!("🧹 Running initial log cleanup...");
            if let Err(e) = cleanup_old_logs(&db_path_for_cleanup, &rotation_config) {
                tracing::warn!("Initial log cleanup failed: {}", e);
            }

            // Start periodic cleanup task (every 24 hours by default)
            crate::logger::start_cleanup_task(
                db_path_for_cleanup.clone(),
                rotation_config,
                cleanup_cancel_token,
            );
        });
    } else {
        tracing::info!("📖 Readonly mode: skipping background refresh and file watcher");
    }

    // Wait for shutdown: either MCP transport closes or cancellation token fires
    tokio::select! {
        result = server.waiting() => {
            tracing::info!("MCP server transport closed");
            result?;
        }
        _ = cancel_token.cancelled() => {
            tracing::info!("🛑 Shutdown signal received, stopping MCP server...");
        }
    }

    tracing::info!("✅ MCP server shut down cleanly");
    Ok(())
}

#[cfg(test)]
mod federation_helpers_tests {
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
}
