#!/usr/bin/env bash
# scripts/qc.sh — Local QC gate that mirrors CI exactly
#
# Bash equivalent of scripts/qc.ps1 — runs on Linux/macOS.
#
# Every cargo invocation below is the CI workflow's own command (same flags,
# same --workspace scope); src/qc_gate_tests.rs fails when the two drift
# apart. Do not "simplify" a command here without changing ci.yml too.
#
# Usage:
#   bash scripts/qc.sh          # run all checks
#   bash scripts/qc.sh --fast   # compile the integration tests without running them; skip the load-dynamic clippy
#
# Exit code 0 = all clear, non-zero = failure (blocks push via pre-push hook)

set -uo pipefail

# ── Parse arguments ──────────────────────────────────────────────────────────
FAST=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --fast|-Fast) FAST=true; shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# ── Colors ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;90m'
YELLOW='\033[1;33m'
NC='\033[0m'

FAILED=()

# NOTE: do NOT redirect TMP/TMPDIR here. Test tempdirs must stay outside any
# registered codesearch repo: doctor/hook-state tests walk up the tree and
# would adopt this repo's own .git/.codesearch.db. Build artifacts are
# AV-safe via .cargo/config.toml's in-repo target-dir.

step() {
    local name="$1"
    echo ""
    echo -e "${CYAN}==============================="
    echo -e "  $name"
    echo -e "===============================${NC}"

    local start_ms
    start_ms=$(date +%s%N 2>/dev/null || date +%s)

    shift
    local cmd="$*"
    if eval "$cmd"; then
        local end_ms
        end_ms=$(date +%s%N 2>/dev/null || date +%s)
        # date +%s%N gives nanoseconds; date +%s gives seconds (fallback)
        if [[ "$start_ms" == *N ]]; then
            # %N not supported (macOS) — use seconds
            local elapsed=$(( end_ms - start_ms ))
            echo -e "  ${GREEN}PASSED (${elapsed}s)${NC}"
        else
            local elapsed_ms=$(( (end_ms - start_ms) / 1000000 ))
            echo -e "  ${GREEN}PASSED (${elapsed_ms}ms)${NC}"
        fi
    else
        local end_ms
        end_ms=$(date +%s%N 2>/dev/null || date +%s)
        if [[ "$start_ms" == *N ]]; then
            local elapsed=$(( end_ms - start_ms ))
            echo -e "  ${RED}FAILED (${elapsed}s)${NC}"
        else
            local elapsed_ms=$(( (end_ms - start_ms) / 1000000 ))
            echo -e "  ${RED}FAILED (${elapsed_ms}ms)${NC}"
        fi
        FAILED+=("$name")
    fi
}

echo ""
echo -e "${YELLOW}======================================"
echo -e "  codesearch QC gate"
echo -e "  Mirrors .github/workflows/ci.yml"
echo -e "======================================${YELLOW}"

# ── Step 1: cargo fmt --all --check (CI: test-linux) ──
step "cargo fmt --all --check" "cargo fmt --all -- --check 2>&1"

# ── Step 2: cargo check (fast type check) ──
# --workspace, like every CI step below: without it cargo builds the root
# package only, so the core crate's own tests are never compiled here and a
# red CI reaches develop unseen (the gate said "mirrors CI" while doing so).
step "cargo check --workspace" "cargo check --workspace 2>&1"

# ── Step 3: cargo clippy (CI: test-linux) ──
step "cargo clippy --workspace" "cargo clippy --workspace --all-targets -- -D warnings 2>&1"

# ── Step 4: cargo test --lib (CI: test-linux + test-windows) ──
step "cargo test --workspace --lib" "cargo test --workspace --lib -- --nocapture 2>&1"

# ── Step 5: cargo test --test '*' (CI: test-linux + test-windows) ──
# ── Step 6: cargo clippy load-dynamic (CI: test-linux) ──
if ! $FAST; then
    step "cargo test --workspace (integration)" "cargo test --workspace --test '*' -- --nocapture --skip ignore 2>&1"
    step "cargo clippy (load-dynamic)" "cargo clippy --no-default-features --features load-dynamic --all-targets -- -D warnings 2>&1"
else
    # Compile-only: a test that is never compiled cannot fail locally. The
    # integration tests were skipped outright here, which is exactly how a red
    # CI reached develop without anyone seeing it.
    step "cargo test --workspace (integration, compile only)" "cargo test --workspace --test '*' --no-run 2>&1"
    echo ""
    echo -e "  ${GRAY}(-Fast: integration tests compiled, not run; load-dynamic clippy not run)${NC}"
fi

# ── Summary ──
echo ""
echo -e "${YELLOW}======================================"
echo -e "  QC Summary"
echo -e "======================================${NC}"

if [[ ${#FAILED[@]} -eq 0 ]]; then
    echo -e "  ${GREEN}ALL CHECKS PASSED${NC}"
    echo ""
    exit 0
else
    echo -e "  ${RED}FAILED:${NC}"
    for name in "${FAILED[@]}"; do
        echo -e "    ${RED}- $name${NC}"
    done
    echo ""
    echo -e "  ${RED}Push blocked. Fix the errors above.${NC}"
    echo ""
    exit 1
fi
