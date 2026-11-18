#!/usr/bin/env bash
# PostToolUse companion for edit-guard: records "codesearch consulted"
# markers per file path after qualifying MCP calls, into the same state file
# edit-guard reads. Fires on:
#   - mcp__codesearch__find_impact           (ANY outcome counts — including
#     "no results" / "no SCIP backend": counting failures too is what keeps
#     the guard from blocking permanently)
#   - mcp__codesearch__find with kind="usages"
#     (the kind check is a string compare HERE, not in the matcher regex —
#     matchers filter on tool name only)
#
# Marker sources (all available ones are written):
#   1. a file-ish INPUT field (file/path/file_path) — find_impact's schema
#      names its target `symbol_name` and carries no path, so this serves
#      older schemas and hand-crafted calls
#   2. every path the RESPONSE mentions: find/find_impact hits carry
#      path/file fields, including inside the JSON-stringified text payload
#      MCP wraps them in. Serve-mode paths are alias-prefixed or project-
#      relative and resolve against the registered root of the input's
#      `project` alias. One consultation marks every file it listed — the
#      per-file treadmill only kicks in when the agent moves to files the
#      answer did not touch.
#   3. when neither yields a single path — an empty result or a "no SCIP
#      backend" error — a REPO-level marker on the project's registered
#      root: codesearch was consulted and answered nothing, the one case
#      where per-file attribution is impossible but the protocol was
#      followed. edit-guard accepts that marker for any file in that repo.
#      (This is the fix for the reported permanent wedge: find_impact on a
#      repo whose C# symbol index is missing wrote no marker at all, so
#      every .cs edit in a registered repo stayed denied.)
#
# PostToolUse hooks cannot block anything: this script never emits a
# decision and ALWAYS exits 0.
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

project=$(echo "$raw" | jq -r '.tool_input.project // empty' 2>/dev/null | jq_str)

keys=()

# Source 1: a file-ish input field — first non-empty (jq's `//` treats "" as
# truthy, so the empties are filtered explicitly; the ps1 twin does the same).
input_p=$(echo "$raw" | jq -r '
    [ .tool_input.file, .tool_input.path, .tool_input.file_path ]
    | map(select(. != null and . != ""))
    | .[0] // empty
' 2>/dev/null | jq_str)
if [ -n "$input_p" ]; then
    k="$(norm_state_key "$input_p")"
    [ -n "$k" ] && keys+=("$k")
fi

# Source 2: paths mentioned anywhere in the tool response — as an object, as
# a JSON string, or inside the MCP text envelope's JSON-stringified payloads.
while IFS= read -r rp; do
    [ -n "$rp" ] || continue
    abs="$(resolve_result_path "$rp" "$project")"
    [ -z "$abs" ] && continue
    k="$(norm_state_key "$abs")"
    [ -n "$k" ] && keys+=("$k")
done < <(echo "$raw" | jq -r '
    [ (.tool_response | select(type == "object")),
      (.tool_response | select(type == "string") | try fromjson catch null),
      (.tool_response | select(type == "object") | .content[]?.text? | strings | try fromjson catch null)
    ]
    | map(select(. != null)
        | [ .. | objects | (.path?, .file?, .file_path?) ])
    | add // []
    | map(select(type == "string" and . != ""))
    | unique | .[]
' 2>/dev/null | jq_str)

# Source 3: repo-level fallback — consulted, but nothing attributable.
if [ "${#keys[@]}" -eq 0 ] && [ -n "$project" ]; then
    root="$(registered_project_root "$project")"
    if [ -n "$root" ]; then
        k="$(norm_state_key "$root")"
        [ -n "$k" ] && keys+=("$k")
    fi
fi

[ "${#keys[@]}" -gt 0 ] || exit 0
mapfile -t keys < <(printf '%s\n' "${keys[@]}" | awk '!seen[$0]++')

state_file="${TMPDIR:-/tmp}/.codesearch-edit-guard-state.json"
window=300
now=$(date +%s)

# Upsert each key, pruning expired entries so the file cannot grow without
# bound; corrupt/unreadable state starts over from the fresh markers.
for key in "${keys[@]}"; do
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
done

exit 0
