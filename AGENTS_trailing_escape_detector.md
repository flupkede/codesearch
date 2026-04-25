# AGENTS_trailing_escape_detector.md

Scoped instructions for branch `feature/regex-trailing-escape`.
**Do not implement on `feature/mcp-multi-repo`** — cut this branch from `master`
after the multi-repo PR (which contains the BM25+scan regex fix) is merged.

---

## Context — what is broken

After the BM25-then-scan regex fix landed on `feature/mcp-multi-repo`, validation
testing found one query pattern that still fails:

| Query | Result | Should be |
|---|:--:|---|
| `\bimpl` | 5 hits ✅ | works |
| `impl` | 5 hits ✅ | works |
| **`impl\b`** | **0 hits** ❌ | should match `impl ` followed by non-word |
| **`Result\b`** | **0 hits** ❌ | should match `Result` token boundaries |
| **`match\b`** | **0 hits** ❌ | should match `match` keyword |

`identifier` followed by a trailing escape (`\b`, `\s`, `\w`, `\d`, etc.) or
character class (`[A-Z]+`, `[abc]`, etc.) returns zero results despite many
matches existing in the corpus.

## Root cause

The detector `regex_has_anchorable_token` in `src/mcp/mod.rs` already handles
the leading-escape case correctly via the `need_separator` flag: after a `\X`
escape or `[...]` class, the next alphanumeric run is **not** counted as
anchorable because Tantivy's BM25 analyzer merges escape-content with the
following alphanumerics into one token (`\bimpl` → `bimpl`, not `impl`).

But the same merging happens **in reverse** for trailing escapes. `impl\b`
gets analyzed by Tantivy into a token containing `impl` mixed with the `\b`
escape content, not the bare token `impl`. So when the detector marks
`impl\b` as anchorable (it sees a 3+ alphanumeric run "impl" with no leading
escape), the BM25 path is taken, and BM25 finds zero candidates because no
indexed chunk contains a token matching the merged form. The regex
post-filter then has nothing to filter, and the response is empty without an
error.

The detector's forward scan does not look ahead one position when an
alphanumeric run reaches the threshold. That is what this branch fixes.

## Known state — verify before starting

1. **The detector lives at the start of `src/mcp/mod.rs`.** It is a 50-line
   function with a `need_separator` flag, manual byte-walking, and special
   handling for `\X` escapes and `[...]` classes. **Read the whole function**
   before editing — the existing logic is subtle and the `need_separator`
   handling must be preserved exactly as-is for the leading-escape case.

2. **Eight detector tests already exist** in the same file's test module
   (search for `test_regex_has_anchorable_token_`). They cover plain
   identifier, generic with word, short-word-below-threshold, word-boundary
   pattern, method-call pattern, character-classes-don't-count, empty, and
   pure-punctuation. **All eight must still pass after this fix** —
   regressions there mean the leading-escape behaviour broke.

3. **Three end-to-end behaviour tests** also exist: `test_regex_anchorable_uses_bm25_path`,
   `test_regex_tokenless_uses_scan_path`, `test_regex_no_match_returns_empty`.
   These must also still pass.

4. **The scan path is correct.** This branch only changes routing logic in the
   detector. The scan and BM25 paths themselves are not modified.

## Implementation

### Step 1 — extend the detector

