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
# the guards honour at runtime), repoB is not listed. One case pins the
# CODESEARCH_HOME relocation fallback with REPOS_CONFIG unset. The serve hub
# is only contacted by the grep-guard smoke case, which forces a dead port
# so the answer is deterministic. State lives in a temp TMPDIR; the env
# overrides are process-scoped and die with this script.

set -u

HOOKS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$HOOKS_DIR/codesearch-common.sh"

PASS=0
FAIL=0
TMP_ROOT="$(mktemp -d)"
STATE_DIR="$TMP_ROOT/state"
mkdir -p "$STATE_DIR"

cleanup() {
    unset CODESEARCH_REPOS_CONFIG CODESEARCH_SERVER CODESEARCH_HOME TMPDIR
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

REPO_A="$TMP_ROOT/repoA"
REPO_B="$TMP_ROOT/repoB"
mkdir -p "$REPO_A" "$REPO_B"
git init -q "$REPO_A"
git init -q "$REPO_B"
touch "$REPO_A/x.cs" "$REPO_A/m.ts" "$REPO_A/p.py" "$REPO_A/z.cs" "$REPO_A/w.cs" "$REPO_A/v.cs" "$REPO_B/b.cs"
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
post_impact_symbolic() { # $1 = project ("" for none), $2 = tool_response JSON
    jq -n --arg p "$1" --argjson resp "$2" \
        '{tool_name: "mcp__codesearch__find_impact", tool_input: ({symbol_name: "CloseAsync"} + (if $p == "" then {} else {project: $p} end)), tool_response: $resp}'
}
mcp_text_response() { jq -cn --arg t "$1" '{content: [{type: "text", text: $t}]}'; }

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
    run_edit "$(edit_event "$ROOT_A/p.py")"
    expect_deny "covered repo: Edit .py denied (find kind=usages required)" 'kind="usages"'
}
case_non_code_file_allows() {
    # Docs/config/data can never benefit from a usages lookup: they pass
    # without any consultation (the .md misfire this pins).
    run_edit "$(edit_event "$REPO_A/docs/notes.md")"
    expect_allow "covered repo: Edit .md allowed without consultation"
    run_edit "$(edit_event "$REPO_A/config/settings.json")"
    expect_allow "covered repo: Edit .json allowed without consultation"
}
case_razor_needs_usages() {
    run_edit "$(edit_event "$REPO_A/Components/Counter.razor")"
    expect_deny "covered repo: Edit .razor denied (find kind=usages required)" 'kind="usages"'
}
case_sql_needs_usages() {
    run_edit "$(edit_event "$REPO_A/migrations/001_init.sql")"
    expect_deny "covered repo: Edit .sql denied (find kind=usages required)" 'kind="usages"'
}
case_ipynb_needs_usages() {
    run_edit "$(edit_event "$REPO_A/analysis/notebook.ipynb")"
    expect_deny "covered repo: Edit .ipynb denied (find kind=usages required)" 'kind="usages"'
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
    run_post "$(post_find "$ROOT_A/p.py" "usages")"
    run_edit "$(edit_event "$ROOT_A/p.py")"
    expect_allow "after find(kind=usages) marker: Edit .py allowed"
    run_edit "$(multiedit_event "$REPO_A/x.cs" "$ROOT_A/p.py")"
    expect_allow "MultiEdit with every path marked allowed"
}
case_response_paths_mark() {
    # find_impact's real schema carries symbol_name, not a file path — the
    # reported permanent wedge: no input field matched, so no marker was
    # ever written and every .cs edit in a registered repo stayed denied.
    # The response's hit paths (alias-prefixed, inside the MCP text
    # envelope's JSON payload) must attribute instead.
    local hits resp
    hits="$(jq -cn --arg p "repoA/z.cs" '{results: [{path: $p, chunk_id: 7}]}')"
    resp="$(mcp_text_response "$hits")"
    run_post "$(post_impact_symbolic "repoA" "$resp")"
    run_edit "$(edit_event "$ROOT_A/z.cs")"
    expect_allow "find_impact(symbol_name) with response hit paths: Edit .cs allowed"
    # A STRING tool_response (same payload, no envelope object) must
    # attribute identically — indexing .content on a string aborted the
    # whole jq program once.
    local hits2
    hits2="$(jq -cn --arg p "repoA/v.cs" '{results: [{path: $p, chunk_id: 9}]}')"
    run_post "$(jq -cn --arg p "repoA" --arg t "$hits2" \
        '{tool_name: "mcp__codesearch__find_impact", tool_input: {project: $p, symbol_name: "CloseAsync"}, tool_response: $t}')"
    run_edit "$(edit_event "$ROOT_A/v.cs")"
    expect_allow "string tool_response parses the same JSON payload: Edit .cs allowed"
}
case_symbolic_without_project_still_denied() {
    # No project alias: response paths cannot resolve and no repo root is
    # known, so nothing is attributable — the edit stays denied. Pins the
    # requirement to scope the consultation to the project.
    local resp
    resp="$(mcp_text_response "No symbol index available for csharp")"
    run_post "$(post_impact_symbolic "" "$resp")"
    run_edit "$(edit_event "$REPO_A/w.cs")"
    expect_deny "find_impact without project: no marker, Edit .cs still denied" "find_impact"
}
case_no_backend_marks_repo() {
    # A pathless "no SCIP backend" answer writes the repo-level fallback
    # marker: codesearch was consulted and answered nothing, so any file in
    # the repo is unlocked for the window. This is the exact missing-C#-
    # index wedge from the field report.
    local resp
    resp="$(mcp_text_response "No symbol index available for csharp")"
    run_post "$(post_impact_symbolic "repoA" "$resp")"
    run_edit "$(edit_event "$ROOT_A/p.py")"
    expect_allow "pathless no-backend answer: repo marker unblocks other files"
}

