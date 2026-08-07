# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
#
# Why this exists: Claude Code loads MCP tool schemas lazily. codesearch's own
# `initialize` instructions (see docs) are advisory only - nothing stops the
# model from reaching for the always-on Grep/Glob tools instead, especially
# under time pressure. This hook makes the preference structural instead of
# advisory: a Grep call against an indexed internal path is blocked with
# actionable guidance for as long as the codesearch serve hub is reachable.
#
# Grep is auto-allowed ONLY when codesearch is genuinely unreachable ("plat").
# Crucially, a low-confidence / empty codesearch *result* is a SUCCESSFUL call
# ("reformulate your query"), NOT "codesearch is down" - so it must never open
# the grep escape hatch. The previous version used a blind "same query retried
# within 5 min" proxy that could not tell those two apart and leaked grep on
# every low-confidence result. We now probe the unauthenticated /healthz
# liveness endpoint directly, which is the only signal that actually means
# "codesearch is down".
#
# Blocks the Grep call when ALL of:
#   - the search path is internal (empty/relative, or absolute-but-inside the
#     current git repo), AND
#   - codesearch covers THIS repo (indexed .codesearch.db at git root, or the
#     CODESEARCH_SERVER opt-in for remote/hub-only setups), AND
#   - the codesearch serve hub answers its /healthz liveness probe (it's UP)
#
# Passes through (exit 0, no block) when:
#   - the path is outside the current git repo (codesearch doesn't cover
#     arbitrary external paths well; grep is the right tool there)
#   - codesearch does not cover this repo (no local index, no CODESEARCH_SERVER)
#   - the codesearch serve hub does not answer /healthz - it's down, so grep
#     is genuinely all you have
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

$names = @($inp.PSObject.Properties.Name)
$path  = if ($names -contains 'path') { [string]$inp.path } else { '' }

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
function Test-CodesearchCoversRepo {
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

if (-not (Test-CodesearchCoversRepo)) { exit 0 }

# ------------------------------------------------------------------
# 3. Is the codesearch serve hub actually UP right now? (Liveness probe.)
#
# This is the ONLY condition under which grep is auto-allowed: codesearch is
# genuinely unreachable ("plat"). We probe the unauthenticated /healthz
# liveness endpoint (fixed {"status":"ok"} body, no API key required). A
# reachable server -> DENY grep and force a codesearch reformulation, even
# when a previous codesearch call returned a low-confidence / empty result -
# an empty *result* is a SUCCESSFUL call, not a dead server, so it must NOT
# open the escape hatch. Only a connection-level failure (refused / DNS /
# timeout) means the server is down -> ALLOW grep.
#
# Base URL resolution (mirrors codesearch src/constants.rs):
#   CODESEARCH_SERVER (full base URL, e.g. http://host:port)
#   > http://127.0.0.1:$CODESEARCH_SERVE_PORT
#   > http://127.0.0.1:39725  (DEFAULT_SERVE_URL / DEFAULT_SERVE_PORT)
# ------------------------------------------------------------------
function Get-CodesearchBaseUrl {
    if ($env:CODESEARCH_SERVER)     { return ($env:CODESEARCH_SERVER.TrimEnd('/')) }
    if ($env:CODESEARCH_SERVE_PORT) { return "http://127.0.0.1:$($env:CODESEARCH_SERVE_PORT)" }
    return 'http://127.0.0.1:39725'
}

function Test-CodesearchLive {
    $base = Get-CodesearchBaseUrl
    try {
        # Short timeout keeps grep latency low; /healthz answers instantly.
        $null = Invoke-WebRequest -Uri "$base/healthz" -TimeoutSec 2 -UseBasicParsing
        return $true
    } catch {
        # An HTTP error RESPONSE (4xx/5xx) still proves the server is reachable
        # and up; only a connection-level failure means it's genuinely down.
        try {
            if ($null -ne $_.Exception -and $null -ne $_.Exception.Response) { return $true }
        } catch {}
        return $false
    }
}

# codesearch is DOWN -> grep is genuinely all you have, let it through.
if (-not (Test-CodesearchLive)) { exit 0 }

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
$msg = @"
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
