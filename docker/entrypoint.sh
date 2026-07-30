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
#   INDEX_JOB_REPO_READY_SECS index-job mode: max seconds to wait for ONE repo to
#                             reach warm/open (default 600). Per repo, not per job:
#                             the batch worst case must stay inside the platform's
#                             job replicaTimeout. Exceeding it ABORTS the job — a
#                             mid-warmup index must never be tarred over the good
#                             snapshot. Raise it if a corpus legitimately needs longer.
#   SERVE_STOP_GRACE_SECS     index-job mode: seconds to wait for the local serve to
#                             honour SIGTERM before SIGKILL, before the snapshot tar
#                             (default 30).
#
set -euo pipefail

MODE="${CODESEARCH_RUN_MODE:-serve}"
DATA_DIR="${DATA_DIR:-/data}"
PORT="${CODESEARCH_SERVE_PORT:-39725}"
# Single source of truth for the loopback management API the index-job drives.
API_BASE="http://127.0.0.1:${PORT}"
DOCS_DIR="${DATA_DIR}/docs"
KB_DIR="${DATA_DIR}/custom-kb"
SNAPSHOT_NAME="codesearch-snapshot.tgz"
SNAPSHOT_LOCAL="/tmp/${SNAPSHOT_NAME}"
CONFIG_DIR="${HOME}/.codesearch"
# Per-repo readiness budget for the index-job — see wait_repo_ready for why this
# is per repo and why it must stay well under the platform job timeout.
INDEX_JOB_REPO_READY_SECS="${INDEX_JOB_REPO_READY_SECS:-600}"
# How long the index-job waits for serve to honour SIGTERM before SIGKILL, so a
# hung serve cannot burn the whole replicaTimeout right before the snapshot.
SERVE_STOP_GRACE_SECS="${SERVE_STOP_GRACE_SECS:-30}"

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
  local name base="${API_BASE}" resp code
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
  #
  # tar exits 1 ("Some files differ" / "file changed as we read it") when ANY
  # tracked file is touched mid-archive — common when snapshotting an index the
  # live serve process is still writing to, and BENIGN for a point-in-time
  # restore snapshot (LMDB readers are MVCC; a momentarily-shifted data.mdb
  # still restores). Only a FATAL tar error (exit >= 2, e.g. disk-full/ENOSPC)
  # should abort the upload. Capture stderr to a side file so a real failure is
  # diagnosable instead of silently swallowed by /dev/null.
  local tar_err=0
  tar czf "${SNAPSHOT_LOCAL}" -C / \
    --exclude='*.onnx' --exclude='*.onnx_data' \
    --exclude='*.lock' --exclude='lock.mdb' \
    "${DATA_DIR#/}" "${CONFIG_DIR#/}" 2>"${SNAPSHOT_LOCAL}.tarerr" || tar_err=$?
  if [ "${tar_err}" -ge 2 ]; then
    log "WARN: snapshot tar failed (exit ${tar_err}): $(tr '\n' ' ' < "${SNAPSHOT_LOCAL}.tarerr" 2>/dev/null)"
    rm -f "${SNAPSHOT_LOCAL}" "${SNAPSHOT_LOCAL}.tarerr"
    return 1
  fi
  [ "${tar_err}" -eq 1 ] && log "note: tar reported file-changed (exit 1) — benign for a live snapshot, proceeding"
  rm -f "${SNAPSHOT_LOCAL}.tarerr"
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
  local base="${API_BASE}"
  local tries="${1:-60}"
  until curl -fsS "${base}/healthz" >/dev/null 2>&1; do
    tries=$((tries - 1))
    [ "${tries}" -le 0 ] && { log "WARN: serve did not become healthy in time"; return 1; }
    sleep 2
  done
}

