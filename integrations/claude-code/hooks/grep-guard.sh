#!/usr/bin/env bash
# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
# Bash/macOS/Linux twin of grep-guard.ps1 — see that file for full rationale.
# Requires: jq, curl
#
# Grep is auto-allowed ONLY when the codesearch serve hub is genuinely
# unreachable ("plat"). A low-confidence / empty codesearch *result* is a
# SUCCESSFUL call ("reformulate your query"), NOT "codesearch is down" — so it
# must never open the grep escape hatch. The previous version used a blind
# "same query retried within 5 min" proxy that could not tell those two apart
# and leaked grep on every low-confidence result. We now probe the
# unauthenticated /healthz liveness endpoint directly.
#
# The target repo is resolved from the GREP TARGET itself (its own git root),
# never from the hook's cwd — absolute paths into a different repo resolve
# against THAT repo (#54). When the target repo is covered and LIVE but
# MID-REINDEX (branch-switch full refresh, #55), the deny is a WAIT-AND-RETRY
# instruction instead: searching a mid-rebuild index returns stale/empty
# results and must not degrade into a manual grep approval on every checkout.
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
[ "$tool" != "Grep" ] && exit 0

path=$(echo "$raw" | jq -r '.tool_input.path // empty')

# ------------------------------------------------------------------
# 1+2. Resolve the TARGET repo (the repo this Grep is aimed at) and check
#      codesearch coverage THERE — never in the hook's cwd.
#
# History (#54): coverage used to be resolved from the hook's cwd. An
# absolute-path Grep into a DIFFERENT indexed repo then failed the
# startswith(cwd-repo-root) test, looked "external", and was allowed even
# though its target repo was fully covered. The guard now follows the
# target: empty/relative paths resolve against the cwd repo (they are
# relative to it by definition), absolute paths resolve against the git
# root of the path itself.
#
# Coverage signal: a local .codesearch.db at the TARGET repo's root (a
# running serve hub alone is NOT a signal — it covers many repos and being
# alive says nothing about this one), or the explicit CODESEARCH_SERVER
# opt-in for pure remote-serve setups.
# ------------------------------------------------------------------
target_root=""
norm="${path%/}"
norm="${norm%\\}"
case "$norm" in
    [A-Za-z]:[\\/]*|/[a-zA-Z]/*|//*)
        # Absolute path: find the git root OF THE TARGET (may be a different
        # repo than the cwd one — that is the whole point of #54).
        probe="$norm"
        [ -f "$probe" ] && probe="$(dirname "$probe")"
        target_root=$(git -C "$probe" rev-parse --show-toplevel 2>/dev/null || true)
        ;;
    *)
        # Empty or relative path: resolves against the cwd repo.
        target_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
        ;;
esac

# Not inside any git repo (or git unusable) -> external target: grep is right.
[ -z "$target_root" ] && exit 0

codesearch_covers=false
if [ -d "$target_root/.codesearch.db" ]; then
    codesearch_covers=true
elif [ -n "${CODESEARCH_SERVER:-}" ]; then
    codesearch_covers=true
fi

[ "$codesearch_covers" = false ] && exit 0

# ------------------------------------------------------------------
# 3. Is the codesearch serve hub actually UP right now? (Liveness probe.)
#
# This is the ONLY condition under which grep is auto-allowed: codesearch is
# genuinely unreachable ("plat"). We probe the unauthenticated /healthz
# liveness endpoint (fixed {"status":"ok"} body, no API key required). A
# reachable server -> DENY grep and force a codesearch reformulation, even when
# a previous codesearch call returned a low-confidence / empty result — an empty
# *result* is a SUCCESSFUL call, not a dead server, so it must NOT open the
# escape hatch. Only a connection-level failure means the server is down.
#
# Base URL resolution (mirrors codesearch src/constants.rs):
#   CODESEARCH_SERVER (full base URL) > http://127.0.0.1:$CODESEARCH_SERVE_PORT
#   > http://127.0.0.1:39725  (DEFAULT_SERVE_URL / DEFAULT_SERVE_PORT)
# ------------------------------------------------------------------
if [ -n "${CODESEARCH_SERVER:-}" ]; then
    base="${CODESEARCH_SERVER%/}"
elif [ -n "${CODESEARCH_SERVE_PORT:-}" ]; then
    base="http://127.0.0.1:${CODESEARCH_SERVE_PORT}"
else
    base="http://127.0.0.1:39725"
fi

# curl: -sS quiet (errors to stderr), --max-time 2 short timeout; the body is
# discarded via the shell redirect below. We deliberately do NOT use curl's
# `-o /dev/null` — on Windows/Git-Bash a native curl.exe fails writing to the
# translated /dev/null path (exit 23) even on a healthy 200, which would
# misreport an UP server as down. Shell-level `>/dev/null` avoids that.
# Without -f, any HTTP response (even 4xx/5xx) yields exit 0 = reachable/up;
# only a connection-level failure (exit 7/28/…) means it's down -> allow grep.
if ! curl -sS --max-time 2 "${base}/healthz" >/dev/null 2>&1; then
    exit 0  # codesearch is DOWN -> grep is genuinely all you have, let it through
fi

# ------------------------------------------------------------------
# 3.5 Is the TARGET repo mid-reindex right now? (Freshness probe, #55.)
#
# Liveness (/healthz) says the server is UP; it says nothing about index
# FRESHNESS. Right after a branch switch the serve watcher fires a full
# refresh, and searches against the mid-rebuild index return stale/empty
# results — which used to push the agent into a manual grep approval on
# every routine checkout. GET /indexing?path=<repo root> resolves the
# target to its registered repo and reports an active reindex. When one is
# in flight, deny with a WAIT-AND-RETRY instruction: the tree did not
# change, the index is just catching up — waiting beats grepping.
#
# Backward compat: an older serve without this endpoint answers 404; the
# probe is skipped and behaviour is exactly the pre-#55 deny. The ?path=
# value is percent-encoded (jq @uri) — a repo root containing spaces, +,
# &, # or non-ASCII would otherwise make the probe fail or answer for the
# WRONG repo prefix, silently disabling the wait-and-retry path.
# ------------------------------------------------------------------
enc=$(printf '%s' "$target_root" | jq -sRr @uri)
if fresh_json=$(curl -sS --max-time 2 "${base}/indexing?path=${enc}" 2>/dev/null); then
    covered=$(echo "$fresh_json" | jq -r '.covered // false' 2>/dev/null || echo false)
    indexing=$(echo "$fresh_json" | jq -r '.indexing // false' 2>/dev/null || echo false)
    if [ "$covered" = "true" ] && [ "$indexing" = "true" ]; then
        msg=$(cat <<'EOF'
codesearch is LIVE, but THIS repo's index is being rebuilt right now (a
branch switch or file change fired the serve watcher's full refresh).
Searching immediately would return stale or empty results — that is the
rebuild in progress, not a miss, and NOT a reason to grep.

WAIT 15-30 seconds (Bash: sleep 20), then RETRY your codesearch call — it
will answer normally once the rebuild lands. The working tree did not
change; only the index is catching up, so grep adds nothing here.

If the rebuild still has not landed after ~2 minutes, run your codesearch
call anyway (a partially fresh index still beats grep) or ask the user how
to proceed.
EOF
)
        jq -n --arg msg "$msg" '{
          hookSpecificOutput: {
            hookEventName: "PreToolUse",
            permissionDecision: "deny",
            permissionDecisionReason: $msg
          }
        }'
        exit 0
    fi
fi

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
msg=$(cat <<'EOF'
codesearch is LIVE for this repo (its /healthz probe just answered) — use it,
do NOT fall back to Grep. Grep on an indexed internal path is only auto-allowed
when the codesearch serve hub is actually DOWN, which it is not right now.

IMPORTANT: a low-confidence or EMPTY codesearch result is a SUCCESSFUL call that
means "reformulate your query" — it does NOT mean codesearch is down and it will
NOT unblock Grep. Reformulate instead of grepping.

Step 1 — load the deferred MCP tool schemas (Claude Code defers all MCP tools;
this is a one-time step per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Step 2 — pick the RIGHT tool (this is usually why a query came back empty):
  find(symbol="Name", kind="definition")   -- known symbol / type / function definition
  find(symbol="Name", kind="usages")       -- all call sites of a known symbol
  explore(kind="outline", target="path")   -- every symbol in one file
  search(query="concept", mode="semantic") -- concepts / cross-file, the DEFAULT
  search(query="exact", mode="literal", regex=true) -- exact syntax / pattern

Query hygiene (this is what produces "low_confidence: []"):
  * Do NOT paste grep-style multi-term alternations ("a|b|c", "::", "fn foo(")
    into search — BM25 tokenises on punctuation and the match scores below the
    relevance floor, so you get an empty result even though the string exists.
  * Use ONE clean term, or switch to find()/explore() for exact symbols.

Multi-repo serve mode: if the call returns a "scope_required" or "Unknown alias"
error, you MUST pass project="<repo-alias>" (single repo) or group="<group>"
(cross-repo). The error response LISTS the valid available_projects /
available_groups — pick from that list (the alias may differ from the folder
name).

Grep is always allowed for targets outside any git repo, and for repos
codesearch does not cover (no local index, no CODESEARCH_SERVER).
EOF
)

jq -n --arg msg "$msg" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: $msg
  }
}'
exit 0
