# codesearch-server

An HTTP server for code search with PostgreSQL-backed storage and server-side
embedding. This is a separate binary, not a storage mode for the local CLI.
`codesearch` and `codesearch-core` do not depend on SQLx or pgvector. The server
reuses core chunking and embedding providers, **not** local LMDB/Tantivy indices,
local embedding caches, the CLI registry, or client-side SQL integration.

## Security and supported capabilities

**No authentication or authorization. Loopback only.** The binary refuses a
non-loopback `--listen` address; default `127.0.0.1:39726`. Before all routes,
the router requires a loopback IP or exact `localhost` HTTP authority (optional
valid nonzero port) and rejects non-loopback/malformed browser Origins with HTTP
403. Native clients may omit Origin. This blocks hostile Host/DNS-rebinding
requests; it is not authentication, and forwarded headers are not trusted.
Any local process can
read and modify all registered repositories. Do not proxy, port-forward or
expose this service to untrusted callers. There is no shared-key compatibility.
Auth, federation, publisher/scheduler integration, filesystem watching, symbol
navigation, working-copy namespaces and repository-wide snapshot publication
are not implemented. Publication is explicit, per-file HTTP input; the server
never reads a caller-supplied filesystem path. `/health` is process liveness,
not a database-readiness claim.

## Database prerequisites

Use a dedicated database. Provision **pg_search 0.27.1** and **pgvector >=0.8**;
other pg_search versions are rejected rather than silently using a legacy API.
The qualified image is
`paradedb/paradedb:0.27.1-pg17@sha256:8e5483653e2af1b904428dd025e4ac9564c01a4e10d3af196e73537f8cb94c3f`
(PostgreSQL 17.12, pg_search 0.27.1, vector 0.8.6). Self-managed pg_search needs
`shared_preload_libraries='pg_search'` and a PostgreSQL restart. A DBA must run:

```sql
CREATE EXTENSION vector;
CREATE EXTENSION pg_search CASCADE;
SELECT extname, extversion FROM pg_extension;
```

The application role needs connection and schema/table/index creation and CRUD
privileges. Startup creates `cs_server_*` tables and a source-code-tokenized
ParadeDB BM25 index transactionally; concurrent startup is serialized. All
repositories and the PostgreSQL embedding cache share one recorded embedding
fingerprint/dimension. A mismatch or unknown schema version fails startup.
Changing model, dimensions, retrieval prefixes or server document representation
requires a **separate database and reindex**, never an automatic destructive reset.
The database fingerprint includes the server document representation; incompatible
representations are rejected without migration or automatic deletion. As in core, endpoint changes
are allowed for the same model identity; operators must ensure the replacement
endpoint really serves the same embedding space (an opaque model name cannot
prove that). Endpoint URLs are not persisted as embedding identity.

Set `CODESEARCH_DATABASE_URL` through your process environment/secret manager.
TLS defaults to `verify-full` (configure a trusted CA and hostname); only an
explicit URL `sslmode` parameter overrides it. `sslmode=disable` is for isolated
loopback test databases, not remote deployments. URLs/parser/connection errors
are redacted, SQL logging is disabled and HTTP storage/provider errors expose
only categories, not credentials, source text or response bodies.

## Run

```sh
# Normal build uses the same ONNX feature as the CLI.
cargo build -p codesearch-server
export CODESEARCH_DATABASE_URL='postgres://ROLE@HOST/DEDICATED_DB'
# Optional remote OpenAI-compatible provider; no saved local defaults are read.
export CODESEARCH_EMBED_PROVIDER=openai
export CODESEARCH_EMBED_BASE_URL=https://your-embedding-endpoint.example/v1
export CODESEARCH_EMBED_API_KEY='...'
export CODESEARCH_EMBED_DIMENSIONS=768
cargo run -p codesearch-server -- --model YOUR_REMOTE_MODEL
```

