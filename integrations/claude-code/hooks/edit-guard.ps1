# PreToolUse hook: require a recent codesearch consultation before edits.
# Fires on Edit/Write/MultiEdit. Windows twin of edit-guard.sh — keep the
# two behaviorally equivalent.
#
# When the edited file's repo is codesearch-registered (git root listed in
# ~/.codesearch/repos.json, honoring CODESEARCH_REPOS_CONFIG, or a
# CODESEARCH_SERVER opt-in), every touched file needs a marker proving the
# agent consulted codesearch for exactly that path within the last 5
# minutes: mcp__codesearch__find_impact for SCIP-backed languages
# (.cs .ts .tsx .mts .cts), mcp__codesearch__find kind="usages" for
# everything else. Markers are written by the edit-guard-post PostToolUse
# companion into $env:TEMP\.codesearch-edit-guard-state.json.
#
# Lenient acceptance: ANY marker for the path within the window lets the
# edit through — the marker proves the agent consulted codesearch for this
# file, which is the point.
#
# Fail-open on: no target paths, paths outside any git repo, unregistered
# repos, or a crashed hook — all allow. Missing/corrupt state counts as NOT
# consulted (deny on covered repos, allow everywhere else); so does an
# expired marker, and the next consultation refreshes it.
#
# Install: see ../README.md (or run `codesearch hooks claude install`).

$ErrorActionPreference = 'Stop'

try {
    . (Join-Path $PSScriptRoot 'codesearch-common.ps1')
    $raw = [Console]::In.ReadToEnd()
    if ([string]::IsNullOrWhiteSpace($raw)) { exit 0 }
    $data = $raw | ConvertFrom-Json
} catch {
    exit 0  # never block a tool call because the hook failed to parse its own input
}

$tool = $data.tool_name
$inp  = $data.tool_input

if ($tool -ne 'Edit' -and $tool -ne 'Write' -and $tool -ne 'MultiEdit') { exit 0 }
if ($null -eq $inp) { exit 0 }

# Target paths: the primary file_path, plus (defensively) per-edit entries.
$candidates = @()
$names = @($inp.PSObject.Properties.Name)
if ($names -contains 'file_path') { $candidates += [string]$inp.file_path }
if (($names -contains 'edits') -and $null -ne $inp.edits) {
    foreach ($e in @($inp.edits)) {
        if ($null -ne $e -and @($e.PSObject.Properties.Name) -contains 'file_path') {
            $candidates += [string]$e.file_path
        }
    }
}
$paths = @($candidates | Where-Object { -not [string]::IsNullOrEmpty($_) } | Select-Object -Unique)
if ($paths.Count -eq 0) { exit 0 }  # nothing attributable -> allow (fail-open)

$stateFile = Join-Path $env:TEMP '.codesearch-edit-guard-state.json'
$window    = 300
$now       = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()

function Test-CheckedRecently {
    param([string]$Key)
    if (-not (Test-Path -LiteralPath $stateFile -PathType Leaf)) { return $false }
    try {
        $state = Get-Content -LiteralPath $stateFile -Raw | ConvertFrom-Json
        $entry = $state.PSObject.Properties[$Key]
        if ($null -eq $entry) { return $false }
        $ts = [long]$entry.Value.ts
        return (($now - $ts) -lt $window)
    } catch {
        return $false  # corrupt/unreadable state counts as absent
    }
}

$failPath = $null
$failTool = $null
foreach ($p in $paths) {
    $root = Resolve-TargetGitRoot $p
    if (-not $root) { continue }                    # outside any git repo -> allow
    if (-not (Test-CodesearchTargetRegistered $root)) { continue }  # unregistered -> fail-open
    $key = ConvertTo-StateKey $p
    if (Test-CheckedRecently $key) { continue }
    $failPath = $p
    $ext = [System.IO.Path]::GetExtension($p).TrimStart('.').ToLowerInvariant()
    if (@('cs', 'ts', 'tsx', 'mts', 'cts') -contains $ext) {
        $failTool = 'mcp__codesearch__find_impact (SCIP-backed language)'
    } else {
        $failTool = 'mcp__codesearch__find(symbol, kind="usages")'
    }
    break
}

if ($null -eq $failPath) { exit 0 }  # every path checked recently -> allow

$msg = @"
edit-guard: this edit needs a codesearch consultation first.

Blocked path: $failPath
Required call: $failTool — on that exact file.

Any outcome counts: "no results" and "no SCIP backend" still prove you
consulted codesearch for this file. Run the call, then retry the SAME
edit — it stays allowed for 5 minutes.

Why: find_impact (C#/TS) / find kind="usages" (other languages) before
edits keeps refactors caller-aware; this guard makes that protocol
structural for codesearch-registered repos. Unregistered repos, non-git
paths and unparseable events fail open and are never blocked.
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
