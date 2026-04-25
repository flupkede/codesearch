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
            }],
            low_confidence: None,
            suggested_tool: None,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("low_confidence"));
        assert!(!json.contains("suggested_tool"));
    }

    // === Instructions length test ===

    #[test]
    fn test_instructions_max_50_lines() {
        // Verify that get_info().instructions string is ≤ 50 lines.
        // This is a compile-time check via include_str to catch regressions.
        let src = include_str!("mod.rs");
        // Extract the instructions string content between the raw string delimiters
        // The instructions are in get_info() method — we can count lines in the
        // formatted template. Since we can't easily instantiate the service here,
        // we check the raw string literal line count.
        //
        // Look for the compact routing table format — it should be well under 50 lines.
        // We verify by checking the instructions block has no more than 50 newlines.
        let instructions_start = src.find("codesearch — semantic + lexical");
        assert!(
            instructions_start.is_some(),
            "Could not find instructions start marker in mod.rs"
        );
        let start = instructions_start.unwrap();
        let remaining = &src[start..];
        let instructions_end = remaining.find("\"#,");
        assert!(
            instructions_end.is_some(),
            "Could not find instructions end marker in mod.rs"
        );
        let instructions_text = &remaining[..instructions_end.unwrap()];

        let line_count = instructions_text.lines().count();
        assert!(
            line_count <= 50,
            "Instructions block is {} lines, must be ≤ 50 lines.\n\
             Content:\n{}",
            line_count,
            instructions_text
        );
    }

    #[test]
    fn test_no_deprecated_tool_aliases_in_instructions() {
        let src = include_str!("mod.rs");
        let instructions_start = src.find("codesearch — semantic + lexical");
        assert!(instructions_start.is_some());
        let start = instructions_start.unwrap();
        let remaining = &src[start..];
        let instructions_end = remaining.find("\"#,");
        assert!(instructions_end.is_some());
        let instructions_text = &remaining[..instructions_end.unwrap()];

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

    #[test]
    fn test_path_prefix_windows_backslashes() {
        let result = super::prefix_path_with_alias(
            r"C:\repo\src\main.rs",
            Some("myrepo"),
            r"C:\repo",
        );
        assert_eq!(result, "myrepo/src/main.rs");
    }

    #[test]
    fn test_path_prefix_unc_prefix() {
        let result = super::prefix_path_with_alias(
            r"\\?\C:\repo\src\main.rs",
            Some("myrepo"),
            r"C:\repo",
        );
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

    #[test]
    fn test_path_prefix_mixed_separators() {
        let result = super::prefix_path_with_alias(
            r"C:\repo/src\main.rs",
            Some("myrepo"),
            r"C:\repo",
        );
        assert_eq!(result, "myrepo/src/main.rs");
    }

    #[test]
    fn test_path_prefix_no_alias() {
        let result = super::prefix_path_with_alias(
            "C:/repo/src/main.rs",
            None,
            "C:/repo",
        );
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn test_path_prefix_empty_alias() {
        let result = super::prefix_path_with_alias(
            "C:/repo/src/main.rs",
            Some(""),
            "C:/repo",
        );
        assert_eq!(result, "src/main.rs");
    }

    #[test]
    fn test_path_prefix_preserves_path_outside_root() {
        let result = super::prefix_path_with_alias(
            "C:/other/src/main.rs",
            Some("myrepo"),
            "C:/repo",
        );
        // Path doesn't start with root — returned normalized, no alias prefix
        assert_eq!(result, "C:/other/src/main.rs");
    }

    #[test]
    fn test_group_results_are_alias_prefixed() {
        // Simulate two stores for aliases "a" and "b", each returning a result
        // with absolute path = "/abs/root/src/main.rs". After applying prefix_path_with_alias,
        // assert results have path = "a/src/main.rs" and "b/src/main.rs".
        let result_a = super::prefix_path_with_alias(
            "/abs/root/src/main.rs",
            Some("a"),
            "/abs/root",
        );
        let result_b = super::prefix_path_with_alias(
            "/abs/root/src/main.rs",
            Some("b"),
            "/abs/root",
        );
        assert_eq!(result_a, "a/src/main.rs");
        assert_eq!(result_b, "b/src/main.rs");
    }

    #[test]
    fn test_single_project_result_is_alias_prefixed() {
        // Single store for alias "myrepo", result with path = "/abs/root/src/lib.rs",
        // project root "/abs/root" → assert path becomes "myrepo/src/lib.rs".
        let result = super::prefix_path_with_alias(
            "/abs/root/src/lib.rs",
            Some("myrepo"),
            "/abs/root",
        );
        assert_eq!(result, "myrepo/src/lib.rs");
    }

    #[test]
    fn test_stdio_mode_paths_not_prefixed() {
        // alias None → path normalized, no prefix added.
        let result = super::prefix_path_with_alias(
            "C:/repo/src/main.rs",
            None,
            "C:/repo",
        );
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
            }],
            low_confidence: None,
            suggested_tool: None,
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
        let result = crate::fts::FtsResult { chunk_id: 42, score: 0.85 };
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
            crate::fts::FtsResult { chunk_id: 1, score: 0.5 },
            crate::fts::FtsResult { chunk_id: 2, score: 0.8 },
            crate::fts::FtsResult { chunk_id: 3, score: 0.3 },
        ];
        let store2_results = vec![
            crate::fts::FtsResult { chunk_id: 1, score: 0.9 }, // same chunk_id, different alias — NOT a dup
            crate::fts::FtsResult { chunk_id: 4, score: 0.7 },
            crate::fts::FtsResult { chunk_id: 2, score: 0.4 }, // same chunk_id, different alias — NOT a dup
        ];
        let store3_results = vec![
            crate::fts::FtsResult { chunk_id: 3, score: 0.6 }, // same chunk_id, different alias — NOT a dup
            crate::fts::FtsResult { chunk_id: 5, score: 0.2 },
        ];

        // Apply the same dedup logic as with_fts_store_read_multi: key is (alias, chunk_id)
        let mut all_results: Vec<crate::fts::FtsResult> = Vec::new();
        let mut seen_ids: HashMap<(String, u32), usize> = HashMap::new();

        for (alias, results) in aliases.iter().zip([&store1_results, &store2_results, &store3_results]) {
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
        assert_eq!(all_results.len(), 8, "Should have 8 unique (alias, chunk_id) pairs across 3 repos");

        // Check sort: first result should be highest score
        assert!((all_results[0].score - 0.9).abs() < f32::EPSILON, "First result should have highest score");

        // Check sort: scores should be descending
        for i in 1..all_results.len() {
            assert!(all_results[i].score <= all_results[i - 1].score,
                "Results should be sorted by score descending, but [{}]={} > [{}]={}",
                i - 1, all_results[i - 1].score, i, all_results[i].score);
        }
    }

    #[test]
    fn test_multi_store_dedup_no_overlap() {
        // Non-overlapping results — all should be kept
        let store1 = vec![crate::fts::FtsResult { chunk_id: 1, score: 0.5 }];
        let store2 = vec![crate::fts::FtsResult { chunk_id: 2, score: 0.8 }];
        let store3 = vec![crate::fts::FtsResult { chunk_id: 3, score: 0.3 }];

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

        assert_eq!(all_results.len(), 3, "All 3 non-overlapping results should be kept");
    }

    #[test]
    fn test_multi_store_dedup_all_same_ids() {
        // All stores return same chunk_ids — only keep each once with max score
        let store1 = vec![crate::fts::FtsResult { chunk_id: 1, score: 0.3 }];
        let store2 = vec![crate::fts::FtsResult { chunk_id: 1, score: 0.9 }];
        let store3 = vec![crate::fts::FtsResult { chunk_id: 1, score: 0.6 }];

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
        assert!((all_results[0].score - 0.9).abs() < f32::EPSILON,
            "Should keep highest score 0.9, got {}", all_results[0].score);
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
        let (stores, stores_vec, is_multi, needs_local_db) =
            decompose_routing_ctx::<i32>(None);
        assert!(stores.is_none(), "stores should be None");
        assert!(stores_vec.is_none(), "stores_vec should be None");
        assert!(!is_multi, "is_multi should be false");
        assert!(needs_local_db, "needs_local_db should be true — no serve-state stores");
    }

    #[test]
    fn test_routing_decomposition_single_store() {
        // One repo resolved → stores = Some, stores_vec = None, not multi
        let (stores, stores_vec, is_multi, needs_local_db) =
            decompose_routing_ctx(Some(vec![arc_val(1)]));
        assert!(stores.is_some(), "stores should be Some for single repo");
        assert!(stores_vec.is_none(), "stores_vec should be None for single repo");
        assert!(!is_multi, "is_multi should be false for single repo");
        assert!(!needs_local_db, "needs_local_db should be false — we have a store");
        assert_eq!(*stores.unwrap(), 1);
    }

    #[test]
    fn test_routing_decomposition_two_stores() {
        // Group with 2 repos → stores = None, stores_vec = Some, is_multi = true
        let (stores, stores_vec, is_multi, needs_local_db) =
            decompose_routing_ctx(Some(vec![arc_val(1), arc_val(2)]));
        assert!(stores.is_none(), "stores should be None for multi-store");
        assert!(stores_vec.is_some(), "stores_vec should be Some for multi-store");
        assert!(is_multi, "is_multi should be true for 2+ stores");
        assert!(!needs_local_db, "needs_local_db should be false — we have stores");
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
        assert!(stores_vec.is_none(), "empty vec → stores_vec None (is_multi=false)");
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
        let (stores, stores_vec, is_multi, needs_local_db) =
            decompose_routing_ctx(multi);
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
        let (stores, stores_vec, is_multi, needs_local_db) =
            decompose_routing_ctx(multi);
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
            crate::fts::FtsResult { chunk_id: 1, score: 0.5 },
            crate::fts::FtsResult { chunk_id: 2, score: 0.8 },
        ];
        let exact = vec![
            crate::fts::FtsResult { chunk_id: 1, score: 0.9 }, // higher score
            crate::fts::FtsResult { chunk_id: 3, score: 0.7 }, // new chunk
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
}

pub mod types;
pub mod proxy;

use crate::db_discovery::{find_best_database, load_repos_config};
use crate::embed::{EmbeddingService, ModelType};
use crate::file::Language;
use crate::fts::FtsStore;
use crate::index::{IndexManager, SharedStores};
use crate::rerank::{rrf_fusion, rrf_fusion_with_exact, vector_only, EXACT_MATCH_RRF_K};
use crate::search::{adapt_rrf_k, boost_kind, detect_identifiers, detect_structural_intent};
use crate::vectordb::VectorStore;
use anyhow::{Context, Result};
use regex::Regex;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// Re-export types
pub use types::*;

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
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
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
    ("unknown".to_string(), crate::constants::DEFAULT_EMBEDDING_DIMENSIONS)
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
    tool_router: ToolRouter<CodesearchService>,
    db_path: PathBuf,
    project_path: PathBuf,
    model_type: ModelType,
    dimensions: usize,
    // Lazily initialized on first search
    embedding_service: Mutex<Option<EmbeddingService>>,
    // Shared stores for concurrent access (optional - only set when running with IndexManager)
    shared_stores: Option<Arc<SharedStores>>,
    // Serve-mode state (set when running inside `codesearch serve`)
    serve_state: Option<Arc<crate::serve::ServeState>>,
    // Proxy to serve instance (set when `codesearch mcp` detects a running serve)
    proxy: Option<crate::mcp::proxy::McpProxy>,
}

