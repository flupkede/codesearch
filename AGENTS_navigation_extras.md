# AGENTS — Branch `feature/mcp-navigation-extras`

> Scoped instructions for any coding agent (OpenCode, Claude Code) working on this branch.
> Self-contained: everything needed to implement the work is below.
> Parallel branch `feature/mcp-multi-repo` handles a separate concern — do not touch it here.

## Why this branch exists

codesearch returns chunk metadata from search results (path, line range, kind, signature, score) but gives agents no efficient way to navigate from there without an external read-tool call on precise line offsets. This branch adds six targeted navigation and retrieval tools directly to the MCP surface. All features reuse data that is already indexed — no changes to chunking, embedding, or ranking.

## Prerequisites

- Branch A (`feature/mcp-rebrand-hybrid-search`) should have merged first. If it has not, implement on top of current `main` and expect minor merge conflicts on `src/mcp/mod.rs` and `src/mcp/types.rs` tool descriptions only.
- Branch B (`feature/mcp-literal-search-tool`) is independent — no dependency either way.

## Scope — what to implement

Six new MCP tools, in priority order:

1. `file_outline(path, project?)` — structural skyline of a file
2. `get_chunk(chunk_id, context_lines?, project?)` — chunk body + surrounding lines
3. `find_imports(path, project?)` — what does this file import/use?
4. `find_dependents(symbol_or_path, project?)` — who imports this file/module?
5. `similar_chunks(chunk_id, limit?, project?)` — semantic neighbours of a chunk
6. `literal_search` highlight improvement — match-bearing line instead of first line of chunk

The `project?` parameter is a forward-compatibility stub for `feature/mcp-multi-repo`. In this branch it is accepted but ignored — single-repo behavior only. Do NOT implement multi-repo logic here.

## Scope — what NOT to touch

- `src/chunker/` — no changes to AST parsing or chunk extraction
- `src/rerank/`, `src/search/` — no ranking changes
- `src/embed/` — no embedding model changes
- `src/fts/tantivy_store.rs` — only read operations, no new index fields
- `src/vectordb/` — only read operations via existing `get_chunk`, `search` APIs
- Multi-repo logic, `codesearch serve`, HTTP transport — belongs in `feature/mcp-multi-repo`
- The `compact=false` mode — do not expand it; `get_chunk` replaces its use case

## Detailed plan per tool

---

### 1. `file_outline(path, project?)`

**Description for `#[tool]`:**
```
List all indexed top-level symbols in a file — kind, signature, and line range, no body content.
Use this to understand a file's structure before deciding which chunks to read.
Much cheaper than reading the full file.

USE FOR: "what functions are in auth.rs", "show me the structure of this module".
DO NOT USE FOR: finding where a symbol is defined across the codebase → use `find_definition`.
```

**Request type (`src/mcp/types.rs`):**
```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileOutlineRequest {
    /// File path, relative to project root or absolute.
    pub path: String,
    /// Forward-compat stub — ignored in this branch.
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FileOutlineItem {
    pub chunk_id: u32,
    pub kind: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}
```

**Implementation (`src/mcp/mod.rs`):**
- Normalize `path` to absolute using `project_path` as base (reuse existing normalize logic).
- Open VectorStore (shared stores if available, else standalone — same pattern as all other tools).
- Call a new `VectorStore::chunks_for_file(path: &str) -> Result<Vec<ChunkMeta>>` (see below).
- Sort results by `start_line` ascending.
- Return `Vec<FileOutlineItem>` as JSON.
- If no chunks found: return informative message "No indexed chunks found for path. Verify the file is within the project root and the index is up to date."

**New VectorStore method (`src/vectordb/mod.rs` or equivalent):**
```rust
pub fn chunks_for_file(&self, path: &str) -> Result<Vec<ChunkMeta>> {
    // Iterate LMDB, filter by normalized path.
    // ChunkMeta = { id, kind, signature, start_line, end_line }
    // No content needed — keep this lightweight.
}
```
Use the existing LMDB iteration pattern. Normalize the incoming path the same way paths are stored (reuse `normalize_path_str`). This must be a read-only operation.

---

### 2. `get_chunk(chunk_id, context_lines?, project?)`

**Description for `#[tool]`:**
```
Retrieve the full content of a specific chunk by its ID, plus optional surrounding lines for context.
Use this after semantic_search or file_outline to read the actual code without loading the whole file.

USE FOR: reading a specific function/class body after finding it via search.
Set context_lines (default 0, max 20) to include lines before and after the chunk.
```

