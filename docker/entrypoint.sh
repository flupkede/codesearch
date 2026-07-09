#!/usr/bin/env bash
#
# codesearch federation cloud entrypoint — TWO modes (CODESEARCH_RUN_MODE):
#
#   serve      (default) — the long-running Container App. RESTORE-FIRST: pulls the
#              prebuilt index snapshot from blob and serves it. It never registers
#              or full-indexes the heavy DOCS corpus, and never snapshots, so it
#              stays light and runs on a SMALL replica (1-2 GiB). The ONE exception
#              is the small custom-KB repo: on each git pull (KB_PULL_INTERVAL_SECS)
#              serve runs a cheap INCREMENTAL reindex of just the changed KB files so
#              new articles become searchable WITHOUT a restart. Fresh DOCS content
#              still arrives only via a new snapshot from the index-job, picked up on
#              the next cold start (scale-to-zero makes cold starts frequent).
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
#   KB_GIT_URL / GIT_PAT      Curated KB git repo (cloned to /data/custom-kb).
#   KB_POLL_INTERVAL_SECS     serve mode: how often to CHEAPLY poll the KB remote
#                             HEAD (git ls-remote — ref advertisement only, no
#                             objects). On a change, pull + incremental reindex fire
#                             immediately, so a pushed KB edit becomes searchable in
#                             ~this many seconds instead of the full pull interval
#                             (default 30).
#   KB_PULL_INTERVAL_SECS     serve mode: safety-net cadence — force a full git pull
#                             (+ reindex-on-change) even when the cheap poll saw no
#                             change or failed, self-healing a missed ls-remote
#                             (default 900).
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
KB_DIR="${DATA_DIR}/custom-kb"
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