fresh_state() { rm -f "$STATE_DIR/.codesearch-edit-guard-state.json"; }

post_find_symbol() { # $1 = symbol, $2 = extra-input JSON object string
    jq -cn --arg s "$1" --argjson extra "$2" \
        '{tool_name: "mcp__codesearch__find", tool_input: ({symbol: $s, kind: "usages"} + $extra)}'
}

case_path_symbol_marks_file() {
    # edit-guard demands find(symbol=<path>, kind="usages") for non-SCIP
    # code — agents run exactly that shape, so a path-like symbol must mark
    # the path it names (project-root resolution), not only the repo via
    # the fallback. The reported conftest.py treadmill: the demanded call
    # itself wrote no marker, so the retry denied again.
    local resp
    touch "$REPO_A/s.py"
    fresh_state
    run_edit "$(edit_event "$ROOT_A/s.py")"
    expect_deny "isolation: fresh state denies Edit s.py" 'kind="usages"'
    resp="$(mcp_text_response "No usages found for 's.py'. The symbol may not be indexed.")"
    run_post "$(jq -cn --arg p "repoA" --arg t "$resp" \
        '{tool_name: "mcp__codesearch__find", tool_input: {symbol: "s.py", kind: "usages", project: $p}, tool_response: $t}')"
    run_edit "$(edit_event "$ROOT_A/s.py")"
    expect_allow "find(symbol=<path>, usages, project): path-level marker written"
}

case_dotted_symbol_marks_repo() {
    # A dotted non-path symbol (obj.method()) is not path-like: the strict
    # shape gate must skip source 1b so the pathless answer still marks the
    # repo root. The bash twin once matched any dot (case *.*), fabricated a
    # <root>/obj.method() key and starved the repo fallback — bash and pwsh
    # disagreed on the same event.
    local resp sf
    fresh_state
    resp="$(mcp_text_response "No usages found for 'obj.method()'. The symbol may not be indexed.")"
    run_post "$(post_find_symbol "obj.method()" '{"project": "repoA"}' \
        | jq -cn --arg t "$resp" '. + {tool_response: $t}')"
    run_edit "$(edit_event "$ROOT_A/p.py")"
    expect_allow "dotted non-path symbol: repo fallback still fires (pwsh parity)"
    sf="$STATE_DIR/.codesearch-edit-guard-state.json"
    if grep -q 'obj.method()' "$sf" 2>/dev/null; then
        bad "dotted symbol: no <root>/obj.method() junk key may be written"
    else
        ok "dotted symbol: no junk file-level key"
    fi
}

case_cwd_scoped_marks() {
    # Serve-mode agents scope with cwd (worktree-safe), which carries no
    # project — the reported todo_mcp wedge: 20 hits attributed to nothing,
    # every subsequent edit denied. Response paths must resolve against the
    # cwd repo's registered root (both shape candidates are marked, since
    # alias-prefixed vs repo-relative depends on serve mode).
    local hits
    touch "$REPO_A/t.py"
    fresh_state
    hits="$(jq -cn --arg p "repoA/t.py" '{results: [{path: $p, chunk_id: 3}]}')"
    run_post "$(jq -cn --arg c "$REPO_A" --arg t "$hits" \
        '{tool_name: "mcp__codesearch__find", tool_input: {symbol: "git_repo", kind: "usages", cwd: $c}, tool_response: $t}')"
    run_edit "$(edit_event "$ROOT_A/t.py")"
    expect_allow "cwd-scoped find(usages) with hits: response paths mark the cwd repo"
}

