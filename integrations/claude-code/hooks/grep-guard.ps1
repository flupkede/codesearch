# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
#
# Why this exists: Claude Code loads MCP tool schemas lazily. codesearch's own
# `initialize` instructions (see docs) are advisory only — nothing stops the
# model from reaching for the always-on Grep/Glob tools instead, especially
# under time pressure. This hook makes the preference structural instead of
# advisory: the FIRST Grep call against an internal path is blocked with
# actionable guidance; if the same query is retried within 5 minutes (i.e.
# codesearch was tried and came up empty), it is let through.
#
# Blocks the first Grep call for a given (pattern, path) pair when:
#   - codesearch appears to be active (running process, CODESEARCH_SERVER env,
#     or an indexed .codesearch.db at the git root), AND
#   - the search path is internal (empty/relative, or absolute-but-inside the
#     current git repo)
#
# Passes through (exit 0, no block) when:
#   - codesearch is not running and no local index is found — grep is all
#     you have, so don't get in the way
#   - the path is outside the current git repo (codesearch doesn't cover
#     arbitrary external paths well; grep is the right tool there)
#   - the same (pattern, path) pair was already blocked in the last 5 minutes
#     (covers the legitimate "codesearch found nothing, now try grep" case)
#
# Install: see ../README.md (or run ../install.ps1 to wire this up automatically).

$ErrorActionPreference = 'Stop'

try {
    $raw = [Console]::In.ReadToEnd()
    if ([string]::IsNullOrWhiteSpace($raw)) { exit 0 }
    $data = $raw | ConvertFrom-Json
} catch {
    exit 0  # never block a tool call because the hook failed to parse its own input
}

$tool = $data.tool_name
$inp  = $data.tool_input

if ($tool -ne 'Grep') { exit 0 }
if ($null -eq $inp)   { exit 0 }

$names   = @($inp.PSObject.Properties.Name)
$path    = if ($names -contains 'path')    { [string]$inp.path }    else { '' }
$pattern = if ($names -contains 'pattern') { [string]$inp.pattern } else { '' }

# ------------------------------------------------------------------
# 1. Is the path internal to the current repo?
# ------------------------------------------------------------------
$isInternal = $true
if ($path -and $path -ne '.' -and $path -ne './') {
    $normPath = $path.TrimEnd('/\')
    # Absolute paths (Windows drive letter, or Git-Bash /c/... style)
    if ($normPath -match '^([A-Za-z]:[\\/]|/[a-zA-Z]/|//)') {
        try {
            $gr = (& git rev-parse --show-toplevel 2>$null)
            if ($LASTEXITCODE -eq 0 -and $gr) {
                $gr  = $gr.Trim() -replace '[/\\]', [System.IO.Path]::DirectorySeparatorChar
                $abs = $normPath   -replace '[/\\]', [System.IO.Path]::DirectorySeparatorChar
                if (-not $abs.StartsWith($gr, [System.StringComparison]::OrdinalIgnoreCase)) {
                    $isInternal = $false
                }
            }
        } catch {
            $isInternal = $false  # can't determine git root -> assume external, allow grep
        }
    }
    # Relative paths ("src/", "../sibling/") stay internal = $true
}

if (-not $isInternal) { exit 0 }

# ------------------------------------------------------------------
# 2. Is codesearch actually available FOR THIS REPO? Don't block if it isn't.
#
# NOTE: we deliberately do NOT treat "a codesearch process is running" as
# sufficient. codesearch commonly runs as a persistent background `serve`
# hub covering many registered repos (`codesearch index list`) — that
# process is alive nearly all the time on a dev machine, regardless of
# whether the CURRENT directory is one of the repos it actually indexes.
# Using process-presence alone made this hook fire in every directory on
# the machine, including ones with no index at all. A local `.codesearch.db`
# at the git root is the precise, fast signal that THIS repo is indexed.
# ------------------------------------------------------------------
function Test-CodesearchAvailable {
    try {
        $gr = (& git rev-parse --show-toplevel 2>$null)
        if ($LASTEXITCODE -eq 0 -and $gr) {
            $gr = $gr.Trim()
            if (Test-Path (Join-Path $gr '.codesearch.db')) { return $true }
        }
    } catch {}

    # Explicit opt-in escape hatch for pure remote-serve setups with no local
    # .codesearch.db (this repo's index lives only on a remote `codesearch
    # serve` host). Requires the user to consciously set this env var, so it
    # can't spuriously fire the way "any process running" did.
    if ($env:CODESEARCH_SERVER) { return $true }

    return $false
}

if (-not (Test-CodesearchAvailable)) { exit 0 }

# ------------------------------------------------------------------
# 3. Retry cache: same (pattern, path) blocked recently -> let it through.
#    Covers "tried codesearch, it returned nothing, falling back to grep".
# ------------------------------------------------------------------
$cacheFile = Join-Path $env:TEMP '.codesearch-grep-guard.json'
$cacheTTL  = 300  # seconds

$cache = @{}
if (Test-Path $cacheFile) {
    try {
        $stored = Get-Content $cacheFile -Raw | ConvertFrom-Json
        $now    = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
        foreach ($prop in $stored.PSObject.Properties) {
            if (($now - [long]$prop.Value) -lt $cacheTTL) {
                $cache[$prop.Name] = [long]$prop.Value
            }
        }
    } catch {}
}

$cacheKey = "$pattern|$path"
if ($cache.ContainsKey($cacheKey)) {
    exit 0  # already blocked once this window -> allow the retry
}

$cache[$cacheKey] = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
try {
    $cache | ConvertTo-Json -Compress | Set-Content $cacheFile -NoNewline
} catch {}

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
$msg = @"
codesearch is active for this repo — try it before Grep for code discovery.

Step 1 — load the deferred MCP tool schemas (Claude Code defers all MCP tools;
this is a one-time step per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Step 2 — search:
  mcp__codesearch__search(query="$pattern", mode="semantic")             -- concepts, identifiers, cross-file
  mcp__codesearch__search(query="$pattern", mode="literal", regex=true)  -- exact pattern / regex
  mcp__codesearch__find(symbol="...", kind="definition")                 -- symbol definition
  mcp__codesearch__find(symbol="...", kind="usages")                     -- all call sites

Multi-repo serve mode: if the search returns a "scope_required" or
"Unknown alias" error, you MUST pass project="<repo-alias>" (single repo) or
group="<group>" (cross-repo). The error response LISTS the valid
available_projects / available_groups — pick from that list (the alias may
differ from the folder name). Example:
  mcp__codesearch__search(query="$pattern", mode="semantic", project="<alias-from-error>")

This exact Grep call is auto-unblocked if you retry it within 5 minutes
(i.e. codesearch returned nothing useful — go ahead and grep).
Grep is always allowed for paths outside the current repo.
"@

$out = @{
    hookSpecificOutput = @{
        hookEventName            = 'PreToolUse'
        permissionDecision       = 'deny'
        permissionDecisionReason = $msg
    }
}
$out | ConvertTo-Json -Depth 10 -Compress
exit 0
