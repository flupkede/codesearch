# AGENTS — Branch `feat/mcp-rebrand-hybrid-search`

> Scoped instructions for any coding agent (OpenCode, Claude Code, Copilot) working on this branch. This file is self-contained: everything needed to execute the work is below. A parallel Branch B handles a separate concern and must not be touched here.

## Why this branch exists

Agents observing codesearch via MCP frequently fall back to `grep`, even for queries codesearch would handle well. Root cause diagnosis on `src/mcp/mod.rs` and `src/mcp/types.rs`:

1. **Description framing.** `semantic_search` is advertised as *"Search code semantically using natural language"*. Agents read literal/identifier queries as "not natural language" and route away — despite the fact that the implementation already fuses vector + Tantivy FTS and boosts exact-identifier matches via `rrf_fusion_with_exact`.
2. **`find_references` misrepresents its behavior.** The impl is `fts_store.search(&symbol, ...)` — a substring FTS search that returns definitions, comments, docstrings, and string literals alongside actual usages. After one noisy result, agents stop trusting it.
3. **No confidence signal.** `semantic_search` returns `limit` results regardless of top RRF score. Agents cannot distinguish "strong match" from "grasping".
4. **Over-long, negative `instructions` block.** The current `get_info().instructions` is ~150 lines and repeats "NEVER use grep". Negative framing backfires; agents route on concrete tool descriptions anyway.

This branch makes **zero algorithm changes**. It is a tool-surface and description pass.

## Scope — what to implement

1. Rewrite `semantic_search` description as a hybrid tool (positive routing).
2. Add optional `mode` parameter to `semantic_search` so callers can hint intent.
3. Add a low-confidence signal to `semantic_search` responses.
4. Split the current `find_references` into two new tools: `find_definition` and `find_usages`. Keep `find_references` as a deprecated alias.
5. Shrink the server-wide `instructions` block and replace negative framing with a compact routing table.

## Scope — what NOT to touch

- `src/chunker/` — no changes to chunking.
- `src/rerank/` — no changes to RRF fusion, `boost_kind`, etc.
- `src/search/` — no changes to `detect_identifiers`, `detect_structural_intent`, `adapt_rrf_k`.
- `src/embed/` — no embedding model changes.
- `src/fts/tantivy_store.rs` — no new Tantivy query types in this branch. If you feel tempted to add `search_regex` or `search_phrase`, stop — that belongs in Branch B (`feat/mcp-literal-search-tool`).
- Any new dependency in `Cargo.toml`.

## File-by-file plan

### `src/mcp/types.rs`

Extend `SemanticSearchRequest`:

```rust
pub struct SemanticSearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub compact: Option<bool>,
    pub filter_path: Option<String>,

    /// Override auto-detection of query intent.
    /// "auto" (default) | "semantic" | "lexical" | "hybrid"
    /// - "semantic": skip FTS fusion, use vector results only
    /// - "lexical":  skip embedding, use FTS path only
    /// - "hybrid":   force full hybrid even if auto would choose a single path
    pub mode: Option<String>,
}
```

