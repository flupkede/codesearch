use super::*;
use crate::cache::{normalize_filter_path, normalize_path_str, path_matches_filter};
use crate::chunker::ChunkKind;

// ── detect_identifiers ───────────────────────────────────────────────────

#[test]
fn test_detect_identifiers_pascal_case() {
    let ids = detect_identifiers("find the VectorStore struct");
    assert!(ids.contains(&"VectorStore".to_string()));
}

#[test]
fn test_detect_identifiers_snake_case() {
    let ids = detect_identifiers("where is find_git_root defined");
    assert!(ids.contains(&"find_git_root".to_string()));
}

#[test]
fn test_detect_identifiers_camel_case() {
    let ids = detect_identifiers("show me insertChunksWithIds");
    assert!(ids.contains(&"insertChunksWithIds".to_string()));
}

#[test]
fn test_detect_identifiers_plain_words_ignored() {
    // Plain lowercase words that are not identifiers
    let ids = detect_identifiers("what does this function do");
    assert!(ids.is_empty());
}

#[test]
fn test_detect_identifiers_mixed_query() {
    let ids = detect_identifiers("how does VectorStore handle find_git_root");
    assert!(ids.contains(&"VectorStore".to_string()));
    assert!(ids.contains(&"find_git_root".to_string()));
}

// ── detect_structural_intent ─────────────────────────────────────────────

#[test]
fn test_detect_structural_intent_struct_keyword() {
    let kind = detect_structural_intent("struct VectorStore definition");
    assert_eq!(kind, Some(ChunkKind::Struct));
}

#[test]
fn test_detect_structural_intent_fn_keyword() {
    let kind = detect_structural_intent("fn find_git_root implementation");
    assert!(matches!(kind, Some(ChunkKind::Function)));
}

#[test]
fn test_detect_structural_intent_class_keyword() {
    let kind = detect_structural_intent("class IndexManager definition");
    assert_eq!(kind, Some(ChunkKind::Class));
}

#[test]
fn test_detect_structural_intent_enum_keyword() {
    let kind = detect_structural_intent("enum ChunkKind variants");
    assert_eq!(kind, Some(ChunkKind::Enum));
}

#[test]
fn test_detect_structural_intent_trait_keyword() {
    let kind = detect_structural_intent("trait Searchable implementation");
    assert_eq!(kind, Some(ChunkKind::Trait));
}

#[test]
fn test_detect_structural_intent_no_identifier_returns_none() {
    // Structural keyword present but no identifier → None
    let kind = detect_structural_intent("how does a struct work");
    assert_eq!(kind, None);
}

#[test]
fn test_detect_structural_intent_no_keyword_returns_none() {
    // Identifier present but no structural keyword → None
    let kind = detect_structural_intent("show me VectorStore");
    assert_eq!(kind, None);
}

#[test]
fn test_detect_structural_intent_plain_query_returns_none() {
    let kind = detect_structural_intent("how does error handling work");
    assert_eq!(kind, None);
}

#[test]
fn test_detect_structural_intent_respects_quiet_mode() {
    // With quiet=true, info_print! calls inside detect_structural_intent
    // must not panic — they should silently be suppressed.
    crate::output::set_quiet(true);
    let kind = detect_structural_intent("struct VectorStore");
    assert_eq!(kind, Some(ChunkKind::Struct));
    crate::output::set_quiet(false);
}

// ── JsonResult compact serialization ─────────────────────────────────────

#[test]
fn test_json_result_full_includes_content() {
    let r = JsonResult {
        path: "src/foo.rs".to_string(),
        start_line: 1,
        end_line: 10,
        kind: "Function".to_string(),
        content: Some("fn foo() {}".to_string()),
        score: 0.9,
        signature: None,
        context_prev: None,
        context_next: None,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"content\""));
    assert!(json.contains("fn foo()"));
}

