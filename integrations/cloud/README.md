# Cloud deployment — running `codesearch serve` as a federation remote peer

This guide describes how to host `codesearch serve` on a cloud container platform as a
**federation remote peer**: an always-on (or scale-to-zero) endpoint that your local
`codesearch` clients fan read queries out to via `@peer` group references.

## Architecture: build / serve split

A cost-effective cloud topology splits the workload into two containers, both built from
the same image but run with different entrypoint modes:

| Component | Shape | Job | Writes the index? |
|---|---|---|---|
| **Indexer job** | heavier (e.g. 4 vCPU / 8 GiB); runs on a schedule or trigger | builds/rebuilds the full embed index from source content and uploads a snapshot to blob storage | yes |
| **Serve replica** | light (e.g. 1 vCPU / 1–2 GiB); long-running; scale-to-zero | restores the latest snapshot and serves `search` / `get_chunk` / management REST | DOCS: no · custom-kb: incremental only |

Why split:

- Building a fresh semantic index is memory-heavy (embedding model + LMDB vector store). You
  only need that capacity during a rebuild, so it runs as a short-lived **job**.
- Serving queries from a restored index is cheap, so the **serve replica** runs on minimal
  resources and can scale to zero when idle.
- The serve replica never **rebuilds the heavy DOCS corpus** and never uploads a snapshot, so
  it cannot corrupt the published snapshot and survives restarts by re-restoring. The one
  exception is the small **custom-KB** repo: after each `git pull` that brings new commits, the
  serve replica runs a cheap **incremental** reindex of just that repo in-process (see
  *Curated KB auto-refresh* below), so new KB articles become searchable without a redeploy.
  Incremental refresh is memory-bounded (`INCREMENTAL_REFRESH_BATCH_SIZE`), so it stays within
  the 1–2 GiB replica; the DOCS corpus full build stays job-only precisely because re-embedding
  thousands of files at once is what a small replica cannot afford.

The two components communicate only via the **snapshot blob** (produced by the indexer,
consumed by the serve replica). There is no shared filesystem or database between them.

## Prerequisites

- A cloud subscription on a container platform that supports (a) scheduled/on-demand
  container **jobs**, (b) long-running container **apps** with ingress and optional
  scale-to-zero, and (c) a blob/object store. This guide uses **Azure Container Apps** as the
  reference platform; the shape maps directly to any equivalent platform.
- `az` CLI logged in, with permission to create a resource group, a Container Apps
  environment, a storage account, and container app/job resources.
- The `codesearch` container image published to a registry the platform can pull from.

## Provisioning (generic)

Replace every `<...>` placeholder with your own values.

1. **Resource group + environment**

   ```bash
   RESOURCE_GROUP="<your-resource-group>"
   LOCATION="<your-region>"            # e.g. westeurope
   ENV_NAME="<your-containerapps-env>"

   az group create --name "$RESOURCE_GROUP" --location "$LOCATION"
   az containerapp env create --name "$ENV_NAME" \
     --resource-group "$RESOURCE_GROUP" --location "$LOCATION"
   ```

2. **Blob storage** (for the index snapshot + source content)

   ```bash
   STORAGE="<your-storage-account>"    # globally unique name
   az storage account create --name "$STORAGE" \
     --resource-group "$RESOURCE_GROUP" --location "$LOCATION" --sku Standard_LRS
   ```

   Create a container (e.g. `codesearch`) and generate a **SAS URL** with read/write/list on
   it. The indexer writes snapshots here; the serve replica reads them.

3. **API key** (the shared secret clients use to authenticate to the serve peer)

   ```bash
   API_KEY="$(openssl rand -hex 32)"   # keep this; you'll configure local clients with it
   ```

4. **Secrets** — register `API_KEY`, the blob `SAS_URL`, and (if you host a curated KB) the
   git credentials as container app secrets. The container's `entrypoint.sh` reads these as
   environment variables — see the image's `docker/entrypoint.sh` for the canonical contract:
   `CODESEARCH_SERVE_API_KEY`, `BLOB_SAS_URL`, `KB_GIT_URL`, `KB_PAT`,
   `KB_POLL_INTERVAL_SECS`, `KB_PULL_INTERVAL_SECS`.

## Deploy the indexer job

The indexer runs the image with the **build entrypoint mode**: it pulls source content,
builds the full embed index, and uploads a snapshot blob.

```bash
az containerapp job create \
  --name "<your-indexer-job>" \
  --resource-group "$RESOURCE_GROUP" \
  --environment "$ENV_NAME" \
  --trigger-type Schedule \
  --cron-expression "0 */6 * * *" \
  --cpu 4.0 --memory 8.0Gi \
  --image "<your-registry>/codesearch:latest" \
  --env-vars CODESEARCH_SERVE_API_KEY=secretref:api-key BLOB_SAS_URL=secretref:sas-url \
  --args "indexer-job"
```

Run it once on demand to produce the first snapshot:

```bash
az containerapp job start --name "<your-indexer-job>" --resource-group "$RESOURCE_GROUP"
```

## Deploy the serve replica

The serve replica runs the image with the **serve entrypoint mode**: it restores the latest
snapshot read-only and serves queries. Bind it to ingress so it has a public FQDN.

