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
# against THAT repo (#54, and see the #199 note below: POSIX absolute paths
# used to slip through this very test). Coverage is decided by REGISTRATION
# with the local serve hub (repos.json), not by a .codesearch.db directory
# (#199). When the target repo is covered and LIVE but MID-REINDEX
# (branch-switch full refresh, #55), the deny is a WAIT-AND-RETRY
# instruction instead: searching a mid-rebuild index returns stale/empty
# results and must not degrade into a manual grep approval on every checkout.
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

# Shared coverage/target-resolution helpers (repos.json registration, path
# normalization) live in codesearch-common.sh, shared with edit-guard.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/codesearch-common.sh"

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty' | jq_str)
[ "$tool" != "Grep" ] && exit 0

path=$(echo "$raw" | jq -r '.tool_input.path // empty' | jq_str)

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
# History (#199): the #54 rewrite still mis-binned POSIX absolute paths
# (/home/..., /tmp/...): its absolute-detection pattern only matched
# Windows-style roots (C:\, C:/, MSYS /c/, UNC //server), so a POSIX
# absolute target fell into the relative branch and resolved against the
# hook's cwd — with a session cwd that is not itself a git repo (a parent
# directory holding several repos) the target resolved to NOTHING, looked
# external, and the guard silently allowed every grep. `/*` now covers
# every absolute path.
#
# Coverage signal (#199): the target's git root is REGISTERED with the
# local serve hub (~/.codesearch/repos.json — the same registration list
# the hub itself resolves queries by). A `.codesearch.db` directory at
# the git root was only ever a proxy for that and is wrong in both
# directions: a stale db from a since-unregistered repo denied Grep while
# the hub could not actually answer for it (unknown alias), and a
# registered repo whose db directory was gone slipped through uncovered.
# Matching the git ROOT (never a path prefix) also carves out nested
# repos for free: an unregistered clone nested inside a registered repo
# resolves to its OWN git root, equals no registration, and is correctly
# treated as uncovered. The explicit CODESEARCH_SERVER opt-in stays for
# pure remote-serve setups with no local repos.json (#199 tracks that it
# is a URL override rather than a coverage signal — to be reworked
# separately).
# ------------------------------------------------------------------
target_root="$(resolve_target_git_root "$path")"

# Not inside any git repo (or git unusable) -> external target: grep is right.
[ -z "$target_root" ] && exit 0

# Covered = the target's git root is registered with the serve hub
# (repos.json), or CODESEARCH_SERVER opts a pure remote-serve setup in.
target_registered "$target_root" || exit 0

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

Grep is always allowed for targets outside any git repo, for repos that are
not registered with the codesearch serve hub (repos.json), and for pure
remote-serve setups opted in via CODESEARCH_SERVER.
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
