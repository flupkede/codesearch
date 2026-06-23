# codesearch — Federation Feature Plan

**Status:** Draft · **Scope:** codesearch Rust repo · **Related:** `codesearch-federation-aprimo-mcp.md` (aprimo_mcp + ops side, in the aprimo_mcp repo)

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
    pub url: String,                 // e.g. "https://codesearch.example.com"
    pub api_key: String,
    pub group: Option<String>,       // external group to query (default "all")
    pub timeout_secs: Option<u64>,   // default 15
}
```

- `#[serde(default)]` → fully backwards compatible; existing local-only configs unchanged.
- A group references a remote via `@`-prefix, e.g. `"docs": ["@cloud"]`.
- `resolve_group` returns `Vec<Target>` where `Target = Local { alias, path } | Remote { peer }`.
- The virtual `"all"` group stays LOCAL (never fans out to remotes).

## Phase 1 — REST endpoints

First task: CONFIRM no REST search endpoint exists today (search currently appears MCP-mediated only — tests `test_*_search_request_with_group` in mcp/mod.rs). If absent, add to the serve router (src/serve/mod.rs), guarded by the existing `NetworkAuthConfig` middleware when network-bound:

| Method | Path | Body/Query | Returns |
|---|---|---|---|
| POST | `/search` | `SemanticSearchRequest` | list of `FusedResult` |
| POST | `/find` | find request (kind/symbol) | definition/usages results |
| POST | `/explore` | explore request | outline/similar chunks |
| GET | `/chunk/{id}` | `?project=&context_lines=` | chunk content |
| GET | `/status` | — | projects/groups/index status |

Shapes mirror the existing MCP request types (src/mcp/types.rs) so server-to-server federation and the agent MCP tools share contracts.

## Phase 2 — Federation dispatch + merge

In `CodesearchService` (src/mcp/mod.rs:2585), for each read-only tool handler (search/find/get_chunk/explore/find_impact/status): split resolved targets. Local targets open stores as today (`get_or_open_stores()`). Remote targets call the REST endpoints via a federation client reusing `build_serve_client_with_key()` (auto-attaches `Authorization: Bearer <key>`) over HTTPS — reqwest does TLS natively, no new dependency.

Merge via the existing **RRF fusion** (src/rerank/mod.rs: `rrf_fusion` / `rrf_fusion_with_exact`): treat each remote's result list as an additional input list with the same `k`. Remote chunk IDs are namespaced (e.g. `"cloud:12345"`) to avoid collision with local IDs; `get_chunk` routes by prefix.

### Failure semantics

Remote timeout/unreachable → NEVER hard-fail. Return local-only results and add a `warnings: ["remote 'cloud' unreachable: <reason>"]` field. Config errors (unknown remote name referenced in a group) DO fail hard at startup/query-time with a clear message.

### Scope

Only READ tools federate. Write tools (index/reindex/add/rm) stay local — the cloud index is maintained by the delivery pipeline (see aprimo_mcp plan), not by MCP writes.

## Phase 3 — TLS + ops hardening

- TLS termination via Caddy reverse proxy (codesearch has no built-in TLS).
- Per-remote API key stored in Azure Key Vault; injected as env at serve start.
- Remote query result caching + health/fallback.
- ACI Dockerfile bundling codesearch + Caddy + harvest-timer + git-pull (ops; details in aprimo_mcp plan).

## Test plan (Rust)

- `resolve_group` returns mixed Local+Remote targets; unknown remote name → error.
- Federation client: attaches Bearer header, HTTPS, timeout, body parsing.
- RRF merge: local + remote lists merge correctly; chunk-ID namespacing prevents collision.
- Failure path: remote unreachable → local-only results + `warnings`, no panic.
- REST endpoints: identical contracts with MCP counterparts; auth rejected without key on network bind.

## Open items

- Decide the `find_impact` (SCIP, C#-only today) federation story — probably not needed for docs-only cloud.
- Result caching strategy for remote queries (Phase 3).