Modify `regex_has_anchorable_token` so that when the alphanumeric run reaches
length 3, instead of immediately returning `true`, it peeks at the **next**
byte. If that byte is `\` or `[`, the run is "merged with trailing escape" —
treat as not anchorable, reset run, continue.

Sketch (do not copy verbatim — adapt to the existing function structure):

```rust
if c.is_alphanumeric() || c == '_' {
    if need_separator {
        // ... existing logic, unchanged ...
    }
    run += 1;
    if run >= 3 {
        // Look ahead: is the next byte an escape or class start?
        // If yes, BM25 will merge the run with following content → not anchorable.
        let next_idx = i + 1;
        if next_idx < bytes.len() {
            let next_c = bytes[next_idx] as char;
            if next_c == '\\' || next_c == '[' {
                run = 0;
                need_separator = true; // entering escape territory
                i += 1;
                continue;
            }
        }
        return true;
    }
} else {
    run = 0;
    need_separator = false;
}
```

The peek must happen **only when `run >= 3`**, not on every alphanumeric
character — peeking on every char is wasteful and produces wrong behaviour
when alphanumerics are still building toward the threshold.

### Step 2 — add tests

Append to the same test module, with the existing `test_regex_has_anchorable_token_*`
prefix:

```rust
#[test]
fn test_regex_has_anchorable_token_trailing_word_boundary() {
    assert!(!regex_has_anchorable_token(r"impl\b"));
    assert!(!regex_has_anchorable_token(r"Result\b"));
    assert!(!regex_has_anchorable_token(r"match\b"));
}

#[test]
fn test_regex_has_anchorable_token_trailing_class() {
    assert!(!regex_has_anchorable_token(r"impl[A-Z]"));
    assert!(!regex_has_anchorable_token(r"foo[abc]+"));
}

#[test]
fn test_regex_has_anchorable_token_trailing_escape_with_more_after() {
    // After the merged trailing escape, if there's a clean run later, it counts.
    assert!(regex_has_anchorable_token(r"impl\b\s+function_name"));
    //                                              ^^^^^^^^^^^^^ this is anchorable
}

#[test]
fn test_regex_has_anchorable_token_trailing_escape_at_end_only() {
    // At the very end of the pattern: still not anchorable.
    assert!(!regex_has_anchorable_token(r"impl\s"));
}

#[test]
fn test_regex_has_anchorable_token_both_sides_escaped() {
    // \bimpl\b — leading escape disqualifies "impl", trailing irrelevant.
    assert!(!regex_has_anchorable_token(r"\bimpl\b"));
}
```

### Step 3 — end-to-end regression test

Add one behaviour test that demonstrates the fix end-to-end. Use the same
in-memory test harness as the existing behaviour tests (look for
`test_regex_tokenless_uses_scan_path` and copy its setup):

```rust
#[test]
fn test_regex_trailing_escape_uses_scan_path() {
    // Corpus contains "impl Foo for Bar".
    // Query "impl\b" must return ≥ 1 result and use the scan path
    // (score == 0.0 marker), confirming routing-decision fix.
}
```

### Step 4 — manual smoke test

After all tests pass, rebuild the binary, start serve, and run these queries
against the codesearch.git index. All must return non-zero hits:

- `impl\b` → expect ≥ 5 hits
- `Result\b` → expect ≥ 5 hits
- `match\b` → expect ≥ 5 hits
- `impl[A-Z]` → expect ≥ 1 hit (class-followed)

Also re-run the existing six tokenless smoke queries (from
`AGENTS.md` Status table) — must all still return ≥ 5 hits each. No
regressions on the leading-escape side.

## Acceptance criteria

- `cargo test --lib` passes — all 8 existing detector tests + 5 new detector
  tests + existing 3 behaviour tests + 1 new behaviour test all pass.
- `cargo test --all` passes.
- `cargo clippy --all-targets -- -D warnings` clean.
- Manual smoke test: all four trailing-escape queries above return ≥ 1 hit.
- Manual smoke test: all six original tokenless queries from
  `feature/mcp-multi-repo` Status table still return ≥ 5 hits (no regression).

## Out of scope

- Changing the scan path or BM25 path themselves — this branch is detector-only.
- Adding more regex syntax handling beyond `\X` escapes and `[...]` classes —
  if other patterns surface (e.g. `(?:...)` non-capturing groups followed by
  identifiers), file separate issues. Do not preemptively handle them.
- Performance work. Detector is < 1µs even with the lookahead.

## Commit structure

Single commit. Suggested message:

```
fix(mcp): trailing-escape regex queries route to scan path

Patterns like impl\b, Result\b, match\b previously returned zero results
because the anchorable-token detector did not look ahead past an alphanumeric
run. BM25 receives the raw query string and merges the escape into the
identifier token, producing zero candidates.

Extend regex_has_anchorable_token to peek one position past a run of length
≥ 3. If the next byte is \ or [, the run merges with the following escape
or character class and is not counted as anchorable, routing the query to
the scan path instead.

5 new detector tests + 1 new behaviour test covering all the failing query
patterns reported in feature/mcp-multi-repo validation. Existing 8 detector
tests + 3 behaviour tests pass unchanged.
```
