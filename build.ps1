#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Builds CodeSearch.

.DESCRIPTION
    Always runs `cargo build` (debug or release). Cargo's own incremental
    compilation decides what actually needs to be recompiled — this script
    no longer tries to second-guess it with git-diff heuristics.

    Version bumping is handled by the pre-commit hook, NOT here.

    Harness-independent cargo-stall hardening: before invoking cargo the
    script reports cargo.exe/rustc.exe processes already referencing this
    repo's target dir (a killed previous run leaves orphans that make cargo
    hang forever on "Blocking waiting for file lock"); -KillOrphans clears
    them first. Cargo output is redirected to a temp log file and printed
    afterwards instead of being captured through the pipeline, so lingering
    child processes cannot stall output capture.

.EXAMPLE
    .\build.ps1
    Builds in debug mode

.EXAMPLE
    .\build.ps1 -Release
    Builds in release mode

.EXAMPLE
    .\build.ps1 -KillOrphans
    Kills cargo/rustc processes holding this target dir, then builds in debug mode
#>

param(
    [switch]$Release,
    [switch]$KillOrphans
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

# --- Orphan guard: cargo/rustc processes already running on this machine ---
# A previous run killed mid-build (hook timeout, editor restart, agent abort)
# leaves cargo.exe/rustc.exe alive; the next cargo then hangs forever on
# "Blocking waiting for file lock". There is no reliable per-repo scoping:
# a blocked cargo.exe's own command line never mentions the target dir (only
# its rustc children do, and only mid-build), and neither WMI nor
# Get-Process exposes the working directory. So report ALL cargo/rustc
# processes, flagging the ones that provably reference THIS repo's target
# dir; -KillOrphans clears them all (they may belong to another checkout —
# the flag is explicit, so that is the operator's call).
$TargetDir = Join-Path $ScriptDir "target"
$running = @(Get-Process -Name cargo, rustc -ErrorAction SilentlyContinue)
$holders = @(
    Get-CimInstance Win32_Process -Filter "Name='cargo.exe' OR Name='rustc.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine -like "*$TargetDir*" }
)
if ($running.Count -gt 0) {
    foreach ($h in $holders) {
        Write-Host "  [lock] PID $($h.ProcessId) $($h.Name) REFERENCES this target dir: $($h.CommandLine)" -ForegroundColor Yellow
    }
    foreach ($p in $running | Where-Object { $_.Id -notin $holders.ProcessId }) {
        Write-Host "  [lock] PID $($p.Id) $($p.Name) running (no target-dir reference on its command line — possibly another checkout or an early-phase build)" -ForegroundColor DarkYellow
    }
    if ($KillOrphans) {
        foreach ($p in $running) {
            try {
                Stop-Process -Id $p.Id -Force -ErrorAction Stop
                Write-Host "  [lock] killed PID $($p.Id)" -ForegroundColor DarkYellow
            } catch {
                Write-Host "  [lock] could not kill PID $($p.Id): $($_.Exception.Message)" -ForegroundColor Red
            }
        }
    } else {
        Write-Host "  [lock] WARNING: cargo/rustc process(es) are already running; cargo may hang on 'Blocking waiting for file lock'." -ForegroundColor Yellow
        Write-Host "  [lock] Re-run with -KillOrphans to clear them first." -ForegroundColor Yellow
    }
}

# Determine build mode
$BuildMode = if ($Release) { "release" } else { "debug" }

Write-Host "Building in $BuildMode mode..." -ForegroundColor Yellow

# --- File-redirect output: log to a temp file, print afterwards ---
# Capturing cargo's streams through the PowerShell pipeline stalls when a
# lingering child process inherits the pipe and never closes it (the same
# orphan class the guard above reports). Redirecting all streams to a file
# lets this script finish reading the result even if children linger; the
# log path is printed up front so a live watcher can tail it.
$LogPath = Join-Path $env:TEMP ("codesearch-build-{0}.log" -f (Get-Date -Format "yyyyMMdd-HHmmss"))
Write-Host "  [log] $LogPath" -ForegroundColor DarkGray
# Literal args, not array splatting: `cargo @args *> file` mangles the
# splatted array on this pwsh/cargo combination (cargo receives a stray
# truncated argument), while literal args with the same redirection are fine.
if ($Release) {
    & cargo build --release *> $LogPath
} else {
    & cargo build *> $LogPath
}
$BuildExitCode = $LASTEXITCODE
Get-Content $LogPath | ForEach-Object { Write-Host $_ }

if ($BuildExitCode -ne 0) {
    Write-Host "Build failed! Full log: $LogPath" -ForegroundColor Red
    exit $BuildExitCode
}

Write-Host "Build completed: target/$BuildMode/codesearch.exe" -ForegroundColor Green
