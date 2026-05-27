#!/usr/bin/env bash
# scripts/qc.sh — Local QC gate that mirrors CI exactly
#
# Bash equivalent of scripts/qc.ps1 — runs on Linux/macOS.
#
# Usage:
#   bash scripts/qc.sh          # run all checks
#   bash scripts/qc.sh --fast   # skip slow checks (integration tests)
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
NC='\033[0m'

FAILED=()

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

# ── Step 1: cargo fmt --check (CI: test-linux) ──
step "cargo fmt --check" "cargo fmt -- --check 2>&1"

# ── Step 2: cargo check (fast type check) ──
step "cargo check" "cargo check 2>&1"

# ── Step 3: cargo clippy (CI: test-linux) ──
step "cargo clippy" "cargo clippy --all-targets -- -D warnings 2>&1"

# ── Step 4: cargo test --lib (CI: test-linux + test-windows) ──
step "cargo test --lib" "cargo test --lib -- --nocapture 2>&1"

# ── Step 5: cargo test --test '*' (CI: test-linux + test-windows) ──
if ! $FAST; then
    step "cargo test (integration)" "cargo test --test '*' -- --nocapture --skip ignore 2>&1"
else
    echo ""
    echo -e "  ${GRAY}(skipping integration tests — --fast mode)${NC}"
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
