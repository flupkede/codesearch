#!/usr/bin/env bash
# PostToolUse companion for edit-guard: records a "codesearch consulted"
# marker per file path after qualifying MCP calls, into the same state file
# edit-guard reads. Fires on:
#   - mcp__codesearch__find_impact           (ANY outcome counts — including
#     "no results" / "no SCIP backend": the guard cannot know the result, and
#     counting failures too is what keeps it from blocking permanently)
#   - mcp__codesearch__find with kind="usages"
#     (the kind check is a string compare HERE, not in the matcher regex —
#     matchers filter on tool name only)
#
# PostToolUse hooks cannot block anything: this script never emits a
# decision and ALWAYS exits 0. Symbol-only find_impact calls carry no
# file-ish input field and are skipped (nothing to attribute a path to).
# Requires: jq

set -euo pipefail

. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/codesearch-common.sh"

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty' 2>/dev/null | jq_str)
case "$tool" in
    mcp__codesearch__find_impact)
        mark_tool="find_impact"
        ;;
    mcp__codesearch__find)
        kind=$(echo "$raw" | jq -r '.tool_input.kind // empty' 2>/dev/null | jq_str)
        [ "$kind" = "usages" ] || exit 0
        mark_tool="find_usages"
        ;;
    *) exit 0 ;;
esac

# find_impact names its target via `file`/`path`; find uses `path`. Accept
# any of them — FIRST NON-EMPTY (jq's `//` treats "" as truthy, so the
# empties are filtered explicitly; the ps1 twin does the same) — so both
# tools attribute to the same key.
p=$(echo "$raw" | jq -r '
    [ .tool_input.file, .tool_input.path, .tool_input.file_path ]
    | map(select(. != null and . != ""))
    | .[0] // empty
' 2>/dev/null | jq_str)
[ -z "$p" ] && exit 0

key="$(norm_state_key "$p")"
[ -z "$key" ] && exit 0

state_file="${TMPDIR:-/tmp}/.codesearch-edit-guard-state.json"
window=300
now=$(date +%s)

# Upsert, pruning expired entries so the file cannot grow without bound; a
# window-expired entry is simply dropped and the fresh write re-creates it.
tmp="$(mktemp)"
merged=false
if [ -f "$state_file" ]; then
    if jq --arg k "$key" --arg t "$mark_tool" --argjson now "$now" --argjson window "$window" '
        with_entries(
            (.value.ts // -1 | (tonumber? // -1)) as $ts
            | select(($now - $ts) < $window)
        ) + { ($k): { tool: $t, ts: $now } }
    ' < "$state_file" > "$tmp" 2>/dev/null && [ -s "$tmp" ]; then
        merged=true
    fi
fi
if [ "$merged" != true ]; then
    # Missing or corrupt state: start over from this single marker.
    jq -n --arg k "$key" --arg t "$mark_tool" --argjson now "$now" \
        '{ ($k): { tool: $t, ts: $now } }' > "$tmp" 2>/dev/null || true
fi
mv -f "$tmp" "$state_file" 2>/dev/null || true
rm -f "$tmp" 2>/dev/null || true

exit 0
