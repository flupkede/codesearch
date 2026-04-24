# AGENTS_auto-regex_and_confidence.md

Scoped instructions for a new branch `feature/auto-regex-confidence`.
**Do not implement on `feature/mcp-multi-repo`** — create this branch from `master`
after the multi-repo PR is merged.

---

## Context

Triggered by a real agent failure: an LLM (OpenCode) called `literal_search` with query
`ActivitiesListModelResponse = null`, received noisy results because BM25 tokenized `=`
and `null` separately, decided the tool was useless, and fell back to `rg` — which was
not installed.

A tool-description improvement was already committed in the parent branch (added sub-mode
selection guidance with examples like `foo = null`, `Vec<T>`, `return x;`). This branch
adds **automatic safeguards inside codesearch** so that even an LLM that ignores the
description still gets correct results.

Two independent improvements:

1. **Auto-regex promotion** — detect code-pattern punctuation in a literal query and
   automatically apply `regex::escape` (with whitespace relaxation) + enable regex mode.
2. **Low-confidence signal for literal search** — mirror the existing
   `low_confidence`/`suggested_tool` fields from `SemanticSearchResponse` into literal
   search responses, so the LLM always has a concrete next-step hint when results are
   empty or weak.

---

## Build rules (identical to parent branch)

- Target: `C:\WorkArea\AI\codesearch\target` (set by `.cargo/config.toml` — never override)
- **Always DEBUG. `--release` is forbidden.**
- **Use MCP tools for all code exploration and editing.** Bash only for `cargo build`,
  `cargo test`, `cargo clippy`.
- No `print!`/`println!` anywhere in `src/mcp/` — enforced by
  `test_mcp_no_raw_stdout_calls`.
- `anyhow::Result<T>` from fallible functions. Never `.unwrap()`/`.expect()` in library
  code.
- Windows path hygiene: normalize through `crate::cache::normalize_path_str`.
- Deterministic tests only — no `sleep`.

---

## Known state — verify before starting

Before writing any code, read this section to understand what already exists and what
assumptions in this document need to be verified against the live codebase.

### 1. `LiteralSearchResponse` does not exist yet — introducing it is a breaking change

The current `literal_search` handler in `src/mcp/mod.rs` returns one of two formats
depending on `request.format`:

- `format = "grep"` → a plain `String` of lines (`path:line:snippet\n...`)
- anything else → `serde_json::to_string(&items)` where `items: Vec<LiteralSearchResultItem>`

**There is no wrapping response struct.** The items are serialized as a bare JSON array.

Introducing `LiteralSearchResponse { results, ... }` changes the JSON shape from
`[{...}, {...}]` to `{"results":[{...}, {...}], ...}`. That is a **breaking change** for
any MCP client that parses the array directly.

**Decision: introduce the wrapping type in this branch and treat it as a deliberate
pre-1.0 semver bump.** Document the shape change prominently in the commit message and
the PR description. Update any downstream tests or clients that pattern-match on `[` as
the first character of the response.

Before writing the struct, search the repo for any code that parses the literal_search
response and expects a bare array:

```bash
git grep -n "literal_search\|LiteralSearchResultItem" -- src/ tests/
```

Update every such site to use the new `LiteralSearchResponse.results` field.

### 2. Grep-format output cannot carry JSON fields

When `format = "grep"`, the output is plain text and cannot include
`auto_promoted_to_regex` or `low_confidence` as JSON keys. Use `# ` comment prefix lines
(the standard grep-comment convention):

```
# auto-promoted to regex mode (query contained code-like punctuation)
# low confidence — consider: search with mode='semantic'
src/foo.rs:42:ActivitiesListModelResponse = null;
src/bar.rs:17:ActivitiesListModelResponse = response;
```

Line-splitting grep consumers skip `#`-prefixed lines by convention, so this is safe.

### 3. The BM25 low-confidence threshold must be instrumented before it is chosen

BM25 absolute scores depend on corpus size, field lengths, query-term frequencies, and
the specific Tantivy tokenizer configuration. Any hardcoded value (like `0.5`) is a
guess until measured against real traffic.

**Commit 3 of this branch must be an instrumentation-only commit.** Add:

```rust
// In literal_search, after collecting `items`:
if let Some(top) = items.first() {
    tracing::debug!(
        target: "codesearch::literal_confidence",
        query = %request.query,
        top_bm25_score = top.score,
        result_count = items.len(),
        "literal_search score sample"
    );
}
```

