#!/usr/bin/env bash
# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
# Bash/macOS/Linux twin of grep-guard.ps1 — see that file for full rationale.
# Requires: jq
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
[ "$tool" != "Grep" ] && exit 0

pattern=$(echo "$raw" | jq -r '.tool_input.pattern // empty')
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
# 2. Is codesearch actually available?
# ------------------------------------------------------------------
codesearch_available=false
if pgrep -x codesearch >/dev/null 2>&1; then
    codesearch_available=true
elif [ -n "${CODESEARCH_SERVER:-}" ]; then
    codesearch_available=true
else
    git_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
    if [ -n "$git_root" ] && [ -d "$git_root/.codesearch.db" ]; then
        codesearch_available=true
    fi
fi

[ "$codesearch_available" = false ] && exit 0

# ------------------------------------------------------------------
# 3. Retry cache: same (pattern, path) blocked recently -> let it through.
# ------------------------------------------------------------------
cache_file="${TMPDIR:-/tmp}/.codesearch-grep-guard.json"
cache_ttl=300
now=$(date +%s)
cache_key="${pattern}|${path}"

# NOTE: feed the cache file to jq via stdin redirection (`< file`), never as a
# positional path argument. On Windows/Git-Bash with a native jq.exe, POSIX-style
# paths (/tmp/...) passed as jq CLI args fail to resolve ("Could not open file")
# even though the same path works fine for bash builtins and stdin redirection,
# since bash itself resolves the path for `<` before jq ever sees it.
if [ -f "$cache_file" ]; then
    blocked_at=$(jq -r --arg k "$cache_key" '.[$k] // empty' < "$cache_file" 2>/dev/null || true)
    if [ -n "$blocked_at" ] && [ $((now - blocked_at)) -lt "$cache_ttl" ]; then
        exit 0  # already blocked once this window -> allow the retry
    fi
fi

# Prune stale entries and record this block
if [ -f "$cache_file" ]; then
    tmp=$(mktemp)
    jq --arg k "$cache_key" --argjson now "$now" --argjson ttl "$cache_ttl" \
        'with_entries(select(($now - .value) < $ttl)) + {($k): $now}' \
        < "$cache_file" > "$tmp" 2>/dev/null && mv "$tmp" "$cache_file" || true
else
    jq -n --arg k "$cache_key" --argjson now "$now" '{($k): $now}' > "$cache_file" 2>/dev/null || true
fi

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
msg=$(cat <<EOF
codesearch is active for this repo — try it before Grep for code discovery.

Step 1 — load the deferred MCP tool schemas (Claude Code defers all MCP tools;
this is a one-time step per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Step 2 — search:
  mcp__codesearch__search(query="$pattern", mode="semantic")             -- concepts, identifiers, cross-file
  mcp__codesearch__search(query="$pattern", mode="literal", regex=true)  -- exact pattern / regex
  mcp__codesearch__find(symbol="...", kind="definition")                 -- symbol definition
  mcp__codesearch__find(symbol="...", kind="usages")                     -- all call sites

This exact Grep call is auto-unblocked if you retry it within 5 minutes
(i.e. codesearch returned nothing useful — go ahead and grep).
Grep is always allowed for paths outside the current repo.
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