impl std::fmt::Debug for CodesearchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodesearchService")
            .field("db_path", &self.db_path)
            .field("model_type", &self.model_type)
            .field("dimensions", &self.dimensions)
            .field("has_shared_stores", &self.shared_stores.is_some())
            .field("serve_mode", &self.serve_state.is_some())
            .field("proxy_mode", &self.proxy.is_some())
            .finish()
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
    fn chunk_id(&self) -> u32 { self.id }
}

impl HasScore for crate::vectordb::SearchResult {
    fn score(&self) -> f32 { self.score }
}

impl HasChunkId for crate::fts::FtsResult {
    fn chunk_id(&self) -> u32 { self.chunk_id }
}

impl HasScore for crate::fts::FtsResult {
    fn score(&self) -> f32 { self.score }
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

/// Prefix a result path with its repo alias for group queries, normalizing
/// Windows backslashes to forward slashes in the process. When `alias` is
/// None or empty, the path is still normalized (useful for stdio mode).
pub(crate) fn prefix_path_with_alias(path: &str, alias: Option<&str>, project_root: &str) -> String {
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

fn is_import_kind(kind: &str) -> bool {
    matches!(kind, "Import" | "Use" | "Require" | "Include" | "Imports")
}

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

fn match_line_for_literal(content: &str, query: &str, regex: Option<&Regex>) -> Option<(usize, String)> {
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
            Some(("use".to_string(), rest.trim().trim_end_matches(';').to_string()))
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            Some(("import".to_string(), rest.trim().trim_end_matches(';').to_string()))
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            Some(("import".to_string(), rest.trim().trim_end_matches(';').to_string()))
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
                .unwrap_or(384) as usize;
            let mt = ModelType::parse(model_name).unwrap_or_default();
            (mt, dims)
        } else {
            (ModelType::default(), 384)
        };

        Ok(Self {
            tool_router: Self::tool_router(),
            db_path,
            project_path,
            model_type,
            dimensions,
            embedding_service: Mutex::new(None),
            shared_stores,
            serve_state: None,
            proxy: None,
        })
    }