Add new request types:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDefinitionRequest {
    /// Symbol name (function, class, method, struct, trait, enum, type)
    pub symbol: String,
    /// Optional filter to a specific kind. If omitted, all definition kinds are searched.
    /// Accepted: "Function" | "Class" | "Method" | "Struct" | "Trait" | "Enum" | "TypeAlias" | "Interface"
    pub kind: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindUsagesRequest {
    pub symbol: String,
    pub limit: Option<usize>,
}
```

Extend the semantic-search response wrapper with low-confidence signaling. Preferred: wrap the existing `Vec<SearchResultItem>` in a response struct rather than adding fields to every item.

```rust
#[derive(Debug, Serialize)]
pub struct SemanticSearchResponse {
    pub results: Vec<SearchResultItem>,
    /// Set when the top RRF score is below the confidence threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,
    /// Populated alongside `low_confidence`. Suggests a better-suited tool for this query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
}
```

### `src/mcp/mod.rs` — `semantic_search`

Replace the `#[tool(description = ...)]` with this exact text:

```
Hybrid code search over tree-sitter AST chunks: vector embeddings + Tantivy FTS + exact-identifier boosting, fused with RRF.

USE FOR:
- Conceptual queries ("where is auth handled", "how do we log errors")
- Identifier lookups — function/class/variable names are boosted via exact-match FTS
- Mixed natural-language + symbol queries

DO NOT USE FOR:
- Finding a symbol's definition specifically — use `find_definition`
- Finding all call-sites of a symbol — use `find_usages`

OPTIONAL `mode`: "auto" (default) | "semantic" | "lexical" | "hybrid".
Returns metadata only by default (compact=true). Set compact=false for inline content.
```

Handler changes:

- Parse `request.mode.as_deref().unwrap_or("auto")`.
- `"semantic"` → return vector results directly, skip FTS fusion.
- `"lexical"` → skip `embed_query`; call FTS directly and return those.
- `"hybrid"` | `"auto"` → keep current hybrid behavior unchanged.
- After fusion and boosting, inspect `results.first().map(|r| r.score)`:
  - If present and `< 0.02` (constant, name it `LOW_CONFIDENCE_THRESHOLD` at module scope with a brief comment), set `low_confidence = true`.
  - `suggested_tool`: if `detect_identifiers(&request.query)` is non-empty → `"find_definition"`; otherwise → `"literal_search"` (tool will exist after Branch B merges; the hint is informational meanwhile).
- Return `SemanticSearchResponse` instead of raw `Vec<SearchResultItem>`.

### `src/mcp/mod.rs` — new `find_definition` tool

```
Locate the definition of a symbol (function, class, method, struct, trait, enum, type).
Uses FTS + chunk-kind filter to exclude usages, comments, and string literals.

USE FOR: "where is X defined", "show me the declaration of X".
DO NOT USE FOR: finding all call-sites → use `find_usages`.
```

Impl outline:

- FTS search on `request.symbol`, `limit * 3` hits.
- Resolve `chunk_id` → full chunk via `VectorStore::get_chunk` (pattern already used in existing `find_references`).
- Filter by `kind ∈ {"Function", "Class", "Method", "Struct", "Trait", "Enum", "TypeAlias", "Interface"}`.
- If `request.kind` is provided, further restrict to that exact kind.
- Return `Vec<ReferenceItem>` (reuse existing struct) truncated to `limit`.

### `src/mcp/mod.rs` — new `find_usages` tool

```
Find call-sites and other usages of a symbol across the codebase.
Uses FTS; excludes the chunks that are the symbol's own definition.

USE FOR: impact analysis, refactoring, "who calls X".
DO NOT USE FOR: finding the definition itself → use `find_definition`.
```

Impl outline:

- FTS search on `request.symbol`.
- For each hit, fetch the chunk.
- Exclude hits where `kind` is a definition kind **and** the chunk's `signature` contains the symbol name verbatim at a likely definition position (e.g. `fn <symbol>(`, `class <symbol>`, `struct <symbol>`, `def <symbol>(`). Best-effort substring check is acceptable for v1 — document the limitation in a comment.
- Return `Vec<ReferenceItem>`.

### `src/mcp/mod.rs` — `find_references` (deprecated alias)

Keep the tool registered so existing agent configs don't break. New description:

```
DEPRECATED. Use `find_definition` to locate a symbol's declaration, or `find_usages` to find call-sites.
This tool is retained as an alias for `find_usages` and may be removed in a future version.
```

Implementation: delegate to the new `find_usages` handler. No behavioral change from the agent's perspective beyond the description.

### `src/mcp/mod.rs` — `get_info().instructions`

Replace the current ~150-line block with ≤ 50 lines. Required structure:

```
codesearch — semantic + lexical code search MCP server.

TOOLS:
| Tool              | Use for                                              |
|-------------------|------------------------------------------------------|
| semantic_search   | Conceptual queries, identifier + natural-language mix |
| find_definition   | Where is symbol X defined                             |
| find_usages       | Who uses / calls symbol X                             |
| index_status      | Verify the index is ready                             |
| find_databases    | Discover available indexes                            |

Indexing is done via CLI: `codesearch index`. The MCP server cannot index.

Current project: {project}
Current database: {db} ({exists})
Model: {model} ({dims}d)
```

Drop every occurrence of "NEVER use grep". The routing table is self-explanatory.

# AGENTS — Branch `feat/mcp-literal-search-tool`

> Scoped instructions for any coding agent (OpenCode, Claude Code, Copilot) working on this branch. This file is self-contained: everything needed to execute the work is below. A parallel Branch A (`feat/mcp-rebrand-hybrid-search`) handles a separate concern and is expected to have merged before this branch is rebased onto `main`.

## Why this branch exists

Agents observing codesearch via MCP fall back to `grep` for pure literal lookups: error codes, TODO tags, env var names, URLs, hardcoded strings, regex patterns. The reason is simple — **there is no MCP tool that exposes literal/regex search without going through the semantic pipeline.**

Internally, `semantic_search` does fuse Tantivy FTS results, but:

- Every call pays the embedding cost (~50-200ms) even for pure literal queries.
- Tantivy's regex, phrase, and field-scoped capabilities are hidden behind the natural-language framing.
- Non-identifier literals (error strings with spaces, URLs, regex patterns) are not picked up by `detect_identifiers`, so they get no exact-match boost — and are then diluted by vector neighbours in RRF fusion.

This branch adds one dedicated tool: `literal_search`. No changes to semantic search, ranking, or indexing.

## Prerequisite

This branch assumes Branch A (`feat/mcp-rebrand-hybrid-search`) has merged to `main`. In Branch A, `semantic_search` sets `suggested_tool = "literal_search"` when the query has no detected identifiers and top score is low — so `literal_search` must exist as an advertised tool once Branch A's hint takes effect. If this branch is opened while Branch A is still in review, rebase onto `main` after A merges before final PR.

## Scope — what to implement

1. Add `search_regex` and `search_phrase` methods on `FtsStore`.
2. Add `LiteralSearchRequest` / `LiteralSearchResultItem` types.
3. Add a new `literal_search` MCP tool that:
   - skips the embedding service entirely,
   - routes the query to `search_exact`, `search_phrase`, or `search_regex` based on input flags,
   - supports `file_glob` and `language` post-filters,
   - supports `format = "json" | "grep"` output.

## Scope — what NOT to touch

- `semantic_search`, `find_definition`, `find_usages`, `find_references`, `index_status`, `find_databases` — do not modify.
- The embedding pipeline (`src/embed/`).
- The chunker (`src/chunker/`).
- The ranker (`src/rerank/`, `src/search/`).
- The vector store (`src/vectordb/`) — read-only access only, via existing APIs.
- `Cargo.toml` — no new dependencies; Tantivy already provides everything needed.

## File-by-file plan

### `src/fts/tantivy_store.rs`

Add two public methods to `FtsStore`. Signatures:

```rust
/// Search using a regex pattern over the content field.
/// Pattern syntax is Tantivy's (Rust-regex-compatible).
/// Returns hits ordered by BM25 score.
pub fn search_regex(
    &self,
    pattern: &str,
    limit: usize,
) -> Result<Vec<FtsResult>>;

/// Search for an exact phrase (consecutive tokens) over the content field.
/// The phrase is tokenised with the same analyser as indexed content.
pub fn search_phrase(
    &self,
    phrase: &str,
    limit: usize,
) -> Result<Vec<FtsResult>>;
```

Implementation notes:

- `search_regex` uses `tantivy::query::RegexQuery::from_pattern(pattern, content_field)`.
- `search_phrase` uses `tantivy::query::PhraseQuery::new(vec![...])` built from tokens produced by the same tokenizer the index uses. If the phrase tokenises to a single term, fall back to a `TermQuery` and document this in a comment.
- Both methods return `FtsResult` in the same shape as `search` / `search_exact` so callers can reuse chunk-resolution code.
- Errors from malformed regex must surface as `anyhow::Error` with a clear message — do **not** panic.

Add unit tests in the same file (under `#[cfg(test)] mod tests`):

- `search_regex` finds chunks matching a simple pattern on a fixture index.
- `search_regex` with a malformed pattern returns `Err`, not panic.
- `search_phrase` returns only hits where the words appear consecutively.
- `search_phrase` with a single-word phrase degrades to term search without error.

### `src/mcp/types.rs`

Add:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LiteralSearchRequest {
    /// The literal string, phrase, or regex pattern to search for.
    pub query: String,

    /// Treat `query` as a regex pattern (Rust-regex / Tantivy syntax). Default: false.
    pub regex: Option<bool>,

    /// Treat `query` as an exact phrase (words in order). Default: false.
    /// If `query` is wrapped in double quotes, `phrase` is implied (the quotes are stripped).
    pub phrase: Option<bool>,

    /// Glob filter on result file path, e.g. "src/**/*.rs".
    pub file_glob: Option<String>,

    /// Language filter, e.g. "rust", "python". Post-filter on `Language::from_path`.
    pub language: Option<String>,

    /// Maximum number of results (default: 50).
    pub limit: Option<usize>,

    /// Output format: "json" (default) or "grep".
    /// "grep" returns a single text block with `{path}:{line}: {snippet}` lines.
    pub format: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LiteralSearchResultItem {
    pub path: String,
    pub line: usize,
    pub snippet: String,
    pub score: f32,
}
```

### `src/mcp/mod.rs`

Register a new tool:

```rust
#[tool(
    description = "Exact string, phrase, or regex search over the indexed codebase. Tantivy FTS only — no embedding, no semantic reranking. Fast (~10-50ms).\n\nUSE FOR:\n- Error messages, TODO/FIXME tags, env var names, URLs, hardcoded strings\n- Regex patterns (set regex=true)\n- Exact phrases (wrap query in double quotes OR set phrase=true)\n- Narrowed scope via file_glob or language filter\n\nDO NOT USE FOR:\n- Conceptual / natural-language queries → use `semantic_search`\n- Finding usages of a symbol where you don't know the exact spelling → use `find_usages`\n\nReturns 0 results if the literal is not in any indexed file — in that case escalate to `semantic_search` for conceptual lookup.\nSet format=\"grep\" for `path:line:` output."
)]
async fn literal_search(
    &self,
    Parameters(request): Parameters<LiteralSearchRequest>,
) -> Result<CallToolResult, McpError> { ... }
```

Handler logic:

1. Call `ensure_database_exists()` early.
2. Determine mode from flags:
   - If `regex == Some(true)` → regex mode.
   - Else if `phrase == Some(true)` → phrase mode.
   - Else if query starts and ends with `"` and has length ≥ 3 → phrase mode, strip quotes.
   - Else → exact mode (existing `search_exact` with `structural_intent = None`).
