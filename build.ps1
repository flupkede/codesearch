#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Build script for codesearch with auto-versioning.

.DESCRIPTION
    This script:
    1. Checks if code has changed (via git diff)
    2. Increments version in Cargo.toml only if code changed
    3. Builds only if code changed

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

# Check if code has changed
Write-Host "Checking for code changes..." -ForegroundColor Cyan
$ChangedFiles = git diff --name-only HEAD 2>$null
if (-not $ChangedFiles) {
    $ChangedFiles = git diff --name-only 2>$null
}

if (-not $ChangedFiles) {
    Write-Host "No changes detected, skipping build" -ForegroundColor Green
    exit 0
}

Write-Host "Changes detected" -ForegroundColor Yellow

# Increment version in Cargo.toml FIRST
$CargoToml = Join-Path $ScriptDir "Cargo.toml"
if (Test-Path $CargoToml) {
    $Lines = Get-Content $CargoToml
    $NewLines = @()
    $VersionUpdated = $false
    
    foreach ($Line in $Lines) {
        if (-not $VersionUpdated -and $Line -match '^version\s*=\s*"(\d+\.\d+)\.(\d+)"') {
            $Major = $Matches[1]
            $Patch = [int]$Matches[2]
            $NewPatch = $Patch + 1
            $NewVersion = "$Major.$NewPatch"
            $Line = "version = `"$NewVersion`""
            $VersionUpdated = $true
            Write-Host "Version incremented to $NewVersion" -ForegroundColor Green
        }
        $NewLines += $Line
    }
    
    if ($VersionUpdated) {
        $NewLines | Out-File -FilePath $CargoToml -Encoding utf8
    }
}

# Build
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

Write-Host "✓ Build completed: target/$BuildMode/codesearch.exe" -ForegroundColor Green
