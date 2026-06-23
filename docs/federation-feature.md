# codesearch — Federation Feature Plan

**Status:** Phase 1 + Phase 2 (search/get_chunk) shipped · **Scope:** codesearch Rust repo · **Related:** `codesearch-federation-aprimo-mcp.md` (aprimo_mcp + ops side, in the aprimo_mcp repo)

> **Phase status:** Phase 1 (REST endpoints) — ✅ done · Phase 2 (federation dispatch) — ✅ done for `search` + `get_chunk`; `find`/`explore`/`find_impact` federation deferred (see Open items) · Phase 3 (TLS + ops hardening) — ⏳ planned.

## Context

Goal: let one codesearch serve delegate READ queries (docs/KB) to a REMOTE peer serve over TLS, so a team can share ONE cloud-hosted knowledge base while each dev keeps code search local. This document covers the **codesearch Rust** feature work (REST endpoints, `remotes` config, federation dispatch, RRF merge, TLS). Home-dir consolidation, custom-KB git delivery and the ACI indexer are documented in the aprimo_mcp repo (`docs/codesearch-federation-aprimo-mcp.md`).

## Storage truths (why federation, not a shared DB)

- Vector DB = LMDB (`heed`+`arroy`, `.codesearch.db/`); FTS = Tantivy; SCIP = LMDB. `VectorStore` (src/vectordb/store.rs) is a concrete struct — NO trait abstraction, NO remote/CouchDB backend possible.
- LMDB is memory-mapped → CANNOT run on a network FS (SMB/NFS/Azure Files corrupts). Single writer per DB via OS file lock (`fs2` on `.codesearch.db/writer.lock`).
- Conclusion: each serve instance owns its own local DB. Sharing is at **query-result** level (federation) + **source-file** level (delivery), NEVER at DB level.

## Serve already supports non-localhost (verified, live)

- `CODESEARCH_SERVE_HOST`/`--host` (default 127.0.0.1), `CODESEARCH_SERVE_PORT`/`--port` (default 39725). Issue #114 / `feature/host-binding`.
- Non-localhost bind MANDATES `CODESEARCH_SERVE_API_KEY` (Bearer auth); `NetworkAuthConfig` middleware (src/serve/mod.rs:57) protects all routes incl `/mcp`. `/mcp` = Streamable HTTP MCP transport.
- `CODESEARCH_ALLOWED_ROOTS`, `CODESEARCH_REPO_IDLE_TIMEOUT_SECS` (1800s). NO built-in TLS → needs reverse proxy (Caddy).

## Config schema (Phase 2)

Add a typed `remotes` map to `ReposConfig` (src/db_discovery/repos.rs):

```rust
pub struct ReposConfig {
    pub repos: HashMap<String, PathBuf>,
    pub groups: HashMap<String, Vec<String>>,
    pub repos_meta: HashMap<String, RepoMeta>,
    #[serde(default)]
    pub remotes: HashMap<String, RemotePeer>,   // NEW
}

pub struct RemotePeer {
    #[serde(alias = "base_url")]
    pub url: String,                 // e.g. "https://codesearch.example.com" (accepts legacy "base_url")
    #[serde(default)]
    pub api_key: String,             // empty allowed (skips Bearer header)
    pub group: Option<String>,       // external group to query (default "all")
    pub timeout_secs: Option<u64>,   // default 15
}
```

- `#[serde(default)]` → fully backwards compatible; existing local-only configs unchanged.
- A group references a remote via `@`-prefix, e.g. `"docs": ["@cloud"]`.
- The federation-aware resolver is `resolve_group_targets(group)` (returns `Vec<Target>`); the original `resolve_group()` stays local-only for back-compat. `Target = Local { alias, path } | Remote { peer_name, peer }`.
- The virtual `"all"` group stays LOCAL (never fans out to remotes).

## Phase 1 — REST endpoints (✅ done)

Confirmed: NO REST search endpoint existed (search was MCP-mediated only). Added to the serve router (src/serve/mod.rs), guarded by the existing `require_auth_for_network` layer (Bearer/X-API-Key on network bind; pass-through on localhost). Path constants live in `src/constants.rs`:

| Method | Path | Body/Query | Returns |
|---|---|---|---|
| POST | `/search` | `SearchRequest` | fused results (semantic or literal) |
| POST | `/find` | `FindRequest` | definition/usages/imports/dependents |
| POST | `/explore` | `ExploreRequest` | outline/similar chunks |
| GET | `/chunk/{id}` | `?project=&context_lines=&group=` | `GetChunkResponse` |
| GET | `/status` | — | projects/groups/index status |

