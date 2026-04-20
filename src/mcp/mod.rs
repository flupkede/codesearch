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
}

pub mod types;

use crate::db_discovery::{find_best_database, find_databases};
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
}

impl std::fmt::Debug for CodesearchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodesearchService")
            .field("db_path", &self.db_path)
            .field("model_type", &self.model_type)
            .field("dimensions", &self.dimensions)
            .field("has_shared_stores", &self.shared_stores.is_some())
            .finish()
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

    /// Check if database exists and return error if not
    fn ensure_database_exists(&self) -> Result<(), String> {
        if !self.db_path.exists() {
            return Err(format!(
                "❌ No index database found at: {}\n\n\
                 ⚠️  IMPORTANT: This MCP server cannot index the codebase itself. Indexing takes 30-60 seconds and must be done manually.\n\n\
                 To fix this, run the following command in your terminal:\n\
                 $ cd {}\n\
                 $ codesearch index\n\n\
                 For more information about database locations, use the find_databases tool.",
                self.db_path.display(),
                self.project_path.display()
            ));
        }
        Ok(())
    }

    /// Execute a read-only action against the vector store, using shared stores when available
    /// and falling back to opening a standalone store otherwise.
    async fn with_vector_store_read<R, F>(&self, mut action: F) -> Result<R>
    where
        F: FnMut(&VectorStore) -> anyhow::Result<R>,
    {
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

    /// Execute a read-only action against the FTS store, using shared stores when available
    /// and falling back to opening a standalone FtsStore otherwise.
    async fn with_fts_store_read<R, F>(&self, action: F) -> Result<R>
    where
        F: Fn(&FtsStore) -> Result<R>,
    {
        if let Some(ref stores) = self.shared_stores {
            let fts = stores.fts_store.read().await;
            return action(&fts);
        }

        // Fallback: open a new FtsStore
        let fts_store = FtsStore::new(&self.db_path).context("Error opening FTS store")?;
        action(&fts_store)
    }

    #[tool(
        description = "Hybrid code search over tree-sitter AST chunks: vector embeddings + Tantivy FTS + exact-identifier boosting, fused with RRF.\n\nUSE FOR:\n- Conceptual queries (\"where is auth handled\", \"how do we log errors\")\n- Identifier lookups — function/class/variable names are boosted via exact-match FTS\n- Mixed natural-language + symbol queries\n\nDO NOT USE FOR:\n- Finding a symbol's definition specifically — use `find_definition`\n- Finding all call-sites of a symbol — use `find_usages`\n\nOPTIONAL `mode`: \"auto\" (default) | \"semantic\" | \"lexical\" | \"hybrid\".\nReturns metadata only by default (compact=true). Prefer `get_chunk` to read full body/context for a selected result."
    )]
    async fn semantic_search(
        &self,
        Parameters(request): Parameters<SemanticSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(10);
        let compact = request.compact.unwrap_or(true);
        let mode = request.mode.as_deref().unwrap_or("auto");
        let identifiers = detect_identifiers(&request.query);
        let has_identifiers = !identifiers.is_empty();

        tracing::debug!(
            "MCP semantic_search: query='{}', limit={}, compact={}, mode='{}'",
            request.query,
            limit,
            compact,
            mode
        );

        // Ensure database exists
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // === Mode: "lexical" — FTS only, no embedding ===
        if mode == "lexical" {
            tracing::debug!("MCP: mode=lexical — skipping embedding service");
            return self
                .semantic_search_lexical(&request, &identifiers, limit, compact)
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
            .with_vector_store_read(|store| {
                store
                    .search(&query_embedding, limit * 3)
                    .context("Error searching vector store")
            })
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
            return self.build_semantic_response(results, &request, compact, has_identifiers);
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
            .with_fts_store_read(|fts_store| {
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
            })
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
        self.build_semantic_response(results, &request, compact, has_identifiers)
    }

    // === Helper methods (not exposed as tools) ===

    /// Lexical-only search: FTS without embedding service.
    async fn semantic_search_lexical(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
    ) -> Result<CallToolResult, McpError> {
        let structural_intent = detect_structural_intent(&request.query);

        let mut fts_results = self
            .with_fts_store_read(|fts_store| {
                fts_store.search(&request.query, limit * 3, structural_intent)
            })
            .await
            .unwrap_or_default();

        // Also do exact search if identifiers detected
        for ident in identifiers {
            let exact = match self
                .with_fts_store_read(|fts_store| {
                    fts_store.search_exact(ident, limit * 2, structural_intent)
                })
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
            .resolve_fts_to_search_results(&fts_results, limit)
            .await;

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        self.build_semantic_response(results, request, compact, !identifiers.is_empty())
    }

    /// Build the final SemanticSearchResponse with low-confidence signaling.
    fn build_semantic_response(
        &self,
        results: Vec<crate::vectordb::SearchResult>,
        request: &SemanticSearchRequest,
        compact: bool,
        has_identifiers: bool,
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

        let items: Vec<SearchResultItem> = results
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
    ) -> Vec<crate::vectordb::SearchResult> {
        self.with_vector_store_read(|store| {
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
        })
        .await
        .unwrap_or_default()
    }

    // === find_definition tool ===

    #[tool(
        description = "Locate the definition of a symbol (function, class, method, struct, trait, enum, type).\nUses FTS + chunk-kind filter to exclude usages, comments, and string literals.\n\nUSE FOR: \"where is X defined\", \"show me the declaration of X\".\nDO NOT USE FOR: finding all call-sites → use `find_usages`."
    )]
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

        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // Search with extra results — we'll filter down
        let fts_results = match self
            .with_fts_store_read(|fts_store| fts_store.search(&request.symbol, limit * 3, None))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error searching: {}",
                    e
                ))]));
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No definition found for '{}'. The symbol may not be indexed.",
                request.symbol
            ))]));
        }

        // Resolve chunk metadata and filter by definition kinds
        let items: Vec<ReferenceItem> = match self
            .with_vector_store_read(|store| {
                let items = fts_results
                    .iter()
                    .filter_map(|fts_result| {
                        if let Ok(Some(chunk)) = store.get_chunk(fts_result.chunk_id) {
                            if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                                return None;
                            }
                            if let Some(ref requested_kind) = request.kind {
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
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error opening database: {}",
                    e
                ))]));
            }
        };

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

    #[tool(
        description = "Find call-sites and other usages of a symbol across the codebase.\nUses FTS; excludes the chunks that are the symbol's own definition.\n\nUSE FOR: impact analysis, refactoring, \"who calls X\".\nDO NOT USE FOR: finding the definition itself → use `find_definition`."
    )]
    async fn find_usages(
        &self,
        Parameters(request): Parameters<FindUsagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.find_usages_impl(request.symbol.clone(), request.limit.unwrap_or(20))
            .await
    }

    /// Shared implementation for find_usages and the deprecated find_references alias.
    async fn find_usages_impl(
        &self,
        symbol: String,
        limit: usize,
    ) -> Result<CallToolResult, McpError> {
        tracing::debug!("MCP find_usages: symbol='{}', limit={}", symbol, limit);

        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        let fts_results = match self
            .with_fts_store_read(|fts_store| fts_store.search(&symbol, limit * 2, None))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error searching: {}",
                    e
                ))]));
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No usages found for '{}'. The symbol may not be indexed.",
                symbol
            ))]));
        }

        // Resolve chunks and exclude definition chunks
        let items: Vec<ReferenceItem> = match self
            .with_vector_store_read(|store| {
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
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error opening database: {}",
                    e
                ))]));
            }
        };

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No usages found for '{}' (only definitions were found). Try find_definition() to locate the declaration.",
                symbol
            ))]));
        }

        let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "DEPRECATED. Use `find_definition` to locate a symbol's declaration, or `find_usages` to find call-sites.\nThis tool is retained as an alias for `find_usages` and may be removed in a future version."
    )]
    async fn find_references(
        &self,
        Parameters(request): Parameters<FindReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.find_usages_impl(request.symbol, request.limit.unwrap_or(20))
            .await
    }

    #[tool(
        description = "List all indexed top-level symbols in a file — kind, signature, and line range, no body content.\nUse this to understand a file's structure before deciding which chunks to read.\nMuch cheaper than reading the full file.\n\nUSE FOR: \"what functions are in auth.rs\", \"show me the structure of this module\".\nDO NOT USE FOR: finding where a symbol is defined across the codebase → use `find_definition`."
    )]
    async fn file_outline(
        &self,
        Parameters(request): Parameters<FileOutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        let _project = request.project.as_deref();
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        let normalized = normalize_tool_path(&request.path, &self.project_path);
        let items = match self
            .with_vector_store_read(|store| {
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
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error reading outline: {}",
                    e
                ))]));
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
        let _project = request.project.as_deref();
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        let mut clamped = false;
        let mut context_lines = request.context_lines.unwrap_or(0);
        if context_lines > 20 {
            context_lines = 20;
            clamped = true;
        }

        let chunk = match self
            .with_vector_store_read(|store| store.get_chunk(request.chunk_id))
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Chunk {} not found. Verify the chunk_id and index state.",
                    request.chunk_id
                ))]));
            }
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error loading chunk: {}",
                    e
                ))]));
            }
        };

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

    #[tool(
        description = "List all imports/dependencies declared in a source file.\nUses tree-sitter AST data already present in the index — no re-parsing needed.\n\nUSE FOR: \"what does auth.rs depend on\", understanding a file's dependencies before refactoring.\nDO NOT USE FOR: finding who imports this file → use `find_dependents`."
    )]
    async fn find_imports(
        &self,
        Parameters(request): Parameters<FindImportsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let _project = request.project.as_deref();
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // Import statements are currently represented mostly as gap-classified
        // `Imports` chunks (not always as per-statement AST definition chunks).
        // We therefore parse lines inside import-like chunks and use an FTS
        // fallback when no import-like chunks exist for the file.
        let normalized = normalize_tool_path(&request.path, &self.project_path);

        let mut items = match self
            .with_vector_store_read(|store| {
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
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error reading imports: {}",
                    e
                ))]));
            }
        };

        if items.is_empty() {
            // Fallback: no import-kind chunks found for this file. Broaden the
            // search to common import keywords and filter to the target path.
            // Limitation: this only finds chunks containing these literal words;
            // language-specific import forms that lack these keywords will be missed.
            let fallback_limit = 40usize;
            let mut all_hits: Vec<(u32, f32)> = Vec::new();
            let mut seen_ids: HashSet<u32> = HashSet::new();

            for keyword in &["import", "use", "from", "require", "include"] {
                let hits = self
                    .with_fts_store_read(|fts_store| {
                        fts_store.search_exact(keyword, fallback_limit, None)
                    })
                    .await
                    .unwrap_or_default();
                for h in hits {
                    if seen_ids.insert(h.chunk_id) {
                        all_hits.push((h.chunk_id, h.score));
                    }
                }
            }

            items = self
                .with_vector_store_read(|store| {
                    let mut out = Vec::new();
                    for (chunk_id, _) in &all_hits {
                        if let Some(chunk) = store.get_chunk(*chunk_id)? {
                            if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                out.extend(parse_import_lines(&chunk.content, chunk.start_line));
                            }
                        }
                    }
                    Ok(out)
                })
                .await
                .unwrap_or_default();
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

    #[tool(
        description = "Find all files that import or depend on a given module, file path, or symbol.\nEssential for impact analysis: \"if I change this module, what else breaks?\"\n\nUSE FOR: refactoring impact analysis, understanding who depends on a module.\nDO NOT USE FOR: finding usages of a specific function call → use `find_usages`."
    )]
    async fn find_dependents(
        &self,
        Parameters(request): Parameters<FindDependentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let _project = request.project.as_deref();
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        let limit = request.limit.unwrap_or(20).min(200);
        let high_limit = (limit * 10).max(200); // generous budget for filtering

        // Two-phase search strategy:
        // 1. `search_exact` — precise term match on signature+content. Better at
        //    finding specific identifiers inside import regions without drowning
        //    in noise from non-import chunks.
        // 2. If that yields no import-kind results, fall back to `search`
        //    (QueryParser, broader tokenization) with a large limit.
        //
        // Limitation: the chunker does not emit per-statement AST import chunks;
        // imports are gap-classified as `Imports` kind. Chunks whose kind doesn't
        // match `is_import_kind()` will be missed regardless of search method.
        let exact_hits = self
            .with_fts_store_read(|fts_store| {
                fts_store.search_exact(&request.symbol_or_path, high_limit, None)
            })
            .await
            .unwrap_or_default();

        let fts_results = if exact_hits.is_empty() {
            self.with_fts_store_read(|fts_store| {
                fts_store.search(&request.symbol_or_path, high_limit, None)
            })
            .await
            .unwrap_or_default()
        } else {
            exact_hits
        };

        let mut items = match self
            .with_vector_store_read(|store| {
                let mut seen_paths = HashSet::new();
                let mut out = Vec::new();
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
                        // symbol, rather than returning the entire chunk content.
                        let import_statement = if chunk
                            .content
                            .to_lowercase()
                            .contains(&request.symbol_or_path.to_lowercase())
                        {
                            chunk
                                .content
                                .lines()
                                .find(|l| {
                                    l.to_lowercase()
                                        .contains(&request.symbol_or_path.to_lowercase())
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
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error resolving dependents: {}",
                    e
                ))]));
            }
        };

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

    #[tool(
        description = "Find chunks semantically similar to a given chunk (by chunk_id).\nUses the chunk's existing embedding — no new embedding call needed.\n\nUSE FOR: finding duplicate implementations, similar patterns, related code across the codebase.\nDO NOT USE FOR: finding where a symbol is used → use `find_usages`."
    )]
    async fn similar_chunks(
        &self,
        Parameters(request): Parameters<SimilarChunksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let _project = request.project.as_deref();
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        let limit = request.limit.unwrap_or(5).min(20);

        let results = match self
            .with_vector_store_read(|store| {
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
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error finding similar chunks: {}",
                    e
                ))]));
            }
        };

        let json = serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Search code using literal/FTS matching without embeddings. Three modes: exact (default), regex (set regex=true), phrase (set phrase=true). Supports file_glob and language post-filters. Use this when you need fast exact text search, pattern matching, or phrase search. Does NOT require an embedding model."
    )]
    async fn literal_search(
        &self,
        Parameters(request): Parameters<LiteralSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(20);
        let output_format = request.format.as_deref().unwrap_or("json");

        tracing::debug!(
            "MCP literal_search: query='{}', regex={:?}, phrase={:?}, limit={}, file_glob={:?}, language={:?}, format={}",
            request.query,
            request.regex,
            request.phrase,
            limit,
            request.file_glob,
            request.language,
            output_format
        );

        // Ensure database exists
        if let Err(e) = self.ensure_database_exists() {
            return Ok(CallToolResult::success(vec![Content::text(e)]));
        }

        // Use shared FTS store when available, open standalone otherwise
        let fts_results = match self
            .with_fts_store_read(|fts_store| {
                // Determine search mode and execute
                if request.regex.unwrap_or(false) {
                    fts_store.search_regex(&request.query, limit * 3)
                } else if request.phrase.unwrap_or(false) {
                    fts_store.search_phrase(&request.query, limit * 3)
                } else {
                    // Default: BM25 exact term search
                    fts_store.search(&request.query, limit * 3, None)
                }
            })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error searching: {}",
                    e
                ))]));
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No results found for '{}'. Try a different query or mode.",
                request.query
            ))]));
        }

        // Resolve chunk metadata and apply post-filters using shared store helper
        let lang_filter = request.language.clone();
        let glob_filter = request.file_glob.clone();
        let snippet_regex = if request.regex.unwrap_or(false) {
            Regex::new(&request.query).ok()
        } else {
            None
        };
        // Pre-compute normalized project root for stripping absolute paths in glob matching
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };
        let items: Vec<LiteralSearchResultItem> = match self
            .with_vector_store_read(|store| {
                let items: Vec<LiteralSearchResultItem> = fts_results
                    .iter()
                    .filter_map(|fts_result| {
                        let chunk = store.get_chunk(fts_result.chunk_id).ok()??;
                        Some((chunk, fts_result.score))
                    })
                    .filter(|(chunk, _)| {
                        // Language post-filter
                        if let Some(ref lang) = lang_filter {
                            let file_lang = Language::from_path(std::path::Path::new(&chunk.path));
                            if file_lang.name() != lang {
                                return false;
                            }
                        }
                        // file_glob post-filter (strip project root to get relative path)
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
                    .map(|(chunk, score)| {
                        // Prefer the first line that actually matches the query.
                        // Edge case: if FTS tokenization matched across boundaries and no
                        // concrete line contains the literal/regex, fall back to first line.
                        let (match_offset, snippet) = match_line_for_literal(
                            &chunk.content,
                            &request.query,
                            snippet_regex.as_ref(),
                        )
                        .unwrap_or_else(|| {
                            (
                                0,
                                chunk.content.lines().next().unwrap_or("").to_string(),
                            )
                        });

                        LiteralSearchResultItem {
                            path: chunk.path,
                            start_line: chunk.start_line + match_offset,
                            end_line: chunk.end_line,
                            snippet,
                            score,
                            kind: if chunk.kind.is_empty() {
                                None
                            } else {
                                Some(chunk.kind)
                            },
                            signature: chunk.signature.filter(|s| !s.is_empty()),
                        }
                    })
                    .collect();
                Ok(items)
            })
            .await
        {
            Ok(items) => items,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error resolving search results: {}",
                    e
                ))]));
            }
        };

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

    #[tool(
        description = "Get the status of the semantic search index including model info and statistics. Check this before searching to verify the index is ready."
    )]
    async fn index_status(&self) -> Result<CallToolResult, McpError> {
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
            };
            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let stats = match self
            .with_vector_store_read(|store| store.stats().context("Error getting index stats"))
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
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find all available codesearch databases in current directory, parent directories, and globally tracked repositories. Use this to discover which databases are available for searching."
    )]
    async fn find_databases(&self) -> Result<CallToolResult, McpError> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let dbs = find_databases().unwrap_or_default();

        let mut response_dbs = Vec::new();

        for db_info in &dbs {
            // Get stats for this database
            let (total_chunks, total_files, model) = if db_info.db_path.exists() {
                // Try to read model from metadata
                let metadata_path = db_info.db_path.join("metadata.json");
                let model_name = if metadata_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                            json.get("model_short_name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string()
                        } else {
                            "unknown".to_string()
                        }
                    } else {
                        "unknown".to_string()
                    }
                } else {
                    "unknown".to_string()
                };

                // Try to get stats - need to infer dimensions from model name
                let dims = match model_name.as_str() {
                    "minilm-l6" | "minilm-l6-q" | "minilm-l12" | "minilm-l12-q" | "bge-small"
                    | "bge-small-q" | "e5-multilingual" => 384,
                    "bge-base" | "jina-code" | "nomic-v1.5" => 768,
                    "bge-large" | "mxbai-large" => 1024,
                    _ => 384, // default
                };

                // Try to get stats
                if let Ok(store) = VectorStore::new(&db_info.db_path, dims) {
                    if let Ok(stats) = store.stats() {
                        (stats.total_chunks, stats.total_files, model_name)
                    } else {
                        (0, 0, model_name)
                    }
                } else {
                    (0, 0, model_name)
                }
            } else {
                (0, 0, "not found".to_string())
            };

            response_dbs.push(DatabaseInfoResponse {
                database_path: db_info.db_path.display().to_string(),
                project_path: db_info.project_path.display().to_string(),
                is_current_directory: db_info.is_current,
                depth_from_current: db_info.depth,
                total_chunks,
                total_files,
                model,
            });
        }

        // Build message based on what was found
        let message = if dbs.is_empty() {
            "❌ No databases found. Run 'codesearch index' to create an index.".to_string()
        } else if dbs.iter().any(|d| d.is_current) {
            format!(
                "✅ Found {} database(s). Current directory has an index.",
                dbs.len()
            )
        } else {
            format!("⚠️  Found {} database(s) in parent/global directories, but not in current directory.", dbs.len())
        };

        let response = FindDatabasesResponse {
            databases: response_dbs,
            message,
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
| Tool              | Use for                                              |
|-------------------|------------------------------------------------------|
| semantic_search   | Conceptual queries, identifier + natural-language mix |
| find_definition   | Where is symbol X defined                             |
| find_usages       | Who uses / calls symbol X                             |
| find_references   | DEPRECATED — alias for find_usages                    |
| index_status      | Verify the index is ready                             |
| find_databases    | Discover available indexes                            |
| literal_search    | Fast exact text search (regex, phrase, BM25)          |

Indexing is done via CLI: `codesearch index`. The MCP server cannot index.

Current project: {project}
Current database: {db} ({exists})
Model: {model} ({dims}d)
"#,
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
