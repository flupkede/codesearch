# codesearch Federation — Azure Cloud Deployment Plan

**Status:** planning · **Scope:** deploy ONE cloud-hosted `codesearch serve` (docs/KB peer) on Azure, fed by blob-stored markdown · **Related:** `docs/federation-feature.md` (the Rust federation feature, Phase 1+2 shipped)

> **Hard constraint that shaped this whole plan:** the operator has **no Owner / User Access Administrator anywhere relevant**, so the design uses **ZERO role assignments** — no managed identity grants. Everything runs on **SAS tokens + inline ACA secrets** the operator can create and rotate alone.

## Verified rights (az, 2026-06)

- **Delaware.SSOT** (`9b8dab06-…`): active roles are only `Cognitive Services Contributor` (sub) + `Key Vault Secrets Officer`/`Administrator` on the `Aprimo` RG vaults (`kv-aprimo-mcp-{dev,qa,prod}`, `kv-aprimo-devops`). **No standing Contributor.**
- **PIM-eligible:** `Contributor` on **resource group `Aprimo`** — *self-activatable* (no colleague approval). Activating it grants resource-create on the Aprimo RG, time-boxed. Contributor still cannot assign roles → MI is out, but this design needs none, and KV Admin already covers all secrets.
- **MSDN sub** ("Visual Studio Professional with MSDN", `c8438481-…`): operator is **Owner** — full rights, no PIM. Use as a **zero-friction sandbox** to build/test Phase 1. Caveats: MSDN monthly credit cap + dev/test licensing → not a long-term prod home.

**Deployment home decision:** build & test Phase 1 on the **MSDN sub** (Owner, no friction); promote to **Delaware.SSOT → RG `Aprimo`** (PIM-activate Contributor per deploy session) as the governed home next to the aprimo KVs and data. Either way: no role assignments, no colleague.

## Why this shape (recap of the decisions)

- **DB is derived, not source-of-truth.** The LMDB index (`.codesearch.db/`) is rebuilt from the source corpus on every cold start. So the cloud container needs **no persistent volume** — ephemeral disk is correct. (And LMDB *cannot* live on Azure Files anyway — memory-mapped → corruption.)
- **Blob = durable source-of-truth** for the scraped docs corpus. Producers write `.md` to blob; the codesearch container materializes blob → local dir and indexes.
- **Producers use the Blob SDK; codesearch consumes via azcopy.** The two roles differ:
  - `aprimo_mcp` + `ia-anthropic-readonly` (*write*) → shared `BlobStorageProvider` (Python `azure-storage-blob`).
  - `codesearch` (*read*) → `azcopy sync` at the acquisition boundary. NOT a native blob backend in the Rust indexer: its incremental engine (`FileMetaStore` mtime/hash change-detection in `src/index/manager.rs`) is built on local files, and azcopy already *is* the blob↔dir delta-sync engine. Reimplementing it inside codesearch buys nothing.
- **No FSW for docs.** Only **full** and **incremental** indexing, both already built into codesearch:
  - *incremental* = `azcopy sync` (only changed blobs land, fresh mtime) → codesearch `refresh` → only changed/deleted files re-embedded.
  - *full* = clear file-meta + DB → index everything. This is also what every cold start does (ephemeral DB). With `min-replicas 1` the container stays warm so incremental between syncs is meaningful.
- **TLS** via ACA ingress (free cert on `*.azurecontainerapps.io`), so **no Caddy** in the image.

## Phasing

### Phase 1 — remote = index-from-blob; scraping runs LOCALLY on the laptop

Producers run on the operator's laptop for now (auth via `az login`), writing `.md` to blob. The cloud side is purely: sync blob → index → serve.

