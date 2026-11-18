# PostToolUse companion for edit-guard: records a "codesearch consulted"
# marker per file path after qualifying MCP calls, into the same state file
# edit-guard reads. Windows twin of edit-guard-post.sh — keep the two
# behaviorally equivalent. Fires on:
#   - mcp__codesearch__find_impact   (ANY outcome counts — including "no
#     results" / "no SCIP backend": counting failures too is what keeps the
#     guard from blocking permanently)
#   - mcp__codesearch__find with kind="usages" (string compare HERE — the
#     matcher regex can only filter on tool name)
#
# Marker sources (all available ones are written):
#   1. a file-ish INPUT field (file/path/file_path)
#   1b. a PATH-LIKE `symbol` input (separator or extension): the exact
#      find(symbol=<path>, kind="usages") call edit-guard demands marks the
#      path it names — project root, else cwd repo, else the shell cwd.
#   2. every path the RESPONSE mentions — file-path-shaped values only
#      (separator or extension; a bare symbol once resolved to a bogus
#      <root>/git_repo key), joined onto the `project` alias's registered
#      root or, for cwd-scoped calls, the `cwd` repo's registered root
#      (both shape candidates — repo-relative vs alias-prefixed).
#   3. when neither yields a path — empty result / "no SCIP backend" — a
#      REPO-level marker on the project's registered root, else the cwd
#      repo's root when registered.
#
# PostToolUse hooks cannot block anything: no decision is emitted and this
# script ALWAYS exits 0.

$ErrorActionPreference = 'Stop'

try {
    . (Join-Path $PSScriptRoot 'codesearch-common.ps1')
    $raw = [Console]::In.ReadToEnd()
    if ([string]::IsNullOrWhiteSpace($raw)) { exit 0 }
    $data = $raw | ConvertFrom-Json
} catch {
    exit 0
}

$tool = $data.tool_name
$inp  = $data.tool_input

switch ($tool) {
    'mcp__codesearch__find_impact' { $markTool = 'find_impact' }
    'mcp__codesearch__find' {
        $kind = if ($inp -and @($inp.PSObject.Properties.Name) -contains 'kind') { [string]$inp.kind } else { '' }
        if ($kind -ne 'usages') { exit 0 }
        $markTool = 'find_usages'
    }
    default { exit 0 }
}

$project = if ($inp -and @($inp.PSObject.Properties.Name) -contains 'project') { [string]$inp.project } else { '' }
$symbol  = if ($inp -and @($inp.PSObject.Properties.Name) -contains 'symbol') { [string]$inp.symbol } else { '' }
$callCwd = if ($inp -and @($inp.PSObject.Properties.Name) -contains 'cwd')   { [string]$inp.cwd }    else { '' }

# cwd-scoped calls (serve-mode worktree-safe scoping) carry no `project`:
# the cwd repo's registered root stands in for it, for response attribution
# and the repo fallback alike.
$cwdRoot = ''
if ([string]::IsNullOrEmpty($project) -and -not [string]::IsNullOrEmpty($callCwd)) {
    $candidate = Resolve-TargetGitRoot $callCwd
    if (-not [string]::IsNullOrEmpty($candidate) -and (Test-CodesearchTargetRegistered $candidate)) {
        $cwdRoot = $candidate
    }
}

# Collect path-ish strings from any parsed JSON shape, recursing into
# JSON-stringified text payloads (the MCP envelope's content[].text). Values
# without a separator or extension are not file paths (a bare symbol once
# resolved to a bogus <root>/git_repo key).
function Test-FilePathShape([string]$Value) {
    ($Value -match '[/\\]') -or ($Value -match '\.[A-Za-z0-9]+$')
}