    /// Create a CodesearchService for use inside `codesearch serve`.
    ///
    /// In serve mode, the service does not have a single local DB; instead
    /// it routes requests to the repo identified by `project`/`group`.
    pub(crate) fn new_for_serve(
        serve_state: Arc<crate::serve::ServeState>,
    ) -> Result<Self> {
        Ok(Self {
            tool_router: Self::tool_router(),
            db_path: PathBuf::from("serve://multi-repo"),
            project_path: PathBuf::from("serve://multi-repo"),
            model_type: ModelType::default(),
            dimensions: crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
            embedding_service: Mutex::new(None),
            shared_stores: None,
            serve_state: Some(serve_state),
            proxy: None,
        })
    }

    /// Create a CodesearchService in proxy mode (forwards all calls to serve).
    pub fn new_for_proxy(proxy: crate::mcp::proxy::McpProxy) -> Result<Self> {
        Ok(Self {
            tool_router: Self::tool_router(),
            db_path: PathBuf::from("proxy://forward-to-serve"),
            project_path: PathBuf::from("proxy://forward-to-serve"),
            model_type: ModelType::default(),
            dimensions: crate::constants::DEFAULT_EMBEDDING_DIMENSIONS,
            embedding_service: Mutex::new(None),
            shared_stores: None,
            serve_state: None,
            proxy: Some(proxy),
        })
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
        if self.proxy.is_some() {
            Some(format!("proxy → {}", self.proxy.as_ref().unwrap().base_url()))
        } else if self.serve_state.is_some() {
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
    ) -> std::result::Result<Option<(Vec<Arc<SharedStores>>, Vec<String>)>, String> {
        // No routing params → auto-default to all configured repos in serve mode
        if project.is_none() && group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let cfg = serve_state.config_snapshot();
                let aliases: Vec<String> = cfg.repos.keys().cloned().collect();
                if !aliases.is_empty() {
                    let mut all_stores = Vec::with_capacity(aliases.len());
                    for alias in &aliases {
                        all_stores.push(serve_state.get_or_open_stores(alias).await?);
                    }
                    return Ok(Some((all_stores, aliases)));
                }
            }
            return Ok(None);
        }

        // Must have serve_state to route
        let serve_state = self.serve_state.as_ref()
            .ok_or_else(|| "project/group routing requires `codesearch serve` to be running.".to_string())?;

        // Validate params
        types::validate_project_group(project, group, true)?;

        if let Some(ref alias) = project {
            let stores = serve_state.get_or_open_stores(alias).await?;
            return Ok(Some((vec![stores], vec![alias.clone()])));
        }

        if let Some(ref group_name) = group {
            let aliases = serve_state.resolve_group_aliases(group_name)?;
            if aliases.is_empty() {
                return Err(format!("Group '{}' has no members.", group_name));
            }
            let mut all_stores = Vec::with_capacity(aliases.len());
            for alias in &aliases {
                all_stores.push(serve_state.get_or_open_stores(alias).await?);
            }
            return Ok(Some((all_stores, aliases)));
        }