Each REST handler constructs a per-request `CodesearchService` bound to the shared `ServeState`, calls the existing `#[tool]` method via `Parameters(req)`, and returns the tool's JSON payload unwrapped from `CallToolResult`. The embedding model (ONNX) is shared serve-wide via `ServeState` so it loads once lazily and is reused by all MCP sessions + REST handlers. Tool errors return HTTP 200 with a `_mcp_is_error: true` marker (MCP semantics); HTTP 500 only on rare `McpError`.

## Phase 2 — Federation dispatch + merge (✅ done for search + get_chunk)

In `CodesearchService` (src/mcp/mod.rs), the **`search`** and **`get_chunk`** tool methods now federate when the requested `group` contains remote targets (`@`-prefixed). Other read tools (`find`, `explore`, `find_impact`, `status`) stay local for now — see Open items.

**`search`:** `split_group_targets(group)` separates local repos from remote peers. Local targets are searched via the existing internal `semantic_search`/`literal_search` handlers (these ignore `@remote` group members, so they search ONLY the local repos in the group). Remote targets are fanned out concurrently (tokio `JoinSet`) to the cloud's REST `/search` via `FederationClient` (new module `src/federation/mod.rs`, built on `build_serve_client_with_key()` with key=`None`; each request attaches `.bearer_auth(peer.api_key)` individually so different peers can use different keys). reqwest does TLS natively, no new dependency.

**Merge** via RRF-interleave of disjoint ranked lists: each item's score = `1/(k + rank + 1)` with `k = DEFAULT_RRF_K` (20). Since local and remote indexes are disjoint (different repos/KB), there is no chunk-id collision; the union is sorted by RRF score (stable, local-first on ties) and truncated to `limit`. (The existing `rrf_fusion`/`rrf_fusion_with_exact` in `src/rerank/mod.rs` operate on single-index `SearchResult`/`FtsResult` slices by `chunk_id`; the cross-source merge is a separate `merge_ranked_lists` helper because the inputs are already-rendered `SearchResultItem` lists, not raw store chunks.)

Remote hits carry a `source: "<peer_name>"` tag and a `chunk_ref: "<peer_name>:<chunk_id>"` field (new optional fields on `SearchResultItem`). **`get_chunk`** routes a `chunk_ref` to the originating peer's REST `/chunk/:id` (the `chunk_ref` field drives routing — not a `chunk_id` prefix). This makes remote hits actionable from a federated result set.

### Failure semantics

Remote timeout/unreachable → NEVER hard-fail. Every remote failure mode (transport error, non-2xx, `_mcp_is_error`, non-JSON body, task panic) is converted to a warning: return local-only results (or the union of reachable peers) and add a `warnings: ["remote 'cloud' unreachable: <reason>"]` field on the response.

Config errors are **lenient, not hard-failing**: an unknown `@<peer>` reference is pruned with a `tracing::warn!` at config load (`reconcile()`) and re-checked leniently at query time (`resolve_group_targets` skips unknown peers with a warning). The system never crashes on a hand-edited config — the bad entry is dropped and the rest of the group still resolves.

### Scope

Only READ tools federate. Write tools (index/reindex/add/rm) stay local — the cloud index is maintained by the delivery pipeline (see aprimo_mcp plan), not by MCP writes.

## Phase 3 — TLS + ops hardening

- TLS termination via Caddy reverse proxy (codesearch has no built-in TLS).
- Per-remote API key stored in Azure Key Vault; injected as env at serve start.
- Remote query result caching + health/fallback.
- ACI Dockerfile bundling codesearch + Caddy + harvest-timer + git-pull (ops; details in aprimo_mcp plan).

## Test plan (Rust)

- `resolve_group_targets` returns mixed Local+Remote targets; unknown remote name → pruned with warning (lenient, not error). The virtual `"all"` group never federates.
- Federation client: per-peer Bearer header (empty key → no header), HTTPS, timeout, body parsing; URL construction handles the `/chunk/:id` path substitution.
- RRF merge: disjoint local + remote ranked lists interleave by `1/(k+rank+1)`; stable local-first tiebreak; truncation to `limit`; `source` + `chunk_ref` tagging on remote hits.
- Failure path: remote unreachable → local-only results + `warnings`, no panic.
- REST endpoints: identical contracts with MCP counterparts; auth rejected without key on network bind.
- 529 tests pass (513 lib + 16 new across repos config, federation client, and helpers).

## Open items

- **`find` / `explore` federation (deferred from Phase 2):** these read tools currently stay local. Federation for them would be simple result-list concatenation (no cross-source ranking needed for definition/usages lookups). Follow-up.
- Decide the `find_impact` (SCIP, C#-only today) federation story — probably not needed for docs-only cloud.
- Result caching strategy for remote queries (Phase 3).
- Code-health follow-ups (non-blocking): cache one `FederationClient` on `CodesearchService` for cross-call HTTP keep-alive; share the `CallToolResult` text-extraction helper between `extract_call_tool_text` and `call_tool_result_to_json`.