And set the constant initially to a never-fires value:

```rust
/// BM25 score threshold for low-confidence signalling in literal search.
/// IMPORTANT: this value is unvalidated. It is set to f32::MAX (never fires)
/// until real query scores have been collected from production traffic.
/// Update after reviewing `RUST_LOG=codesearch::literal_confidence=debug` output.
const LITERAL_LOW_CONFIDENCE_BM25: f32 = f32::MAX;
```

**Only after scores are collected** (commit 5 of this branch), swap the constant to a
calibrated value. The commit message for that swap must include the observed score
distribution (e.g. "p50=1.8, p10=0.6, chose threshold=0.5 to flag bottom ~10% of hits").

### 4. The parent branch's description already has the regex guidance

The description was updated in the parent branch to include:

> *Sub-mode selection: Queries with operators, brackets, or punctuation
> (`foo = null`, `Vec<T>`, `return x;`, `a::b`) -> set `regex=true` and write the query
> as a regex. BM25 tokenizes on punctuation otherwise, producing noisy results.*

The auto-promotion logic in item 1 of this branch is a **programmatic backstop** for
when the LLM ignores the description. No further description changes are needed.

---

## Item 1 — Auto-regex promotion

### Why

Tantivy BM25 tokenizes on whitespace and common punctuation. A query like
`ActivitiesListModelResponse = null` tokenizes as two independent terms
(`ActivitiesListModelResponse`, `null`). Any chunk containing either term alone scores
a hit, flooding results with noise.

`regex::escape("foo = null")` produces `foo\ =\ null` — every character becomes a
literal. But users usually want whitespace tolerance around operators. **After escape,
replace escaped-space sequences with `\s+`** so `foo = null` becomes the regex
`foo\s+=\s+null`.

### Where

`src/mcp/mod.rs`, inside the `literal_search` handler.

### Step 1 — pure detector function

Add near `truncate_line_around_match` (or wherever pure helpers live):

```rust
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
```

### Step 2 — promotion logic in `literal_search`

Near the top of the handler body, after extracting `request`:

```rust
let user_set_regex  = request.regex.unwrap_or(false);
let user_set_phrase = request.phrase.unwrap_or(false);
let auto_promoted   = !user_set_regex
    && !user_set_phrase
    && looks_like_code_pattern(&request.query);

let (effective_query, effective_regex) = if auto_promoted {
    let escaped = regex::escape(&request.query);
    // Relax escaped spaces to \s+ so "foo = null" becomes "foo\s+=\s+null"
    let relaxed = escaped.replace(r"\ ", r"\s+");
    (relaxed, true)
} else {
    (request.query.clone(), user_set_regex)
};
```

Then replace every downstream use of `request.query` with `effective_query` and
`request.regex` with `effective_regex` inside the rest of the handler.

### Step 3 — wrapping response type

See Known state §1 before implementing. Add to `src/mcp/types.rs`:

```rust
/// Response from `search(mode="literal")`.
///
/// Replaces the previous bare `Vec<LiteralSearchResultItem>` JSON array.
/// Breaking change introduced in feature/auto-regex-confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiteralSearchResponse {
    pub results: Vec<LiteralSearchResultItem>,

    /// True when codesearch auto-escaped the query and enabled regex mode
    /// because the original query contained code-like punctuation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_promoted_to_regex: Option<bool>,

    /// Actionable note for the LLM caller (present iff auto_promoted_to_regex or
    /// low_confidence is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    /// True when results are empty or top BM25 score is below threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,

    /// Suggested next tool when low_confidence is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
}
```

At the end of `literal_search`, replace the current bare-array JSON with:

```rust
let note = if auto_promoted {
    Some("Query auto-promoted to regex mode (contained code-like punctuation). \
          Pass `regex=true` explicitly next time for predictable behaviour.".to_string())
} else {
    None
};

let top_score = items.first().map(|i| i.score);
let (low_confidence, suggested_tool) =
    compute_literal_low_confidence(top_score, &request.query);

let combined_note = match (note.as_deref(), low_confidence) {
    (Some(n), Some(true)) => Some(format!(
        "{} Also: {}",
        n,
        suggested_tool.as_deref().unwrap_or("try a different query")
    )),
    (Some(n), _) => Some(n.to_string()),
    (None, Some(true)) => Some(format!(
        "Low confidence. Consider: {}",
        suggested_tool.as_deref().unwrap_or("try a different query")
    )),
    _ => None,
};

let response = LiteralSearchResponse {
    results: items,
    auto_promoted_to_regex: if auto_promoted { Some(true) } else { None },
    note: combined_note,
    low_confidence,
    suggested_tool,
};
```