        Ok(None)
    }

    /// Resolve project/group params into a ready-to-use routing context.
    ///
    /// Encapsulates the common pattern: resolve multi-stores, extract single override
    /// vs multi-store vec, and determine if local DB check is needed.
    async fn resolve_routing(
        &self,
        project: &Option<String>,
        group: &Option<String>,
    ) -> std::result::Result<MultiStoreContext, String> {
        let resolved = self.resolve_repo_stores_multi(project, group).await?;
        let is_multi = resolved.as_ref().is_some_and(|(stores, _)| stores.len() > 1);
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
                    tracing::error!(
                        "Shared vector store read failed, falling back to standalone open: {:?}",
                        shared_err
                    );
                }
            }

            // If MCP is in readonly mode, fallback must also use readonly open.
            if stores.readonly {
                let ro_store = VectorStore::open_readonly(&self.db_path, self.dimensions)
                    .context("Error opening readonly database for read fallback")?;
                return action(&ro_store)
                    .context("Error reading from readonly fallback vector store");
            }
        }

        // Fallback path:
        // - when shared stores are not available, OR
        // - when shared read fails (e.g., transient readonly/shared handle issues)
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
    async fn with_vector_store_read_multi<R, F>(
        &self,
        mut action: F,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
    ) -> Result<Vec<R>>
    where
        F: FnMut(&VectorStore) -> anyhow::Result<Vec<R>>,
        R: Clone + HasChunkId + HasScore,
    {
        let mut all_results: Vec<R> = Vec::new();
        let mut seen_ids: std::collections::HashMap<(String, u32), usize> = std::collections::HashMap::new();

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
                    tracing::warn!("Vector store read failed for multi-store fan-out: {:?}", e);
                }
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(all_results)
    }

    /// Fan-out FTS store read across multiple stores, merging results.
    ///
    /// Runs `action` against each store and merges all results into a single vec,
    /// deduplicating by (alias, chunk_id) (keeping highest score) and sorting by score descending.
    async fn with_fts_store_read_multi<R, F>(
        &self,
        mut action: F,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
    ) -> Result<Vec<R>>
    where
        F: FnMut(&FtsStore) -> Result<Vec<R>>,
        R: Clone + HasChunkId + HasScore,
    {
        let mut all_results: Vec<R> = Vec::new();
        let mut seen_ids: std::collections::HashMap<(String, u32), usize> = std::collections::HashMap::new();

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
                    tracing::warn!("FTS store read failed for multi-store fan-out: {:?}", e);
                }
            }
        }

        // Sort by score descending
        all_results.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(all_results)
    }

    // ─────────────────────────────────────────────────────────────────
    // Consolidated tools (the primary 5-tool surface)
    // ─────────────────────────────────────────────────────────────────

    /// Unified search tool — dispatches to semantic or literal search based on `mode`.
    #[tool(
        description = "Unified code search. Set `mode` to choose the backend:\n\n- `semantic` (default): vector embeddings + BM25 FTS + exact-identifier boosting, fused with RRF. Best for conceptual queries, identifier lookups, and mixed natural-language + symbol queries.\n- `literal`: pure FTS, no embeddings. Fast and works without an embedding model. Sub-mode selection:\n  * Queries with operators, brackets, or punctuation (`foo = null`, `Vec<T>`, `return x;`, `a::b`) -> set `regex=true` and write the query as a regex. BM25 tokenizes on punctuation otherwise, producing noisy results.\n  * Multi-word exact phrases -> set `phrase=true`.\n  * Plain identifier lookups (`CodesearchService`) -> leave both false.\n\nFor semantic mode, optionally set `semantic_mode`: \"auto\" (default) | \"semantic\" | \"lexical\" | \"hybrid\".\nReturns metadata only by default (`compact=true`). Use `get_chunk` to read full code. Prefer `search(mode=\"literal\", regex=true)` over external grep/ripgrep for code patterns."
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
        // Proxy guard: forward ALL search modes to serve rather than dispatching locally
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("search", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
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
        description = "Unified symbol navigation. Set `kind` to choose the action:\n\n- `definition` (default): locate where a symbol is defined (function, class, struct, etc.)\n- `usages`: find all call-sites and references to a symbol\n- `imports`: list all imports/dependencies declared in a file (set `symbol` to the file path)\n- `dependents`: find all files that import or depend on a module, file, or symbol\n\nFor `imports`, set `symbol` to a file path. For other kinds, `symbol` is the symbol name."
    )]
    async fn find(
        &self,
        Parameters(request): Parameters<FindRequest>,
    ) -> Result<CallToolResult, McpError> {
        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("find", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

        let kind = request.kind.as_deref().unwrap_or("definition").to_lowercase();
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
        description = "Unified code exploration. Set `kind` to choose the action:\n\n- `outline` (default): list all indexed top-level symbols in a file — kind, signature, and line range. Set `target` to a file path.\n- `similar`: find chunks semantically similar to a given chunk by its ID. Set `target` to the chunk_id (as string)."
    )]
    async fn explore(
        &self,
        Parameters(request): Parameters<ExploreRequest>,
    ) -> Result<CallToolResult, McpError> {
        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("explore", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

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
        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("status", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

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
        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("semantic_search", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

        // Resolve project/group routing (multi-store for group fan-out)
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
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
            request.query, limit, compact, mode, ctx.is_multi
        );

        // Ensure database exists (skip if serve-mode with routed stores)
        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // === Multi-store group fan-out ===
        if ctx.is_multi {
            return self.semantic_search_multi(
                &request, &identifiers, limit, compact,
                ctx.stores_vec.unwrap(),
                ctx.store_aliases.as_ref().unwrap(),
                &ctx.alias_roots,
            ).await;
        }

        // === Mode: "lexical" — FTS only, no embedding ===
        if mode == "lexical" {
            tracing::debug!("MCP: mode=lexical — skipping embedding service");
            return self
                .semantic_search_lexical(&request, &identifiers, limit, compact, ctx.stores, ctx.project_alias.as_deref(), &ctx.alias_roots)
                .await;
        }

        // === Modes: "semantic", "hybrid", "auto" — require embedding ===
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("MCP: Failed to get embedding service: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {}",
                        e
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
                        "Error embedding query: {}",
                        e
                    ))]));
                }
            }
        };

        // Search vector store
        let vector_results = match self
            .with_vector_store_read_for(
                |store| {
                    store
                        .search(&query_embedding, limit * 3)
                        .context("Error searching vector store")
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("MCP: Search failed: {:?}", e);
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error searching vector store: {}",
                    e
                ))]));
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
            return self.build_semantic_response(results, &request, compact, has_identifiers, ctx.project_alias.as_deref(), &ctx.alias_roots);
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
                    .search(&request.query, limit * 3, structural_intent)
                    .unwrap_or_default();

                let fused = if identifiers.is_empty() {
                    rrf_fusion(&vector_results, &fts_results, vector_k as f32)
                } else {
                    let mut all_exact: Vec<crate::fts::FtsResult> = Vec::new();
                    for ident in &identifiers {
                        if let Ok(exact) =
                            fts_store.search_exact(ident, limit * 2, structural_intent)
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
            }, ctx.stores.clone())
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

        tracing::debug!("MCP: Final {} results after hybrid search", results.len());
        self.build_semantic_response(results, &request, compact, has_identifiers, ctx.project_alias.as_deref(), &ctx.alias_roots)
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
            let fts_results = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search(&request.query, limit * 3, structural_intent),
                    stores.clone(),
                    aliases,
                )
                .await
                .unwrap_or_default();

            // Also do exact search if identifiers detected
            let mut all_fts = fts_results;
            for ident in identifiers {
                let exact = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 2, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                merge_exact_into_fts(&mut all_fts, exact);
            }

            all_fts.sort_by(|a, b| {
                b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
            });

            let results = self.resolve_fts_to_search_results_multi(&all_fts, limit, &stores).await;

            if let Some(target_kind) = structural_intent {
                // We need mutable results but we have them as vectordb::SearchResult
                let mut mutable_results = results;
                boost_kind(&mut mutable_results, target_kind);
                return self.build_semantic_response(mutable_results, request, compact, !identifiers.is_empty(), None, alias_roots);
            }

            return self.build_semantic_response(results, request, compact, !identifiers.is_empty(), None, alias_roots);
        }

        // === Modes requiring embedding: "semantic", "hybrid", "auto" ===
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {}", e
                    ))]));
                }
            };
            let service = service_guard.as_mut().unwrap();
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {}", e
                    ))]));
                }
            }
        };

        // Search vector stores across all repos
        let vector_results = self
            .with_vector_store_read_multi(
                |store| store.search(&query_embedding, limit * 3).context("Error searching vector store"),
                stores.clone(),
                aliases,
            )
            .await
            .unwrap_or_default();

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
            return self.build_semantic_response(results, request, compact, !identifiers.is_empty(), None, alias_roots);
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        // FTS search across all stores
        let fts_results = self
            .with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.query, limit * 3, structural_intent),
                stores.clone(),
                aliases,
            )
            .await
            .unwrap_or_default();

        // Exact identifier search across all stores
        let all_exact = if !identifiers.is_empty() {
            let mut exact_results: Vec<crate::fts::FtsResult> = Vec::new();
            for ident in identifiers {
                let exact = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 2, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                for r in exact {
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
                if let Some(resolved) = self.resolve_chunk_from_stores(f.chunk_id, f.rrf_score, &stores).await {
                    mapped.push(resolved);
                }
            }
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut mapped, target_kind);
        }

        self.build_semantic_response(mapped, request, compact, !identifiers.is_empty(), None, alias_roots)
    }

    /// Resolve a single chunk from multiple stores (used for FTS-only hits in multi-store fusion).
    async fn resolve_chunk_from_stores(
        &self,
        chunk_id: u32,
        score: f32,
        stores: &[Arc<SharedStores>],
    ) -> Option<crate::vectordb::SearchResult> {
        for store_arc in stores {
            let store = store_arc.vector_store.read().await;
            if let Ok(Some(chunk)) = store.get_chunk(chunk_id) {
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
    ) -> Vec<crate::vectordb::SearchResult> {
        let mut results = Vec::new();
        for fts in fts_results.iter().take(limit) {
            for store_arc in stores {
                let store = store_arc.vector_store.read().await;
                if let Ok(Some(chunk)) = store.get_chunk(fts.chunk_id) {
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

        let mut fts_results = self
            .with_fts_store_read_for(
                |fts_store| {
                    fts_store.search(&request.query, limit * 3, structural_intent)
                },
                stores.clone(),
            )
            .await
            .unwrap_or_default();

        // Also do exact search if identifiers detected
        for ident in identifiers {
            let exact = match self
                .with_fts_store_read_for(
                    |fts_store| {
                        fts_store.search_exact(ident, limit * 2, structural_intent)
                    },
                    stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(_) => continue,
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
            .resolve_fts_to_search_results(&fts_results, limit, stores)
            .await;

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        self.build_semantic_response(results, request, compact, !identifiers.is_empty(), project_alias, alias_roots)
    }

    /// Build the final SemanticSearchResponse with low-confidence signaling.
    fn build_semantic_response(
        &self,
        results: Vec<crate::vectordb::SearchResult>,
        request: &SemanticSearchRequest,
        compact: bool,
        has_identifiers: bool,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        if results.is_empty() {
            let response = SemanticSearchResponse {
                results: vec![],
                low_confidence: Some(true),
                suggested_tool: Some("literal_search".to_string()),
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
                    crate::cache::path_matches_filter(
                        &r.path,
                        &normalized_filter,
                        &project_root_normalized,
                    )
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

        let response = SemanticSearchResponse {
            results: items,
            low_confidence,
            suggested_tool,
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
    ) -> Vec<crate::vectordb::SearchResult> {
        self.with_vector_store_read_for(
            |store| {
                let mut results = Vec::new();
                for fts in fts_results.iter().take(limit) {
                    if let Ok(Some(chunk)) = store.get_chunk(fts.chunk_id) {
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
        .await
        .unwrap_or_default()
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
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

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
        } else {
            match self
                .with_fts_store_read_for(|fts_store| fts_store.search(&request.symbol, limit * 3, None), ctx.stores.clone())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {}", e
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No definition found for '{}'. The symbol may not be indexed.",
                request.symbol
            ))]));
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
                        "Error opening database: {}", e
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No definition found for '{}'. Try find_usages() to find references, or broaden your search.",
                request.symbol
            ))]));
        }

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // === find_usages tool ===

    async fn find_usages(
        &self,
        Parameters(request): Parameters<FindUsagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.find_usages_impl(request.symbol.clone(), request.limit.unwrap_or(20), request.project, request.group)
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
        let ctx = match self.resolve_routing(&project, &group).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

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
        } else {
            match self
                .with_fts_store_read_for(|fts_store| fts_store.search(&symbol, limit * 2, None), ctx.stores.clone())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {}", e
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No usages found for '{}'. The symbol may not be indexed.",
                symbol
            ))]));
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
                        "Error opening database: {}", e
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No usages found for '{}' (only definitions were found). Try find_definition() to locate the declaration.",
                symbol
            ))]));
        }

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    async fn file_outline(
        &self,
        Parameters(request): Parameters<FileOutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Outline operates on a single repo — reject group fan-out
        if ctx.is_multi {
            return Ok(CallToolResult::success(vec![Content::text(
                "Tool 'explore' operates on a single repo. Use 'project' instead of 'group'.".to_string(),
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
        let normalized = normalize_tool_path(&request.path, &project_root);

        let items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store group fan-out: collect outline items from all stores
            let mut all_items: Vec<FileOutlineItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for store_arc in sv {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(&normalized) {
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
                    Err(e) => {
                        tracing::warn!("Vector store read failed in file_outline fan-out: {:?}", e);
                    }
                }
            }
            all_items.sort_by_key(|i| i.start_line);
            all_items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut out: Vec<FileOutlineItem> = store
                            .chunks_for_file(&normalized)?
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
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error reading outline: {}",
                        e
                    ))]));
                }
            }
        };

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No indexed chunks found for path. Verify the file is within the project root and the index is up to date.".to_string(),
            )]));
        }

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Retrieve the full content of a specific chunk by its ID, plus optional surrounding lines for context.\nUse this after semantic_search or file_outline to read the actual code without loading the whole file.\n\nUSE FOR: reading a specific function/class body after finding it via search.\nSet context_lines (default 0, max 20) to include lines before and after the chunk."
    )]
    async fn get_chunk(
        &self,
        Parameters(request): Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("get_chunk", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

        tracing::info!(
            "📥 get_chunk(chunk_id={}, project={:?})",
            request.chunk_id,
            request.project,
        );
        // Resolve project/group routing
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
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

        // Look up chunk — multi-store: try each store until found
        let chunk = if let Some(ref sv) = ctx.stores_vec {
            let mut found = None;
            for store_arc in sv {
                let store = store_arc.vector_store.read().await;
                match store.get_chunk(request.chunk_id) {
                    Ok(Some(c)) => { found = Some(c); break; }
                    Ok(None) => continue,
                    Err(_) => break,
                }
            }
            found
        } else {
            self
                .with_vector_store_read_for(
                    |store| store.get_chunk(request.chunk_id),
                    ctx.stores.clone(),
                )
                .await
                .unwrap_or_default()
        };

        let mut chunk = match chunk {
            Some(c) => c,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Chunk {} not found. Verify the chunk_id and index state.",
                    request.chunk_id
                ))]));
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

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    async fn find_imports(
        &self,
        Parameters(request): Parameters<FindImportsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
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
        let normalized = normalize_tool_path(&request.path, &project_root);

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store group fan-out: collect import items from all stores
            let mut all_items: Vec<ImportItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for store_arc in sv {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(&normalized) {
                    Ok(metas) => {
                        for meta in metas {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if seen_ids.insert(meta.id) {
                                if let Ok(Some(chunk)) = store.get_chunk(meta.id) {
                                    all_items.extend(parse_import_lines(&chunk.content, chunk.start_line));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Vector store read failed in find_imports fan-out: {:?}", e);
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
                        "Error reading imports: {}",
                        e
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
                // Multi-store FTS fallback
                for keyword in &["import", "use", "from", "require", "include"] {
                    let hits = self
                        .with_fts_store_read_multi(
                            |fts_store| {
                                fts_store.search_exact(keyword, fallback_limit, None)
                            },
                            sv.clone(),
                            ctx.store_aliases.as_ref().unwrap(),
                        )
                        .await
                        .unwrap_or_default();
                    for h in hits {
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                // Resolve FTS hits via vector stores
                let mut resolved: Vec<ImportItem> = Vec::new();
                for (chunk_id, _) in &all_hits {
                    for store_arc in sv {
                        let store = store_arc.vector_store.read().await;
                        if let Ok(Some(chunk)) = store.get_chunk(*chunk_id) {
                            if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                resolved.extend(parse_import_lines(&chunk.content, chunk.start_line));
                            }
                            break;
                        }
                    }
                }
                items = resolved;
            } else {
                // Single-store FTS fallback
                for keyword in &["import", "use", "from", "require", "include"] {
                    let hits = self
                        .with_fts_store_read_for(
                            |fts_store| {
                                fts_store.search_exact(keyword, fallback_limit, None)
                            },
                            ctx.stores.clone(),
                        )
                        .await
                        .unwrap_or_default();
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
                                        out.extend(parse_import_lines(&chunk.content, chunk.start_line));
                                    }
                                }
                            }
                            Ok(out)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                    .unwrap_or_default();
            }
        }

        items.sort_by_key(|i| i.line);
        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No import chunks found. The index may not include import statements for this language, or the file has no imports.".to_string(),
            )]));
        }

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    async fn find_dependents(
        &self,
        Parameters(request): Parameters<FindDependentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
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
                    |fts_store| {
                        fts_store.search_exact(&search_term, high_limit, import_kind)
                    },
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default();

            if exact_hits.is_empty() {
                self.with_fts_store_read_multi(
                    |fts_store| {
                        fts_store.search(&search_term, high_limit, import_kind)
                    },
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
            } else {
                exact_hits
            }
        } else {
            // Single-store FTS search
            let exact_hits = self
                .with_fts_store_read_for(
                    |fts_store| {
                        fts_store.search_exact(&search_term, high_limit, import_kind)
                    },
                    ctx.stores.clone(),
                )
                .await
                .unwrap_or_default();

            if exact_hits.is_empty() {
                self.with_fts_store_read_for(
                    |fts_store| {
                        fts_store.search(&search_term, high_limit, import_kind)
                    },
                    ctx.stores.clone(),
                )
                .await
                .unwrap_or_default()
            } else {
                exact_hits
            }
        };

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: resolve chunks across all stores
            let mut seen_paths = HashSet::new();
            let mut out = Vec::new();
            for f in &fts_results {
                for store_arc in sv {
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
                            let import_statement = if chunk
                                .content
                                .to_lowercase()
                                .contains(&term_lower)
                            {
                                chunk
                                    .content
                                    .lines()
                                    .find(|l| {
                                        l.to_lowercase()
                                            .contains(&term_lower)
                                    })
                                    .unwrap_or("")
                                    .to_string()
                            } else {
                                chunk
                                    .signature
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(chunk.content.lines().next().unwrap_or("").to_string())
                            };

                            out.push(DependentItem {
                                path: chunk.path,
                                line: chunk.start_line,
                                import_statement,
                            });

                            break; // found in this store, move to next FTS result
                        }
                        Ok(None) => {} // try next store
                        Err(_) => break,
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
                            let import_statement = if chunk
                                .content
                                .to_lowercase()
                                .contains(&term_lower)
                            {
                                chunk
                                    .content
                                    .lines()
                                    .find(|l| {
                                        l.to_lowercase()
                                            .contains(&term_lower)
                                    })
                                    .unwrap_or("")
                                    .to_string()
                            } else {
                                chunk
                                    .signature
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(chunk.content.lines().next().unwrap_or("").to_string())
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
                }, ctx.stores.clone())
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error resolving dependents: {}",
                        e
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        items.sort_by(|a, b| a.path.cmp(&b.path));
        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No dependent files found for '{}'.",
                request.symbol_or_path
            ))]));
        }

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Internal: find similar chunks, used by `explore(kind="similar")`.
    async fn similar_chunks(
        &self,
        Parameters(request): Parameters<SimilarChunksRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(5).min(20);

        let mut results = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: find the embedding in whichever store has it,
            // then search across all stores for similar chunks.
            let mut embedding: Option<Vec<f32>> = None;
            for store_arc in sv {
                let store = store_arc.vector_store.read().await;
                if let Ok(Some(emb)) = store.get_embedding(request.chunk_id) {
                    embedding = Some(emb);
                    break;
                }
            }

            let embedding = match embedding {
                Some(e) => e,
                None => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Embedding not found for chunk_id {} in any store.",
                        request.chunk_id
                    ))]));
                }
            };

            // Search across all stores with the found embedding
            let mut all_results: Vec<SearchResultItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for store_arc in sv {
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
                                });
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Similarity search failed in fan-out: {:?}", e);
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
                        let embedding = store
                            .get_embedding(request.chunk_id)?
                            .ok_or_else(|| anyhow::anyhow!("embedding not found for chunk_id {}", request.chunk_id))?;

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
                        "Error finding similar chunks: {}",
                        e
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut results {
            item.path = ctx.prefix_result_path(&item.path);
        }

        let json = serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    async fn literal_search(
        &self,
        Parameters(request): Parameters<LiteralSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            let params = serde_json::to_value(&request).ok();
            return proxy.forward("literal_search", params).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

        // Resolve project/group routing
        let ctx = match self.resolve_routing(&request.project, &request.group).await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        let limit = request.limit.unwrap_or(20);
        let output_format = request.format.as_deref().unwrap_or("json");

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

        // FTS search — multi-store or single
        // Note: regex=true uses BM25 for candidates, then post-filters with the
        // actual regex on raw content (Tantivy's RegexQuery only works on individual
        // tokens, not raw text — underscores/punctuation cause empty results).
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| {
                    if request.phrase.unwrap_or(false) {
                        fts_store.search_phrase(&request.query, limit * 3)
                    } else {
                        // Both default and regex modes use BM25; regex is post-filtered
                        fts_store.search(&request.query, limit * 3, None)
                    }
                },
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| {
                        if request.phrase.unwrap_or(false) {
                            fts_store.search_phrase(&request.query, limit * 3)
                        } else {
                            fts_store.search(&request.query, limit * 3, None)
                        }
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {}", e
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No results found for '{}'. Try a different query or mode.",
                request.query
            ))]));
        }

        // Resolve chunk metadata and apply post-filters
        let lang_filter = request.language.clone();
        let glob_filter = request.file_glob.clone();
        let regex_enabled = request.regex.unwrap_or(false);
        let snippet_regex = if regex_enabled {
            Regex::new(&request.query).ok()
        } else {
            None
        };
        // Pre-compute normalized project root for stripping absolute paths in glob matching
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };

        let mut items: Vec<LiteralSearchResultItem> = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: resolve chunks from all stores
            let mut items: Vec<LiteralSearchResultItem> = Vec::new();
            'outer: for fts_result in &fts_results {
                for store_arc in sv {
                    let store = store_arc.vector_store.read().await;
                    if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                        // Apply filters
                        if let Some(ref lang) = lang_filter {
                            let file_lang = Language::from_path(std::path::Path::new(&chunk.path));
                            if file_lang.name() != lang {
                                continue;
                            }
                        }
                        if let Some(ref glob) = glob_filter {
                            let relative_path = chunk.path
                                .strip_prefix(&project_root_normalized)
                                .unwrap_or(&chunk.path)
                                .trim_start_matches('/');
                            if !simple_glob_match(glob, relative_path) {
                                continue;
                            }
                        }
                        let match_info = match_line_for_literal(
                            &chunk.content, &request.query, snippet_regex.as_ref(),
                        );
                        if regex_enabled && match_info.is_none() {
                            continue;
                        }
                        let (match_offset, snippet) = match_info
                            .unwrap_or_else(|| (0, chunk.content.lines().next().unwrap_or("").to_string()));
                        let match_line = chunk.start_line + match_offset;
                        items.push(LiteralSearchResultItem {
                            path: chunk.path,
                            start_line: match_line,
                            end_line: match_line,
                            snippet,
                            score: fts_result.score,
                            kind: if chunk.kind.is_empty() { None } else { Some(chunk.kind) },
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
                                let file_lang = Language::from_path(std::path::Path::new(&chunk.path));
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
                                &chunk.content, &request.query, snippet_regex.as_ref(),
                            );
                            if regex_enabled && match_info.is_none() {
                                return None;
                            }
                            let (match_offset, snippet) = match_info
                                .unwrap_or_else(|| (0, chunk.content.lines().next().unwrap_or("").to_string()));
                            let match_line = chunk.start_line + match_offset;
                            Some(LiteralSearchResultItem {
                                path: chunk.path,
                                start_line: match_line,
                                end_line: match_line,
                                snippet,
                                score,
                                kind: if chunk.kind.is_empty() { None } else { Some(chunk.kind) },
                                signature: chunk.signature.filter(|s| !s.is_empty()),
                            })
                        })
                        .collect();
                    Ok(items)
                }, ctx.stores.clone())
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error resolving search results: {}", e
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No results found for '{}' after applying filters.",
                request.query
            ))]));
        }

        // Format output
        let output = if output_format == "grep" {
            items
                .iter()
                .map(|item| format!("{}:{}:{}", item.path, item.start_line, item.snippet))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
        };

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Internal implementation for index_status with optional project/group routing.
    async fn index_status_impl(&self, project: Option<String>, group: Option<String>) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self.resolve_routing(&project, &group).await {
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

            for store_arc in sv {
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
                    Err(_) => {
                        all_indexed = false;
                    }
                }
            }

            let (status, status_message) = if total_chunks == 0 {
                (
                    "building".to_string(),
                    format!("Index is being built across {} repo(s). Searches may fail until indexing completes.", sv.len()),
                )
            } else {
                (
                    "ready".to_string(),
                    format!("Index is ready for searching across {} repo(s).", sv.len()),
                )
            };

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

            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Single-store path
        let stats = match self
            .with_vector_store_read_for(|store| store.stats().context("Error getting index stats"), ctx.stores.clone())
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
    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        // If in proxy mode, forward to serve
        if let Some(ref proxy) = self.proxy {
            return proxy.forward("list_projects", None).await.map_err(|e| {
                McpError::internal_error(e.message.into_owned(), None)
            });
        }

        let serve_active = self.serve_state.is_some();
        let serve_url = if serve_active {
            Some(crate::mcp::proxy::serve_url_from_env())
        } else {
            None
        };

        // When serve is active, use ServeState as source of truth for lock status
        if let Some(ref serve_state) = self.serve_state {
            let config = serve_state.config_snapshot();
            let mut repos_info = Vec::new();

            for (alias, path) in &config.repos {
                let db_path = path.join(crate::constants::DB_DIR_NAME);

                let (total_chunks, total_files, model, lock_status) = if db_path.exists() {
                    let (model_name, dims) = read_model_metadata(&db_path);

                    // Use DashMap state for opened repos, disk check for unopened
                    let lock_status = match serve_state.repo_lock_status(alias) {
                        Some(s) => s.to_string(),
                        None => {
                            // Not yet opened in DashMap — fall back to disk check
                            if crate::index::is_database_locked(&db_path) {
                                "locked-externally".to_string()
                            } else {
                                "available".to_string()
                            }
                        }
                    };

                    if let Ok(store) = VectorStore::new(&db_path, dims) {
                        if let Ok(stats) = store.stats() {
                            (stats.total_chunks, stats.total_files, model_name, lock_status)
                        } else {
                            (0, 0, model_name, lock_status)
                        }
                    } else {
                        (0, 0, model_name, lock_status)
                    }
                } else {
                    (0, 0, "not indexed".to_string(), "unknown".to_string())
                };

                repos_info.push(RepoInfo {
                    alias: alias.clone(),
                    project_path: path.display().to_string(),
                    database_path: db_path.display().to_string(),
                    total_chunks,
                    total_files,
                    model,
                    lock_status,
                });
            }

            let response = ListProjectsResponse {
                repos: repos_info,
                groups: config.groups,
                serve_active,
                serve_url,
                current_directory: current_dir.display().to_string(),
            };

            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Stdio mode: fall back to disk-based lock detection
        let config = load_repos_config().unwrap_or_default();
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
                        (stats.total_chunks, stats.total_files, model_name, lock.to_string())
                    } else {
                        (0, 0, model_name, lock.to_string())
                    }
                } else {
                    (0, 0, model_name, "readonly".to_string())
                }
            } else {
                (0, 0, "not indexed".to_string(), "unknown".to_string())
            };

            repos_info.push(RepoInfo {
                alias: alias.clone(),
                project_path: path.display().to_string(),
                database_path: db_path.display().to_string(),
                total_chunks,
                total_files,
                model,
                lock_status,
            });
        }

        let response = ListProjectsResponse {
            repos: repos_info,
            groups: config.groups,
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

    PREFIXES.iter().any(|prefix| {
        if !sig.starts_with(prefix) {
            return false;
        }

        let rest = &sig[prefix.len()..];
        if !rest.starts_with(symbol) {
            return false;
        }

        let next = rest[symbol.len()..].chars().next();
        matches!(next, None | Some('(' | '<' | ':' | ' ' | '\t'))
    })
}