# Make sure a repo's index is built/refreshed; wait_repo_ready() then blocks
# for completion. Two cases:
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
#     exactly the signal wait_repo_ready() blocks on — note that warmup never reports
#     "indexing", so the alias-specific status check is the ONLY signal here.
#     /reindex?force=true is also unused (returns 500 in this deployment).
#
#   - NOT YET REGISTERED (first-ever cold build, no snapshot existed): POST /repos
#     {path} to build the index from scratch (202; background; shows "indexing").
#     A hard failure to kick this off ABORTS the job (die) so we never go on to
#     upload a broken/empty snapshot over a good one.
rebuild_repo() {
  local path="$1" name base="${API_BASE}" resp code
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

# Hard pre-upload guard: confirm the repo has a populated AND SEARCHABLE index
# before we snapshot it. GET /repos/<alias>/info reports {"chunks":N,"indexed":B}.
#
# Both properties must be checked, and they mean different things:
#   chunks < 1            → the index is empty. Usually a vendor whose source
#                           vanished. Recoverable at batch level: the caller
#                           prunes just this vendor and keeps going.
#   chunks >= 1, !indexed → chunks exist but the HNSW graph was never committed.
#                           This index LOOKS healthy in every count-based check
#                           yet `VectorStore::search` refuses to run on it, so
#                           the vendor answers 0 results — and a read-only serve
#                           replica can never repair it (build_index needs a
#                           write txn MDB_RDONLY rejects). Publishing this over
#                           a good snapshot is strictly destructive, so it is
#                           NOT prunable: the caller must abort the whole job.
#
#   chunks >= 1, indexed
#   absent/null           → the graph state is UNKNOWN, and this is treated as
#                           FAILURE, not as "probably fine". See below — unknown
#                           and failure are not disjoint states here.
#
# Why unknown must be fail-closed. `indexed` is populated only when the repo has
# a live open store (info_handler asks `get_opened_stores`). A repo that is still
# WARMING is absent from the state map — the write path registers `Warm` only
# after the incremental refresh completes — so it reports `indexed: null` while
# `chunks` falls back to metadata.json FROM THE RESTORED SNAPSHOT and is
# therefore reassuringly non-zero. That is exactly the mid-warmup repo we must
# not tar. "Older serve build that lacks the field" is NOT a real cause here:
# the binary and this script ship in the same image.
#
# Exit codes: 0 = ready, 1 = empty (prunable), 2 = graph missing or unverifiable
# (fatal).
VERIFY_EMPTY=1
VERIFY_NO_GRAPH=2
# How many times to re-ask /info when `indexed` comes back unknown, to absorb a
# transient `try_read()` contention against a warmup still holding the lock.
VERIFY_INFO_RETRIES=3
verify_index_ready() {
  local name="$1" base="${API_BASE}" info chunks indexed tries="${VERIFY_INFO_RETRIES}"
  while : ; do
    info="$(api "${base}/repos/${name}/info" 2>/dev/null || true)"
    indexed="$(json_field "${info}" indexed)"
    [ -n "${indexed}" ] && break
    tries=$((tries - 1))
    [ "${tries}" -le 0 ] && break
    sleep 5
  done
  chunks="$(json_field "${info}" chunks)"
  # Absent is NOT the same as zero, and the difference decides between "delete
  # this vendor" and "abort the job". info_handler ALWAYS emits `chunks` (it is
  # initialised to 0 and unconditionally serialised), so a truly empty repo
  # reports `"chunks": 0` — a present field. Empty output here therefore means
  # the response was not parseable JSON at all: a 500, a 404, an error body, a
  # reset connection. Routing that to VERIFY_EMPTY would hand a transient blip
  # to prune_dead_vendor, which rm -rf's the vendor's source AND index and then
  # uploads the snapshot without it. Unknown is fatal; only a parsed 0 is empty.
  # Absent or non-numeric are both UNKNOWN. `[ "$x" -lt 1 ]` on garbage exits 2,
  # which silently reads as "not less than 1", so the shape is tested up front
  # rather than relied on.
  case "${chunks}" in
    ''|*[!0-9]*)
      log "verify: repo '${name}' — /info gave no usable 'chunks' value (got '${chunks:-<none>}'),"
      log "        so the index state is UNKNOWN. Refusing to guess (guessing EMPTY here"
      log "        would delete a possibly healthy vendor)."
      return "${VERIFY_NO_GRAPH}" ;;
  esac
  if [ "${chunks}" -lt 1 ]; then
    log "verify: repo '${name}' reports chunks=${chunks} — index looks EMPTY"
    return "${VERIFY_EMPTY}"
  fi
  case "${indexed}" in
    true)
      log "verify: repo '${name}' OK — ${chunks} chunks indexed, HNSW graph present"
      # Explicit: without it the function's status is the status of the last
      # `log`, and an echo onto a closed stdout would read as VERIFY_EMPTY.
      return 0 ;;
    false)
      log "verify: repo '${name}' has ${chunks} chunks but indexed=false — the HNSW graph is"
      log "        MISSING, so semantic search would return 0 results for this vendor."
      return "${VERIFY_NO_GRAPH}" ;;
    *)
      log "verify: repo '${name}' — ${chunks} chunks, but /info reported indexed=<unknown> after"
      log "        ${VERIFY_INFO_RETRIES} tries. The repo has no live store, which means it is most"
      log "        likely STILL WARMING — and its chunk count came from the previously"
      log "        restored snapshot, not from a finished build. Refusing to guess."
      return "${VERIFY_NO_GRAPH}" ;;
  esac
}

