#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Simple build script for codesearch.

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

Write-Host "Building codesearch..." -ForegroundColor Cyan

if ($Release) {
    & cargo build --release
} else {
    & cargo build
}

if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit $LASTEXITCODE
}

$BuildMode = if ($Release) { "release" } else { "debug" }
Write-Host "✓ Build completed: target/$BuildMode/codesearch.exe" -ForegroundColor Green