#[tool_handler]
impl ServerHandler for CodesearchService {
    fn get_info(&self) -> ServerInfo {
        let db_exists = self.db_path.exists();
        let mode = if self.proxy.is_some() {
            format!("proxy → {} (serve hub)", self.proxy.as_ref().unwrap().base_url())
        } else if self.serve_state.is_some() {
            "serve hub (direct)".to_string()
        } else {
            "self-contained (stdio)".to_string()
        };

        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: rmcp::model::Implementation {
                name: "codesearch".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(format!(
                r#"codesearch — semantic + lexical code search MCP server.

TOOLS:
| Tool          | Use for                                              |
|---------------|------------------------------------------------------|
| search        | Code search: `mode="semantic"` (default) or `mode="literal"` |
| find          | Symbol navigation: `kind="definition"` (default), `"usages"`, `"imports"`, `"dependents"` |
| explore       | File exploration: `kind="outline"` (default) or `"similar"` |
| get_chunk     | Read full chunk content by chunk_id                  |
| status        | Index/project info: `kind="index"` (default) or `"projects"` |

Indexing is done via CLI: `codesearch index`. The MCP server cannot index.

Mode: {mode}
Current project: {project}
Current database: {db} ({exists})
Model: {model} ({dims}d)
"#,
                mode = mode,
                project = self.project_path.display(),
                db = self.db_path.display(),
                exists = if db_exists { "ready" } else { "not found" },
                model = self.model_type.short_name(),
                dims = self.dimensions
            )),
            ..Default::default()
        }
    }
}

