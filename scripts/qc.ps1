# scripts/qc.ps1 — Local QC gate that mirrors CI exactly
#
# Every cargo invocation below is the CI workflow's own command (same flags,
# same --workspace scope); src/qc_gate_tests.rs fails when the two drift
# apart. Do not "simplify" a command here without changing ci.yml too.
#
# Usage:
#   pwsh -NoProfile -File scripts\qc.ps1          # run all checks
#   pwsh -NoProfile -File scripts\qc.ps1 -Fast     # compile the integration tests without running them; skip the load-dynamic clippy
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
Step "cargo fmt --all --check" {
    cargo fmt --all -- --check 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt found unformatted code" }
}

# ── Step 2: cargo check (fast type check) ──
# --workspace, like every CI step below: without it cargo builds the root
# package only, so the core crate's own tests are never compiled here and a
# red CI reaches develop unseen (the gate said "mirrors CI" while doing so).
Step "cargo check --workspace" {
    cargo check --workspace 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }
}

# ── Step 3: cargo clippy (CI: test-linux) ──
Step "cargo clippy --workspace" {
    cargo clippy --workspace --all-targets -- -D warnings 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "cargo clippy found warnings" }
}

# ── Step 4: cargo test --lib (CI: test-linux + test-windows) ──
Step "cargo test --workspace --lib" {
    cargo test --workspace --lib -- --nocapture 2>&1 | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "library tests failed" }
}

# ── Step 5: cargo test --test '*' (CI: test-linux + test-windows) ──
# ── Step 6: cargo clippy load-dynamic (CI: test-linux) ──
if (-not $Fast) {
    Step "cargo test --workspace (integration)" {
        cargo test --workspace --test '*' -- --nocapture --skip ignore 2>&1 | Write-Host
        if ($LASTEXITCODE -ne 0) { throw "integration tests failed" }
    }
    Step "cargo clippy (load-dynamic)" {
        cargo clippy --no-default-features --features load-dynamic --all-targets -- -D warnings 2>&1 | Write-Host
        if ($LASTEXITCODE -ne 0) { throw "load-dynamic clippy found warnings" }
    }
}
else {
    # Compile-only: a test that is never compiled cannot fail locally. The
    # integration tests were skipped outright here, which is exactly how a red
    # CI reached develop without anyone seeing it.
    Step "cargo test --workspace (integration, compile only)" {
        cargo test --workspace --test '*' --no-run 2>&1 | Write-Host
        if ($LASTEXITCODE -ne 0) { throw "integration tests failed to compile" }
    }
    Write-Host "`n  (-Fast: integration tests compiled, not run; load-dynamic clippy not run)" -ForegroundColor DarkGray
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
