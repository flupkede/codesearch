#!/usr/bin/env bash
# Self-test suite for the codesearch Claude Code guard hooks: edit-guard +
# its edit-guard-post PostToolUse companion, plus a grep-guard smoke case
# pinning the codesearch-common.sh extraction. NOT wired into cargo or CI —
# run manually:
#   bash integrations/claude-code/hooks/run-tests.sh
# Requires: bash, jq, git (and curl for the grep-guard smoke case).
#
# Coverage is simulated without a real index: repoA is "registered" via a
# temp repos.json passed through CODESEARCH_REPOS_CONFIG (the same override
# the guards honour at runtime), repoB is not listed. The serve hub is only
# contacted by the grep-guard smoke case, which forces a dead port so the
# answer is deterministic. State lives in a temp TMPDIR; the env overrides
# are process-scoped and die with this script.

set -u

HOOKS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HOOKS_DIR/codesearch-common.sh"

PASS=0
FAIL=0
TMP_ROOT="$(mktemp -d)"
STATE_DIR="$TMP_ROOT/state"
mkdir -p "$STATE_DIR"

cleanup() {
    unset CODESEARCH_REPOS_CONFIG CODESEARCH_SERVER TMPDIR
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

REPO_A="$TMP_ROOT/repoA"
REPO_B="$TMP_ROOT/repoB"
mkdir -p "$REPO_A" "$REPO_B"
git init -q "$REPO_A"
git init -q "$REPO_B"
touch "$REPO_A/x.cs" "$REPO_A/m.ts" "$REPO_A/p.py" "$REPO_B/b.cs"
ROOT_A="$(git -C "$REPO_A" rev-parse --show-toplevel)"
ROOT_B="$(git -C "$REPO_B" rev-parse --show-toplevel)"

REPOS_JSON="$TMP_ROOT/repos.json"
jq -n --arg a "$ROOT_A" '{repos: {repoA: $a}}' > "$REPOS_JSON"

export TMPDIR="$STATE_DIR"
export CODESEARCH_REPOS_CONFIG="$REPOS_JSON"
unset CODESEARCH_SERVER

ok() { PASS=$((PASS + 1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }

run_edit() { OUT="$(printf '%s' "$1" | bash "$HOOKS_DIR/edit-guard.sh" 2>/dev/null)"; }
run_post() { printf '%s' "$1" | bash "$HOOKS_DIR/edit-guard-post.sh" >/dev/null 2>&1; }

edit_event() { jq -n --arg p "$1" '{tool_name: "Edit", tool_input: {file_path: $p}}'; }
multiedit_event() {
    jq -n --arg a "$1" --arg b "$2" \
        '{tool_name: "MultiEdit", tool_input: {file_path: $a, edits: [{file_path: $a}, {file_path: $b}]}}'
}
post_find_impact() { jq -n --arg f "$1" '{tool_name: "mcp__codesearch__find_impact", tool_input: {file: $f}}'; }
post_find() { jq -n --arg f "$1" --arg k "$2" '{tool_name: "mcp__codesearch__find", tool_input: {path: $f, kind: $k}}'; }

expect_allow() {
    if [ -z "$OUT" ]; then
        ok "$1"
    else
        bad "$1 — expected silent allow, got: $OUT"
    fi
}
expect_deny() { # $1 = label, $2 = optional substring the reason must contain
    local d="" reason=""
    if [ -n "$OUT" ]; then
        d="$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision // empty' 2>/dev/null | jq_str 2>/dev/null)"
        reason="$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null | jq_str 2>/dev/null)"
    fi
    if [ "$d" != "deny" ]; then
        bad "$1 — expected deny, got: ${OUT:-<empty>}"
        return
    fi
    if [ $# -ge 2 ] && ! printf '%s' "$reason" | grep -q "$2"; then
        bad "$1 — deny message missing '$2'"
        return
    fi
    ok "$1"
}

case_no_coverage_allows() {
    run_edit "$(edit_event "$REPO_B/b.cs")"
    expect_allow "unregistered repo: Edit .cs fails open (silent allow)"
}
case_covered_cs_denied() {
    run_edit "$(edit_event "$REPO_A/x.cs")"
    expect_deny "covered repo: Edit .cs denied (find_impact required)" "find_impact"
}
case_covered_ts_denied() {
    run_edit "$(edit_event "$REPO_A/m.ts")"
    expect_deny "covered repo: Edit .ts denied (find_impact required)" "find_impact"
}
case_covered_py_needs_usages() {
    run_edit "$(edit_event "$REPO_A/p.py")"
    expect_deny "covered repo: Edit .py denied (find kind=usages required)" 'kind="usages"'
}
case_definition_does_not_mark() {
    run_post "$(post_find "$REPO_A/m.ts" "definition")"
    run_edit "$(edit_event "$REPO_A/m.ts")"
    expect_deny "find kind=definition does not mark: Edit .ts still denied" "find_impact"
}
case_find_impact_marker_allows() {
    run_post "$(post_find_impact "$REPO_A/x.cs")"
    run_edit "$(edit_event "$REPO_A/x.cs")"
    expect_allow "after find_impact marker: Edit .cs allowed"
}
case_empty_first_field_falls_through() {
    # jq's `//` treats "" as truthy: the post hook must filter empties
    # explicitly or {"file":"","path":...} writes NO marker while the ps1
    # twin writes one (shell drift, review round 1).
    run_post "$(jq -n --arg p "$REPO_A/m.ts" \
        '{tool_name: "mcp__codesearch__find_impact", tool_input: {file: "", path: $p}}')"
    run_edit "$(edit_event "$REPO_A/m.ts")"
    expect_allow "empty-string file field falls through to path: marker written"
}
case_multiedit_partial_marks_denied() {
    run_edit "$(multiedit_event "$REPO_A/x.cs" "$REPO_A/p.py")"
    expect_deny "MultiEdit: marked .cs + unmarked .py denied, failing path named" "$REPO_A/p.py"
}
case_find_usages_marker_allows() {
    run_post "$(post_find "$REPO_A/p.py" "usages")"
    run_edit "$(edit_event "$REPO_A/p.py")"
    expect_allow "after find(kind=usages) marker: Edit .py allowed"
    run_edit "$(multiedit_event "$REPO_A/x.cs" "$REPO_A/p.py")"
    expect_allow "MultiEdit with every path marked allowed"
}
case_window_expiry_denies_again() {
    local sf="$STATE_DIR/.codesearch-edit-guard-state.json"
    local old=$(( $(date +%s) - 400 ))
    jq --argjson old "$old" 'with_entries(.value.ts = $old)' < "$sf" > "$sf.new" &&
        mv "$sf.new" "$sf"
    run_edit "$(edit_event "$REPO_A/x.cs")"
    expect_deny "expired marker (>5 min): Edit .cs denied again" "find_impact"
}
case_grep_guard_smoke() {
    # Regression pin for the codesearch-common.sh extraction: grep-guard
    # still resolves + guards the covered repo, and still auto-allows when
    # the hub is unreachable (dead port -> /healthz probe fails).
    local out
    out="$(printf '{"tool_name":"Grep","tool_input":{"path":"%s"}}' "$REPO_A" |
        CODESEARCH_SERVE_PORT=1 bash "$HOOKS_DIR/grep-guard.sh" 2>/dev/null)"
    if [ -z "$out" ]; then
        ok "grep-guard smoke: covered repo + hub down -> allow"
    else
        bad "grep-guard smoke — expected silent allow, got: $out"
    fi
}

case_no_coverage_allows
case_covered_cs_denied
case_covered_ts_denied
case_covered_py_needs_usages
case_definition_does_not_mark
case_find_impact_marker_allows
case_empty_first_field_falls_through
case_multiedit_partial_marks_denied
case_find_usages_marker_allows
case_window_expiry_denies_again
case_grep_guard_smoke

echo
echo "edit-guard self-tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
