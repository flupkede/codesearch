#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Builds CodeSearch.

.DESCRIPTION
    Always runs `cargo build` (debug or release). Cargo's own incremental
    compilation decides what actually needs to be recompiled — this script
    no longer tries to second-guess it with git-diff heuristics.

    Version bumping is handled by the pre-commit hook, NOT here.

.EXAMPLE
    .\build.ps1
    Builds in debug mode

.EXAMPLE
    .\build.ps1 -Release
    Builds in release mode
#>

param(
    [switch]$Release
)

$ErrorActionPreference = "Stop"

# Change to script directory (where Cargo.toml is located)
$ScriptDir = $PSScriptRoot
Set-Location $ScriptDir

# --- Self-heal: force core.bare = false (cargo fingerprint-safe) ---
# This repo lives at codesearch.git as a bare+working-tree hybrid: full
# checked-out tree + .git/index, but core.bare intermittently resets to `true`
# (VS Code's git integration rewrites .git/config on ref changes). When
# core.bare=true, cargo's source fingerprinting aborts with
# "did not expect repo ... to be bare", breaking every build. Force it false
# before invoking cargo. Idempotent and harmless for a normal (truly non-bare)
# checkout too.
try {
    & git -C $ScriptDir config core.bare $false 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "  [self-heal] ensured core.bare=false (cargo fingerprint-safe)" -ForegroundColor DarkGray
    }
} catch {
    # Non-fatal — if git isn't reachable or this isn't a git repo, let cargo
    # run and surface its own error.
}

# Determine build mode
$BuildMode = if ($Release) { "release" } else { "debug" }

Write-Host "Building in $BuildMode mode..." -ForegroundColor Yellow

if ($Release) {
    & cargo build --release
} else {
    & cargo build
}

if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit $LASTEXITCODE
}

Write-Host "Build completed: target/$BuildMode/codesearch.exe" -ForegroundColor Green
