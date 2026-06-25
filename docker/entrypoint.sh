#!/usr/bin/env bash
#
# codesearch federation cloud entrypoint (ACA scale-to-zero + blob snapshot).
#
# Materializes the source corpus from Azure Blob Storage (and, optionally, a
# curated KB git repo) into a LOCAL ephemeral directory, then runs
# `codesearch serve` over it.
#
# Because Azure Container Apps scale-to-zero DESTROYS the replica (the local
# LMDB index on ephemeral disk is lost), we persist a SNAPSHOT of the index +
# embedding cache to a separate blob container. On a cold start we restore that
# snapshot so the container comes up WARM (no mass re-embedding) — only a delta
# sync + incremental reindex runs. LMDB never lives on network storage; it only
# travels as an inert tarball, so there is no memory-mapped-FS corruption risk.
#
# Required env:
#   BLOB_SAS_URL              SAS URL to the docs blob container (synced to /data/docs).
#   CODESEARCH_SERVE_API_KEY  Bearer key; mandatory because we bind non-localhost.
#
# Optional env:
#   SNAPSHOT_SAS_URL          SAS URL (read+write+list) to the snapshot container.
#                             When set, enables warm-restore + periodic snapshot.
#   KB_GIT_URL                Git URL of the curated KB repo (cloned to /data/aprimo).
#   GIT_PAT                   PAT injected into KB_GIT_URL for private repos.
#   REINDEX_INTERVAL_SECS     Incremental reindex cadence (default 900 = 15 min).
#   SNAPSHOT_INTERVAL_SECS    Snapshot-upload cadence (default 1800 = 30 min).
#   DATA_DIR                  Working root for synced source (default /data).
#   CODESEARCH_SERVE_PORT     Serve port (default 39725).
#
set -euo pipefail

DATA_DIR="${DATA_DIR:-/data}"
PORT="${CODESEARCH_SERVE_PORT:-39725}"
REINDEX_INTERVAL_SECS="${REINDEX_INTERVAL_SECS:-900}"
SNAPSHOT_INTERVAL_SECS="${SNAPSHOT_INTERVAL_SECS:-1800}"
DOCS_DIR="${DATA_DIR}/docs"
KB_DIR="${DATA_DIR}/aprimo"
SNAPSHOT_NAME="codesearch-snapshot.tgz"
SNAPSHOT_LOCAL="/tmp/${SNAPSHOT_NAME}"
CONFIG_DIR="${HOME}/.codesearch"

log() { echo "[entrypoint] $*"; }
die() { echo "[entrypoint] FATAL: $*" >&2; exit 1; }

# --- Validate required configuration (fail fast, no silent fallbacks) --------
[ -n "${BLOB_SAS_URL:-}" ] || die "BLOB_SAS_URL is required"
[ -n "${CODESEARCH_SERVE_API_KEY:-}" ] || die "CODESEARCH_SERVE_API_KEY is required (non-localhost bind)"

mkdir -p "${DOCS_DIR}" "${CONFIG_DIR}"

# Splice a blob name into a container SAS URL: "<base>/<name>?<sas>".
snapshot_blob_url() {
  local base="${SNAPSHOT_SAS_URL%%\?*}"   # strip ?sas
  local sas="${SNAPSHOT_SAS_URL#*\?}"      # keep sas
  printf '%s/%s?%s' "${base%/}" "${SNAPSHOT_NAME}" "${sas}"
}

# --- Source acquisition helpers ----------------------------------------------
sync_blob() {
  log "azcopy sync blob -> ${DOCS_DIR}"
  # --delete-destination keeps the local mirror in lock-step with the blob so
  # deletions propagate and codesearch's incremental pass can drop them.
  azcopy sync "${BLOB_SAS_URL}" "${DOCS_DIR}" \
    --delete-destination=true --compare-hash=MD5 2>&1 | sed 's/^/[azcopy] /' || \
    log "WARN: azcopy sync failed (continuing with existing local copy)"
}

sync_kb() {
  [ -n "${KB_GIT_URL:-}" ] || return 0
  local url="${KB_GIT_URL}"
  if [ -n "${GIT_PAT:-}" ]; then
    url="$(printf '%s' "${KB_GIT_URL}" | sed -E "s#^https://#https://${GIT_PAT}@#")"
  fi
  if [ -d "${KB_DIR}/.git" ]; then
    log "git pull KB -> ${KB_DIR}"
    git -C "${KB_DIR}" pull --ff-only 2>&1 | sed 's/^/[git] /' || log "WARN: git pull failed"
  else
    log "git clone KB -> ${KB_DIR}"
    git clone --depth 1 "${url}" "${KB_DIR}" 2>&1 | sed 's/^/[git] /' || log "WARN: git clone failed"
  fi
}