# Build the azcopy --exclude-path list that protects every codesearch index dir
# living inside ${DOCS_DIR} from --delete-destination.
#
# --exclude-path matches by RELATIVE-PATH PREFIX (no wildcards), so a single
# ".codesearch.db" only covers a root-level index (the legacy MONOLITHIC layout,
# ${DOCS_DIR}/.codesearch.db). In the PER-VENDOR layout each index lives one level
# down (${DOCS_DIR}/<vendor>/.codesearch.db), so every one needs its own prefix
# entry — otherwise --delete-destination would treat the restored vendor indexes
# as "extra" and DELETE them (the blob holds only source .md, never the index).
# We enumerate the vendor subdirs present locally (post-restore) and emit one
# "<vendor>/.codesearch.db" prefix each, keeping the bare root entry for the
# legacy layout. Semicolon-separated, as azcopy expects.
#
# DATA-SAFETY COUPLING: the ".codesearch.db" literal below MUST match the Rust
# DB_DIR_NAME constant (src/constants.rs). If that constant is ever renamed and
# this is not, the exclusion stops matching and --delete-destination wipes every
# index. Keep the two in lockstep.
docs_index_exclusions() {
  local excl=".codesearch.db" d
  for d in "${DOCS_DIR}"/*/; do
    [ -d "${d}" ] || continue                       # no subdirs → glob stays literal
    excl="${excl};$(basename "${d%/}")/.codesearch.db"
  done
  printf '%s' "${excl}"
}

sync_blob() {
  local exclusions
  exclusions="$(docs_index_exclusions)"
  log "azcopy sync blob -> ${DOCS_DIR} (protecting: ${exclusions})"
  # --delete-destination keeps the local mirror in lock-step with the blob so
  # deletions propagate. No --compare-hash=MD5 — that needs a user_xattr the
  # container overlayfs lacks (transfer fails); size+mtime compare needs none.
  #
  # CRITICAL: the exclusion list (see docs_index_exclusions) keeps every
  # codesearch index dir under ${DOCS_DIR} from being deleted — the index is
  # owned by the snapshot/indexer and must never be clobbered by the corpus sync
  # (this also protects the serve app's restored indexes on cold start).
  azcopy sync "${BLOB_SAS_URL}" "${DOCS_DIR}" \
    --delete-destination=true \
    --exclude-path="${exclusions}" 2>&1 | sed 's/^/[azcopy] /' || \
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

# serve mode: after a KB git pull brings new commits, ask the LOCAL serve to
# incrementally re-embed the custom-kb repo so new/changed articles become
# searchable WITHOUT a restart. Incremental only (no ?force): re-embeds just the
# added/changed/removed files — cheap enough for the 1-2 GiB serve replica (the
# KB corpus is small). Fire-and-forget: POST /repos/<alias>/reindex returns 202
# and runs in the background. Never aborts the pull loop on error (logs a WARN
# and retries next cycle). A 409 means a reindex is already running (e.g. a lazy
# FSW pickup of the same pull) — expected and harmless.
reindex_kb() {
  local name base="http://127.0.0.1:${PORT}" resp code
  name="$(basename "${KB_DIR}")"
  resp="$(api_code -X POST "${base}/repos/${name}/reindex" || true)"
  code="${resp##*$'\n'}"          # last line = HTTP status
  case "${code}" in
    200|201|202) log "incremental reindex accepted for '${name}' (HTTP ${code})" ;;
    409) log "reindex already in progress for '${name}' (HTTP 409) — skipping" ;;
    404) log "'${name}' not yet registered on serve — awaiting a snapshot that includes it (HTTP 404, expected during bootstrap)" ;;
    *) log "WARN: reindex request for '${name}' failed — HTTP ${code:-<none>}: ${resp%$'\n'*}" ;;
  esac
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
      # Drop stale lock files a prior snapshot may have captured (the original
      # serve/indexer was killed while holding write locks, so .writer.lock /
      # lock.mdb / tantivy locks can be baked in). A fresh container has no other
      # process, so any lock here is stale; leaving them makes serve report the
      # repo "locked by another codesearch process". LMDB recreates lock.mdb on
      # open; the app-level *.lock files are pure stale-guards.
      find "${DATA_DIR}" \( -name '.writer.lock' -o -name '.tantivy-writer.lock' \
        -o -name '.tantivy-meta.lock' -o -name 'lock.mdb' \) -delete 2>/dev/null || true
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
  # Exclude model weights (baked into the image) and lock files (never valid to
  # carry across containers — see restore_snapshot for why).
  tar czf "${SNAPSHOT_LOCAL}" -C / \
    --exclude='*.onnx' --exclude='*.onnx_data' \
    --exclude='*.lock' --exclude='lock.mdb' \
    "${DATA_DIR#/}" "${CONFIG_DIR#/}" 2>/dev/null || { log "WARN: snapshot tar failed"; return 1; }
  azcopy copy "${SNAPSHOT_LOCAL}" "$(snapshot_blob_url)" --overwrite=true 2>&1 | sed 's/^/[azcopy] /' || {
    log "WARN: snapshot upload failed"; rm -f "${SNAPSHOT_LOCAL}"; return 1;
  }
  rm -f "${SNAPSHOT_LOCAL}"
  log "snapshot uploaded"
}

# --- Local serve control (used by index-job) ---------------------------------
api() { curl -fsS -H "Authorization: Bearer ${CODESEARCH_SERVE_API_KEY}" "$@"; }

# Like api() but never aborts the script on HTTP >= 400: emits the response body
# followed by a final line containing the HTTP status code. Callers split off the
# trailing line to branch on the code (and surface the body on failure) instead of
# silently swallowing errors with `>/dev/null 2>&1` (which hid the old 500s).
api_code() {
  curl -sS -H "Authorization: Bearer ${CODESEARCH_SERVE_API_KEY}" -w $'\n%{http_code}' "$@"
}

wait_healthz() {
  local base="http://127.0.0.1:${PORT}"
  local tries="${1:-60}"
  until curl -fsS "${base}/healthz" >/dev/null 2>&1; do
    tries=$((tries - 1))
    [ "${tries}" -le 0 ] && { log "WARN: serve did not become healthy in time"; return 1; }
    sleep 2
  done
}

# Make sure a repo's index is built/refreshed; wait_until_indexed() then blocks for
# completion. Two cases:
#
#   - ALREADY REGISTERED (index restored from a prior snapshot): do NOT issue any
#     reindex here. serve's Phase-1 STARTUP WARMUP already opens the repo in write
#     mode and runs an incremental refresh (re-embedding only added/changed/removed
#     docs) the moment serve starts — and it holds the LMDB write lock for the whole
#     refresh. A competing POST /repos/<alias>/reindex opens a SECOND write handle on
#     the same LMDB env and fails with HTTP 500 "locked by another codesearch
#     process" (observed). So we let the warmup own the refresh and simply wait for
#     the repo to reach a ready ("warm") state. During warmup /status reports the
#     repo as "closed"; it flips to "warm" only after the refresh completes, which is
#     exactly the signal wait_until_indexed() blocks on. /reindex?force=true is also
#     unused (returns 500 in this deployment).
#
#   - NOT YET REGISTERED (first-ever cold build, no snapshot existed): POST /repos
#     {path} to build the index from scratch (202; background; shows "indexing").
#     A hard failure to kick this off ABORTS the job (die) so we never go on to
#     upload a broken/empty snapshot over a good one.
rebuild_repo() {
  local path="$1" name base="http://127.0.0.1:${PORT}" resp code
  name="$(basename "$path")"
  if api "${base}/status" 2>/dev/null | grep -q "\"alias\":\"${name}\""; then
    log "repo '${name}' already registered — serve startup warmup is incrementally \
refreshing it; waiting for warmup to finish (no competing reindex)"
    return 0
  fi
  log "repo '${name}' not registered — full index build of ${path}"
  resp="$(api_code -X POST "${base}/repos" -H "Content-Type: application/json" \
    -d "{\"path\":\"${path}\"}" || true)"
  code="${resp##*$'\n'}"          # last line = HTTP status
  case "${code}" in
    200|201|202) log "build accepted for '${name}' (HTTP ${code})" ;;
    409) log "build already in progress for '${name}' (HTTP 409) — will wait for it" ;;
    *) die "build request for '${name}' failed — HTTP ${code:-<none>}: ${resp%$'\n'*}" ;;
  esac
}

# Hard pre-upload guard: confirm the repo actually has a populated index before we
# snapshot it. GET /repos/<alias>/info reports {"chunks":N,...}. chunks < 1 means the
# index is empty/broken — refuse to upload so we never clobber a known-good snapshot.
verify_index_ready() {
  local name="$1" base="http://127.0.0.1:${PORT}" info chunks
  info="$(api "${base}/repos/${name}/info" 2>/dev/null || true)"
  chunks="$(printf '%s' "${info}" | sed -n 's/.*"chunks":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n1)"
  if [ -z "${chunks}" ] || [ "${chunks}" -lt 1 ] 2>/dev/null; then
    log "verify: repo '${name}' reports chunks=${chunks:-<none>} — index looks EMPTY"
    return 1
  fi
  log "verify: repo '${name}' OK — ${chunks} chunks indexed"
}

# Block until the requested rebuild has STARTED and then FINISHED (or timeout).
# /reindex (and /repos) return 202 immediately and run in the background, so two
# phases close the start-up race:
#   1. After the 202 there's a short window before the background task flips the
#      repo to "indexing". Phase 1 waits for a real build to be observable (status
#      "indexing", or an already-ready "open"/"warm" when an incremental reindex
#      finds no deltas and completes instantly) so we never mistake the gap for
#      "done".
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
# Sequential-safe build wait: block until the single in-flight build finishes.
# The index-job builds ONE vendor at a time, so a "indexing" status anywhere in
# /status can only be that one build — no per-alias parsing needed. (This is why
# we don't reuse wait_until_indexed here: its start-detection short-circuits as
# "already ready" the moment ANY earlier vendor is open, which would let the next
# build be submitted before the current one finishes — reintroducing the parallel
# builds that OOM-killed serve.)
wait_active_build_done() {
  local base="http://127.0.0.1:${PORT}" waited=0 body
  sleep 5   # let the 202 flip the repo into "indexing" before we start checking
  while [ "${waited}" -lt "${INDEX_JOB_MAX_WAIT_SECS}" ]; do
    body="$(api "${base}/status" 2>/dev/null || true)"
    if ! printf '%s' "${body}" | grep -q '"status":"indexing"'; then
      log "build settled after ~$((waited + 5))s"; return 0
    fi
    sleep 10; waited=$((waited + 10))
  done
  log "WARN: build still 'indexing' after ${waited}s — proceeding to verify"
  return 0
}

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

  # PER-VENDOR SPLIT: build/refresh one index per immediate subfolder of
  # ${DOCS_DIR} (akeneo, bynder, …) instead of a single monolithic "docs" repo.
  # Smaller per-vendor indexes rebuild faster, use less peak memory, warm up
  # quicker on the serve side, and rank fairly (a small vendor is no longer
  # drowned by a large one). Each vendor is registered under its folder name and
  # is queryable as its own project / mounted remotely as <peer>/<vendor>.
  # Build STRICTLY ONE AT A TIME: submit → wait for THIS build to finish →
  # verify → next. Submitting all vendors at once made serve hold every vendor's
  # embedding model + working set simultaneously and get OOM-killed (SIGKILL) on
  # the job memory limit. Sequential build caps peak memory to a single index.
  # verify_index_ready runs inline (empty/broken vendor aborts before upload, so
  # one bad build can never clobber the good snapshot).
  local vendor found=0 vname
  for vendor in "${DOCS_DIR}"/*/; do
    [ -d "${vendor}" ] || continue     # empty ${DOCS_DIR} → glob stays literal
    vname="$(basename "${vendor%/}")"
    rebuild_repo "${vendor%/}"         # strip trailing slash so basename is clean
    wait_active_build_done             # block until this single build completes
    verify_index_ready "${vname}" \
      || die "index verification failed for '${vname}' (empty/broken) — refusing to upload over the good snapshot"
    found=1
  done
  [ "${found}" -eq 1 ] \
    || die "no vendor subfolders under ${DOCS_DIR} — nothing to index (expected ${DOCS_DIR}/<vendor>/…)"
  if [ -d "${KB_DIR}/.git" ]; then
    rebuild_repo "${KB_DIR}"
    wait_active_build_done
    verify_index_ready "$(basename "${KB_DIR}")" \
      || die "index verification failed for custom-kb (empty/broken) — refusing to upload over the good snapshot"
  fi
  upload_snapshot || die "snapshot upload failed — job is the source of truth, aborting"

  log "index-job done — shutting down local serve"
  kill "${serve_pid}" 2>/dev/null || true
  wait "${serve_pid}" 2>/dev/null || true
  exit 0
}