Work items:
1. **Shared `BlobStorageProvider`** (Python) added to `aprimo_mcp` and `ia-anthropic-readonly` — uploads normalized `.md` to a blob container (e.g. `kb`, prefixes `docs/` and `aprimo/`). Auth via `DefaultAzureCredential` locally.
2. **codesearch container** — new `Dockerfile` + `docker/entrypoint.sh`:
   - multi-stage build: `cargo build --release` → **model pre-warm** (bake the fastembed model into the image; loaded by ONNX from a local path, never from blob) → slim runtime with `azcopy` + `git` + the binary
   - entrypoint (`docker/entrypoint.sh`): **restore snapshot** (if any) → `azcopy sync <BLOB_SAS_URL> /data/docs` (+ optional `git pull` of curated KB into `/data/aprimo`) → `codesearch serve` on `0.0.0.0:39725` (registered repos auto-index on start; incremental when a snapshot was restored)
   - background loop: every `REINDEX_INTERVAL_SECS` → `azcopy sync` + POST `/repos/<alias>/reindex` (incremental); every `SNAPSHOT_INTERVAL_SECS` → upload an index snapshot to blob
   - **no Caddy** (ACA does TLS), **no FSW** (full-on-start + incremental-on-timer only)
3. **codesearch code changes (small, shipped):**
   - unauthenticated `/healthz` probe (`/status` sits behind auth on network bind) — stage 1.
   - **cloud keep-warm in serve** — `--keep-warm-url` / `CODESEARCH_KEEP_WARM_URL` + `--idle-suspend-secs` / `CODESEARCH_IDLE_SUSPEND_SECS` (default 7200). Serve self-pings its own ingress `/healthz` while the most-recent real tool call is younger than the idle window, then stops so ACA suspends; the next real query wakes it. This is the **2h-idle-then-suspend** mechanism — self-contained, no Logic App, no managed identity / role assignment.
   - index full/incremental behavior unchanged (already implemented).
4. **ACA app** — **scale-to-zero (min-replicas 0)**, external HTTPS ingress, inline secrets (API key, blob SAS, snapshot SAS). Warm wake via snapshot restore; 2h warm window via serve keep-warm.
5. **Dev wiring** — `remotes` entry in `repos.json` → `@cloud` group, with `timeout_secs: 90` so the client waits through a cold-start wake (~20-45s) instead of falling back to local-only.

### Suspend / wake model (option D)

- **Suspend:** serve keep-warm pings its FQDN while idle < 2h. After 2h with no real query it stops → ACA scales the replica to zero (~5 min cooldown). Idle cost ≈ €0.
- **Wake:** a real federated query hits the ingress → ACA cold-starts a replica → entrypoint restores the blob snapshot (index + embedding cache, *not* the baked model) → serve answers. No mass re-embedding.
- **Cold-start latency:** ~20-45s typical (image pull amortized by node cache; snapshot restore + model load dominate). The dev client's `timeout_secs: 90` absorbs it. First-ever start (no snapshot) = full index.
- **Snapshot safety:** LMDB never runs on network storage; it only travels as an inert tarball in a *separate* `snapshots` blob container, so there is no memory-mapped-FS corruption risk.

### Phase 2 — cloudify scraping

- Separate **Python scraper app** driven by a **JSON sources config**: list of source URLs, each with optional credentials and assigned to one of **two schedules** — **monthly** or **weekly** — depending on source type.
- Runs as an **ACA Job** (cron) → writes to the same blob. The Phase-1 index flow picks it up on the next incremental sync. No change to the index side.

Example sources config (Phase 2):
```jsonc
{
  "sources": [
    { "url": "https://docs.example.com/",        "schedule": "monthly" },
    { "url": "https://internal.portal/api/docs",  "schedule": "weekly",
      "credentials": { "type": "basic", "secretRef": "src-portal-creds" } }
  ]
}
```

## Azure resources (all creatable with Contributor on one RG)

| Resource | Purpose | Role-assignment needed? |
|---|---|---|
| Resource group | scope you own | — (you have Contributor) |
| Storage account + blob container | durable source corpus | none — use account-key **SAS** |
| Container Apps environment | shared host for this + future apps | none |
| ACA app `codesearch-serve` | the serve peer | none |
| Image registry | host the image | **GHCR** (PAT) or **ACR admin-user** — neither needs a role assignment |
| Your Key Vault (existing) | secret source-of-truth / rotation | you have Secrets Officer; values copied inline to ACA |

**Secrets are all inline ACA secrets** (paste-in at `az containerapp create/update`), sourced/rotated from your Key Vault. No MI → KV link (that would need a role assignment).

## az commands (Phase 1 — actual SSOT/Aprimo deployment)

Resources live in **subscription `Delaware.SSOT`, RG `Aprimo`, region `westeurope`**, created under a **PIM-activated Contributor** role (self-activated, 8h, no colleague). Already provisioned: storage `staprmocsfed001`, blob container `docs`, ACA env `cae-aprimo-shared`.