# A vendor whose index came up EMPTY after warmup (0 chunks / 0 files) is dead
# weight: a ghost whose source vanished (but whose folder still holds stray
# non-index files prune_ghost_vendors conservatively keeps), or a stale/corrupt
# index the incremental warmup could not repair. Either way a SINGLE dead vendor
# must NOT veto the whole snapshot, which also carries every healthy vendor's
# freshly baked deltas. Best-effort: unregister it (closes the store + drops the
# repos.json entry) and remove the orphan folder so it is neither re-baked into
# the snapshot nor served as an empty project. Every step is tolerant — a failure
# here only logs a WARN and returns so the job keeps going. The batch-level guard
# (found==0 -> die) still aborts if NO vendor is healthy.
prune_dead_vendor() {
  local name="$1" vendor="${DOCS_DIR}/${1}" base="${API_BASE}"
  log "vendor '${name}' is empty/broken after warmup — best-effort prune (will NOT abort the batch)"
  if api -X DELETE "${base}/repos/${name}" >/dev/null 2>&1; then
    log "  unregistered '${name}' from repos.json"
  else
    log "  WARN: unregister '${name}' failed (continuing — folder removal still attempted)"
  fi
  rm -rf "${vendor}" || log "  WARN: could not remove orphan dir for '${name}'"
}

