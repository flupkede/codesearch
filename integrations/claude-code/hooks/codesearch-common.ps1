# Shared helpers for the codesearch Claude Code guard hooks. Dot-sourced by
# grep-guard.ps1, edit-guard.ps1 and edit-guard-post.ps1:
#   . (Join-Path $PSScriptRoot 'codesearch-common.ps1')
#
# PowerShell twin of codesearch-common.sh — keep the two behaviorally
# equivalent. Coverage model: a repo is covered when its git root is
# REGISTERED in ~/.codesearch/repos.json (CODESEARCH_REPOS_CONFIG overrides
# the location, CODESEARCH_HOME relocates the whole global root), or when
# CODESEARCH_SERVER opts a pure remote-serve setup
# in. Everything here fails open: an unresolvable coverage question must
# allow, never deny.

function Get-CodesearchReposConfigFile {
    if ($env:CODESEARCH_REPOS_CONFIG) { return $env:CODESEARCH_REPOS_CONFIG }
    if ($env:CODESEARCH_HOME) { return (Join-Path $env:CODESEARCH_HOME 'repos.json') }
    return (Join-Path $HOME '.codesearch/repos.json')
}

# True when a shell command line invokes grep/egrep/fgrep/zgrep/rg — the Bash
# tool escape hatch around the Grep-tool guard. Same contract as
# codesearch-common.sh's command_grep_family: quoted spans are stripped first
# ("echo 'grep is slow'" is not an invocation), separators split, and tokens
# compare whole after lowercasing, .exe and path-prefix removal ("progress"
# and "rgx" never match; /usr/bin/grep does).
function Test-GrepFamilyCommand {
    param([string]$Cmd)
    if ([string]::IsNullOrWhiteSpace($Cmd)) { return $false }
    $stripped = [regex]::Replace($Cmd, '"(?:\\.|[^"\\])*"', ' ')
    $stripped = [regex]::Replace($stripped, "'(?:\\.|[^'\\])*'", ' ')
    foreach ($token in ($stripped -split '[\s;|&()`]+')) {
        if (-not $token) { continue }
        $word = $token.ToLowerInvariant() -replace '\.exe$', '' -replace '^.*[\\/]', ''
        if (@('grep', 'egrep', 'fgrep', 'zgrep', 'rg') -contains $word) { return $true }
    }
    return $false
}

function ConvertTo-NormRepoPath {
    param([string]$P)
    $p = $P
    if ($p.StartsWith('\\?\')) { $p = $p.Substring(4) }
    $p = $p -replace '\\', '/'
    while ($p -ne '/' -and $p.EndsWith('/')) { $p = $p.TrimEnd('/') }
    return $p
}

function Test-CodesearchRepoPathEqual {
    param([string]$A, [string]$B)
    $a = ConvertTo-NormRepoPath $A
    $b = ConvertTo-NormRepoPath $B
    # Exact on POSIX-style paths; case-insensitive for Windows drive-letter
    # paths (NTFS folds case throughout), mirroring codesearch-common.sh.
    if ($a -match '^[A-Za-z]:/') { $a = $a.ToLowerInvariant() }
    if ($b -match '^[A-Za-z]:/') { $b = $b.ToLowerInvariant() }
    return [string]::Equals($a, $b, [System.StringComparison]::Ordinal)
}

function Test-CodesearchTargetRegistered {
    param([string]$Root)
    if ($env:CODESEARCH_SERVER) { return $true }
    $cfg = Get-CodesearchReposConfigFile
    if ([string]::IsNullOrWhiteSpace($cfg)) { return $false }
    if (-not (Test-Path -LiteralPath $cfg -PathType Leaf)) { return $false }
    try { $json = Get-Content -LiteralPath $cfg -Raw | ConvertFrom-Json } catch { return $false }
    if ($null -eq $json -or $null -eq $json.repos) { return $false }
    foreach ($prop in $json.repos.PSObject.Properties) {
        $val = $prop.Value
        $reg = if ($val -is [string]) { $val } else { $val | ConvertTo-Json -Compress -Depth 10 }
        if ([string]::IsNullOrEmpty($reg)) { continue }
        if (Test-CodesearchRepoPathEqual $reg $Root) { return $true }
    }
    return $false
}

function Resolve-TargetGitRoot {
    param([string]$Path)
    $root = $null
    $norm = $Path.TrimEnd('/', '\')
    if ($norm -match '^([A-Za-z]:[\\/]|/)') {
        # Absolute path (Windows drive, UNC, or any POSIX root): the git root
        # OF THE TARGET, not of the hook's cwd.
        $probe = $norm
        try {
            if (Test-Path -LiteralPath $probe -PathType Leaf) { $probe = Split-Path -Parent $probe }
        } catch {}
        try {
            $gr = (& git -C $probe rev-parse --show-toplevel 2>$null)
            if ($LASTEXITCODE -eq 0 -and $gr) { $root = "$gr".Trim() }
        } catch {}
    } else {
        # Empty or relative path: resolves against the cwd repo.
        try {
            $gr = (& git rev-parse --show-toplevel 2>$null)
            if ($LASTEXITCODE -eq 0 -and $gr) { $root = "$gr".Trim() }
        } catch {}
    }
    return $root
}

function ConvertTo-StateKey {
    param([string]$P)
    if ([string]::IsNullOrEmpty($P)) { return '' }
    $p = $P
    $norm = $p.TrimEnd('/', '\')
    if (-not ($norm -match '^([A-Za-z]:[\\/]|/)')) {
        $p = (Join-Path (Get-Location).Path $p)
    }
    $k = ConvertTo-NormRepoPath $p
    if ($k -match '^[A-Za-z]:/') { $k = $k.ToLowerInvariant() }
    return $k
}