function Get-ResultPaths {
    param($Node)
    $out = @()
    if ($null -eq $Node) { return $out }
    if ($Node -is [System.Management.Automation.PSCustomObject]) {
        foreach ($prop in $Node.PSObject.Properties) {
            if (@('path', 'file', 'file_path') -contains $prop.Name -and $prop.Value -is [string] -and $prop.Value) {
                if (Test-FilePathShape $prop.Value) { $out += $prop.Value }
            } else {
                $out += Get-ResultPaths $prop.Value
            }
        }
    } elseif ($Node -is [System.Collections.IEnumerable] -and $Node -isnot [string]) {
        foreach ($item in $Node) { $out += Get-ResultPaths $item }
    } elseif ($Node -is [string]) {
        $trimmed = $Node.Trim()
        if ($trimmed.StartsWith('{') -or $trimmed.StartsWith('[')) {
            try { $out += Get-ResultPaths ($trimmed | ConvertFrom-Json) } catch { }
        }
    }
    return @($out | Select-Object -Unique)
}

$keys = @()

# Source 1: a file-ish input field, first non-empty (the .sh twin filters
# jq's `//`-treated-as-truthy empties the same way).
foreach ($field in @('file', 'path', 'file_path')) {
    if ($inp -and @($inp.PSObject.Properties.Name) -contains $field) {
        $candidate = [string]$inp.$field
        if (-not [string]::IsNullOrEmpty($candidate)) {
            $k = ConvertTo-StateKey $candidate
            if (-not [string]::IsNullOrEmpty($k)) { $keys += $k }
            break
        }
    }
}

# Source 1b: a path-like `symbol` marks the path it names — the exact call
# shape edit-guard itself demands.
if (-not [string]::IsNullOrEmpty($symbol) -and (Test-FilePathShape $symbol)) {
    $marked = $false
    if (-not [string]::IsNullOrEmpty($project) -or -not [string]::IsNullOrEmpty($cwdRoot)) {
        foreach ($abs in (Resolve-ResultPath -Path $symbol -ProjectAlias $project -FallbackRoot $cwdRoot)) {
            $k = ConvertTo-StateKey $abs
            if (-not [string]::IsNullOrEmpty($k)) { $keys += $k; $marked = $true }
        }
    }
    if (-not $marked) {
        $k = ConvertTo-StateKey $symbol
        if (-not [string]::IsNullOrEmpty($k)) { $keys += $k }
    }
}

# Source 2: paths mentioned anywhere in the tool response.
if ($null -ne $data.tool_response) {
    foreach ($rp in (Get-ResultPaths $data.tool_response)) {
        foreach ($abs in (Resolve-ResultPath -Path $rp -ProjectAlias $project -FallbackRoot $cwdRoot)) {
            if ([string]::IsNullOrEmpty($abs)) { continue }
            $k = ConvertTo-StateKey $abs
            if (-not [string]::IsNullOrEmpty($k)) { $keys += $k }
        }
    }
}

# Source 3: repo-level fallback — consulted, but nothing attributable.
if ($keys.Count -eq 0) {
    $root = ''
    if (-not [string]::IsNullOrEmpty($project)) {
        $root = Get-RegisteredProjectRoot $project
    } elseif (-not [string]::IsNullOrEmpty($cwdRoot)) {
        $root = $cwdRoot
    }
    if (-not [string]::IsNullOrEmpty($root)) {
        $k = ConvertTo-StateKey $root
        if (-not [string]::IsNullOrEmpty($k)) { $keys += $k }
    }
}

if ($keys.Count -eq 0) { exit 0 }
$keys = @($keys | Select-Object -Unique)

$stateFile = Join-Path $env:TEMP '.codesearch-edit-guard-state.json'
$window    = 300
$now       = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()

# Upsert each key, pruning expired entries; corrupt/unreadable state starts over.
foreach ($key in $keys) {
    $state = @{}
    if (Test-Path -LiteralPath $stateFile -PathType Leaf) {
        try {
            $stored = Get-Content -LiteralPath $stateFile -Raw | ConvertFrom-Json
            foreach ($pr in $stored.PSObject.Properties) {
                $ts = 0L
                try { $ts = [long]$pr.Value.ts } catch {}
                if (($now - $ts) -lt $window) {
                    $state[$pr.Name] = @{ tool = [string]$pr.Value.tool; ts = $ts }
                }
            }
        } catch {}
    }
    $state[$key] = @{ tool = $markTool; ts = $now }
    try {
        $state | ConvertTo-Json -Depth 5 -Compress | Set-Content -LiteralPath $stateFile -NoNewline
    } catch {}
}

exit 0