```bash
RG=Aprimo; LOC=westeurope; ST=staprmocsfed001; ENV=cae-aprimo-shared
KEY=$(az storage account keys list -n $ST -g $RG --query "[0].value" -o tsv)

# Snapshot container (separate from the docs source so it is never indexed):
az storage container create --account-name $ST -n snapshots --auth-mode key --account-key "$KEY"

# Read SAS for the docs source; read+write+list SAS for snapshots:
DOCS_SAS=$(az storage container generate-sas --account-name $ST -n docs \
  --permissions rl --expiry 2026-12-31T00:00:00Z --account-key "$KEY" -o tsv)
SNAP_SAS=$(az storage container generate-sas --account-name $ST -n snapshots \
  --permissions rwl --expiry 2026-12-31T00:00:00Z --account-key "$KEY" -o tsv)
API_KEY=$(openssl rand -hex 32)

# Image — GHCR (no ACR rights needed) OR ACR (Contributor on the RG can create it):
#   az acr create -n acraprimocsfed -g $RG --sku Basic --admin-enabled true
#   az acr build -r acraprimocsfed -t codesearch-serve:latest .

FQDN="https://codesearch-serve.<env-hash>.westeurope.azurecontainerapps.io"  # known after first create
az containerapp create -n codesearch-serve -g $RG --environment $ENV \
  --image <registry>/codesearch-serve:latest \
  --ingress external --target-port 39725 --transport http \
  --min-replicas 0 --max-replicas 1 \
  --secrets api-key=$API_KEY docs-sas="$DOCS_SAS" snap-sas="$SNAP_SAS" \
  --env-vars \
     CODESEARCH_SERVE_HOST=0.0.0.0 \
     CODESEARCH_SERVE_PORT=39725 \
     CODESEARCH_SERVE_API_KEY=secretref:api-key \
     BLOB_SAS_URL="https://$ST.blob.core.windows.net/docs?secretref:docs-sas" \
     SNAPSHOT_SAS_URL="https://$ST.blob.core.windows.net/snapshots?secretref:snap-sas" \
     REINDEX_INTERVAL_SECS=900 \
     SNAPSHOT_INTERVAL_SECS=1800 \
     CODESEARCH_KEEP_WARM_URL="$FQDN" \
     CODESEARCH_IDLE_SUSPEND_SECS=7200
# After create, read the real FQDN and `az containerapp update` CODESEARCH_KEEP_WARM_URL to it.
```

> `--min-replicas 0` = scale-to-zero. The keep-warm task holds the replica up for 2h after
> the last real query (`CODESEARCH_IDLE_SUSPEND_SECS=7200`), then lets ACA suspend it.

## Dev wiring (`repos.json`)

```json
{
  "remotes": {
    "cloud": {
      "url": "https://codesearch-serve.<env-hash>.<region>.azurecontainerapps.io",
      "api_key": "<API_KEY>",
      "group": "docs",
      "timeout_secs": 90
    }
  },
  "groups": { "docs": ["@cloud"] }
}
```

`timeout_secs: 90` lets the federated query wait through a scale-to-zero cold-start wake (~20-45s) instead of timing out at the 15s default and returning local-only + a warning.

## Status / to-verify

- [x] `/healthz` unauthenticated probe — shipped (stage 1).
- [x] Cloud keep-warm in serve (2h-idle-then-suspend) — shipped (stage 2).
- [x] Storage `staprmocsfed001` + `docs` container + ACA env `cae-aprimo-shared` — created.
- [ ] `snapshots` container, SAS tokens, image push, ACA app create — stage 3.
- **SAS expiry rotation** — account-key SAS expires; schedule a rotation reminder (or regenerate via pipeline).
- **Snapshot consistency** — the index tarball is taken on a loop tick before reindex (quiescent window); acceptable for Phase 1. A future `codesearch snapshot` using `mdb_env_copy` would make it transactionally clean.
- **Keep-warm self-ping reachability** — confirm the container can reach its own public FQDN through ACA ingress (egress allowed by default).
- **federation coverage** — only `search` + `get_chunk` federate today (`find`/`explore`/`find_impact` deferred, per `federation-feature.md`). Fine for docs/KB.