**Request type:**
```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetChunkRequest {
    /// chunk_id as returned by semantic_search, file_outline, find_definition, or find_usages.
    pub chunk_id: u32,
    /// Lines of surrounding context to include (default: 0, max: 20).
    pub context_lines: Option<usize>,
    /// Forward-compat stub — ignored in this branch.
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetChunkResponse {
    pub chunk_id: u32,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub signature: Option<String>,
    pub content: String,
    /// Lines before the chunk start (up to context_lines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    /// Lines after the chunk end (up to context_lines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
}
```

**Implementation:**
- Fetch chunk via `VectorStore::get_chunk(chunk_id)` (already exists).
- If `context_lines > 0`: read the source file from disk at `chunk.path`, extract lines `[start - context_lines, start)` and `(end, end + context_lines]`. Clamp to file boundaries. Use `tokio::fs::read_to_string` — this tool is async.
- Cap `context_lines` at 20 to prevent token abuse. If caller sends > 20, silently clamp and note it in response (add `"context_lines_clamped": true` field).
- If file cannot be read from disk (deleted since indexing): return chunk content from DB only, set `context_before`/`context_after` to null, add note "source file not readable, returning indexed content only".

**Note:** This tool makes `compact=false` on `semantic_search` largely redundant. Do NOT remove `compact=false` in this branch (backward compat) but update `semantic_search` description to mention `get_chunk` as the preferred alternative.

---

### 3. `find_imports(path, project?)`

**Description for `#[tool]`:**
```
List all imports/dependencies declared in a source file.
Uses tree-sitter AST data already present in the index — no re-parsing needed.

USE FOR: "what does auth.rs depend on", understanding a file's dependencies before refactoring.
DO NOT USE FOR: finding who imports this file → use `find_dependents`.
```

**Request type:**
```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindImportsRequest {
    pub path: String,
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ImportItem {
    pub imported: String,     // the module/symbol being imported
    pub line: usize,
    pub kind: String,         // "use", "import", "require", "include", etc.
}
```

**Implementation:**
- `chunks_for_file(path)` → filter by `kind ∈ {"Import", "Use", "Require", "Include"}`.
- If the chunker does not currently emit an "Import" kind for these statements: check `src/chunker/extractor.rs` to verify. If import statements are not chunked as a distinct kind, fall back to FTS: `fts_store.search_exact("import", limit*2, None)` filtered to `path`, which gives approximate results. Document this limitation in a code comment.
- Return `Vec<ImportItem>` sorted by `line` ascending.
- If empty: "No import chunks found. The index may not include import statements for this language, or the file has no imports."

**Implementation note on import kinds:** Check what kinds the current tree-sitter chunker actually emits (look at `src/chunker/extractor.rs`). Use whatever kind name is already used — do NOT change the chunker to add new kinds in this branch. If imports are not separately chunked, the FTS fallback is acceptable for v1 and must be clearly documented.

---

### 4. `find_dependents(symbol_or_path, project?)`

**Description for `#[tool]`:**
```
Find all files that import or depend on a given module, file path, or symbol.
Essential for impact analysis: "if I change this module, what else breaks?"

USE FOR: refactoring impact analysis, understanding who depends on a module.
DO NOT USE FOR: finding usages of a specific function call → use `find_usages`.
```

**Request type:**
```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDependentsRequest {
    /// Module name, file path, or symbol to find dependents of.
    /// Examples: "auth", "src/auth.rs", "UserService"
    pub symbol_or_path: String,
    pub limit: Option<usize>,
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DependentItem {
    pub path: String,
    pub line: usize,
    pub import_statement: String,   // the raw import chunk signature/content
}
```

**Implementation:**
- This is essentially `find_usages` scoped to import-kind chunks.
- FTS search for `symbol_or_path` with a filter on `kind ∈ {"Import", "Use", "Require", "Include"}`.
- Same fallback caveat as `find_imports` if import kinds are not emitted by the chunker.
- Deduplicate by file path — if a file imports the same thing twice, show once.
- Return `Vec<DependentItem>` sorted by path.

---

### 5. `similar_chunks(chunk_id, limit?, project?)`