#[test]
fn test_json_result_compact_omits_content() {
    let r = JsonResult {
        path: "src/foo.rs".to_string(),
        start_line: 1,
        end_line: 10,
        kind: "Function".to_string(),
        content: None,
        score: 0.9,
        signature: None,
        context_prev: None,
        context_next: None,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("\"content\""));
    assert!(!json.contains("\"context_prev\""));
    assert!(!json.contains("\"context_next\""));
}

#[test]
fn test_json_result_compact_retains_required_fields() {
    let r = JsonResult {
        path: "src/vectordb/store.rs".to_string(),
        start_line: 42,
        end_line: 80,
        kind: "Struct".to_string(),
        content: None,
        score: 0.75,
        signature: Some("VectorStore".to_string()),
        context_prev: None,
        context_next: None,
    };
    let json = serde_json::to_string(&r).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["path"], "src/vectordb/store.rs");
    assert_eq!(v["start_line"], 42);
    assert_eq!(v["end_line"], 80);
    assert_eq!(v["kind"], "Struct");
    assert_eq!(v["score"], 0.75);
    assert_eq!(v["signature"], "VectorStore");
    assert!(v.get("content").is_none());
}

#[test]
fn test_json_result_context_omitted_when_none() {
    let r = JsonResult {
        path: "src/foo.rs".to_string(),
        start_line: 1,
        end_line: 5,
        kind: "Block".to_string(),
        content: Some("let x = 1;".to_string()),
        score: 0.5,
        signature: None,
        context_prev: None,
        context_next: None,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("\"context_prev\""));
    assert!(!json.contains("\"context_next\""));
    assert!(!json.contains("\"signature\""));
}

// ── No stdout in search module ────────────────────────────────────────────

#[test]
fn test_no_raw_eprintln_in_search_module() {
    // Verify the search module contains no bare eprintln! macro *calls*
    // (calls that bypass quiet mode). All output must go through info_print!
    // or warn_print!. This test scans source text and skips comment lines
    // and lines where the token appears only inside a quoted string.
    let src = include_str!("mod.rs");
    let needle = concat!("eprint", "ln!("); // split so this literal doesn't self-trigger

    let violations: Vec<(usize, &str)> = src
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
                return false;
            }
            if !trimmed.contains(needle) {
                return false;
            }
            // Allow only if the needle appears exclusively inside a string literal
            // (i.e. every occurrence is preceded by a quote character).
            // Simple heuristic: reject if needle appears at a non-quoted position.
            !trimmed
                .split(needle)
                .skip(1) // parts after each occurrence
                .zip(trimmed.split(needle)) // parts before each occurrence
                .all(|(_, before)| before.ends_with('"') || before.ends_with("concat!("))
        })
        .collect();

    assert!(
        violations.is_empty(),
        "Found bare eprintln! calls in search/mod.rs (bypasses quiet mode):\n{}",
        violations
            .iter()
            .map(|(i, l)| format!("  line {}: {}", i + 1, l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[cfg(windows)]
#[test]
fn test_path_filter_matches_absolute_windows_path_under_root() {
    let project_root = normalize_path_str(r"C:\WorkArea\AI\codesearch");
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        r"\\?\C:\WorkArea\AI\codesearch\src\index\mod.rs",
        &filter,
        &project_root,
    ));
}

// Unix counterpart: native forward-slash absolute path. normalize_path_str
// intentionally leaves '\' untouched on Unix (see file_meta.rs Aikido
// rationale), so the Windows-path variant is gated off there.
#[cfg(unix)]
#[test]
fn test_path_filter_matches_absolute_unix_path_under_root() {
    let project_root = normalize_path_str("/work/codesearch");
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        "/work/codesearch/src/index/mod.rs",
        &filter,
        &project_root,
    ));
}

#[test]
fn test_path_filter_rejects_non_matching_absolute_path_under_root() {
    let project_root = normalize_path_str(r"C:\WorkArea\AI\codesearch");
    let filter = normalize_filter_path("src/");
    assert!(!path_matches_filter(
        r"C:\WorkArea\AI\codesearch\tests\index_test.rs",
        &filter,
        &project_root,
    ));
}

