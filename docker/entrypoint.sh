#!/usr/bin/env bash
#
# codesearch federation cloud entrypoint — TWO modes (CODESEARCH_RUN_MODE):
#
#   serve      (default) — the long-running Container App. RESTORE-ONLY: pulls the
#              prebuilt index snapshot from blob and serves it read-only. It never
#              registers, full-indexes, reindexes, or snapshots, so it never does
#              heavy (memory-hungry) work and can run on a SMALL replica (1-2 GiB).
#              Fresh content arrives via a new snapshot, picked up on the next cold
#              start (scale-to-zero makes cold starts frequent).
#
#   index-job  — a short-lived Container Apps JOB. Does the HEAVY lifting on a big
#              replica (4-8 GiB): sync the corpus from blob, build/refresh the index
#              (full embed of thousands of docs), upload the resulting snapshot, then
#              EXIT 0. Run it on a schedule (after each harvest) and/or manually.
#
# This split exists because a full index build is memory-heavy (embedding thousands
# of docs at once) while serving/warm-restore is light. Sizing one app for the build
# wastes RAM on every active serving window; the Job pays for big RAM only for the
# few minutes it runs.
#
# Required env (both modes):
#   BLOB_SAS_URL              SAS URL to the docs blob container (synced to /data/docs).
#   SNAPSHOT_SAS_URL          SAS URL (read+write+list) to the snapshot container.
#   CODESEARCH_SERVE_API_KEY  Bearer key (serve binds non-localhost; the job's local
#                             serve also enforces it).
#
# Optional env:
#   CODESEARCH_RUN_MODE       "serve" (default) | "index-job".
#   KB_GIT_URL / GIT_PAT      Curated KB git repo (cloned to /data/aprimo).
#   DATA_DIR                  Working root (default /data).
#   CODESEARCH_SERVE_PORT     Serve port (default 39725).
#   INDEX_JOB_MAX_WAIT_SECS   Max seconds the job waits for indexing to finish
#                             (default 3600).
#
set -euo pipefail

MODE="${CODESEARCH_RUN_MODE:-serve}"
DATA_DIR="${DATA_DIR:-/data}"
PORT="${CODESEARCH_SERVE_PORT:-39725}"
DOCS_DIR="${DATA_DIR}/docs"
KB_DIR="${DATA_DIR}/aprimo"
SNAPSHOT_NAME="codesearch-snapshot.tgz"
SNAPSHOT_LOCAL="/tmp/${SNAPSHOT_NAME}"
CONFIG_DIR="${HOME}/.codesearch"
INDEX_JOB_MAX_WAIT_SECS="${INDEX_JOB_MAX_WAIT_SECS:-3600}"

log() { echo "[entrypoint] $*"; }
die() { echo "[entrypoint] FATAL: $*" >&2; exit 1; }

# --- Validate required configuration (fail fast, no silent fallbacks) --------
[ -n "${BLOB_SAS_URL:-}" ] || die "BLOB_SAS_URL is required"
[ -n "${CODESEARCH_SERVE_API_KEY:-}" ] || die "CODESEARCH_SERVE_API_KEY is required"

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
  # deletions propagate. No --compare-hash=MD5 — that needs a user_xattr the
  # container overlayfs lacks (transfer fails); size+mtime compare needs none.
  azcopy sync "${BLOB_SAS_URL}" "${DOCS_DIR}" \
    --delete-destination=true 2>&1 | sed 's/^/[azcopy] /' || \
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

# --- Snapshot restore / upload -----------------------------------------------
# Restore the index + embedding cache from blob. Source (.md) and the live
# .codesearch.db live under DATA_DIR; the persistent embedding cache + repos.json
# live under CONFIG_DIR. Model weights (*.onnx) are excluded — baked into the
# image. Sets SNAPSHOT_RESTORED=1 on success.
SNAPSHOT_RESTORED=0
restore_snapshot() {
  [ -n "${SNAPSHOT_SAS_URL:-}" ] || { log "no SNAPSHOT_SAS_URL — skipping restore"; return 0; }
  log "restoring snapshot from blob (if present)"
  if azcopy copy "$(snapshot_blob_url)" "${SNAPSHOT_LOCAL}" --overwrite=true 2>&1 | sed 's/^/[azcopy] /'; then
    if [ -f "${SNAPSHOT_LOCAL}" ]; then
      tar xzf "${SNAPSHOT_LOCAL}" -C / 2>&1 | sed 's/^/[snapshot] /' || log "WARN: snapshot extract failed"
      rm -f "${SNAPSHOT_LOCAL}"
      SNAPSHOT_RESTORED=1
      log "snapshot restored"
      return 0
    fi
  fi
  log "no snapshot available"
}