```bash
az containerapp create \
  --name "<your-serve-app>" \
  --resource-group "$RESOURCE_GROUP" \
  --environment "$ENV_NAME" \
  --ingress external --target-port 8080 \
  --min-replicas 0 --max-replicas 1 \
  --cpu 1.0 --memory 2.0Gi \
  --image "<your-registry>/codesearch:latest" \
  --env-vars CODESEARCH_SERVE_API_KEY=secretref:api-key BLOB_SAS_URL=secretref:sas-url \
  --args "serve"
```

- `--min-replicas 0` enables **scale-to-zero**: the replica suspends when idle and wakes on
  the next request (cold-start wake is typically ~20–45s).
- The serve app is **restore-first**: it restores the snapshot on cold start and never rebuilds
  the DOCS corpus or uploads a snapshot. Heavy write operations (`index add`, `index reindex
  --force`) against this peer are **rejected** — the DOCS content lifecycle is owned by the
  indexer job + blob sync. The sole write it performs on its own is a memory-bounded incremental
  reindex of the small **custom-KB** repo after each KB `git pull` (see *Curated KB
  auto-refresh*).

Read its FQDN:

```bash
az containerapp show --name "<your-serve-app>" --resource-group "$RESOURCE_GROUP" \
  --query properties.configuration.ingress.fqdn -o tsv
# → <your-serve-app>.<env-hash>.<region>.azurecontainerapps.io
```

## Connect from your laptop

Register the serve peer in your **local** `codesearch` config (pure client-side config;
nothing is stored in the cloud):

```bash
codesearch remote add cloud \
  --url "https://<your-serve-app>.<env-hash>.<region>.azurecontainerapps.io" \
  --api-key "$API_KEY" \
  --timeout-secs 90
```

Use a longer `--timeout-secs` than the default to absorb scale-to-zero cold starts. Reference
it from a group so queries fan out to it:

```jsonc
// ~/.codesearch/repos.json
{
  "groups": {
    "docs": ["@cloud"]
  }
}
```

Now any `search` / `get_chunk` against the `docs` group fans out to the cloud peer over TLS,
merging remote + local results with Reciprocal Rank Fusion (RRF). Remote misses degrade to
local-only with a `warnings` field — they never hard-fail.

## Manage the peer's indexes from your laptop

The `index` verbs take `--remote <peer>` to operate against the peer's management REST API:

```bash
codesearch index list    --remote cloud                       # GET  /status            — always safe (read-only)
codesearch index add     /data/<your-content> --remote cloud  # POST /repos             — needs a READ-WRITE peer
codesearch index rm      <alias> --remote cloud               # DELETE /repos/:alias
codesearch index reindex <alias> [--force] --remote cloud     # POST /repos/:alias/reindex
```

> ⚠️ On the serve replica, `list` is always safe and an **incremental** `reindex` of an
> already-registered repo succeeds (this is exactly the custom-KB auto-refresh mechanism —
> memory-bounded, so it fits the small replica). `add` (register a new repo) and `reindex
> --force` (destructive full rebuild) still require a **read-write** peer and are rejected here.
> DOCS content changes are made by editing what the indexer job consumes, then re-running the
> indexer to publish a fresh snapshot.

## Operational notes

- **Snapshot refresh** — the indexer job publishes a new snapshot on its schedule; the serve
  replica restores it on the next cold start. To force a refresh, re-run the indexer job, then
  restart the serve replica (or let scale-to-zero + the next request pick it up).
- **Curated KB auto-refresh** — if you host a curated knowledge base in a git repo
  (`KB_GIT_URL`), the serve app runs a background loop that **cheaply polls the remote
  `HEAD`** every `KB_POLL_INTERVAL_SECS` (default 30 — `git ls-remote`, ref advertisement
  only, no object transfer) and only does the real `git pull` when the remote SHA moved, so
  a pushed KB edit propagates in ~seconds instead of minutes. `KB_PULL_INTERVAL_SECS`
  (default 900) is kept as a safety-net that forces a full pull at least that often even if a
  poll was missed. When a pull brings **new commits**, it fires an **incremental**
  `POST /repos/custom-kb/reindex` against its own local API (fire-and-forget, HTTP 202) so
  fresh KB articles become searchable without a redeploy. It reindexes only when `HEAD` moved,
  incremental only (never `--force`), and never aborts the loop on error (a `409` means a
  reindex is already running; a `404` means the KB repo isn't in the restored snapshot yet and
  will be picked up once the next index-job snapshot includes it). The heavy DOCS corpus is not
  touched by this loop.
- **Cold starts** — with `--min-replicas 0`, the first request after idle wakes the replica
  (~20–45s). Health probes (`/healthz`) stay green once warm; expect the first query after
  wake to be slower.
- **Snapshot safety** — the serve replica never rebuilds the DOCS index and never uploads a
  snapshot, so the published snapshot is never at risk. The only write it performs is the
  incremental custom-KB reindex, which touches only that replica's own local LMDB copy (rebuilt
  from git on each replica) — so you can still run multiple replicas or restart freely.

## See also

- `README.md` → **Federation (remote peers)** and **Security** sections (trust model, secret
  transport, redirect handling, cross-instance isolation).
- `docker/entrypoint.sh` → the canonical entrypoint modes and environment-variable contract.
