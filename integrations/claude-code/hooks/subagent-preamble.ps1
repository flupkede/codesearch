# PreToolUse hook: inject a codesearch-first preamble into every Claude Code
# subagent prompt (the `Agent` tool).
#
# Why this exists: a spawned subagent gets a fresh context. It does NOT inherit
# the parent's AGENTS.md, nor the codesearch MCP server's `initialize`
# instructions. It DOES get Grep/Glob as always-on tools, while codesearch's
# tools are deferred (schema-less until an explicit ToolSearch call). Left to
# its own devices, a fresh subagent greps the working directory and never
# touches codesearch. This hook prepends a short, actionable preamble to every
# Agent `prompt` so the subagent knows, before doing anything else, that
# codesearch exists, how to load it, and when to prefer it.
#
# Idempotent: skips injection if the prompt already contains the marker
# (so it composes safely with other hooks on the same Agent matcher, e.g. a
# project-specific scope-injection hook, as long as that hook uses a
# different marker string).
#
# Always exits 0 — never blocks agent spawning, only rewrites the prompt.
#
# Install: see ../README.md (or run ../install.ps1 to wire this up automatically).

$ErrorActionPreference = 'Stop'

try {
    $raw = [Console]::In.ReadToEnd()
    if ([string]::IsNullOrWhiteSpace($raw)) { exit 0 }
    $data = $raw | ConvertFrom-Json
} catch {
    exit 0
}

$tool = $data.tool_name
$inp  = $data.tool_input

if ($tool -ne 'Agent') { exit 0 }
if ($null -eq $inp)    { exit 0 }

$names = @($inp.PSObject.Properties.Name)
if ($names -notcontains 'prompt') { exit 0 }
$prompt = [string]$inp.prompt

$marker = '[[CODESEARCH-PREAMBLE]]'
if ($prompt.Contains($marker)) { exit 0 }

$preamble = @"
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
$prompt
"@

$newInput = @{}
foreach ($p in $inp.PSObject.Properties) {
    if ($p.Name -eq 'prompt') { continue }
    $newInput[$p.Name] = $p.Value
}
$newInput['prompt'] = $preamble

$out = @{
    hookSpecificOutput = @{
        hookEventName            = 'PreToolUse'
        permissionDecision       = 'allow'
        updatedInput             = $newInput
        permissionDecisionReason = 'Injected codesearch-first preamble into subagent prompt (deferred MCP tools need an explicit ToolSearch load).'
    }
}
$out | ConvertTo-Json -Depth 20 -Compress
exit 0