case_cwd_junk_path_marks_repo() {
    # A cwd-scoped consultation with no file-path-shaped response value (a
    # bare symbol string has no separator or extension) marks the cwd
    # repo's ROOT — and never a bogus <root>/<symbol> junk key (the
    # reported husq.aprimo/git_repo marker that matched nothing).
    local resp sf
    fresh_state
    run_edit "$(edit_event "$REPO_A/w.cs")"
    expect_deny "isolation: fresh state denies Edit w.cs" 'find_impact'
    resp="$(mcp_text_response "No usages found for 'git_repo'. The symbol may not be indexed.")"
    run_post "$(jq -cn --arg c "$REPO_A" --arg t "$resp" \
        '{tool_name: "mcp__codesearch__find", tool_input: {symbol: "git_repo", kind: "usages", cwd: $c}, tool_response: $t}')"
    run_edit "$(edit_event "$REPO_A/w.cs")"
    expect_allow "cwd-scoped pathless find(usages): repo marker unblocks any code file"
    run_edit "$(edit_event "$ROOT_A/p.py")"
    expect_allow "cwd-scoped pathless find(usages): cwd repo-root marker unblocks"
    sf="$STATE_DIR/.codesearch-edit-guard-state.json"
    if grep -q 'git_repo' "$sf" 2>/dev/null; then
        bad "junk filter: no <root>/git_repo key may be written"
    else
        ok "junk filter: bare-symbol response wrote no junk key"
    fi
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

case_grep_targets() { # $1 = label, $2 = command, $3 = expected (newline-joined)
    local out
    out="$(cd "$TMP_ROOT" && command_grep_targets "$2")"
    if [ "$out" = "$3" ]; then
        ok "$1"
    else
        bad "$1 — expected: [$3] got: [$out]"
    fi
}

case_bash_grep_targets() {
    # The target is the command's own operand, never the session cwd: this is
    # the kcharts/druid misfire (session in an indexed repo, grep in /tmp or
    # in a sibling repo). `~` and absolute paths pass through; relative
    # operands resolve against the shell cwd or a leading `cd`.
    case_grep_targets "bash grep: relative operand resolves against the cwd" \
        'grep -rn image src/' "$TMP_ROOT/src"
    case_grep_targets "bash grep: absolute operand is kept" \
        'grep -rn image /tmp/elsewhere/docs' "/tmp/elsewhere/docs"
    case_grep_targets "bash grep: cd prefix moves the base" \
        'cd /tmp/elsewhere && grep -n kafka docs.md' "/tmp/elsewhere/docs.md"
    case_grep_targets "bash grep: operand in a second pipeline segment" \
        'wc -l a && grep -rn "is_(None)" audits/' "$TMP_ROOT/audits"
    case_grep_targets "bash grep: pure stream filter has no file target" \
        "find . -type f | grep -v '^./.git/' | sort" ""
    case_grep_targets "bash grep: quoted pattern is not a path" \
        'grep -rn "one/two" .' "$TMP_ROOT"
    case_grep_targets "bash grep: glob operand still names a path" \
        'grep -rn TODO src/*.rs' "$TMP_ROOT/src/*.rs"
    case_grep_targets "bash grep: redirect target is not an operand" \
        'grep -l "apache/druid" infra/superset/* 2>/dev/null' "$TMP_ROOT/infra/superset/*"
    case_grep_targets "bash grep: output redirect and fd dup are dropped" \
        'grep -rn TODO . > out.txt 2>&1' "$TMP_ROOT"
    case_grep_targets "bash grep: input redirect is not an operand" \
        'grep -n TODO < /tmp/input.txt' ""
    case_grep_targets "bash grep: no operand at all is the cwd repo's job" \
        'grep -rn image --include=*.yaml' ""
}

case_covered_via_codesearch_home() {
    # Relocation pin: with REPOS_CONFIG unset, guards must resolve repos.json
    # from $CODESEARCH_HOME (mirrors constants.rs codesearch_home()) — and
    # only from there, so a dev's real global registry cannot leak in.
    local home="$TMP_ROOT/cs-home" saved_cfg="${CODESEARCH_REPOS_CONFIG:-}"
    mkdir -p "$home"
    jq -n --arg a "$ROOT_A" '{repos: {repoA: $a}}' > "$home/repos.json"
    unset CODESEARCH_REPOS_CONFIG
    export CODESEARCH_HOME="$home"
    run_edit "$(edit_event "$REPO_A/x.cs")"
    expect_deny "CODESEARCH_HOME registry: covered .cs denied" "find_impact"
    run_edit "$(edit_event "$REPO_B/b.cs")"
    expect_allow "CODESEARCH_HOME registry: unregistered repo fails open"
    unset CODESEARCH_HOME
    export CODESEARCH_REPOS_CONFIG="$saved_cfg"
}

case_no_coverage_allows
case_covered_cs_denied
case_covered_ts_denied
case_covered_py_needs_usages
case_non_code_file_allows
case_razor_needs_usages
case_sql_needs_usages
case_ipynb_needs_usages
case_definition_does_not_mark
case_find_impact_marker_allows
case_empty_first_field_falls_through
case_multiedit_partial_marks_denied
case_find_usages_marker_allows
case_response_paths_mark
case_symbolic_without_project_still_denied
case_no_backend_marks_repo
case_path_symbol_marks_file
case_dotted_symbol_marks_repo
case_cwd_scoped_marks
case_cwd_junk_path_marks_repo
case_window_expiry_denies_again
case_grep_guard_smoke
case_bash_grep_targets
case_covered_via_codesearch_home

echo
echo "edit-guard self-tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
