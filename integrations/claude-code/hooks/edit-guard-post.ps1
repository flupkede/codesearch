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
#   1. a file-ish INPUT field (file/path/file_path) — find_impact's schema
#      names its target `symbol_name` and carries no path, so this serves
#      older schemas and hand-crafted calls
#   2. every path the RESPONSE mentions (path/file/file_path fields at any
#      depth, including inside the JSON-stringified text payload MCP wraps
#      results in), resolved against the registered root of the input's
#      `project` alias when they are alias-prefixed or project-relative
#   3. when neither yields a path — empty result or a "no SCIP backend"
#      error — a REPO-level marker on the project's registered root, which
#      edit-guard accepts for any file in that repo (the fix for the
#      reported permanent wedge on .cs edits)
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

# Collect path-ish strings from any parsed JSON shape, recursing into
# JSON-stringified text payloads (the MCP envelope's content[].text).
function Get-ResultPaths {
    param($Node)
    $out = @()
    if ($null -eq $Node) { return $out }
    if ($Node -is [System.Management.Automation.PSCustomObject]) {
        foreach ($prop in $Node.PSObject.Properties) {
            if (@('path', 'file', 'file_path') -contains $prop.Name -and $prop.Value -is [string] -and $prop.Value) {
                $out += $prop.Value
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

# Source 2: paths mentioned anywhere in the tool response.
if ($null -ne $data.tool_response) {
    foreach ($rp in (Get-ResultPaths $data.tool_response)) {
        $abs = Resolve-ResultPath -Path $rp -ProjectAlias $project
        if ([string]::IsNullOrEmpty($abs)) { continue }
        $k = ConvertTo-StateKey $abs
        if (-not [string]::IsNullOrEmpty($k)) { $keys += $k }
    }
}

# Source 3: repo-level fallback — consulted, but nothing attributable.
if ($keys.Count -eq 0 -and -not [string]::IsNullOrEmpty($project)) {
    $root = Get-RegisteredProjectRoot $project
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