**Description for `#[tool]`:**
```
Find chunks semantically similar to a given chunk (by chunk_id).
Uses the chunk's existing embedding — no new embedding call needed.

USE FOR: finding duplicate implementations, similar patterns, related code across the codebase.
DO NOT USE FOR: finding where a symbol is used → use `find_usages`.
```

**Request type:**
```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilarChunksRequest {
    pub chunk_id: u32,
    pub limit: Option<usize>,       // default 5, max 20
    pub project: Option<String>,
}
```

**Implementation:**
- Fetch the embedding vector for `chunk_id` from VectorStore. Check if `VectorStore` exposes `get_embedding(chunk_id) -> Result<Vec<f32>>`. If not, add this method — it is a simple LMDB read of the vector data.
- Call `VectorStore::search(&embedding, limit + 1)` — returns the chunk itself as top result.
- Filter out the input `chunk_id` from results.
- Cap limit at 20. Default 5.
- Return `Vec<SearchResultItem>` (reuse existing type, compact only — no full content).
- If embedding not found (chunk_id invalid or DB mismatch): return clear error.

**No embedding service call in this tool.** The vector is already stored. This must not trigger `get_embedding_service()`.

---

### 6. `literal_search` — highlight improvement

This is a targeted fix to `src/mcp/mod.rs` (or `src/fts/tantivy_store.rs`) from Branch B.

**Current behavior:** `LiteralSearchResultItem.snippet` = first line of chunk content.

**New behavior:** snippet = the line within the chunk that actually contains the match, not necessarily the first line. If multiple lines match (regex), return the first matching line. Truncate to 200 chars centered on the match position.

**Implementation:**
- After resolving `chunk_id` → chunk content, scan lines of `chunk.content` for the query string (or regex match if `regex=true`).
- Return the first line that contains the match.
- If no line directly contains it (tokenization boundary): fall back to first line of chunk (current behavior). Document this edge case.
- For grep-format output: this makes `path:line:` refer to the exact matching line, not the chunk start. Update `line` field accordingly (chunk.start_line + offset_within_chunk).

**Dependency:** Branch B must have landed on this branch (or on main before branching). If Branch B is not yet merged, skip this item and implement it as a follow-up commit once B merges.

---

## Acceptance criteria

All of these must hold before PR merge:

- `cargo test --all` passes.
- `cargo clippy --all-targets -- -D warnings` passes.
- Unit test: `file_outline("src/mcp/mod.rs")` returns at least the `CodesearchService` struct and one tool function, sorted by start_line.
- Unit test: `get_chunk(id, context_lines=5)` returns content + 5 lines before and after, clamped at file boundaries.
- Unit test: `get_chunk(id, context_lines=25)` clamps to 20 and sets `context_lines_clamped: true`.
- Unit test: `similar_chunks(id)` does NOT call `EmbeddingService` (verify via log assertion or test double).
- Unit test: `similar_chunks(id)` does NOT return `id` itself in results.
- Manual: `file_outline` on a 500-line Rust file returns in < 50ms (warm LMDB).
- Existing tests pass, including `test_mcp_no_raw_stdout_calls`.
- `project` parameter accepted on all six tools without error, currently ignored.

## What to check in the chunker first

Before implementing `find_imports` and `find_dependents`, inspect `src/chunker/extractor.rs`:

1. What `kind` strings are emitted for Rust `use` statements? Python `import`? TypeScript `import`?
2. Are they chunked individually or merged into the parent module chunk?
3. Document your findings in a comment at the top of the `find_imports` handler.

If import statements are NOT chunked as distinct kinds, note this clearly in the PR description and use the FTS fallback. Do not change the chunker in this branch.

## Commit hygiene

- One logical change per commit.
- Conventional-commit style: `feat(mcp): add file_outline tool`, `feat(mcp): add get_chunk tool`, etc.
- Author: Filip Develter personal GitHub (`flupkede`). Verify `git config user.email` before first commit.

## PR expectations

- Title: `feat(mcp): add navigation tools — file_outline, get_chunk, imports, similar_chunks`
- Target base: `main` (after Branches A and B have merged).
- PR description: one paragraph per tool explaining the user-facing behavior.
- Link this AGENTS file.

## Explicitly out of scope

- Multi-repo support (the `project` param is a stub only) → `feature/mcp-multi-repo`
- Non-AST file indexing (Markdown, YAML, configs) → future branch
- `compact=false` removal → do not touch
- Import graph persistence as a separate index structure → future branch
- Query expansion → explicitly not wanted
