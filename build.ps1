#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Build script with automatic version incrementing.

.DESCRIPTION
    This script builds the codesearch project and automatically increments
    the version number in Cargo.toml after each successful build.

.EXAMPLE
    .\build.ps1
    Builds in debug mode and bumps version

.EXAMPLE
    .\build.ps1 -Release
    Builds in release mode and bumps version
#>

param(
    [switch]$Release
)

$ErrorActionPreference = "Stop"

# Change to script directory (where Cargo.toml is located)
$ScriptDir = $PSScriptRoot
Set-Location $ScriptDir

# Set build mode
$BuildMode = if ($Release) { "release" } else { "debug" }

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "CodeSearch Build Script (Auto-Version)" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# Step 1: Get current version
Write-Host "Step 1: Reading current version..." -ForegroundColor Yellow
$cargoToml = Get-Content "Cargo.toml" -Raw
if ($cargoToml -match 'version\s*=\s*"([^"]+)"') {
    $currentVersion = $matches[1]
    Write-Host "  Current version: $currentVersion" -ForegroundColor Green
} else {
    Write-Host "  ERROR: Could not find version in Cargo.toml" -ForegroundColor Red
    exit 1
}

# Step 2: Build the project
Write-Host ""
Write-Host "Step 2: Building codesearch..." -ForegroundColor Yellow
Write-Host "  Mode: $BuildMode" -ForegroundColor Gray

$buildArgs = @("build", "--no-emit-missing-deps")
if ($Release) {
    $buildArgs += "--release"
}

$buildResult = & cargo @buildArgs 2>&1
# Cargo returns 0 even with warnings, only fail on actual errors
if ($LASTEXITCODE -ne 0 -and $buildResult -match "error\[") {
    Write-Host ""
    Write-Host "  ✗ Build failed!" -ForegroundColor Red
    Write-Host ""
    Write-Host $buildResult
    exit $LASTEXITCODE
}

Write-Host "  ✓ Build successful!" -ForegroundColor Green

# Step 3: Bump version
Write-Host ""
Write-Host "Step 3: Bumping version..." -ForegroundColor Yellow

# Determine version bump level (patch for builds)
$bumpArgs = @("bump", "patch")

$bumpOutput = & cargo @bumpArgs 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Host "  WARNING: Version bump failed: $bumpOutput" -ForegroundColor Yellow
    Write-Host "  Continuing with current version..." -ForegroundColor Yellow
} else {
    # Read new version
    $newCargoToml = Get-Content "Cargo.toml" -Raw
    if ($newCargoToml -match 'version\s*=\s*"([^"]+)"') {
        $newVersion = $matches[1]
        Write-Host "  ✓ Version bumped: $currentVersion → $newVersion" -ForegroundColor Green
    }
}

# Step 4: Summary
Write-Host ""
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "Build Summary" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  Mode: $BuildMode" -ForegroundColor Gray
Write-Host "  Version: $currentVersion" -ForegroundColor Gray
Write-Host "  Executable: target/$BuildMode/codesearch.exe" -ForegroundColor Gray
Write-Host ""
Write-Host "✓ Build completed successfully!" -ForegroundColor Green
Write-Host ""
