# AGENTS.md — `feature/regression-fix`

Hotfix branch voor regressies die uit de post-merge review van PR #18 kwamen.
Twee taken, beide klein, beide blocking voor LLM-clients die de tool gebruiken.

**Read this entirely before writing any code.** Nothing in scope is large; the
risk is missing one of the related test/UX fixes alongside the one-line code
change.

---

## Status

**Branch:** `feature/regression-fix` (cut from `master` at `9a06a14` — the merge
commit of PR #18).
**Working tree:** clean (only this AGENTS.md after first commit).

PR #18 landed two real regressions in `src/mcp/mod.rs` that ship to every user
of `search(mode="literal")`. Both are about how low-confidence is signalled to
the LLM caller. They are independent of the regex / scan-fallback work that
otherwise functions correctly.

---

## Bug 1 — `LITERAL_LOW_CONFIDENCE_BM25 = f32::MAX` flags every success as low confidence

### Demonstration

Built `v0.1.243` (current HEAD of `master`), started serve on port 39726,
queried the codesearch.git index. Verbatim result:

```
Query:        "match_line_for_literal" (regex=true)
Hits:         3 (strong matches)
Top score:    41.485       <- real BM25 score, well above any reasonable threshold
-> low_confidence:  true
-> suggested_tool:  "find with kind='definition' or kind='usages'"
-> note:            "find with kind='definition' or kind='usages'"
```

A successful query with strong matches reports `low_confidence: true` and
suggests a different tool. This will mislead every LLM client into switching
tools when there is nothing wrong with the result.

### Root cause

`src/mcp/mod.rs:2471`:

```rust
const LITERAL_LOW_CONFIDENCE_BM25: f32 = f32::MAX;
```

The doc-comment on that constant says it is set to `f32::MAX` so the threshold
"never fires until calibrated." That reasoning would be correct if the
comparison were `score >= LITERAL_LOW_CONFIDENCE_BM25` (true only when score
hits f32::MAX, which never happens for finite scores -> never fires). But the
actual comparison in `compute_literal_low_confidence` is:

```rust
match top_score {
    Some(score) if score < LITERAL_LOW_CONFIDENCE_BM25 => {  // <- always true for finite scores
        let hint = if is_natural_language { suggest_semantic } else { suggest_find };
        (Some(true), Some(hint.to_string()))
    }
    None => { /* empty results path */ }
    Some(_) => (None, None),
}
```

For any finite BM25 score, `score < f32::MAX` is true, the first arm matches,
and low_confidence is set to true. The `Some(_) => (None, None)` arm is
unreachable for any finite score.

### Fix

Change the constant value to something below typical BM25 floor scores. Until
real-world calibration data is collected via the existing
`codesearch::literal_confidence` tracing target, picking a low-but-not-zero
value is safer than zero (because zero would never fire even on truly weak
results).

```rust
// src/mcp/mod.rs near line 2467
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
```

### Tests to add / fix

The existing test `test_literal_lc_threshold_uses_strictly_less_than` at
`src/mcp/mod.rs:1733` validates the boundary at `f32::MAX`, which is a
non-existent score in practice. The test passes by coincidence (the catch-all
`Some(_) => (None, None)` arm fires when the score equals the threshold). It
must be replaced with a real-world test:

```rust
#[test]
fn test_literal_lc_does_not_fire_on_strong_results() {
    // Strong BM25 score (well above floor) must NOT be flagged low_confidence.
    let (lc, hint) = super::compute_literal_low_confidence(Some(41.5), "anything");
    assert_eq!(lc, None, "strong BM25 results must not be flagged low_confidence");
    assert_eq!(hint, None);
}

#[test]
fn test_literal_lc_fires_on_weak_results() {
    // Score below the floor -> low_confidence true.
    let (lc, hint) = super::compute_literal_low_confidence(
        Some(super::LITERAL_LOW_CONFIDENCE_BM25 - 0.5),
        "CodesearchService",
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
```

Two existing tests at `src/mcp/mod.rs:1710` and `:1721` use
`LITERAL_LOW_CONFIDENCE_BM25 / 2.0` to construct a "weak score". With the new
value of 5.0 these become `2.5`, which is still well below the floor — they
should keep passing without modification. **Verify** they do; if they regress,
that is a bug in the new threshold value.

---

## Bug 2 — `note` field carries a tool name instead of a sentence

### Demonstration

From the same query above:

```
note: "find with kind='definition' or kind='usages'"
```

The `note` field on `LiteralSearchResponse` is documented as

> Actionable note for the LLM caller (present iff auto_promoted_to_regex
> or low_confidence is set).

An "actionable note" reads like a sentence. The current value is a tool
invocation string copied from `suggested_tool`. LLM clients see the same
string twice (once in `suggested_tool`, once in `note`) without explanation.

### Root cause

`src/mcp/mod.rs` inside `literal_search`, near the end of the function:

```rust
let note = if auto_promoted {
    Some(format!(
        "Query auto-promoted to regex mode (original: '{}', effective: '{}'). \
         The query contained code-like punctuation that BM25 would tokenize incorrectly.",
        request.query, effective_query
    ))
} else if low_confidence == Some(true) {
    suggested_tool.clone()  // <- tool name string, not a sentence
} else {
    None
};
```

The auto-promoted branch correctly produces a sentence. The low-confidence
branch copies the tool-name string verbatim.

### Fix

Wrap the suggested tool in an explanatory sentence:

```rust
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
```

After the fix, the same kind of query as above (a real low-confidence case)
should produce:

```
note: "Top result has weak BM25 score; consider using `find with kind='definition' or kind='usages'` for better matches."
```

### Tests to add

Add to the existing test module in `src/mcp/mod.rs`:

```rust
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
            format!("Top result has weak BM25 score; consider using `{}` for better matches.", tool)
        })
    } else {
        None
    };

    let n = note.expect("note must be present when low_confidence is true");
    assert!(n.starts_with("Top result"), "note must read as a sentence, got: {}", n);
    assert!(n.contains("find with kind='definition'"), "note must reference the suggested tool: {}", n);
}
```

---

## Acceptance criteria

All of the following must hold before opening the PR:

1. `cargo test --lib` passes — three new/replaced tests for Bug 1, one new
   test for Bug 2, plus all 358 existing tests still green.
2. `cargo test --all` passes.
3. `cargo clippy --all-targets -- -D warnings` clean.
4. **Manual smoke test:** start serve on port 39726, query the codesearch.git
   index for `match_line_for_literal` with `regex=true`, parse the JSON. The
   response must have:
   - `results` with >= 1 entry, top score > 5.0
   - **No** `low_confidence` field
   - **No** `suggested_tool` field
   - **No** `note` field
5. **Manual smoke test for the empty-results path:** query for
   `zzz_definitely_not_in_code` with `regex=true`. The response must have:
   - `results` empty
   - `low_confidence: true`
   - `suggested_tool` set to a non-empty hint
   - `note` reads as a complete English sentence (starts with "Top result"
     or similar), not a bare tool-name string

The PowerShell harness used in the bug demonstration is reproducible from the
chat history; copy it from there if needed.

---

## Commit structure

One commit. Suggested message:

```
fix(mcp): correct LITERAL_LOW_CONFIDENCE_BM25 threshold and note phrasing

PR #18 landed two regressions in literal_search response signalling:

1. LITERAL_LOW_CONFIDENCE_BM25 was set to f32::MAX with the intent of
   "never fires until calibrated", but the comparison `score < threshold`
   means it fires on every finite score instead. Result: every successful
   literal_search was flagged low_confidence with an unrelated suggested_tool,
   misleading LLM clients into switching tools needlessly.

   Set the threshold to 5.0 (Tantivy BM25 corpus floor for codesearch).
   Replace the boundary test that validated f32::MAX with three tests that
   exercise the strong-result, weak-result, and exact-boundary cases.

2. The `note` field on LiteralSearchResponse received the suggested_tool
   string verbatim ("find with kind='definition'...") instead of a sentence.
   Wrap the tool name in an explanatory sentence describing why the hint
   is being given.

Both fixes are user-facing only; no internal API changes.
```

---

## What is explicitly out of scope

These items came up in the post-merge review of PR #18 but are **not** part of
this hotfix branch:

- Code-duplication in `literal_search` (4 BM25/scan x multi/single paths) —
  refactoring opportunity, separate branch.
- Sequential vs parallel multi-store fan-out (`for store in sv { ... await }`
  could become `join_all`) — performance work, separate branch.
- Magic limit multipliers (`limit * 3`, `limit * 2`, etc.) becoming named
  constants — style work, separate branch.
- Stringly-typed `mode`/`kind`/`format` becoming serde enums — type-safety
  work, separate branch.
- Trailing-escape detector — was already documented as a follow-up; no longer
  relevant since PR #18 already shipped the trailing-escape fix.
- `looks_like_code_pattern` not detecting `.` in `User.create` — design
  choice, documented limitation.
- `ReposConfig::load()` swallowing corrupt-file errors as default — separate
  reliability concern.

If any of those become blockers in production, file separate issues and cut
separate branches. Do not bundle.

---

## Build rules (reference)

- Target dir: `C:\WorkArea\AI\codesearch\target` (set by `.cargo/config.toml`).
- **Always DEBUG.** `--release` is forbidden on this branch.
- All edits via MCP filesystem tools. Bash/PowerShell only for cargo commands.
- Pre-commit hook bumps `Cargo.toml` patch version AND rebuilds
  `target/debug/codesearch.exe` so binary cannot drift behind manifest. Do not
  use `--no-verify` — `copy-to-common.ps1` will refuse to deploy a mismatched
  binary.

## Key files

- `src/mcp/mod.rs:2471` — `LITERAL_LOW_CONFIDENCE_BM25` constant (Bug 1 fix)
- `src/mcp/mod.rs` near `compute_literal_low_confidence` (~line 2473) — the
  comparison logic; no change needed, the fix is purely in the constant value
- `src/mcp/mod.rs` inside `literal_search`, near the response-construction
  block — `note` field assignment (Bug 2 fix)
- `src/mcp/mod.rs:1733` — old test `test_literal_lc_threshold_uses_strictly_less_than`
  to replace with three new tests
- `src/mcp/mod.rs:1710` and `:1721` — existing weak-score tests that should
  keep passing with the new threshold value