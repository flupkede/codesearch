#!/usr/bin/env bash
# PreToolUse hook: steer WebSearch/WebFetch toward codesearch remote doc mounts.
#
# Why this exists: when codesearch has remote documentation projects mounted
# (e.g. cloud/inriver, cloud/example-dam), those indexes usually answer product /
# API / docs questions more precisely — and more currently — than an open web
# search. Nothing structurally stops the model from reaching for the always-on
# WebSearch/WebFetch tools first, so this hook makes the preference structural:
# the FIRST WebSearch/WebFetch is blocked with actionable guidance; if the same
# query is retried within 5 minutes (i.e. the mounts didn't have the answer), it
# is let through.
#
# Passes through (exit 0, no block) when:
#   - there are NO remote mounts to steer toward (nothing indexed to prefer)
#   - the same query was already blocked in the last 5 minutes
#
# Bash/macOS/Linux twin of web-guard.ps1. Requires: jq
#
# Install: see ../README.md (or run `codesearch hooks claude install`).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
case "$tool" in
    WebSearch | WebFetch) ;;
    *) exit 0 ;;
esac

# Query (WebSearch) or target URL (WebFetch) — used for the cache key + guidance.
q=$(echo "$raw" | jq -r '.tool_input.query // .tool_input.url // empty')

# ------------------------------------------------------------------
# 1. Are there any remote doc mounts to steer toward?
#
# Mounts live in repos.json under `.remote_mounts` (canonical "<peer>/<alias>"
# names — the opt-in allowlist). No mounts -> nothing to prefer -> allow the
# web call unimpeded.
# ------------------------------------------------------------------
config="${CODESEARCH_REPOS_CONFIG:-$HOME/.codesearch/repos.json}"
[ -f "$config" ] || exit 0

mounts=$(jq -r '(.remote_mounts // []) | join(", ")' < "$config" 2>/dev/null || true)
[ -z "$mounts" ] && exit 0

# ------------------------------------------------------------------
# 2. Retry cache: same query blocked recently -> let it through.
#    Covers "tried the mounts, they had nothing, now use the web".
#
# NOTE: feed the cache file to jq via stdin redirection (`< file`), never as a
# positional path argument — see grep-guard.sh for the Windows/Git-Bash rationale.
# ------------------------------------------------------------------
cache_file="${TMPDIR:-/tmp}/.codesearch-web-guard.json"
cache_ttl=300
now=$(date +%s)
cache_key="$q"

if [ -f "$cache_file" ]; then
    blocked_at=$(jq -r --arg k "$cache_key" '.[$k] // empty' < "$cache_file" 2>/dev/null || true)
    if [ -n "$blocked_at" ] && [ $((now - blocked_at)) -lt "$cache_ttl" ]; then
        exit 0
    fi
fi

# Prune stale entries and record this block.
if [ -f "$cache_file" ]; then
    tmp=$(mktemp)
    jq --arg k "$cache_key" --argjson now "$now" --argjson ttl "$cache_ttl" \
        'with_entries(select(($now - .value) < $ttl)) + {($k): $now}' \
        < "$cache_file" > "$tmp" 2>/dev/null && mv "$tmp" "$cache_file" || true
else
    jq -n --arg k "$cache_key" --argjson now "$now" '{($k): $now}' > "$cache_file" 2>/dev/null || true
fi

# ------------------------------------------------------------------
# 3. Block with actionable guidance.
# ------------------------------------------------------------------
msg=$(cat <<EOF
codesearch has remote documentation mounts — search those before the web.
Mounted remotes: ${mounts}

These indexed mounts often answer product/API/docs questions more precisely
(and more currently) than a web search. Try codesearch first.

Step 1 — load the deferred MCP tool schemas (one-time per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__get_chunk")

Step 2 — search the relevant mount (compact=false reads matching content inline):
  mcp__codesearch__search(query="${q}", project="<peer/alias>", compact=false)
  mcp__codesearch__get_chunk(chunk_ref="<peer/alias:id from a result>")  # full context

Pick the relevant project from the mounted remotes above.

This exact ${tool} call is auto-unblocked if you retry it within 5 minutes
(i.e. the mounts didn't have the answer — go ahead and use the web).
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