# Prune GHOST vendor folders before they are re-baked into the snapshot. A ghost
# is an indexed vendor whose source .md disappeared from the blob: azcopy sync
# --delete-destination removed the .md, but docs_index_exclusions() PROTECTED its
# .codesearch.db, so the folder survives holding ONLY the index dir. The restored
# repos.json still registers the alias, and the build loop below would no-op on it
# (already registered) while verify_index_ready passes on the stale chunks, so the
# ghost would silently persist in every subsequent snapshot. We detect a ghost as a
# DOCS_DIR/<vendor> folder whose ONLY immediate child is .codesearch.db, unregister
# it via the API (closes the store + drops the repos.json entry) and delete the
# orphan index dir. Conservative: a folder holding even one non-index entry is kept.
prune_ghost_vendors() {
  local base="${API_BASE}" vendor vname first_non_index
  local pruned=0
  for vendor in "${DOCS_DIR}"/*/; do
    [ -d "${vendor}" ] || continue     # empty ${DOCS_DIR} -> glob stays literal
    vname="$(basename "${vendor%/}")"
    # Find the FIRST immediate child that is NOT the index dir. -print -quit stops
    # after one match (we only care whether ANY non-index entry exists).
    first_non_index="$(find "${vendor%/}" -mindepth 1 -maxdepth 1 ! -name '.codesearch.db' -print -quit 2>/dev/null)"
    if [ -z "${first_non_index}" ]; then
      log "ghost vendor '${vname}': source gone (only .codesearch.db remains) -- pruning"
      if api -X DELETE "${base}/repos/${vname}" >/dev/null 2>&1; then
        log "  unregistered '${vname}' from repos.json"
      else
        log "  WARN: unregister '${vname}' failed (continuing -- folder will still be removed)"
      fi
      rm -rf "${vendor%/}" || log "  WARN: could not remove orphan index dir for '${vname}'"
      pruned=1
    fi
  done
  [ "${pruned}" -eq 1 ] || log "no ghost vendors to prune"
}

# Mark every DOCS vendor read-only in the local serve's repos.json, so the
# uploaded snapshot makes serve open DOCS read-only on restore (no warmup
# embed → fits 2 GiB). custom-kb is NOT under DOCS_DIR → stays writable.
# Generic-boundary-safe: the read_only capability lives in the binary; this
# entrypoint makes the cloud-specific "DOCS is read-only here" decision.
#
# FATAL on failure, deliberately. This is step 4 of the clear → warm → wait →
# mark ordering and it is load-bearing for the defect the whole branch exists to
# fix: a snapshot in which a DOCS vendor is still writable makes the 1 vCPU /
# 2 GiB serve replica open it write-mode and run build_index() + an incremental
# embed at warmup — the measured 1.94 GiB / exit-137 crash-loop. A WARN here
# would ship exactly that while the job reports success, so every failure path
# aborts BEFORE upload_snapshot instead.
mark_docs_readonly() {
  local repos_json="${CONFIG_DIR}/repos.json" vendor name marked=0 failed=0
  [ -f "${repos_json}" ] \
    || die "mark_docs_readonly: no repos.json at '${repos_json}' — cannot mark DOCS read-only, and an unmarked snapshot puts serve back in the write-mode warmup OOM"
  command -v jq >/dev/null 2>&1 \
    || die "jq is required in index-job mode: without it DOCS cannot be marked read-only and the snapshot would make the 2 GiB serve replica warm up write-mode"
  for vendor in "${DOCS_DIR}"/*/; do
    [ -d "${vendor}" ] || continue
    name="$(basename "${vendor%/}")"
    # only mark aliases actually registered in repos.json
    if jq -e --arg a "${name}" 'has("repos") and (.repos | has($a))' "${repos_json}" >/dev/null 2>&1; then
      if jq --arg a "${name}" '.repo_read_only[$a] = true' "${repos_json}" > "${repos_json}.tmp" \
         && mv -f "${repos_json}.tmp" "${repos_json}"; then
        log "  marked DOCS vendor '${name}' read-only in repos.json"
        marked=$((marked + 1))
      else
        # Tolerant: this is only cleanup, and under `set -e` a failing rm here
        # would abort with a bare shell error instead of the diagnostic die below.
        rm -f "${repos_json}.tmp" 2>/dev/null || true
        log "  ERROR: could not mark DOCS vendor '${name}' read-only (jq write failed)"
        failed=$((failed + 1))
      fi
    else
      log "  note: DOCS vendor '${name}' not registered — skipping"
    fi
  done
  [ "${failed}" -eq 0 ] \
    || die "${failed} DOCS vendor(s) could not be marked read-only — refusing to upload a snapshot serve would warm up write-mode"
  # The vendor loop already died if NO vendor was healthy, so by the time we get
  # here at least one registered DOCS vendor must exist. Zero marked means the
  # repos.json we are writing is not the one the build used.
  [ "${marked}" -gt 0 ] \
    || die "mark_docs_readonly marked 0 DOCS vendors although the build verified at least one — '${repos_json}' does not describe the index being snapshotted"
  log "marked ${marked} DOCS vendor(s) read-only for the snapshot"
}

