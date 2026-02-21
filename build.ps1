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

# Determine build mode and target exe path
$BuildMode = if ($Release) { "release" } else { "debug" }
$TargetExe = Join-Path $ScriptDir ".." "target" $BuildMode "codesearch.exe"

# Check if code has changed
Write-Host "Checking for code changes..." -ForegroundColor Cyan
$ChangedFiles = git diff --name-only HEAD 2>&1

# Check if git command failed (exit code not 0, and not just "no changes" output)
if ($LASTEXITCODE -ne 0) {
    # If it's not just "no changes detected", it's an actual error
    if ($ChangedFiles -notmatch "^fatal:") {
        Write-Host "ERROR: git diff failed with exit code $LASTEXITCODE" -ForegroundColor Red
        Write-Host "Output: $ChangedFiles" -ForegroundColor Red
        exit $LASTEXITCODE
    }
    # If it's "fatal:" (e.g., not a git repo), exit with error
    if ($ChangedFiles -match "^fatal:") {
        Write-Host "ERROR: git diff failed: $ChangedFiles" -ForegroundColor Red
        exit 1
    }
}

# Only skip build if: 1) no uncommitted changes AND 2) target exe exists AND 3) versions match
if (-not $ChangedFiles) {
    if (Test-Path $TargetExe) {
        # Check if the binary version matches Cargo.toml version
        $CargoVersion = $null
        $CargoTomlPath = Join-Path $ScriptDir "Cargo.toml"
        if (Test-Path $CargoTomlPath) {
            $CargoContent = Get-Content $CargoTomlPath -Raw
            if ($CargoContent -match 'version\s*=\s*"(\d+\.\d+\.\d+)"') {
                $CargoVersion = $Matches[1]
            }
        }
        $BinaryVersion = $null
        try {
            $BinaryOutput = & $TargetExe --version 2>&1
            if ($BinaryOutput -match '(\d+\.\d+\.\d+)') {
                $BinaryVersion = $Matches[1]
            }
        } catch { }

        if ($CargoVersion -and $BinaryVersion -and ($CargoVersion -eq $BinaryVersion)) {
            Write-Host "No changes detected and build exists ($(Split-Path $TargetExe -Leaf) v$BinaryVersion), skipping build" -ForegroundColor Green
            exit 0
        } elseif ($CargoVersion -and $BinaryVersion) {
            Write-Host "Binary version ($BinaryVersion) differs from Cargo.toml ($CargoVersion), rebuilding..." -ForegroundColor Yellow
        } else {
            Write-Host "No changes detected but could not verify version, rebuilding..." -ForegroundColor Yellow
        }
    } else {
        Write-Host "No changes detected but build artifact not found, building..." -ForegroundColor Yellow
    }
}

# Increment version in Cargo.toml ONLY when there are actual code changes
if ($ChangedFiles) {
    Write-Host "Changes detected - incrementing version..." -ForegroundColor Yellow
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
} else {
    Write-Host "No code changes - rebuilding stale binary at current version..." -ForegroundColor Yellow
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

Write-Host "✓ Build completed: target/$BuildMode/codesearch.exe" -ForegroundColor Green