3. Open `FtsStore`. Do **not** open the embedding service. Do **not** call `VectorStore::search` — this tool is FTS-only for retrieval. You may use `VectorStore::get_chunk` to resolve metadata for the snippet.
4. Execute the chosen FTS method with `limit * 3` (raw budget for post-filtering).
5. Resolve each `FtsResult.chunk_id` → chunk metadata via `get_chunk` (shared store if available; fall back to opening one). Use the existing pattern from `find_references`.
6. Apply `language` post-filter: `Language::from_path(path) == requested_language` (case-insensitive).
7. Apply `file_glob` post-filter using the `glob` crate if already a dep, otherwise a simple prefix/suffix matcher. **Do not add new dependencies in this branch** — if `glob` is not already present, ship v1 with prefix/suffix glob support only (`*`, `**` supported; full glob syntax deferred).
8. Take `limit` results.
9. For each result build `LiteralSearchResultItem { path, line: start_line, snippet, score }`. The snippet is the first line of the chunk content, truncated to 200 chars.
10. Serialize:
    - `format == "grep"` → single `Content::text` joining `format!("{}:{}: {}", path, line, snippet)` with `\n`.
    - Otherwise → JSON array of `LiteralSearchResultItem`.

## Acceptance criteria

All of these must hold before PR merge:

