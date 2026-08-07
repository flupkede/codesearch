#!/usr/bin/env bash
# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
# Bash/macOS/Linux twin of grep-guard.ps1 - see that file for full rationale.
# Requires: jq, curl
#
# Grep is auto-allowed ONLY when the codesearch serve hub is genuinely
# unreachable ("plat"). A low-confidence / empty codesearch *result* is a
# SUCCESSFUL call ("reformulate your query"), NOT "codesearch is down" - so it
# must never open the grep escape hatch. The previous version used a blind
# "same query retried within 5 min" proxy that could not tell those two apart
# and leaked grep on every low-confidence result. We now probe the
# unauthenticated /healthz liveness endpoint directly.
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

# Fail open on malformed input. Every extraction below runs under
# `set -euo pipefail`, so a jq parse error would abort the hook with exit 5
# and surface to Claude Code as a hook EXECUTION FAILURE rather than the
# silent pass-through these guards promise. Validating once up front keeps
# the .sh twins matching the .ps1 twins, which already exit 0 on bad JSON.
printf '%s' "$raw" | jq -e . >/dev/null 2>&1 || exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
[ "$tool" != "Grep" ] && exit 0

path=$(echo "$raw" | jq -r '.tool_input.path // empty')

# ------------------------------------------------------------------
# 1. Is the path internal to the current repo?
# ------------------------------------------------------------------
is_internal=true
if [ -n "$path" ] && [ "$path" != "." ] && [ "$path" != "./" ]; then
    case "$path" in
        /*)
            git_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
            if [ -n "$git_root" ]; then
                case "$path" in
                    "$git_root"*) is_internal=true ;;
                    *) is_internal=false ;;
                esac
            else
                is_internal=false  # can't determine git root -> assume external, allow grep
            fi
            ;;
        *) is_internal=true ;;  # relative path stays internal
    esac
fi

[ "$is_internal" = false ] && exit 0

# ------------------------------------------------------------------
# 2. Does codesearch COVER this repo? Don't block if it doesn't.
#
# NOTE: we deliberately do NOT treat "a codesearch process is running" as
# sufficient. codesearch commonly runs as a persistent background `serve`
# hub covering many registered repos (`codesearch index list`) - that
# process is alive nearly all the time on a dev machine, regardless of
# whether the CURRENT directory is one of the repos it actually indexes.
# Using process-presence alone made this hook fire in every directory on
# the machine, including ones with no index at all. A local `.codesearch.db`
# at the git root is the precise, fast signal that THIS repo is indexed.
# ------------------------------------------------------------------
codesearch_covers=false
git_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
if [ -n "$git_root" ] && [ -d "$git_root/.codesearch.db" ]; then
    codesearch_covers=true
elif [ -n "${CODESEARCH_SERVER:-}" ]; then
    # Explicit opt-in escape hatch for pure remote-serve setups with no local
    # .codesearch.db. Requires the user to consciously set this env var, so
    # it can't spuriously fire the way "any process running" did.
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
# a previous codesearch call returned a low-confidence / empty result - an empty
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
# `-o /dev/null` - on Windows/Git-Bash a native curl.exe fails writing to the
# translated /dev/null path (exit 23) even on a healthy 200, which would
# misreport an UP server as down. Shell-level `>/dev/null` avoids that.
# Without -f, any HTTP response (even 4xx/5xx) yields exit 0 = reachable/up;
# only a connection-level failure (exit 7/28/...) means it's down -> allow grep.
if ! curl -sS --max-time 2 "${base}/healthz" >/dev/null 2>&1; then
    exit 0  # codesearch is DOWN -> grep is genuinely all you have, let it through
fi

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
msg=$(cat <<'EOF'
codesearch is LIVE for this repo (its /healthz probe just answered) - use it,
do NOT fall back to Grep. Grep on an indexed internal path is only auto-allowed
when the codesearch serve hub is actually DOWN, which it is not right now.

IMPORTANT: a low-confidence or EMPTY codesearch result is a SUCCESSFUL call that
means "reformulate your query" - it does NOT mean codesearch is down and it will
NOT unblock Grep. Reformulate instead of grepping.

Step 1 - load the deferred MCP tool schemas (Claude Code defers all MCP tools;
this is a one-time step per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Step 2 - pick the RIGHT tool (this is usually why a query came back empty):
  find(symbol="Name", kind="definition")   -- known symbol / type / function definition
  find(symbol="Name", kind="usages")       -- all call sites of a known symbol
  explore(kind="outline", target="path")   -- every symbol in one file
  search(query="concept", mode="semantic") -- concepts / cross-file, the DEFAULT
  search(query="exact", mode="literal", regex=true) -- exact syntax / pattern

Query hygiene (this is what produces "low_confidence: []"):
  * Do NOT paste grep-style multi-term alternations ("a|b|c", "::", "fn foo(")
    into search - BM25 tokenises on punctuation and the match scores below the
    relevance floor, so you get an empty result even though the string exists.
  * Use ONE clean term, or switch to find()/explore() for exact symbols.

Multi-repo serve mode: if the call returns a "scope_required" or "Unknown alias"
error, you MUST pass project="<repo-alias>" (single repo) or group="<group>"
(cross-repo). The error response LISTS the valid available_projects /
available_groups - pick from that list (the alias may differ from the folder
name).

Grep is always allowed for paths OUTSIDE the current repo.
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