# Remove the repo_read_only map from repos.json. This is STEP 1 of the
# clear → warm → wait → mark ordering documented at the index-job tail (see
# mark_docs_readonly there); it is not a standalone cleanup and the read-only
# DOCS feature is NOT disabled.
#
# Running here, before the job's serve starts, makes serve open DOCS in WRITE
# mode so warmup builds and commits every HNSW graph. Step 4 re-marks DOCS
# read-only after serve is stopped, so the snapshot carries both the graphs and
# the flag. Do not delete this call to "simplify" — without it the job's serve
# opens DOCS read-only, never builds a graph, and ships an unsearchable snapshot.
#
# Idempotent. jq is REQUIRED: without it the flags survive into the job's serve
# and the whole ordering silently collapses, so a missing jq is fatal rather
# than a WARN. A jq write failure is likewise fatal for the same reason.
clear_docs_readonly() {
  local repos_json="${CONFIG_DIR}/repos.json"
  [ -f "${repos_json}" ] || { log "clear_docs_readonly: no repos.json — nothing to clear"; return 0; }
  command -v jq >/dev/null 2>&1 \
    || die "jq is required in index-job mode: without it repo_read_only cannot be stripped, the job's serve opens DOCS read-only, and the uploaded snapshot would carry no HNSW graphs"
  if jq -e 'has("repo_read_only")' "${repos_json}" >/dev/null 2>&1; then
    if jq 'del(.repo_read_only)' "${repos_json}" > "${repos_json}.tmp" && mv -f "${repos_json}.tmp" "${repos_json}"; then
      log "stripped repo_read_only flags from repos.json (DOCS opened write-mode for the build)"
    else
      die "could not strip repo_read_only from repos.json (jq write failed) — the build would produce an unsearchable snapshot"
    fi
  else
    log "clear_docs_readonly: no repo_read_only map present — already clean"
  fi
}

# =============================================================================
# index-job mode: build/refresh the index on a big replica, snapshot, exit.
# =============================================================================
# Extract a top-level scalar field out of a JSON object body (e.g. the
# /repos/<alias>/info response). Empty output ONLY when the field is absent or
# JSON null; a literal `false` prints "false".
#
# Deliberately NOT `.[$f] // empty`: jq's `//` treats `false` as an empty value,
# so a boolean field that is genuinely `false` would print nothing and become
# indistinguishable from "field missing". For `indexed` those two mean opposite
# things — "no HNSW graph, abort the job" vs "this serve build cannot tell me,
# don't abort" — so they must not collapse.
#
# Explicit `return 0`: callers assign this inside an `if` body where `set -e` is
# live, and a SIGPIPE from jq into `head -n1` must not abort the job. Absence is
# signalled by empty OUTPUT, never by exit status.
json_field() {
  local body="$1" field="$2"
  printf '%s' "${body}" | jq -r --arg f "${field}" \
    'if has($f) and (.[$f] != null) then (.[$f] | tostring) else empty end' \
    2>/dev/null | head -n1
  return 0
}

# Extract one repo's "status" value out of a GET /status body.
#
# jq-only, by design. The previous sed fallback had to splice the alias into a
# regex, and vendor names are blob-synced folder names: one containing '.', '*'
# or '[' matched the WRONG record and could report a false "warm" — which in
# this job means "graph is built, go ahead and publish". A silently wrong ready
# signal is far worse than a hard failure, and jq is a hard image dependency
# (see the Dockerfile), so require_jq at job start makes this unreachable.
repo_status() {
  local body="$1" name="$2"
  printf '%s' "${body}" | jq -r --arg a "${name}" \
    '.repos[]? | select(.alias == $a) | .status' 2>/dev/null | head -n1
  return 0
}