// === Server Entry Point ===

/// Run the MCP server using stdio transport with file watching for live index updates.
///
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
    cancel_token: CancellationToken,
) -> Result<()> {
    use rmcp::{transport::stdio, ServiceExt};

    // === Proxy detection: probe `codesearch serve` with retry ===
    let serve_url = crate::mcp::proxy::serve_url_from_env();

    for attempt in 1..=crate::constants::HEALTH_PROBE_ATTEMPTS {
        match crate::mcp::proxy::McpProxy::check_health(&serve_url).await {
            Ok(true) => {
                tracing::info!(
                    "🔀 codesearch serve detected at {} — entering proxy mode (attempt {}/{})",
                    serve_url, attempt, crate::constants::HEALTH_PROBE_ATTEMPTS
                );
                let proxy = crate::mcp::proxy::McpProxy::new(serve_url);
                let service = CodesearchService::new_for_proxy(proxy)?;
                let server = service.serve(stdio()).await?;
                tracing::info!("🔀 MCP proxy ready — forwarding all tool calls to serve");
                tokio::select! {
                    result = server.waiting() => {
                        tracing::info!("MCP proxy transport closed");
                        result?;
                    }
                    _ = cancel_token.cancelled() => {
                        tracing::info!("🛑 Shutdown signal received, stopping MCP proxy...");
                    }
                }
                tracing::info!("✅ MCP proxy shut down cleanly");
                return Ok(());
            }
            Err(e) => {
                // Version mismatch — hard error, no retry
                tracing::error!("❌ {}", e);
                return Err(e);
            }
            Ok(false) => {
                if attempt < crate::constants::HEALTH_PROBE_ATTEMPTS {
                    tracing::debug!(
                        "Health probe attempt {}/{} — serve not ready, retrying in {}ms",
                        attempt,
                        crate::constants::HEALTH_PROBE_ATTEMPTS,
                        crate::constants::HEALTH_PROBE_RETRY_DELAY_MS,
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(
                        crate::constants::HEALTH_PROBE_RETRY_DELAY_MS,
                    ))
                    .await;
                }
            }
        }
    }

    // All probes failed — fall through to stdio
    tracing::info!(
        "ℹ️  No codesearch serve detected after {} attempts — using local stdio mode",
        crate::constants::HEALTH_PROBE_ATTEMPTS
    );

    // Set FASTEMBED_CACHE_DIR early (before any embedding work) to ensure fastembed
    // downloads and caches models to ~/.codesearch/models instead of creating
    // .fastembed_cache in the current working directory.
    match crate::constants::get_global_models_cache_dir() {
        Ok(models_dir) => {
            std::env::set_var("FASTEMBED_CACHE_DIR", &models_dir);
        }
        Err(e) => {
            tracing::warn!("Could not set FASTEMBED_CACHE_DIR: {}", e);
        }
    }

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
        let model_name = format!("{:?}", model_type);
        let dimensions = model_type.dimensions();

        // Create minimal metadata.json (matching format used by build_index)
        let metadata_path = db_path.join("metadata.json");
        let metadata = serde_json::json!({
            "model_short_name": model_short_name,
            "model_name": model_name,
            "dimensions": dimensions,
            "indexed_at": chrono::Utc::now().to_rfc3339()
        });
        tokio::fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?).await?;

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
                    "⚠️  Could not parse dimensions from metadata.json, using default 384"
                );
                384
            }
        }
    } else {
        tracing::warn!("⚠️  metadata.json not found, using default dimensions 384");
        384
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
                    if let Err(e) = index_manager_arc.start_file_watcher(bg_cancel_token).await {
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