upload_snapshot() {
  [ -n "${SNAPSHOT_SAS_URL:-}" ] || { log "no SNAPSHOT_SAS_URL — skipping upload"; return 0; }
  log "creating index snapshot (excluding model weights)"
  tar czf "${SNAPSHOT_LOCAL}" -C / \
    --exclude='*.onnx' --exclude='*.onnx_data' \
    "${DATA_DIR#/}" "${CONFIG_DIR#/}" 2>/dev/null || { log "WARN: snapshot tar failed"; return 1; }
  azcopy copy "${SNAPSHOT_LOCAL}" "$(snapshot_blob_url)" --overwrite=true 2>&1 | sed 's/^/[azcopy] /' || {
    log "WARN: snapshot upload failed"; rm -f "${SNAPSHOT_LOCAL}"; return 1;
  }
  rm -f "${SNAPSHOT_LOCAL}"
  log "snapshot uploaded"
}

# --- Local serve control (used by index-job) ---------------------------------
api() { curl -fsS -H "Authorization: Bearer ${CODESEARCH_SERVE_API_KEY}" "$@"; }

wait_healthz() {
  local base="http://127.0.0.1:${PORT}"
  local tries="${1:-60}"
  until curl -fsS "${base}/healthz" >/dev/null 2>&1; do
    tries=$((tries - 1))
    [ "${tries}" -le 0 ] && { log "WARN: serve did not become healthy in time"; return 1; }
    sleep 2
  done
}

# (Re)build a repo's index via the PROVEN POST /repos path. If the repo is
# already registered (restored from a prior snapshot), DELETE it first so the
# subsequent POST does a clean full rebuild that picks up added/removed docs.
# We deliberately do NOT use /reindex?force=true here: that endpoint is flaky in
# this deployment (returns 500), whereas DELETE + POST is reliable. The rebuild
# re-embeds, but the restored embedding cache turns unchanged docs into cache
# hits, so only genuinely new/changed docs cost real work — exactly what the Job
# (running on a big replica) is for. Indexing then runs in the background; poll
# /status to know when it finishes.
rebuild_repo() {
  local path="$1" name base="http://127.0.0.1:${PORT}"
  name="$(basename "$path")"
  if api "${base}/status" 2>/dev/null | grep -q "\"alias\":\"${name}\""; then
    log "repo '${name}' already registered — DELETE then rebuild (picks up changes)"
    api -X DELETE "${base}/repos/${name}" >/dev/null 2>&1 \
      || log "WARN: delete ${name} failed (continuing)"
    sleep 2   # let the eviction settle before re-registering
  fi
  log "registering repo '${name}' (${path}) — full index"
  api -X POST "${base}/repos" -H "Content-Type: application/json" \
    -d "{\"path\":\"${path}\"}" >/dev/null 2>&1 \
    && log "registered repo '${name}'" \
    || log "WARN: register ${path} failed"
}

