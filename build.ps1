#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Build script for codesearch.

.DESCRIPTION
    This script:
    1. Checks if code has changed (via git diff)
    2. Builds only if code changed OR binary is missing
    3. Skips build if binary is up to date

    Version bumping is handled by the pre-commit hook, NOT here.
    This avoids the perpetual version-mismatch → rebuild cycle.

.EXAMPLE
    .\build.ps1
    Builds in debug mode

.EXAMPLE
    .\build.ps1 -Release
    Builds in release mode

.EXAMPLE
    .\build.ps1 -Force
    Forces rebuild regardless of git status
#>

param(
    [switch]$Release,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

# Change to script directory (where Cargo.toml is located)
$ScriptDir = $PSScriptRoot
Set-Location $ScriptDir

# Determine build mode and target exe path
$BuildMode = if ($Release) { "release" } else { "debug" }
$TargetExe = Join-Path $ScriptDir ".." "target" $BuildMode "codesearch.exe"

if ($Force) {
    Write-Host "Force rebuild requested" -ForegroundColor Yellow
} else {
    # Check if target exe already exists
    if (Test-Path $TargetExe) {
        # Check for uncommitted changes
        $ChangedFiles = git diff --name-only HEAD 2>&1

        if ($LASTEXITCODE -ne 0 -and $ChangedFiles -match "^fatal:") {
            Write-Host "ERROR: git diff failed: $ChangedFiles" -ForegroundColor Red
            exit 1
        }

        if (-not $ChangedFiles) {
            Write-Host "No changes detected and build exists, skipping build" -ForegroundColor Green
            Write-Host "  Binary: $TargetExe" -ForegroundColor Gray
            exit 0
        }

        Write-Host "Changed files detected:" -ForegroundColor Yellow
        $ChangedFiles | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
    } else {
        Write-Host "Build artifact not found, building..." -ForegroundColor Yellow
    }
}

# Build
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
