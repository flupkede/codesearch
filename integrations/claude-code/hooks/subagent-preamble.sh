#!/usr/bin/env bash
# PreToolUse hook: inject a codesearch-first preamble into every Claude Code
# subagent prompt (the `Agent` tool). Bash/macOS/Linux twin of
# subagent-preamble.ps1 — see that file for full rationale.
# Requires: jq
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
[ "$tool" != "Agent" ] && exit 0

has_prompt=$(echo "$raw" | jq -r 'if (.tool_input | has("prompt")) then "yes" else "no" end')
[ "$has_prompt" != "yes" ] && exit 0

prompt=$(echo "$raw" | jq -r '.tool_input.prompt')

marker='[[CODESEARCH-PREAMBLE]]'
case "$prompt" in
    *"$marker"*) exit 0 ;;  # already injected, idempotent
esac

preamble=$(cat <<EOF
$marker
SEARCH RULE — read before doing anything:
codesearch is the preferred search tool for code in the current repo.
codesearch tools are DEFERRED: they do NOT appear in your tool list until you load them.

Load them first (before any Grep or Glob):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Then use:
  mcp__codesearch__search(query, mode="semantic")             -- concepts, identifiers, cross-file lookup
  mcp__codesearch__search(query, mode="literal", regex=true)  -- exact patterns / regex
  mcp__codesearch__find(symbol, kind="definition")             -- where a symbol is defined
  mcp__codesearch__find(symbol, kind="usages")                 -- all call sites
  mcp__codesearch__explore(target, kind="outline")             -- file/class structure
  mcp__codesearch__get_chunk(chunk_id)                         -- read a specific code chunk

Multi-repo serve mode: if a call returns "scope_required" or "Unknown alias",
add project="<repo-alias>" (single repo) or group="<group>" (cross-repo). The
error response lists the valid available_projects / available_groups — pick
from that list; the alias may differ from the folder name.

Fall back to Grep/Glob only after codesearch returns no useful results,
or when the path is outside the current repo (codesearch covers internal
paths only unless you're in multi-repo serve mode with an explicit group).

If ToolSearch returns no codesearch tools, codesearch is not active for this
session — proceed with Grep/Glob as normal.
---
EOF
)
preamble="${preamble}${prompt}"

# updatedInput must carry the FULL tool_input (Claude Code replaces, not merges) —
# take the original object and only overwrite `prompt`, so subagent_type,
# description, model, isolation etc. survive untouched.
echo "$raw" | jq --arg p "$preamble" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "allow",
    updatedInput: (.tool_input + {prompt: $p}),
    permissionDecisionReason: "Injected codesearch-first preamble into subagent prompt (deferred MCP tools need an explicit ToolSearch load)."
  }
}'
exit 0