- `cargo test --all` passes.
- `cargo clippy --all-targets -- -D warnings` passes.
- New unit test in `src/mcp/mod.rs` tests module: `find_definition("CodesearchService")` returns at least one result, and all results have `kind == "Struct"` (or the project's equivalent chunk kind for a Rust struct definition).
- New unit test: `find_usages("authenticate")` on a fixture project does **not** include the chunk whose `signature` starts with `fn authenticate(` (or language equivalent).
- New unit test: `semantic_search` with a deliberately nonsensical query (e.g. `"xyzzy_nonexistent_quux"`) returns a response with `low_confidence == Some(true)`.
- New unit test: `semantic_search` with `mode = Some("lexical")` does not invoke the embedding service. Verify via a trace-level log assertion or a test double.
- Existing `test_mcp_no_raw_stdout_calls` still passes (do not break the JSON-RPC contract).
- `get_info().instructions` output is ≤ 50 lines (simple line count in a test).
- Manual: start the server against a real repo, run the 20-query benchmark against main. No regression on conceptual queries.

- Unit tests in `src/fts/tantivy_store.rs` (listed above) pass.
- Integration test against a fixture index:
  - `literal_search(query="TODO")` returns expected TODO hits in `< 100ms` on a warm index.
  - `literal_search(query="handle_\\w+_request", regex=true)` returns regex matches.
  - `literal_search(query="connection refused", phrase=true)` returns only phrase matches (not individual word hits).
  - `literal_search(query="fn new", file_glob="src/mcp/**")` returns only hits whose path starts with `src/mcp/`.
  - `literal_search(query="authenticate", format="grep")` returns a single text block where each line matches `^[^:]+:\d+: .+$`.
- Trace assertion: the `literal_search` code path does not invoke `EmbeddingService`. Recommended approach: in a test, spy on `get_embedding_service` (or equivalent) and assert it is never called during a `literal_search` invocation.
- Existing `test_mcp_no_raw_stdout_calls` still passes.
- Tool appears in `get_info().instructions` routing table (add a row to the table that Branch A introduced).

## Commit hygiene

- Small commits, one logical change each.
- Conventional-commit style (`feat(mcp): ...`, `refactor(mcp): ...`, `test(mcp): ...`, `docs(mcp): ...`).

- Conventional-commit style (`feat(mcp): ...`, `feat(fts): ...`, `test(fts): ...`, `docs(mcp): ...`).
- Author identity = Filip Develter personal GitHub (`flupkede`). Verify `git config user.email` before first commit.

## PR expectations

- Title: `feat(mcp): rebrand semantic_search as hybrid, split find_references`
- Target base: `main`
- Description should include a before/after table of tool names and one-line descriptions.
- Link this AGENTS file in the PR body.

## What comes after this branch

A separate branch `feat/mcp-literal-search-tool` will add a dedicated `literal_search` tool exposing Tantivy regex/phrase/field capabilities without an embedding call. That branch is explicitly out of scope here — do not pre-empt it.

- Title: `feat(mcp): add literal_search tool with regex and phrase support`
- Target base: `main` (after Branch A has merged).
- Description should include:
  - One-paragraph rationale.
  - A short table of example queries and which mode each triggers.
  - Link to this AGENTS file.
  - Link to Branch A's PR for context.

## Deliberately out of scope

- True ripgrep-style boolean query syntax (`foo AND bar NOT baz`).
- Indexing non-AST files (Markdown, YAML, configs). Agents may still fall back to `grep` for those — that's a known gap and will be addressed in a later branch.
- Warm-start or preloading of the embedding model.
- AST-based reference resolution (tree-sitter call-expression queries).
- Query expansion for short ambiguous queries.
- Changes to the `glob` dependency — keep v1 glob support minimal.
