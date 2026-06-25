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
   - multi-stage: `cargo build --release` → runtime image with `azcopy` + `git` + the binary
   - entrypoint: `azcopy sync <blob-sas-url> /data/docs` (+ optional `git pull` of curated KB into `/data/aprimo`) → `codesearch index /data` → start `codesearch serve` on `0.0.0.0:39725`
   - background loop: every `REINDEX_INTERVAL_SECS` → `azcopy sync` + codesearch incremental `refresh`
   - **no Caddy** (ACA does TLS)
3. **codesearch code change (minimal):** add an unauthenticated `/healthz` endpoint for the ACA liveness/readiness probe (`/status` sits behind auth on network bind). Index behavior (full/incremental) needs **no change** — already implemented.
4. **ACA app** — single replica, external HTTPS ingress, inline secrets.
5. **Dev wiring** — `remotes` entry in `repos.json` → `@cloud` group.

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

## az commands (Phase 1 skeleton)

```bash
RG=rg-codesearch; LOC=westeurope; ST=stcodesearchkb; ENV=cae-shared
az group create -n $RG -l $LOC
az storage account create -n $ST -g $RG -l $LOC --sku Standard_LRS
az storage container create --account-name $ST -n kb \
  --auth-mode key --account-key "$(az storage account keys list -n $ST -g $RG --query [0].value -o tsv)"
az containerapp env create -n $ENV -g $RG -l $LOC

# Build/push image — GHCR route (no ACR rights needed):
#   docker build -t ghcr.io/<org>/codesearch-serve:latest . && docker push ...
# OR ACR-admin route:
#   az acr create -n <acr> -g $RG --sku Basic --admin-enabled true
#   az acr build -r <acr> -t codesearch-serve:latest .

# Generate a read SAS for azcopy (account-key SAS — Contributor can list keys):
SAS=$(az storage container generate-sas --account-name $ST -n kb \
  --permissions rl --expiry 2026-12-31T00:00:00Z \
  --account-key "$(az storage account keys list -n $ST -g $RG --query [0].value -o tsv)" -o tsv)
API_KEY=$(openssl rand -hex 32)

az containerapp create -n codesearch-serve -g $RG --environment $ENV \
  --image ghcr.io/<org>/codesearch-serve:latest \
  --ingress external --target-port 39725 --transport http \
  --min-replicas 1 --max-replicas 1 \
  --registry-server ghcr.io --registry-username <gh-user> --registry-password <gh-pat> \
  --secrets api-key=$API_KEY blob-sas="$SAS" \
  --env-vars \
     CODESEARCH_SERVE_HOST=0.0.0.0 \
     CODESEARCH_SERVE_PORT=39725 \
     CODESEARCH_SERVE_API_KEY=secretref:api-key \
     BLOB_SAS_URL="https://$ST.blob.core.windows.net/kb?secretref:blob-sas" \
     REINDEX_INTERVAL_SECS=900
```

## Dev wiring (`repos.json`)

```json
{
  "remotes": {
    "cloud": {
      "url": "https://codesearch-serve.<env-hash>.<region>.azurecontainerapps.io",
      "api_key": "<API_KEY>",
      "group": "docs"
    }
  },
  "groups": { "docs": ["@cloud"] }
}
```

## Open / to-verify

- **`/healthz`** endpoint must be added to the serve router (unauthenticated) before the ACA probe works.
- **SAS expiry rotation** — account-key SAS expires; schedule a rotation reminder (or regenerate via pipeline).
- **Cold-start time** — first request after scale-to-zero = full index. `min-replicas 1` keeps it warm; weigh cost vs latency.
- **federation coverage** — only `search` + `get_chunk` federate today (`find`/`explore`/`find_impact` deferred, per `federation-feature.md`). Fine for docs/KB.
