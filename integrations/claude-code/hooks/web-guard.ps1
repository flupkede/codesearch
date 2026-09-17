# PreToolUse hook: steer WebSearch/WebFetch toward codesearch remote doc mounts.
#
# Why this exists: when codesearch has remote documentation projects mounted
# (e.g. cloud/inriver, cloud/example-dam), those indexes usually answer product /
# API / docs questions more precisely — and more currently — than an open web
# search. Nothing structurally stops the model from reaching for the always-on
# WebSearch/WebFetch tools first, so this hook makes the preference structural:
# the FIRST WebSearch/WebFetch *about a mounted product* is blocked with
# actionable guidance; a further call about that same mount within 5 minutes
# (i.e. the mounts didn't have the answer) is let through.
#
# TOPIC SCOPING (the important part): a mount only earns the right to intercept
# queries about ITS OWN subject. Mounting cloud/acme says "I have the Acme
# docs indexed" — it says nothing about Rust, Claude Code, or Azure CLI. So the
# hook first decides whether the query is plausibly about a mounted product, and
# stays out of the way entirely when it isn't. A mount's alias is its keyword
# (cloud/acme -> "acme"); anything else it should answer for is declared in
# repos.json under `.remote_mount_topics` — see README.
#
# Passes through (exit 0, no block) when:
#   - there are NO remote mounts to steer toward (nothing indexed to prefer)
#   - the query/URL doesn't mention any mounted product or its declared topics.
#     The mounts are a small opt-in allowlist of specific vendor doc sets, so
#     they cannot answer a question about an unrelated vendor, a language
#     runtime, or a model card. Blocking such a call buys nothing and costs a
#     guaranteed wasted round-trip.
#   - the same MOUNT was already steered for in the last 5 minutes. Keyed on the
#     matched mount, NOT on the exact query string, on purpose: the natural
#     follow-up after "the mirror had nothing" is a REFINED web query, and an
#     exact-string key treats every refinement as a fresh first attempt and
#     blocks it again — punishing precisely the correct behaviour.
#
# TWO SILENT-DEGRADATION TRAPS, both of which turn this hook into a no-op
# without any visible error (an "allow" is silent, so a broken gate looks
# exactly like a working one):
#   - NEVER split a mount name with .Split(@('/','\'), [StringSplitOptions]...):
#     PowerShell coerces that array into the single-String separator overload and
#     looks for the literal "/ \", so nothing ever splits, no alias is ever
#     derived, and the gate matches NOTHING. Use `-split` with a regex, or
#     .Substring(.LastIndexOf('/') + 1) as below.
#   - ALWAYS strip CR from values read out of repos.json. A trailing \r silently
#     survives into the alias ("acme`r"), which then never matches anything.
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

# Query (WebSearch) or target URL (WebFetch) — used for matching, the cache key
# and the guidance text.
$names = @($inp.PSObject.Properties.Name)
$q = if ($names -contains 'query') { [string]$inp.query }
     elseif ($names -contains 'url') { [string]$inp.url }
     else { '' }
if ([string]::IsNullOrWhiteSpace($q)) { exit 0 }

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
$topicMap  = $null
try {
    $cfg = Get-Content $config -Raw | ConvertFrom-Json
    if ($cfg.PSObject.Properties.Name -contains 'remote_mounts' -and $cfg.remote_mounts) {
        # Strip CR — see the trap note in the header.
        $mountList = @($cfg.remote_mounts | ForEach-Object { ([string]$_).Trim("`r", "`n", ' ') } |
                       Where-Object { $_ })
    }
    if ($cfg.PSObject.Properties.Name -contains 'remote_mount_topics') {
        $topicMap = $cfg.remote_mount_topics
    }
} catch {
    exit 0  # unreadable/invalid config -> don't get in the way
}
if ($mountList.Count -eq 0) { exit 0 }

# ------------------------------------------------------------------
# 2. Is this query actually ABOUT one of the mounted products?
#
# Keywords per mount: the alias itself, plus any extra terms declared in
# `.remote_mount_topics` (keyed by the full "<peer>/<alias>" or the bare alias).
# Matching is case-insensitive on a word-ish boundary, so "acme" hits
# "Acme DAM API" and "help.acme.com" but not "acmexyz".
#
# No match -> this simply isn't the mirror's subject -> allow, silently.
# ------------------------------------------------------------------
$haystack = $q.ToLowerInvariant()