# Block until the requested rebuild has STARTED and then FINISHED (or timeout).
# Two phases avoid two races introduced by the DELETE+POST rebuild:
#   1. After DELETE the repo briefly disappears from /status, and after POST
#      there's a short window before indexing flips the status to "indexing".
#      Phase 1 waits for a real build to be observable (status "indexing", or an
#      already-ready "open"/"warm" for a cache-instant rebuild) so we never
#      mistake the gap for "done".
#   2. Phase 2 then waits for "indexing" to clear.
# The /status repo objects carry a "status" field per alias.
wait_until_indexed() {
  local base="http://127.0.0.1:${PORT}" waited=0 step=10 body started=0
  # Phase 1: confirm a build is observable (bounded — a huge corpus enters
  # "indexing" within seconds; a fully cache-hit rebuild may go straight to ready).
  local start_wait=0
  while [ "${start_wait}" -lt 120 ]; do
    body="$(api "${base}/status" 2>/dev/null || true)"
    if printf '%s' "${body}" | grep -q '"status":"indexing"'; then
      started=1; break
    fi
    if printf '%s' "${body}" | grep -qE '"status":"(open|warm|readonly)"'; then
      log "repo already ready (cache-instant rebuild) after ~${start_wait}s"; return 0
    fi
    sleep 3; start_wait=$((start_wait + 3))
  done
  [ "${started}" -eq 1 ] && log "build started; waiting for completion" \
    || log "WARN: no 'indexing' observed within ${start_wait}s — proceeding cautiously"
  # Phase 2: wait for indexing to clear, requiring the repo to be present + ready.
  while [ "${waited}" -lt "${INDEX_JOB_MAX_WAIT_SECS}" ]; do
    body="$(api "${base}/status" 2>/dev/null || true)"
    if [ -n "${body}" ] \
        && ! printf '%s' "${body}" | grep -q '"status":"indexing"' \
        && printf '%s' "${body}" | grep -qE '"status":"(open|warm|readonly)"'; then
      log "indexing complete after ~${waited}s"
      return 0
    fi
    sleep "${step}"
    waited=$((waited + step))
    [ $((waited % 60)) -eq 0 ] && log "still indexing... (~${waited}s)"
  done
  log "WARN: indexing did not finish within ${INDEX_JOB_MAX_WAIT_SECS}s — snapshotting anyway"
  return 0
}

# =============================================================================
# index-job mode: build/refresh the index on a big replica, snapshot, exit.
# =============================================================================
run_index_job() {
  log "MODE=index-job — heavy build + snapshot, then exit"
  restore_snapshot   # incremental: re-embed only deltas when a prior snapshot exists
  sync_blob
  sync_kb

  # Run serve locally (no ingress needed) just to drive the indexing API.
  codesearch serve --host 127.0.0.1 --port "${PORT}" --no-tui --quiet=false &
  local serve_pid=$!
  trap 'kill "${serve_pid}" 2>/dev/null || true' EXIT

  wait_healthz 90 || { log "serve never came up"; exit 1; }
  rebuild_repo "${DOCS_DIR}"
  [ -d "${KB_DIR}/.git" ] && rebuild_repo "${KB_DIR}"
  wait_until_indexed

  upload_snapshot || die "snapshot upload failed — job is the source of truth, aborting"

  log "index-job done — shutting down local serve"
  kill "${serve_pid}" 2>/dev/null || true
  wait "${serve_pid}" 2>/dev/null || true
  exit 0
}

# =============================================================================
# serve mode (default): restore the prebuilt snapshot and serve read-only.
# No register / no reindex / no snapshot — never does heavy work.
# =============================================================================
run_serve() {
  log "MODE=serve — restore-only, read-only serving"
  restore_snapshot
  # Keep the local .md mirror current for visibility/debugging, but do NOT index
  # here — the index is whatever the snapshot carried. (Cheap file sync only.)
  sync_blob
  sync_kb

  if [ "${SNAPSHOT_RESTORED}" -ne 1 ]; then
    log "WARN: no index snapshot was restored — serving will be EMPTY."
    log "      Run the 'index-job' Container Apps Job first to seed the snapshot."
  fi

  log "starting codesearch serve on 0.0.0.0:${PORT}"
  # Repos come from the restored repos.json; serve loads + serves their existing
  # indexes. Bind 0.0.0.0; the API key enforces auth on this network bind.
  exec codesearch serve \
    --host 0.0.0.0 \
    --port "${PORT}" \
    --no-tui \
    --quiet=false
}

case "${MODE}" in
  index-job) run_index_job ;;
  serve)     run_serve ;;
  *)         die "unknown CODESEARCH_RUN_MODE '${MODE}' (expected 'serve' or 'index-job')" ;;
esac