# --- Snapshot restore / upload (warm wake without re-embedding) --------------
# Restore the index + embedding cache from blob so serve starts WARM. Source
# (.md) and the live .codesearch.db live under DATA_DIR; the persistent
# embedding cache lives under CONFIG_DIR. Model weights (*.onnx) are excluded —
# they are baked into the image, so we never round-trip ~90 MB.
restore_snapshot() {
  [ -n "${SNAPSHOT_SAS_URL:-}" ] || return 0
  log "restoring snapshot from blob (if present)"
  if azcopy copy "$(snapshot_blob_url)" "${SNAPSHOT_LOCAL}" --overwrite=true 2>&1 | sed 's/^/[azcopy] /'; then
    if [ -f "${SNAPSHOT_LOCAL}" ]; then
      tar xzf "${SNAPSHOT_LOCAL}" -C / 2>&1 | sed 's/^/[snapshot] /' || log "WARN: snapshot extract failed"
      rm -f "${SNAPSHOT_LOCAL}"
      log "snapshot restored — wake will be warm"
      return 0
    fi
  fi
  log "no snapshot available — first index will be a full build"
}

upload_snapshot() {
  [ -n "${SNAPSHOT_SAS_URL:-}" ] || return 0
  log "creating index snapshot (excluding model weights)"
  # Tar relative to / so absolute paths restore cleanly. Exclude the baked
  # ONNX model; keep the index DB(s) and the embedding cache.
  tar czf "${SNAPSHOT_LOCAL}" -C / \
    --exclude='*.onnx' --exclude='*.onnx_data' \
    "${DATA_DIR#/}" "${CONFIG_DIR#/}" 2>/dev/null || { log "WARN: snapshot tar failed"; return 0; }
  azcopy copy "${SNAPSHOT_LOCAL}" "$(snapshot_blob_url)" --overwrite=true 2>&1 | sed 's/^/[azcopy] /' || \
    log "WARN: snapshot upload failed"
  rm -f "${SNAPSHOT_LOCAL}"
}

# --- Background loop: incremental reindex + periodic snapshot -----------------
# Waits for the local serve to answer /healthz, then ticks every reindex
# interval. Alias == the registered directory's name (codesearch convention),
# so /data/docs -> "docs". A snapshot is taken on tick boundaries that cross the
# snapshot interval, BEFORE the reindex writes, when the DB is most quiescent.
background_loop() {
  local base="http://127.0.0.1:${PORT}"
  until curl -fsS "${base}/healthz" >/dev/null 2>&1; do sleep 2; done
  log "serve is live; reindex every ${REINDEX_INTERVAL_SECS}s, snapshot every ${SNAPSHOT_INTERVAL_SECS}s"
  local since_snapshot=0
  while true; do
    sleep "${REINDEX_INTERVAL_SECS}"
    since_snapshot=$((since_snapshot + REINDEX_INTERVAL_SECS))
    if [ -n "${SNAPSHOT_SAS_URL:-}" ] && [ "${since_snapshot}" -ge "${SNAPSHOT_INTERVAL_SECS}" ]; then
      upload_snapshot
      since_snapshot=0
    fi
    sync_blob
    sync_kb
    for alias in docs $( [ -d "${KB_DIR}/.git" ] && echo aprimo ); do
      log "incremental reindex: ${alias}"
      curl -fsS -X POST "${base}/repos/${alias}/reindex" \
        -H "Authorization: Bearer ${CODESEARCH_SERVE_API_KEY}" \
        >/dev/null 2>&1 || log "WARN: reindex ${alias} failed"
    done
  done
}

# --- Cold start: restore -> sync -> serve -------------------------------------
restore_snapshot
sync_blob
sync_kb

REGISTER_ARGS=(--register "${DOCS_DIR}")
if [ -d "${KB_DIR}/.git" ]; then
  REGISTER_ARGS+=(--register "${KB_DIR}")
fi

background_loop &

log "starting codesearch serve on 0.0.0.0:${PORT}"
# create_index defaults true -> registered repos are indexed on startup. With a
# restored snapshot this is an INCREMENTAL pass (DB already present); without
# one it is a full build. Bind 0.0.0.0; CODESEARCH_SERVE_API_KEY enforces auth.
exec codesearch serve \
  --host 0.0.0.0 \
  --port "${PORT}" \
  "${REGISTER_ARGS[@]}" \
  --no-tui \
  --quiet=false
