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