#[test]
fn test_path_filter_matches_relative_dot_slash_input() {
    let project_root = normalize_path_str("C:/WorkArea/AI/codesearch");
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter("./src/lib.rs", &filter, &project_root));
}

// ── sanitize_for_terminal ───────────────────────────────────────────────

#[test]
fn test_sanitize_strips_csi_clear_screen() {
    // \x1b[2J = clear screen
    assert_eq!(sanitize_for_terminal("hello\x1b[2Jworld"), "helloworld");
}

#[test]
fn test_sanitize_strips_csi_with_params() {
    // \x1b[38;5;200m = set 256-color foreground
    assert_eq!(
        sanitize_for_terminal("\x1b[38;5;200mred\x1b[0m text"),
        "red text"
    );
}

#[test]
fn test_sanitize_strips_osc_bel_terminator() {
    // \x1b]0;title\x07 = set window title, BEL terminator
    assert_eq!(sanitize_for_terminal("a\x1b]0;title\x07b"), "ab");
}

#[test]
fn test_sanitize_strips_osc_st_terminator() {
    // \x1b]0;title\x1b\\ = set window title, ST terminator
    assert_eq!(sanitize_for_terminal("a\x1b]0;title\x1b\\b"), "ab");
}

#[test]
fn test_sanitize_strips_single_char_escape() {
    // ESC M = Reverse Index (RI), in the 0x40-0x5F documented range
    assert_eq!(sanitize_for_terminal("a\x1bM b"), "a b");
}

#[test]
fn test_sanitize_strips_control_chars_except_newline_tab() {
    // NUL, BEL, backspace, vertical tab, form feed, CR — all stripped
    assert_eq!(
        sanitize_for_terminal("a\x00b\x07c\x08d\x0be\x0cf\rg"),
        "abcdefg"
    );
    // newline and tab preserved
    assert_eq!(sanitize_for_terminal("a\nb\tc"), "a\nb\tc");
}

#[test]
fn test_sanitize_strips_back_to_back_escapes() {
    // Two consecutive CSI sequences — both stripped
    assert_eq!(sanitize_for_terminal("\x1b[2J\x1b[2Jcleared"), "cleared");
}

#[test]
fn test_sanitize_preserves_unicode() {
    assert_eq!(sanitize_for_terminal("héllo → 世界 🦀"), "héllo → 世界 🦀");
}

#[test]
fn test_sanitize_preserves_empty_and_clean_strings() {
    assert_eq!(sanitize_for_terminal(""), "");
    assert_eq!(sanitize_for_terminal("clean string"), "clean string");
}

#[test]
fn test_sanitize_truncated_escape_dropped_safely() {
    // Truncated CSI at end of string — should not panic
    assert_eq!(sanitize_for_terminal("text\x1b["), "text");
    // Truncated OSC at end of string
    assert_eq!(sanitize_for_terminal("text\x1b]0;unterminated"), "text");
    // Lone ESC at end
    assert_eq!(sanitize_for_terminal("text\x1b"), "text");
}

#[test]
fn test_byte_truncation_preserves_char_boundary() {
    // Regression for issue #148: `&snippet[..100]` panicked when byte
    // offset 100 fell inside a multi-byte character (box-drawing U+2500
    // in comment-art, CJK, emoji). 40 × U+2500 = 120 bytes, so byte 100
    // is inside char #34 (bytes 99..102).
    let s: String = std::iter::repeat_n('─', 40).collect();
    assert!(s.len() > 100, "fixture must exceed 100 bytes");
    let cut = s.floor_char_boundary(100);
    assert!(cut <= 100);
    assert!(s.is_char_boundary(cut), "cut must land on a char boundary");
    let truncated = &s[..cut];
    // All chars are 3 bytes; cut must be a multiple of 3.
    assert_eq!(cut % 3, 0);
    assert_eq!(truncated.chars().count(), cut / 3);
    // The pre-fix code (`&s[..100]`) would panic on this fixture.
}
