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
# Windows twin of web-guard.sh.
#
# Install: see ../README.md (or run `codesearch hooks claude install`).

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

if ($tool -ne 'WebSearch' -and $tool -ne 'WebFetch') { exit 0 }
if ($null -eq $inp) { exit 0 }

# Query (WebSearch) or target URL (WebFetch) — used for the cache key + guidance.
$names = @($inp.PSObject.Properties.Name)
$q = if ($names -contains 'query') { [string]$inp.query }
     elseif ($names -contains 'url') { [string]$inp.url }
     else { '' }

# ------------------------------------------------------------------
# 1. Are there any remote doc mounts to steer toward?
#
# Mounts live in repos.json under `.remote_mounts` (canonical "<peer>/<alias>"
# names — the opt-in allowlist). No mounts -> nothing to prefer -> allow.
# ------------------------------------------------------------------
$config = if ($env:CODESEARCH_REPOS_CONFIG) { $env:CODESEARCH_REPOS_CONFIG }
          else { Join-Path $HOME '.codesearch/repos.json' }
if (-not (Test-Path $config)) { exit 0 }

$mountList = @()
try {
    $cfg = Get-Content $config -Raw | ConvertFrom-Json
    if ($cfg.PSObject.Properties.Name -contains 'remote_mounts' -and $cfg.remote_mounts) {
        $mountList = @($cfg.remote_mounts)
    }
} catch {
    exit 0  # unreadable/invalid config -> don't get in the way
}
if ($mountList.Count -eq 0) { exit 0 }
$mounts = $mountList -join ', '

# ------------------------------------------------------------------
# 2. Retry cache: same query blocked recently -> let it through.
# ------------------------------------------------------------------
$cacheFile = Join-Path $env:TEMP '.codesearch-web-guard.json'
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

$cacheKey = "$q"
if ($cache.ContainsKey($cacheKey)) {
    exit 0  # already blocked once this window -> allow the retry
}

$cache[$cacheKey] = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
try {
    $cache | ConvertTo-Json -Compress | Set-Content $cacheFile -NoNewline
} catch {}

# ------------------------------------------------------------------
# 3. Block with actionable guidance.
# ------------------------------------------------------------------
$msg = @"
codesearch has remote documentation mounts — search those before the web.
Mounted remotes: $mounts

These indexed mounts often answer product/API/docs questions more precisely
(and more currently) than a web search. Try codesearch first.

Step 1 — load the deferred MCP tool schemas (one-time per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__get_chunk")

Step 2 — search the relevant mount (compact=false reads matching content inline):
  mcp__codesearch__search(query="$q", project="<peer/alias>", compact=false)
  mcp__codesearch__get_chunk(chunk_ref="<peer/alias:id from a result>")  # full context

Pick the relevant project from the mounted remotes above.

This exact $tool call is auto-unblocked if you retry it within 5 minutes
(i.e. the mounts didn't have the answer — go ahead and use the web).
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
