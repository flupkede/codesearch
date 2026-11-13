# Core split: codesearch-core crate (stage 1 of codesearch-server, GH #384)
URL: https://github.com/develterf_dlwr/todo/issues/288
Labels: Private, opencode, MCP, codesearch, worktree

Stage 1 of the codesearch-server plan (GitHub issue flupkede/codesearch#384).
Extract a `codesearch-core` crate so the CLI and the future server share chunking, tree-sitter, embedding interface, retrieval and MCP protocol. CLI behavior unchanged, no feature drift.
Branch: features/core-split (off develop, tip eb267b3), per AGENTS.develop.md. Commit AGENTS.md/CLAUDE.md branch-local as you work; drop them before the PR merges.

Todos:
- [ ] Survey crate layout (src/ modules, lib/bin split, tests layout) and record the move-map in the worktree AGENTS.md under `## Plan` before touching code
- [ ] Convert to a cargo workspace: codesearch-core (lib), the existing CLI binary, plus an empty placeholder codesearch-server crate
- [ ] Move chunking + tree-sitter chunking into codesearch-core
- [ ] Move the embedding interface into codesearch-core (keep the local fastembed implementation with it if cleanly shared, judge on coupling)
- [ ] Move retrieval (search, fusion/RRF) into codesearch-core
- [ ] Move the MCP protocol/server layer into codesearch-core
- [ ] Keep in the CLI crate: LMDB storage backend, CLI wiring, serve orchestration, index lifecycle
- [ ] Workspace-level dependency inheritance (workspace = true); fix imports and feature flags; zero behavior change
- [ ] Validate per AGENTS.develop.md: cargo fmt --all -- --check, cargo clippy --all-targets -- -D warnings, cargo test --lib --bins; dev profile only, noisy runs to a background file
- [ ] Update CI workflows for the workspace layout; CHANGELOG entry + README note on the new structure
- [ ] PR into develop with --base develop; after merge tick the stage-1 checkbox on GH issue #384

## Plan — move-map (survey 2026-11-13, features/core-split @ eb267b3)

Layout today: single crate `codesearch` (lib+bin, src/ with 26 modules, lib.rs re-exports).
Seams found: mcp→{serve(ServeState), symbols, db_discovery, federation, index(SharedStores)};
search→{index lifecycle, output}; federation→index(build_serve_client_with_key);
embed/vectordb→{lmdb_registry, cache, constants}; symbols/db_discovery/watch have no upward deps;
ServeState holds Arc<IndexManager> and drives FSW (it is the serve orchestrator → stays CLI).

Workspace: root stays package `codesearch` (CLI; lib.rs becomes a facade) + members `core/`
(codesearch-core) and `server/` (codesearch-server placeholder). `[patch.crates-io]` rmcp→vendor
stays at the workspace root. Shared deps move to `[workspace.dependencies]`.

Move-map (→ codesearch-core unless noted):
- chunker/, file/, utils/, error.rs, constants.rs, limits.rs, logger/, cache/, lmdb_registry.rs,
  testing.rs (plain module, not cfg(test) — test-only items vanish in dependency builds), bench/,
  output.rs (rerank's info_print!/warn_print! macros need it in core) and db_discovery/ (both
  pulled forward into the plumbing stage). DEVIATION: bench/ stayed CLI (benches the public
  `codesearch::` facade, no crate:: core refs) — drop from core move-map.
- embed/ wholesale incl. local fastembed + openai + LMDB embedding cache (cohesive module; cache
  rides core lmdb_registry — coupling judged clean)
- fts/, rerank/, vectordb/ (retrieval)
- search/: query engine → core; CLI command flow (auto-create-index preamble, sync_database,
  output prints) → CLI crate
- index/ SPLIT: SharedStores struct + inherent impls + store-open mechanics → core::index;
  IndexManager + lifecycle fns (add/remove/index/prune/stats/clear) + build_log + key_migration
  + find_git_root → CLI
- symbols/, federation/ (+ build_serve_client_with_key moves in from index/mod.rs)
- mcp/ wholesale; the axum REST mirrors (tail of mcp/mod.rs, ServeState-extractor handlers)
  move to CLI serve/
- Stay CLI: ServeState + serve/ orchestration (axum, TUI, reaper, warmup), watch/,
  cli/, main.rs, index lifecycle

Documented deviation: mcp handlers hold `Arc<SharedStores>` and route multi-repo calls through
ServeState, but ServeState+IndexManager must stay CLI. Core therefore gets (a) SharedStores
(store-open mechanics, no lifecycle) and (b) a core `ServeHandle` trait (async_trait) mirroring
the ServeState surface mcp uses (config_snapshot, get_or_open_stores, record_tool_call,
touch_access, session_disconnected, symbol_registry, embedding_pool, default_model,
heal_missing_symbol_index, status snapshot). CLI ServeState implements it; mcp holds
`Option<Arc<dyn ServeHandle>>`; handler call sites keep their method names.

Import strategy: CLI lib.rs re-exports moved modules under their old paths
(`pub use codesearch_core::chunker;`) so CLI-internal `crate::` references stay valid; moved
files rewrite `crate::` → `codesearch_core::`. Features cuda/tensorrt/csharp/ts-helper are
defined on core and forwarded by the CLI package. Core+server version 0.1.0, publish=false;
the CLI keeps semver + auto-bump.

Stages: 1 plan (this) · 2 workspace shell · 3 plumbing layer · 4 chunker ·
5 retrieval + embed stack + SharedStores split (merged 5+6: embed/cache.rs needs
vectordb::merge_metadata_atomic AND vectordb/store.rs needs embed::EmbeddedChunk —
mutual coupling, one stage or none) · 7 symbols/federation/db_discovery ·
8 mcp + ServeHandle inversion · 9 dep inheritance + features + facade sweep ·
10 full validation trio · 11 CI + Dockerfile + CHANGELOG + README + drop
branch-local agent files · 12 push + PR (--base develop). Per-stage validation:
cargo fmt --check + clippy -D warnings (background, output to file); full trio in stage 10.