function Test-KeywordMatch {
    param([string]$Haystack, [string]$Keyword)
    if ([string]::IsNullOrWhiteSpace($Keyword)) { return $false }
    $pattern = '(^|[^a-z0-9])' + [regex]::Escape($Keyword) + '([^a-z0-9]|$)'
    return [regex]::IsMatch($Haystack, $pattern, [System.Text.RegularExpressions.RegexOptions]::IgnoreCase)
}

function Get-MountTopics {
    param($TopicMap, [string]$Mount, [string]$Alias)
    if ($null -eq $TopicMap) { return @() }
    foreach ($key in @($Mount, $Alias)) {
        $prop = $TopicMap.PSObject.Properties[$key]
        if ($prop -and $prop.Value) {
            return @($prop.Value | ForEach-Object { ([string]$_).Trim("`r", "`n", ' ') } |
                     Where-Object { $_ })
        }
    }
    return @()
}

$matched = @()
foreach ($mount in $mountList) {
    # Alias = the part after the last '/' — the PowerShell equivalent of the
    # shell twin's ${mount##*/}. See the header for why .Split(@('/','\'), ...)
    # must NOT be used here.
    $aliasName = ([string]$mount).Substring(([string]$mount).LastIndexOf('/') + 1)

    $hit = $false
    if (Test-KeywordMatch -Haystack $haystack -Keyword $aliasName) {
        $hit = $true
    } else {
        foreach ($topic in (Get-MountTopics -TopicMap $topicMap -Mount $mount -Alias $aliasName)) {
            if (Test-KeywordMatch -Haystack $haystack -Keyword $topic) {
                $hit = $true
                break
            }
        }
    }

    if ($hit) { $matched += $mount }
}

if ($matched.Count -eq 0) { exit 0 }

$relevant = $matched -join ', '
$primary  = $matched[0]

# ------------------------------------------------------------------
# 3. Retry cache: same mount steered for recently -> let it through.
#    Covers "tried the mounts, they had nothing, now use the web".
# ------------------------------------------------------------------
$cacheFile = Join-Path $env:TEMP '.codesearch-web-guard.json'
$cacheTTL  = 300  # seconds
$now       = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()

$cache = @{}
if (Test-Path $cacheFile) {
    try {
        $stored = Get-Content $cacheFile -Raw | ConvertFrom-Json
        foreach ($prop in $stored.PSObject.Properties) {
            if (($now - [long]$prop.Value) -lt $cacheTTL) {
                $cache[$prop.Name] = [long]$prop.Value
            }
        }
    } catch {}
}

# Keyed on tool + primary matched mount, NOT the raw query: one steer per vendor
# per window. An exact-query key made every refinement of the search terms look
# like a first attempt and blocked it again, which is the opposite of the intent.
$cacheKey = "$tool|$primary"
if ($cache.ContainsKey($cacheKey)) {
    exit 0  # already steered once this window -> allow the follow-up
}

$cache[$cacheKey] = $now
try {
    $cache | ConvertTo-Json -Compress | Set-Content $cacheFile -NoNewline
} catch {}

# ------------------------------------------------------------------
# 4. Block with actionable guidance — naming ONLY the mounts that matched.
# ------------------------------------------------------------------
$msg = @"
This looks like a question about a product whose documentation codesearch has
indexed — search that mirror before the web.
Relevant mount(s): $relevant

These indexed mounts often answer product/API/docs questions more precisely
(and more currently) than a web search, and they cover vendor sites that need a
login and would fail an anonymous fetch anyway.

Step 1 — load the deferred MCP tool schemas (one-time per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__get_chunk")

Step 2 — search the relevant mount (compact=false reads matching content inline):
  mcp__codesearch__search(query="$q", project="$primary", compact=false)
  mcp__codesearch__get_chunk(chunk_ref="${primary}:<id from a result>")  # full context

For a canonical source link, read the doc's front-matter chunk (start_line 0)
and cite its ``url:`` field verbatim rather than reconstructing a URL.

If the mount does NOT have the answer, go straight to the web: any further
$tool call about '$primary' is allowed for the next 5 minutes. You do NOT need
to repeat this call verbatim — refining your search terms is fine and will not
be blocked again.
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