For local inference omit the remote settings, select e.g. `--model minilm-l6-q`.
Local model artifacts may be downloaded by core; no local index/cache is opened.
For machines needing dynamic ONNX use
`--no-default-features --features load-dynamic` and supply `ORT_DYLIB_PATH` for
local inference. Remote-only execution does not load ONNX. Remote provider
query/document transformations and validation come from core `EmbedConfig`;
queries use `embed_queries`, documents use `embed_documents`.

## HTTP contract

JSON, no MCP/federation compatibility surface:

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Process liveness |
| POST | `/repositories` | Register `{name,description}` (identical replay allowed; differing metadata conflicts) |
| GET | `/repositories?after=NAME&limit=100` | Ordered metadata/revision list; keyset pagination, max 1000 |
| GET | `/repositories/NAME/files?after=PATH&limit=100` | Ordered file sync state including tombstones; keyset pagination, max 1000 |
| GET | `/repositories/NAME/file?path=src%2Fmain.rs` | Exact file state, JSON null if never indexed |
| PUT | `/repositories/NAME/files` | Atomic replacement or deletion |
| POST | `/search` | Explicit scoped keyword/vector/hybrid search |

```sh
curl -fsS http://127.0.0.1:39726/repositories -H 'Content-Type: application/json' \
  -d '{"name":"example","description":"Example source"}'
curl -fsS -X PUT http://127.0.0.1:39726/repositories/example/files \
  -H 'Content-Type: application/json' \
  -d '{"path":"src/main.rs","expected_revision":0,"source_revision":"commit-abc","content":"fn main() { println!(\"hello\"); }"}'
curl -fsS http://127.0.0.1:39726/search -H 'Content-Type: application/json' \
  -d '{"repositories":["example"],"query":"hello","mode":"hybrid","limit":10,"path_prefix":"src/"}'
```

Files accept canonical relative slash-separated paths only (no aliases,
backslashes, drive prefixes, empty/dot/parent components). Maximum source size
1 MiB, 2048 chunks per file, request body 2 MiB. Search requires 1–32 explicit
repository names, 1–100 results and a nonempty query of at most 4096 bytes.
Unknown repositories are errors, never silently omitted. `path_prefix` is a
**literal** prefix applied before ranking/limit; `%` and `_` are not wildcards.
Unknown input fields are rejected.

Repository and file lists default to 100 rows, with `limit` from 1 to 1000.
Start without `after`, then pass the last returned `name` (repositories) or
`path` (files), URL-encoded, as `after` until an empty page. The cursor is
exclusive; pages are ordered by that key. Traversal is not a multi-request
snapshot when registrations/publications occur concurrently.

Notebook publication requires valid JSON with a `cells` array and well-formed
cell type/source fields (string source or array of strings). Malformed input
returns HTTP 400 before inference or replacement; `{"cells":[]}` is an
intentional valid empty publication. Notebook outputs/metadata are not indexed.

`expected_revision=0` means never indexed. Each successful file update increments
its revision, even deletion (`content:null`) or empty-file publication
(`content:""`). The `content` field is required; omitting it returns HTTP 422,
never a deletion. Deleted file tombstones are retained so stale writers cannot
recreate with revision zero (ABA). Read state and retry deliberately after HTTP
409; blindly replaying a request whose response was lost returns 409 if its
commit succeeded. `source_revision` is a bounded caller-supplied source label,
not a verified Git commit or whole-repository consistency claim. File state
includes SHA-256 of submitted content, chunk count, deletion flag and source
revision; repository revision increments with every committed file change.

A server document is prepared from final chunk content plus core context,
signature/name and cleaned extracted documentation, using core embedding
preparation semantics. Documentation already inside source content is not added
a second time. This same document is persisted for BM25 and supplied to the
embedding provider; its SHA-256 is the cache key, not the chunker's pre-header
hash. Provider query/document prefixes remain provider transformations guarded
by the database fingerprint. Returned `content` and inclusive physical editor
line bounds describe the code chunk, not synthetic headers/metadata or preceding
documentation; fragments of one long source line retain the same line number.
A documentation-only hit may therefore return code without the matching comment.
**Notebook exception:** `.ipynb` line bounds are synthetic positions over extracted
cell-content lines, not raw JSON editor coordinates; they are for display/order,
not jump-to-source or source re-extraction. Republication replaces persisted
content and bounds with those computed from the submitted source.

