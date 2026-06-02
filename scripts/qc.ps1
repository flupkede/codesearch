# scripts/qc.ps1 — Local QC gate that mirrors CI exactly
#
# Usage:
#   pwsh -NoProfile -File scripts\qc.ps1          # run all checks
#   pwsh -NoProfile -File scripts\qc.ps1 -Fast     # skip slow checks (integration tests)
#
# Exit code 0 = all clear, non-zero = failure (blocks push via pre-push hook)

param(
    [switch]$Fast
)

$ErrorActionPreference = "Stop"
$failed = @()

function Step([string]$Name, [scriptblock]$Block) {
    Write-Host "`n===============================" -ForegroundColor Cyan
    Write-Host "  $Name" -ForegroundColor Cyan
    Write-Host "===============================" -ForegroundColor Cyan
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        & $Block
        $sw.Stop()
        Write-Host "  PASSED ($($sw.ElapsedMilliseconds)ms)" -ForegroundColor Green
    }
    catch {
        $sw.Stop()
        Write-Host "  FAILED ($($sw.ElapsedMilliseconds)ms)" -ForegroundColor Red
        Write-Host $_.Exception.Message -ForegroundColor DarkRed
        $script:failed += $Name
    }
}

Write-Host ""
Write-Host "======================================" -ForegroundColor Yellow
Write-Host "  codesearch QC gate" -ForegroundColor Yellow
Write-Host "  Mirrors .github/workflows/ci.yml" -ForegroundColor Yellow
Write-Host "======================================" -ForegroundColor Yellow

# ── Step 1: cargo fmt --check (CI: test-linux) ──
Step "cargo fmt --check" {
    cargo fmt -- --check 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt found unformatted code" }
}

# ── Step 2: cargo check (fast type check, replaces cargo build --release) ──
Step "cargo check" {
    cargo check 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }
}

# ── Step 3: cargo clippy (CI: test-linux) ──
Step "cargo clippy" {
    cargo clippy --all-targets -- -D warnings 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy found warnings" }
}

# ── Step 4: cargo test --lib (CI: test-linux + test-windows) ──
Step "cargo test --lib" {
    cargo test --lib -- --nocapture 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "library tests failed" }
}

# ── Step 5: cargo test --test '*' (CI: test-linux + test-windows) ──
if (-not $Fast) {
    Step "cargo test (integration)" {
        cargo test --test '*' -- --nocapture --skip ignore 2>&1 | Write-Host
        if ($LASTEXITCODE -ne 0) { throw "integration tests failed" }
    }
}
else {
    Write-Host "`n  (skipping integration tests — -Fast mode)" -ForegroundColor DarkGray
}

# ── Summary ──
Write-Host ""
Write-Host "======================================" -ForegroundColor Yellow
Write-Host "  QC Summary" -ForegroundColor Yellow
Write-Host "======================================" -ForegroundColor Yellow

if ($failed.Count -eq 0) {
    Write-Host "  ALL CHECKS PASSED" -ForegroundColor Green
    Write-Host ""
    exit 0
}
else {
    Write-Host "  FAILED:" -ForegroundColor Red
    foreach ($name in $failed) {
        Write-Host "    - $name" -ForegroundColor Red
    }
    Write-Host ""
    Write-Host "  Push blocked. Fix the errors above." -ForegroundColor Red
    Write-Host ""
    exit 1
}