### Step 4 — grep-format output with comment line

When `output_format == "grep"`, prepend `# ` comment lines for active signals:

```rust
let output = if output_format == "grep" {
    let mut lines: Vec<String> = Vec::new();
    if response.auto_promoted_to_regex == Some(true) {
        lines.push("# auto-promoted to regex mode (query contained code-like punctuation)".to_string());
    }
    if response.low_confidence == Some(true) {
        if let Some(ref hint) = response.suggested_tool {
            lines.push(format!("# low confidence — consider: {}", hint));
        }
    }
    for item in &response.results {
        lines.push(format!("{}:{}:{}", item.path, item.start_line, item.snippet));
    }
    lines.join("\n")
} else {
    serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string())
};
```

### Tests — item 1

All in `src/mcp/mod.rs` test module:

- `test_looks_like_code_pattern_assignment` — `"foo = null"`, `"x = 42"` → `true`
- `test_looks_like_code_pattern_arrow` — `"foo->bar"`, `"x => y"` → `true`
- `test_looks_like_code_pattern_namespace` — `"std::string"`, `"a::b::c"` → `true`
- `test_looks_like_code_pattern_generics` — `"Vec<T>"`, `"HashMap<K, V>"` → `true`
- `test_looks_like_code_pattern_statement_end` — `"return x;"`, `"if (x) {"` → `true`
- `test_looks_like_code_pattern_plain_identifier_false` —
  `"ActivitiesListModelResponse"`, `"foo_bar"` → `false`
- `test_looks_like_code_pattern_dotted_path_false` — `"foo.bar"`, `"System.Console"` →
  `false`
- `test_looks_like_code_pattern_empty_false` — `""` → `false`
- `test_auto_promotion_escapes_and_relaxes_spaces` — query `"foo = null"` →
  effective_query is `r"foo\s+=\s+null"` (not `r"foo\ =\ null"`)
- `test_auto_promoted_skipped_when_user_sets_regex` — `regex=true` → `auto_promoted=false`,
  query unchanged
- `test_auto_promoted_skipped_when_user_sets_phrase` — `phrase=true` → same
- `test_literal_search_response_shape_json` — non-empty results, JSON starts with `{`,
  contains `"results":[`, does NOT start with `[`
- `test_literal_search_response_carries_note_when_promoted` — promoted call → response
  has `auto_promoted_to_regex: Some(true)` and non-empty `note`
- `test_literal_search_response_omits_fields_when_not_promoted` — plain call → JSON does
  not contain `"auto_promoted_to_regex"` or `"note"` keys
- `test_grep_format_includes_comment_when_promoted` — grep output starts with
  `# auto-promoted`
- `test_grep_format_no_comment_when_plain` — plain query, grep format → output does not
  start with `#`

---

## Item 2 — Low-confidence signal for literal search

### Why

`SemanticSearchResponse` already signals `low_confidence + suggested_tool` when RRF
top-score < 0.02. Literal search has nothing equivalent. When BM25 returns zero hits or
weak results, the LLM sees an empty result with no guidance, and the common fallback is
`rg` (which may not be available).

### Helper function

Add near `compute_low_confidence` in `src/mcp/mod.rs`:

```rust
/// Compute low-confidence signalling for literal search results.
///
/// Returns `(low_confidence, suggested_tool)`:
/// - Both `None` when results are strong.
/// - `(Some(true), Some(hint))` when results are absent or weak.
fn compute_literal_low_confidence(
    top_score: Option<f32>,
    query: &str,
) -> (Option<bool>, Option<String>) {
    let word_count = query.split_whitespace().count();
    let has_code_chars = query.chars().any(|c| "{}[]<>=|;:".contains(c));
    let is_natural_language = word_count >= 3 && !has_code_chars;

    let suggest_semantic = "search with mode='semantic'";
    let suggest_regex    = "search with mode='literal' and regex=true";
    let suggest_find     = "find with kind='definition' or kind='usages'";

    match top_score {
        Some(score) if score < LITERAL_LOW_CONFIDENCE_BM25 => {
            let hint = if is_natural_language { suggest_semantic } else { suggest_find };
            (Some(true), Some(hint.to_string()))
        }
        None => {
            let hint = if is_natural_language { suggest_semantic } else { suggest_regex };
            (Some(true), Some(hint.to_string()))
        }
        Some(_) => (None, None),
    }
}
```