Chunking, validation and all embedding/cache lookups finish before deletion.
A repository row lock then rechecks file CAS in the transaction. Cache entries,
chunks, BM25 mutations, file sync state and repository revision commit together.
Global cache insertions use sorted unique document hashes independently of source
chunk order, so cross-repository publications acquire shared keys consistently.
Provider and pre-commit SQL failures preserve the previous published file state.
Transaction atomicity guarantees complete old or complete new state, never a
partial replacement. A transport failure during COMMIT can leave the outcome
unknown (the database may have committed); read file state before deciding
whether to retry. Concurrent writes to different files in one repository serialize
only their final transactions; same-file stale writers conflict. Deleting a
missing file still requires expected revision zero and creates a tombstone.

Keyword mode uses pg_search `|||` **any-token**, plain bound text, source-code
tokenization and BM25. The search transaction uses `SET LOCAL
plan_cache_mode=force_custom_plan` so repeated prepared queries keep the required
ParadeDB scoring scan. Corpus statistics are database-wide. Vector mode uses
exact pgvector cosine ranking within explicit scope (not approximate ANN, so no
filtered ANN underfill). Hybrid uses reciprocal-rank fusion, constant 60, up to
4×limit bounded candidates per branch, one repeatable-read snapshot. Scores
are mode-specific; there is no universal relevance threshold. Search returns
indexed content, line bounds, repository/path and file/source revisions.
Storage/provider failures remain HTTP 503/502 rather than empty hits. No local
cache or silent keyword fallback on vector failure. PostgreSQL statement/idle
transaction timeouts are 30 seconds; pool maximum is eight connections. Exact
vector scans and serialized provider access prioritize scoped retrieval correctness
over large-corpus throughput. Cache retention/garbage collection and per-user
resource quotas are deferred; this service is not safe for public exposure.

## Isolated behavioral qualification

Never point this at a production/shared database. The ignored integration test
creates and drops a unique database; the role must have `CREATEDB` and extension
privileges. Failures deliberately leave the named disposable database available
for inspection. Use the pinned image above with a private disposable volume and
loopback-only random host port, then:

```sh
export CODESEARCH_TEST_DATABASE_URL='postgres://TEST_ROLE:TEST_PASSWORD@127.0.0.1:TEST_PORT/TEST_DB?sslmode=disable'
cargo test -p codesearch-server --lib -- --include-ignored --nocapture
# Or remote-only dynamic linkage:
cargo test -p codesearch-server --no-default-features --features load-dynamic --lib -- --include-ignored --nocapture
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --lib --bins
# Local dependency isolation: neither command should find sqlx/pgvector.
cargo tree -p codesearch-core --edges normal
cargo tree -p codesearch --edges normal
```

The test runs real HTTP server interfaces and real pg_search/pgvector, with an
explicitly **simulated deterministic OpenAI-compatible HTTP embedding boundary**.
It is not end-to-end inference or semantic-quality evidence. Cases include all
search modes, explicit single/multi-repository and path scopes, document/query
prefix semantics, failed inference preserving old publication, SQL failure after
delete, old complete visibility from another connection during blocked insert,
terminated writer connection rollback, competing CAS, server/provider restart
and DB cache reuse, fingerprint/dimension/prefix mismatch, deletion/empty files,
tombstone ABA, validation and unavailable storage returning an error. Regressions
also cover long-function renames/shared slices, same-document cache reuse,
doc-only keyword/vector input, persisted documentation retrieval after restart,
prior-representation refusal without mutation, and Host/Origin HTTP rejection
before all routes. Additional cases cover invalid-notebook preservation, valid
empty notebooks, physical split ranges, 1001-repository pagination, and a real
PostgreSQL trigger enforcing sorted unique cache inserts during concurrent
reversed-source publication (not a timing-based deadlock reproduction). A physical
PostgreSQL/container crash/restart is an additional external qualification step;
the test simulates interruption by terminating the publishing backend, not by
claiming an in-process restart is a database crash.
