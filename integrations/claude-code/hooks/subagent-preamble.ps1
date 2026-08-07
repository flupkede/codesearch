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
# Always exits 0 - never blocks agent spawning, only rewrites the prompt.
#
# Why the preamble steers Grep but NOT Glob: only `Grep` is gated (by
# grep-guard); there is deliberately no Glob guard. Glob matches *filenames*,
# and codesearch has no file-listing primitive to replace it - `search`'s
# `file_glob` is a filter on a content search, not an enumerator. Gating Glob
# would also buy nothing, since `ls` / `find` / `git ls-files` reach the same
# answer through Bash, which is not gated either. So the text below tells the
# subagent when Glob is the RIGHT tool instead of lumping it in with Grep;
# an earlier version promised "prefer codesearch over Grep/Glob" while
# enforcing only the Grep half, which taught agents to avoid Glob for exactly
# the filename and existence questions it is best at.
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

# ------------------------------------------------------------------
# Resolve the codesearch scope for THIS working directory.
#
# In multi-repo serve mode every call needs project= or group=, and a fresh
# subagent has no way to know either. Telling it to "read the alias off the
# scope_required error" only works AFTER a failed call, and the alias is not
# guessable from the folder name (observed: directory NWND.ACME is
# registered as alias NWND.Acme). Worse, an agent that guesses a project=
# silently misses sibling repos: configuration commonly lives in its own
# repo, so the union that group= would have searched never gets searched and
# the agent concludes the code does not exist.
#
# We resolve it up front from repos.json, resolved the way
# db_discovery::repos::config_path() does it: the CODESEARCH_REPOS_CONFIG
# override first, else config_dir()/repos.json = dirs::home_dir()/.codesearch.
# web-guard reads the same file and honours the same override, so both hooks
# must agree - otherwise one session reads two different configs.
# No HTTP, no auth, no latency. Only the alias and the
# names of the groups it belongs to are injected - NOT the group's member
# list, which is the group definition's job to hold and status() to report,
# and NOT anything else from that file (it also stores a remote API key).
#
# The group sort is ORDINAL on purpose: the default Sort-Object is culture-
# and case-insensitive, which would order ("acme","NORTHWIND") differently from
# the .sh twin's codepoint `sort` and break the byte-identical-twins contract.
#
# Fail-open: no git root, no config file, unparseable config, or a directory
# that is not a registered repo all fall through to the generic text below.
# ------------------------------------------------------------------
$scopeBlock = @'
Multi-repo serve mode: if a call returns "scope_required" or "Unknown alias",
add project="<repo-alias>" (single repo) or group="<group>" (cross-repo). The
error response lists the valid available_projects / available_groups - pick
from that list; the alias may differ from the folder name.
'@

# Built from a char code so no literal backslash appears in this file - a bare
# '\' reaches -replace as an invalid regex, and .Replace(char,char) silently
# picks the wrong overload for a multi-char argument.
$backslash = [string][char]92

try {
    $gitRoot = @(& git rev-parse --show-toplevel 2>$null)[0]
    $cfgPath = if ($env:CODESEARCH_REPOS_CONFIG) { $env:CODESEARCH_REPOS_CONFIG }
               else { Join-Path $HOME '.codesearch/repos.json' }
    if (-not [string]::IsNullOrWhiteSpace($gitRoot) -and (Test-Path $cfgPath)) {
        $cfg   = Get-Content $cfgPath -Raw | ConvertFrom-Json
        $root  = ([string]$gitRoot).Replace($backslash, '/').ToLowerInvariant().TrimEnd('/')
        $alias = $null
        if ($null -ne $cfg.repos) {
            foreach ($p in $cfg.repos.PSObject.Properties) {
                $candidate = ([string]$p.Value).Replace($backslash, '/').ToLowerInvariant().TrimEnd('/')
                if ($candidate -eq $root) { $alias = $p.Name; break }
            }
        }
        if ($alias) {
            $groupNames = @()
            if ($null -ne $cfg.groups) {
                foreach ($g in $cfg.groups.PSObject.Properties) {
                    # -ccontains, NOT -contains: the default is case-INSENSITIVE,
                    # while jq's `index` in the .sh twin and the server's own
                    # HashMap::contains_key are both case-sensitive. A hand-added
                    # group member typed from the folder name ("NWND.ACME"
                    # against alias "NWND.Acme") is pruned from the group by
                    # ReposConfig::reconcile(), so advertising that group here
                    # would send the agent to search a group the repo is no
                    # longer in - the silent empty result this hook exists to
                    # prevent - and would disagree with the .sh twin as well.
                    if (@($g.Value) -ccontains $alias) { $groupNames += $g.Name }
                }
            }
            $sorted = [string[]]$groupNames
            [Array]::Sort($sorted, [System.StringComparer]::Ordinal)
            if ($sorted.Count -gt 0) {
                $groupClause = 'group="' + ($sorted -join '" | group="') + '"'
                $scopeBlock = @"
Multi-repo serve mode - the scope for THIS working directory is already
resolved; do not guess it from the folder name, the alias often differs:
  project="$alias"
  $groupClause
Pass project= to search this repo alone, or one of the group= values above to
search it together with its siblings. A sibling repo often holds what this one
does not (configuration is commonly kept in a separate repo), so try group=
before concluding something is absent. Call status(kind="projects") to see
which repos a group covers.
"@
            } else {
                $scopeBlock = @"
Multi-repo serve mode - the scope for THIS working directory is already
resolved; do not guess it from the folder name, the alias often differs:
  project="$alias"
It belongs to no group, so project= is the scope to pass.
"@
            }
        }
    }
} catch {
    # Any failure (no git, unreadable or malformed repos.json) keeps the
    # generic $scopeBlock above. Never a reason to fail an agent spawn.
}
$scopeBlock = $scopeBlock.TrimEnd("`r", "`n")

$preamble = @"
$marker
SEARCH RULE - read before doing anything:
codesearch is the preferred search tool for code in the current repo.
codesearch tools are DEFERRED: they do NOT appear in your tool list until you load them.

Load them first (before any Grep):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Then use:
  mcp__codesearch__search(query, mode="semantic")             -- concepts, identifiers, cross-file lookup
  mcp__codesearch__search(query, mode="literal", regex=true)  -- exact patterns / regex
  mcp__codesearch__find(symbol, kind="definition")             -- where a symbol is defined
  mcp__codesearch__find(symbol, kind="usages")                 -- all call sites
  mcp__codesearch__explore(target, kind="outline")             -- file/class structure
  mcp__codesearch__get_chunk(chunk_id)                         -- read a specific code chunk

$scopeBlock

Fall back to Grep only after codesearch returns no useful results, or when the
path is outside the current repo (codesearch covers internal paths only unless
you're in multi-repo serve mode with an explicit group).

Glob is NOT a substitute for codesearch on content lookup, but it IS the right
tool for filename and existence questions, and nothing here discourages that
use: codesearch has no file-listing primitive, and an empty codesearch result
cannot distinguish "not present" from "not indexed" (.venv, node_modules,
build output and binary assets are never indexed). Glob answers over the real
filesystem, so it is the reliable way to get a NEGATIVE answer.

If ToolSearch returns no codesearch tools, codesearch is not active for this
session - proceed with Grep/Glob as normal.
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