# Sequential-safe build wait: block until this vendor is BOTH (a) not part of an
# in-flight submitted build and (b) actually in a ready state.
#
# Two different signals are involved and conflating them was a real bug:
#
#   1. `"status":"indexing"` is set ONLY for an explicitly submitted
#      `POST /repos` / `POST /repos/<alias>/reindex` build — the first-ever cold
#      build path. The index-job builds ONE vendor at a time, so an "indexing"
#      anywhere in /status can only be that one build; no per-alias parsing is
#      needed for this half, and we must not short-circuit just because an
#      earlier vendor is already open (that would resubmit before the current
#      build finished, reintroducing the parallel builds that OOM-killed serve).
#
#   2. Phase-1 STARTUP WARMUP — the path that actually runs for every vendor
#      restored from a snapshot ("already registered") — never sets "indexing".
#      A warming repo is simply absent from the state map and reports "closed",
#      flipping to "warm" only once the warmup has built AND committed its HNSW
#      graph. Waiting on signal 1 alone therefore returned after the initial 5s
#      sleep for every already-registered vendor ("build settled after ~5s" for
#      all six, job wall-clock 67s), so the job could kill serve and tar the
#      index dir while warmup was still writing it. The uploaded snapshot then
#      carries a missing/half-built vector graph, and neither consumer can
#      recover: a read-only serve cannot build a graph at all (build_index needs
#      a write txn MDB_RDONLY rejects) so it answers 0 results, while a
#      write-mode serve rebuilds every vendor's graph at once on cold start and
#      is OOM-killed on the 2 GiB replica. Both were observed in production.
#
# So we now wait for the named repo to reach warm/open as well.
#
# "readonly" is deliberately NOT accepted as ready. clear_docs_readonly ran
# before serve started, so no repo is CONFIGURED read-only in job mode — a
# "readonly" here can only mean the write open FAILED and try_open_stores fell
# back. That path returns from warmup without ever calling build_index(), i.e.
# exactly the "chunks present, no HNSW graph" state this whole change exists to
# prevent. Accepting it would report the failure as ready and publish a dead
# index, so we keep polling and let the budget expire instead.
#
# RETURNS NON-ZERO ON TIMEOUT, and the caller must treat that as fatal. This
# used to return 0 ("proceeding to verify"), which quietly handed a repo that
# was still WARMING to verify_index_ready — where it reports indexed=null (no
# live store yet) while its chunk count falls back to the previously restored
# snapshot's metadata. That combination is indistinguishable from a healthy
# repo on counts alone, so a timeout has to be a hard stop here rather than a
# soft handoff.
#
# The budget (INDEX_JOB_REPO_READY_SECS, top of file) is PER REPO, and small on
# purpose. The previous global 3600s would, across the whole vendor set, let a
# single stuck repo run the job past the Container Apps replicaTimeout (5400s)
# and lose the whole run — including every healthy vendor's freshly baked
# deltas. 600s x (5 vendors + custom-kb) leaves ample room for restore, tar and
# upload inside that ceiling.
wait_repo_ready() {
  local name="$1" base="${API_BASE}" waited=0 body st="" warned_readonly=0
  sleep 5   # let the 202 flip the repo into "indexing" before we start checking
  while [ "${waited}" -lt "${INDEX_JOB_REPO_READY_SECS}" ]; do
    body="$(api "${base}/status" 2>/dev/null || true)"
    if ! printf '%s' "${body}" | grep -q '"status":"indexing"'; then
      st="$(repo_status "${body}" "${name}")"
      case "${st}" in
        warm|open)
          log "repo '${name}' ready (status=${st}) after ~$((waited + 5))s"; return 0 ;;
        readonly)
          # Log once, not every 10s for the whole budget.
          if [ "${warned_readonly}" -eq 0 ]; then
            warned_readonly=1
            log "WARN: repo '${name}' opened READ-ONLY in the index job — the write open failed,"
            log "      so its HNSW graph is not being built. Still polling; this will time out."
          fi ;;
      esac
    fi
    sleep 10; waited=$((waited + 10))
  done
  log "WARN: repo '${name}' still '${st:-<unknown>}' after ${waited}s — never reached warm/open"
  return 1
}