# =============================================================================
# serve mode (default): restore the prebuilt snapshot and serve it. Never builds
# the heavy DOCS corpus and never snapshots. The only write work is a cheap
# INCREMENTAL reindex of the small custom-kb repo whenever a KB git pull brings
# new commits.
# =============================================================================
run_serve() {
  log "MODE=serve — restore-first serving (docs read-only; custom-kb incrementally refreshed)"
  restore_snapshot
  # Keep the local .md mirror current for visibility/debugging, but do NOT index
  # the DOCS corpus here — that index is whatever the snapshot carried. (Cheap
  # file sync only.) The custom-kb git clone below IS incrementally reindexed.
  sync_blob
  sync_kb

  if [ "${SNAPSHOT_RESTORED}" -ne 1 ]; then
    log "WARN: no index snapshot was restored — serving will be EMPTY."
    log "      Run the 'index-job' Container Apps Job first to seed the snapshot."
  fi

  # Background: keep the custom-KB git clone fresh AND, when a pull brings new
  # commits, ask the local serve to incrementally reindex it so new/changed KB
  # articles become searchable WITHOUT a container restart. Cheap — the KB repo
  # is small (only the custom/ corpus) and incremental refresh re-embeds only the
  # delta, so it fits the 1-2 GiB serve replica. The heavy DOCS corpus stays
  # job-only. Only runs when KB_GIT_URL is set. The first pull fires after the
  # interval, long after Phase-1 startup warmup has released the KB write lock,
  # so there is no contention with warmup.
  if [ -n "${KB_GIT_URL:-}" ]; then
    # Near-instant propagation: cheaply poll the remote HEAD every
    # KB_POLL_INTERVAL_SECS (git ls-remote = ref advertisement only, no objects),
    # and only do the expensive pull + reindex when the remote SHA actually moved.
    # KB_PULL_INTERVAL_SECS is kept as a safety-net: force a full pull at least that
    # often even if the cheap poll saw nothing (self-heals a failed/missed ls-remote).
    # ls-remote uses the stored 'origin' remote so the PAT never lands on argv.
    KB_POLL_INTERVAL_SECS="${KB_POLL_INTERVAL_SECS:-30}"
    KB_PULL_INTERVAL_SECS="${KB_PULL_INTERVAL_SECS:-900}"
    ( kb_branch="$(git -C "${KB_DIR}" rev-parse --abbrev-ref HEAD 2>/dev/null || echo HEAD)"
      secs_since_pull=0
      while sleep "${KB_POLL_INTERVAL_SECS}"; do
        secs_since_pull=$(( secs_since_pull + KB_POLL_INTERVAL_SECS ))
        remote_sha="$(git -C "${KB_DIR}" ls-remote origin "${kb_branch}" 2>/dev/null | awk 'NR==1{print $1}')"
        local_sha="$(git -C "${KB_DIR}" rev-parse HEAD 2>/dev/null || true)"
        force_pull=0
        [ "${secs_since_pull}" -ge "${KB_PULL_INTERVAL_SECS}" ] && force_pull=1
        if { [ -n "${remote_sha}" ] && [ "${remote_sha}" != "${local_sha}" ]; } || [ "${force_pull}" -eq 1 ]; then
          before="${local_sha}"
          sync_kb
          secs_since_pull=0
          after="$(git -C "${KB_DIR}" rev-parse HEAD 2>/dev/null || true)"
          if [ -n "${after}" ] && [ "${before}" != "${after}" ]; then
            log "custom-kb changed (${before:-<none>} -> ${after}) — triggering incremental reindex"
            reindex_kb
          fi
        fi
      done ) &
    log "KB auto-pull loop started (remote-HEAD poll every ${KB_POLL_INTERVAL_SECS}s; forced full pull every ${KB_PULL_INTERVAL_SECS}s; reindex-on-change -> ${KB_DIR})"
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
