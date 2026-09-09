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
# PostToolUse hooks cannot block anything: no decision is emitted and this
# script ALWAYS exits 0. Symbol-only find_impact calls carry no file-ish
# input field and are skipped (nothing to attribute a path to).

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

$p = $null
foreach ($field in @('file', 'path', 'file_path')) {
    if ($inp -and @($inp.PSObject.Properties.Name) -contains $field) {
        $candidate = [string]$inp.$field
        if (-not [string]::IsNullOrEmpty($candidate)) { $p = $candidate; break }
    }
}
if ([string]::IsNullOrEmpty($p)) { exit 0 }

$key = ConvertTo-StateKey $p
if ([string]::IsNullOrEmpty($key)) { exit 0 }

$stateFile = Join-Path $env:TEMP '.codesearch-edit-guard-state.json'
$window    = 300
$now       = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()

# Upsert, pruning expired entries; corrupt/unreadable state starts over.
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

exit 0