### Tests — item 2

Construct test inputs to bracket the threshold directly — don't rely on
`LITERAL_LOW_CONFIDENCE_BM25` having any particular value, since during instrumentation
it is `f32::MAX` and after calibration it is an empirical number.

- `test_literal_lc_natural_language_zero_results` — `"how do we handle auth"`,
  `top_score = None` → `low_confidence = Some(true)`, suggested_tool contains `"semantic"`
- `test_literal_lc_identifier_zero_results` — `"CodesearchService"`, `top_score = None`
  → `low_confidence = Some(true)`, suggested_tool contains `"regex"`
- `test_literal_lc_code_pattern_zero_results` — `"foo = null"`, `top_score = None` →
  suggested_tool contains `"regex"`
- `test_literal_lc_natural_language_weak_score` — `"error handling auth layer"`,
  `top_score = Some(LITERAL_LOW_CONFIDENCE_BM25 - 0.001)` → suggests `semantic`
- `test_literal_lc_identifier_weak_score` — `"handle_request"`,
  `top_score = Some(LITERAL_LOW_CONFIDENCE_BM25 - 0.001)` → suggests `find`
- `test_literal_lc_threshold_uses_strictly_less_than` —
  `top_score = Some(LITERAL_LOW_CONFIDENCE_BM25)` → `(None, None)` (threshold uses `<`,
  not `<=`)
- `test_literal_lc_high_score_returns_none` — `top_score = Some(f32::MAX)` → `(None, None)`
- `test_literal_response_json_has_lc_fields` — `low_confidence = Some(true)` → JSON
  contains `"low_confidence":true` and `"suggested_tool"`
- `test_literal_response_json_omits_lc_fields_when_none` — `low_confidence = None` →
  JSON does not contain `"low_confidence"` or `"suggested_tool"` keys

---

## Acceptance criteria

- `cargo build` — clean, no warnings
- `cargo test --all` — all existing tests plus ≥ 20 new tests from items 1 and 2
- `cargo clippy --all-targets -- -D warnings`
- `test_mcp_no_raw_stdout_calls` still passes
- `test_instructions_max_50_lines` still passes (this branch does not touch `get_info`)
- All sites that previously expected a bare JSON array from `literal_search` have been
  updated to use `LiteralSearchResponse.results`
- Manual smoke: `search(mode="literal", query="ActivitiesListModelResponse = null")`
  against a real codebase returns JSON with `"auto_promoted_to_regex": true`, a `"note"`
  explaining the promotion, and `"results"` containing actual assignment lines

---

## Out of scope

- Tuning `LITERAL_LOW_CONFIDENCE_BM25` — collect data first via instrumentation commit
- Auto-promotion for **semantic** search — embeddings are not harmed by punctuation
- Rewriting the Tantivy analyzer to tokenize code differently — upstream concern, risky
- Changes to `src/cli/mod.rs`, `src/serve/mod.rs`, `README.md` — this branch is
  `src/mcp/mod.rs` + `src/mcp/types.rs` only
- Teaching the LLM via `instructions` field updates — different branch

---

## Commit structure

Each commit green (compiles + tests pass):

1. `feat(mcp): add looks_like_code_pattern detector (pure fn + tests)`
2. `feat(mcp): auto-promote literal queries with code punctuation to regex mode`
   — introduce `LiteralSearchResponse`, update all consumers of bare-array response
3. `feat(mcp): instrument literal search BM25 scores for threshold calibration`
   — `LITERAL_LOW_CONFIDENCE_BM25 = f32::MAX` (never fires yet)
4. `feat(mcp): add compute_literal_low_confidence + wire into LiteralSearchResponse`
5. `chore(mcp): set LITERAL_LOW_CONFIDENCE_BM25 to calibrated value`
   — fill in after reviewing debug logs; commit message must include observed score
   distribution
