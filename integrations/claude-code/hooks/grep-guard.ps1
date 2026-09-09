# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
#
# Why this exists: Claude Code loads MCP tool schemas lazily. codesearch's own
# `initialize` instructions (see docs) are advisory only — nothing stops the
# model from reaching for the always-on Grep/Glob tools instead, especially
# under time pressure. This hook makes the preference structural instead of
# advisory: a Grep call against an indexed internal path is blocked with
# actionable guidance for as long as the codesearch serve hub is reachable.
#
# Grep is auto-allowed ONLY when codesearch is genuinely unreachable ("plat").
# Crucially, a low-confidence / empty codesearch *result* is a SUCCESSFUL call
# ("reformulate your query"), NOT "codesearch is down" — so it must never open
# the grep escape hatch. The previous version used a blind "same query retried
# within 5 min" proxy that could not tell those two apart and leaked grep on
# every low-confidence result. We now probe the unauthenticated /healthz
# liveness endpoint directly, which is the only signal that actually means
# "codesearch is down".
#
# Blocks the Grep call when ALL of:
#   - the search target resolves to a git repo (its OWN root, not the cwd's —
#     absolute paths into a different repo resolve against THAT repo, todo
#     #54), AND
#   - codesearch covers THAT repo (its git root registered in the serve
#     hub's repos.json, or the CODESEARCH_SERVER opt-in for remote/hub-only
#     setups), AND
#   - the codesearch serve hub answers its /healthz liveness probe (it's UP)
#
# Passes through (exit 0, no block) when:
#   - the target is not inside any git repo (external path — grep is the
#     right tool there)
#   - codesearch does not cover the target's repo (git root not registered
#     in repos.json, no CODESEARCH_SERVER)
#   - the codesearch serve hub does not answer /healthz — it's down, so grep
#     is genuinely all you have
#
# When the target repo is covered and LIVE but MID-REINDEX (branch switch
# full refresh, todo #55), the deny message is a WAIT-AND-RETRY instruction
# instead of the standard "use codesearch" one — searching a mid-rebuild
# index returns stale/empty results and must not degrade into a manual grep
# approval on every routine checkout.
#
# Install: see ../README.md (or run ../install.ps1 to wire this up automatically).

$ErrorActionPreference = 'Stop'

# Shared target-resolution helpers live in codesearch-common.ps1 (shared
# with edit-guard). Missing/broken helper file -> fail open, never block.
try {
    . (Join-Path $PSScriptRoot 'codesearch-common.ps1')
} catch {
    exit 0
}

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
# 1+2. Resolve the TARGET repo (the repo this Grep is aimed at) and check
#      codesearch coverage THERE — never in the hook's cwd.
#
# The resolution (absolute path -> the target's own git root, empty/
# relative -> the cwd repo) lives in codesearch-common.ps1 and is shared
# with edit-guard. History (#54, #199): see codesearch-common.sh.
# ------------------------------------------------------------------
$targetRoot = Resolve-TargetGitRoot $path

# Not inside any git repo (or git unusable) -> external target: grep is right.
if (-not $targetRoot) { exit 0 }

# Coverage: the TARGET repo's git root is REGISTERED with the serve hub
# (~/.codesearch/repos.json — the same list the hub itself resolves by),
# matching the bash twin and edit-guard; a running serve hub alone is NOT a
# signal (it covers many repos and being alive says nothing about this one).
# CODESEARCH_SERVER stays the explicit opt-in for pure remote-serve setups.
# Fails open: missing/malformed repos.json counts as unregistered -> allow.
if (-not (Test-CodesearchTargetRegistered $targetRoot)) { exit 0 }

