#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Builds CodeSearch.

.DESCRIPTION
    Always runs `cargo build` (debug or release). Cargo's own incremental
    compilation decides what actually needs to be recompiled — this script
    no longer tries to second-guess it with git-diff heuristics.

    Version bumping is handled by the pre-commit hook, NOT here.

    Output is redirected to .tmp/build-<mode>.log in the repo and printed
    afterwards, so a lingering cargo child process can never stall a live
    pipe. If other cargo/rustc processes are already running, this script
    WARNS but never kills them — they may belong to other sessions, and
    cargo's own file lock will serialize the builds safely.

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

# --- Orphaned-build detection: warn only, NEVER kill ---
# A cargo.exe/rustc.exe from a killed previous run can hold the target-dir
# build lock, making this build block forever on "Blocking waiting for file
# lock". Detect and report; the human decides what to do with them. They may
# perfectly well be legitimate builds from other sessions.
$orphans = @(Get-Process -Name cargo, rustc -ErrorAction SilentlyContinue)
if ($orphans.Count -gt 0) {
    Write-Host "  [warn] cargo/rustc already running (possible orphan holding the target-dir lock):" -ForegroundColor Yellow
    foreach ($p in $orphans) {
        Write-Host ("    pid={0} name={1} started={2}" -f $p.Id, $p.ProcessName, $p.StartTime) -ForegroundColor Yellow
    }
    Write-Host "  [warn] if this build hangs on 'Blocking waiting for file lock', terminate the stale process manually." -ForegroundColor Yellow
}

# Determine build mode
$BuildMode = if ($Release) { "release" } else { "debug" }

Write-Host "Building in $BuildMode mode..." -ForegroundColor Yellow

# --- Redirect output to a file in the repo, print afterwards ---
# Piping cargo's live output can hang indefinitely when a child process
# outlives the build (stale watchexec/rustc holding the pipe open). Writing
# to .tmp/build-<mode>.log and printing the finished file afterwards cannot
# stall: the file handle closes the moment cargo exits.
$TmpDir = Join-Path $ScriptDir ".tmp"
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null
$LogFile = Join-Path $TmpDir "build-$BuildMode.log"

if ($Release) {
    & cargo build --release 2>&1 | Out-File -FilePath $LogFile -Encoding utf8
} else {
    & cargo build 2>&1 | Out-File -FilePath $LogFile -Encoding utf8
}

$BuildExit = $LASTEXITCODE
Get-Content $LogFile | ForEach-Object { Write-Host $_ }

if ($BuildExit -ne 0) {
    Write-Host "Build failed! (full output: $LogFile)" -ForegroundColor Red
    exit $BuildExit
}

Write-Host "Build completed: target/$BuildMode/codesearch.exe" -ForegroundColor Green