run_index_job() {
  log "MODE=index-job — heavy build + snapshot, then exit"
  # jq underpins the whole clear → warm → wait → mark contract (flag rewrites,
  # per-repo status, graph verification). Fail here rather than degrade into
  # publishing an unsearchable snapshot.
  command -v jq >/dev/null 2>&1 \
    || die "jq is required in index-job mode (repos.json flag rewrites, /status parsing, index verification)"
  restore_snapshot   # incremental: re-embed only deltas when a prior snapshot exists
  # Strip any repo_read_only flags RESTORED from a prior snapshot BEFORE serve
  # starts. Critical: if the flags are present, the job's serve opens DOCS
  # read-only → warmup skips build_index() → the HNSW graphs are NOT built/persisted
  # into the uploaded snapshot. The serve replica would then have to build all
  # DOCS graphs at once on cold start (OOM/crash-loop), and a read-only serve
  # would return 0 search results (read-only search needs a persisted graph it
  # cannot build itself). Clearing here makes the job open DOCS WRITE mode so
  # warmup builds+commits every graph — the snapshot then carries ready-to-search
  # indexes and serve warmup is light (graphs already present).
  clear_docs_readonly
  sync_blob
  sync_kb

  # Run serve locally (no ingress needed) just to drive the indexing API.
  codesearch serve --host 127.0.0.1 --port "${PORT}" --no-tui --quiet=false &
  local serve_pid=$!
  trap 'kill "${serve_pid}" 2>/dev/null || true' EXIT

  wait_healthz 90 || { log "serve never came up"; exit 1; }

  # Drop ghost vendors (indexed but source vanished from blob) BEFORE the build
  # loop so they are neither rebuilt nor re-baked into the snapshot.
  prune_ghost_vendors

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
  local vendor found=0 vname vrc
  for vendor in "${DOCS_DIR}"/*/; do
    [ -d "${vendor}" ] || continue     # empty ${DOCS_DIR} → glob stays literal
    vname="$(basename "${vendor%/}")"
    rebuild_repo "${vendor%/}"         # strip trailing slash so basename is clean
    # Fail-closed: a vendor that never reached warm/open may still be building
    # its graph. Tarring now would publish a mid-warmup index over the good
    # snapshot, and the chunk count would not reveal it (see verify_index_ready).
    wait_repo_ready "${vname}" \
      || die "vendor '${vname}' never became ready within ${INDEX_JOB_REPO_READY_SECS}s — refusing to snapshot a possibly mid-warmup index over the good one (raise INDEX_JOB_REPO_READY_SECS if this corpus legitimately needs longer)"
    # Three outcomes, deliberately NOT collapsed into pass/fail:
    #   ready     → count it and move on.
    #   empty     → best-effort prune (see prune_dead_vendor) and skip; a single
    #               dead vendor must not veto the batch, which also carries every
    #               healthy vendor's fresh deltas. Only die if NONE are healthy.
    #   no graph  → FATAL. The index has content but is unsearchable, and unlike
    #               "empty" this is not the vendor's fault — pruning it would
    #               silently delete a healthy corpus to work around a build
    #               failure, and uploading it would publish a dead index over a
    #               good snapshot. Abort and keep the previous snapshot.
    vrc=0; verify_index_ready "${vname}" || vrc=$?
    case "${vrc}" in
      0) found=1 ;;
      "${VERIFY_EMPTY}") prune_dead_vendor "${vname}" ;;
      *) die "vendor '${vname}' has chunks but no HNSW graph — refusing to upload an unsearchable index over the good snapshot" ;;
    esac
  done
  [ "${found}" -eq 1 ] \
    || die "no healthy vendor subfolders under ${DOCS_DIR} — nothing to index (expected ${DOCS_DIR}/<vendor>/…)"
  if [ -d "${KB_DIR}/.git" ]; then
    rebuild_repo "${KB_DIR}"
    wait_repo_ready "$(basename "${KB_DIR}")" \
      || die "custom-kb never became ready within ${INDEX_JOB_REPO_READY_SECS}s — refusing to snapshot a possibly mid-warmup index over the good one"
    # custom-kb is not prunable — it is the curated corpus, so ANY verify failure
    # (empty or missing graph) aborts rather than publishing over a good snapshot.
    verify_index_ready "$(basename "${KB_DIR}")" \
      || die "index verification failed for custom-kb (empty or no HNSW graph) — refusing to upload over the good snapshot"
  fi
  # Stop the local serve BEFORE snapshotting. upload_snapshot tar's the index
  # dir; a live serve can touch LMDB/tantivy files mid-archive → tar exits 1
  # ("file changed as we read it"). Killing serve first quiesces the filesystem
  # so tar reads a stable snapshot. upload_snapshot is pure local tar+azcopy —
  # it does NOT need the serve API.
  # Bounded: `kill` only sends SIGTERM, and an unbounded `wait` on a serve that
  # is slow to handle it (or ignores it) blocks the job here with no escalation
  # until the platform replicaTimeout kills the whole run — losing the snapshot
  # that is already built. Escalate to SIGKILL instead; the tar just needs the
  # process gone, and LMDB is crash-safe.
  log "stopping local serve before snapshot"
  kill "${serve_pid}" 2>/dev/null || true
  serve_stop_waited=0
  while kill -0 "${serve_pid}" 2>/dev/null; do
    [ "${serve_stop_waited}" -ge "${SERVE_STOP_GRACE_SECS}" ] && {
      log "  serve did not exit within ${SERVE_STOP_GRACE_SECS}s — sending SIGKILL"
      kill -9 "${serve_pid}" 2>/dev/null || true
      break
    }
    sleep 1
    serve_stop_waited=$((serve_stop_waited + 1))
  done
  wait "${serve_pid}" 2>/dev/null || true

  # Mark DOCS read-only LAST — after warmup built the graphs, after serve is
  # stopped, immediately before the tar.
  #
  # The ordering is the whole trick and it is not interchangeable:
  #   clear_docs_readonly (top of the job, BEFORE serve starts)
  #     → the job's serve opens DOCS in WRITE mode → warmup runs build_index()
  #       and commits the HNSW graph into LMDB.
  #   wait_repo_ready per vendor
  #     → guarantees that commit actually finished before we tar (without this
  #       the snapshot can carry a half-built graph — see wait_repo_ready).
  #   mark_docs_readonly (here, serve already dead)
  #     → only flips the repos.json flag, touching no index data, so the
  #       snapshot ships ready-to-search graphs PLUS the read-only flag.
  #
  # Why serve needs the flag: with DOCS writable, serve's Phase-1 warmup opens
  # all five vendors write-mode and runs build_index() + an incremental refresh
  # (embedding) on each, holding every one Warm at once. Measured 1.94 GiB on
  # the 1 vCPU / 2 GiB replica → SIGKILL (exit 137) ~30s after startup, in a
  # crash-loop. Read-only warmup returns early: no embed, no build, no refresh.
  # That is only safe BECAUSE the graph is already in the snapshot — a read-only
  # store cannot build one (build_index needs a write txn MDB_RDONLY rejects),
  # which is why an earlier attempt to mark read-only WITHOUT the clear+wait
  # above made search return 0 results and had to be reverted.
  mark_docs_readonly
  upload_snapshot || die "snapshot upload failed — job is the source of truth, aborting"

  log "index-job done"
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