# ------------------------------------------------------------------
# 3. Is the codesearch serve hub actually UP right now? (Liveness probe.)
#
# This is the ONLY condition under which grep is auto-allowed: codesearch is
# genuinely unreachable ("plat"). We probe the unauthenticated /healthz
# liveness endpoint (fixed {"status":"ok"} body, no API key required). A
# reachable server -> DENY grep and force a codesearch reformulation, even
# when a previous codesearch call returned a low-confidence / empty result —
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
    param([string]$Base)
    try {
        # Short timeout keeps grep latency low; /healthz answers instantly.
        $null = Invoke-WebRequest -Uri "$Base/healthz" -TimeoutSec 2 -UseBasicParsing
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
$base = Get-CodesearchBaseUrl
if (-not (Test-CodesearchLive -Base $base)) { exit 0 }

# ------------------------------------------------------------------
# 3.5 Is the TARGET repo mid-reindex right now? (Freshness probe, #55.)
#
# Liveness (/healthz) says the server is UP; it says nothing about index
# FRESHNESS. Right after a branch switch the serve watcher fires a full
# refresh, and searches against the mid-rebuild index return stale/empty
# results — which used to push the agent into a manual grep approval on
# every routine checkout (the exact "stale after branch switch" report).
# GET /indexing?path=<repo root> resolves the target to its registered repo
# and reports an active reindex. When one is in flight, deny with a
# WAIT-AND-RETRY instruction: the tree did not change, the index is just
# catching up — waiting beats grepping.
#
# Backward compat: an older serve without this endpoint answers 404, the
# probe is skipped (catch below), and behaviour is exactly the pre-#55 deny.
# ------------------------------------------------------------------
try {
    $enc  = [uri]::EscapeDataString(($targetRoot -replace '\\', '/'))
    $resp = Invoke-WebRequest -Uri "$base/indexing?path=$enc" -TimeoutSec 2 -UseBasicParsing
    $fresh = $resp.Content | ConvertFrom-Json
    if ($fresh.covered -eq $true -and $fresh.indexing -eq $true) {
        $waitMsg = @"
codesearch is LIVE, but THIS repo's index is being rebuilt right now (a
branch switch or file change fired the serve watcher's full refresh).
Searching immediately would return stale or empty results — that is the
rebuild in progress, not a miss, and NOT a reason to grep.

WAIT 15-30 seconds (Bash: sleep 20), then RETRY your codesearch call — it
will answer normally once the rebuild lands. The working tree did not
change; only the index is catching up, so grep adds nothing here.

If the rebuild still has not landed after ~2 minutes, run your codesearch
call anyway (a partially fresh index still beats grep) or ask the user how
to proceed.
"@
        $waitOut = @{
            hookSpecificOutput = @{
                hookEventName            = 'PreToolUse'
                permissionDecision       = 'deny'
                permissionDecisionReason = $waitMsg
            }
        }
        $waitOut | ConvertTo-Json -Depth 10 -Compress
        exit 0
    }
} catch {
    # 404 (older serve) or probe failure: no freshness signal — fall through
    # to the standard deny below.
}

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
$msg = @"
codesearch is LIVE for this repo (its /healthz probe just answered) — use it,
do NOT fall back to Grep. Grep on an indexed internal path is only auto-allowed
when the codesearch serve hub is actually DOWN, which it is not right now.

IMPORTANT: a low-confidence or EMPTY codesearch result is a SUCCESSFUL call that
means "reformulate your query" — it does NOT mean codesearch is down and it will
NOT unblock Grep. Reformulate instead of grepping.

Step 1 — load the deferred MCP tool schemas (Claude Code defers all MCP tools;
this is a one-time step per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Step 2 — pick the RIGHT tool (this is usually why a query came back empty):
  find(symbol="Name", kind="definition")   -- known symbol / type / function definition
  find(symbol="Name", kind="usages")       -- all call sites of a known symbol
  explore(kind="outline", target="path")   -- every symbol in one file
  search(query="concept", mode="semantic") -- concepts / cross-file, the DEFAULT
  search(query="exact", mode="literal", regex=true) -- exact syntax / pattern

Query hygiene (this is what produces "low_confidence: []"):
  * Do NOT paste grep-style multi-term alternations ("a|b|c", "::", "fn foo(")
    into search — BM25 tokenises on punctuation and the match scores below the
    relevance floor, so you get an empty result even though the string exists.
  * Use ONE clean term, or switch to find()/explore() for exact symbols.

Multi-repo serve mode: if the call returns a "scope_required" or "Unknown alias"
error, you MUST pass project="<repo-alias>" (single repo) or group="<group>"
(cross-repo). The error response LISTS the valid available_projects /
available_groups — pick from that list (the alias may differ from the folder
name).

Grep is always allowed for targets outside any git repo, and for repos
codesearch does not cover (no local index, no CODESEARCH_SERVER).
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
