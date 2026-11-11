#!/usr/bin/env bash
# PreToolUse hook: require a recent codesearch consultation before edits.
# Fires on Edit/Write/MultiEdit. Bash twin of edit-guard.ps1.
# Requires: jq
#
# When the edited file's repo is codesearch-registered (shared coverage model
# with grep-guard: git root listed in ~/.codesearch/repos.json, honoring
# CODESEARCH_REPOS_CONFIG / CODESEARCH_HOME, or a CODESEARCH_SERVER opt-in),
# every touched code file needs a marker proving the agent consulted
# codesearch for exactly that path within the last 5 minutes:
# mcp__codesearch__find_impact for SCIP-backed languages
# (.cs .ts .tsx .mts .cts), mcp__codesearch__find kind="usages" for other
# source code. Docs/config/data files (.md, .json, ...) can never benefit
# from a usages lookup, so they are never gated. The markers are written by
# the edit-guard-post PostToolUse
# companion into ${TMPDIR:-/tmp}/.codesearch-edit-guard-state.json.
#
# Lenient acceptance: ANY marker for the path within the window lets the edit
# through, regardless of which tool wrote it — the marker proves the agent
# consulted codesearch for this file, which is the point; policing
# find_impact-vs-find_usages per extension would deny edits after a
# legitimate-but-"wrong-kind" lookup.
#
# Fail-open on: no target paths, paths outside any git repo, unregistered
# repos, or a crashed hook — all allow. Missing/corrupt state counts as NOT
# consulted (deny on covered repos, allow everywhere else); so does an
# expired marker, and the next consultation refreshes it (no state
# pollution).
#
# Install: see ../README.md (or run `codesearch hooks claude install`).

set -euo pipefail

. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/codesearch-common.sh"

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty' 2>/dev/null | jq_str)
case "$tool" in
    Edit | Write | MultiEdit) ;;
    *) exit 0 ;;
esac

# Target paths: the primary file_path, plus (defensively) any per-edit
# file_path entries a MultiEdit-style payload may carry.
paths=$(echo "$raw" | jq -r '
    [ .tool_input.file_path,
      (.tool_input.edits[]?.file_path // empty)
    ]
    | map(select(. != null and . != ""))
    | unique | .[]
' 2>/dev/null | jq_str)
[ -z "$paths" ] && exit 0  # nothing attributable -> allow (fail-open)

state_file="${TMPDIR:-/tmp}/.codesearch-edit-guard-state.json"
window=300
now=$(date +%s)

# The marker only proves "codesearch was consulted recently for this path";
# a non-numeric ts is corruption, not a stale check.
checked_recently() {
    local key="$1" entry ts
    [ -f "$state_file" ] || return 1
    entry=$(jq -rc --arg k "$key" '.[$k] // empty' < "$state_file" 2>/dev/null | jq_str)
    [ -n "$entry" ] || return 1
    ts=$(printf '%s' "$entry" | jq -r '.ts // empty' 2>/dev/null | jq_str)
    case "$ts" in '' | *[!0-9]*) return 1 ;; esac
    [ $((now - ts)) -lt "$window" ]
}

# Which consultation a path needs before editing: "impact" for SCIP-backed
# languages, "usages" for other source code, "" for non-code files. The
# list mirrors the code-bearing classes of codesearch's own extension map
# (src/file/language.rs — incl. sql and ipynb, whose cells the indexer
# extracts as code) plus razor/cshtml; docs/markup/config (.md .json .yaml
# .css .html .xml ...) never gate. Keys on EXTENSION only: filename-mapped
# code files (Dockerfile, Makefile, ...) pass too — rare refactor targets.
required_consultation() {
    local ext="${1##*.}"
    ext=$(printf '%s' "$ext" | tr '[:upper:]' '[:lower:]')
    case "$ext" in
        cs | ts | tsx | mts | cts) printf 'impact' ;;
        js | mjs | cjs | jsx | py | pyw | pyi | go | rs | java | c | h | cpp | cc | cxx | hpp | hxx | rb | rake | php | swift | kt | kts | dart | sh | bash | zsh | sql | ipynb | proto | razor | cshtml) printf 'usages' ;;
        *) printf '' ;;
    esac
}

fail_path=""
fail_tool=""
while IFS= read -r p; do
    [ -n "$p" ] || continue
    kind="$(required_consultation "$p")"
    [ -z "$kind" ] && continue  # non-code file -> allow (fail-open)
    root="$(resolve_target_git_root "$p")"
    [ -z "$root" ] && continue             # outside any git repo -> allow
    target_registered "$root" || continue  # unregistered repo -> allow (fail-open)
    key="$(norm_state_key "$p")"
    if ! checked_recently "$key"; then
        fail_path="$p"
        if [ "$kind" = impact ]; then
            fail_tool='mcp__codesearch__find_impact (SCIP-backed language)'
        else
            fail_tool='mcp__codesearch__find(symbol, kind="usages")'
        fi
        break
    fi
done <<< "$paths"

[ -z "$fail_path" ] && exit 0  # every path checked recently -> allow

msg=$(cat <<EOF
edit-guard: this edit needs a codesearch consultation first.

Blocked path: $fail_path
Required call: $fail_tool — on that exact file.

Any outcome counts: "no results" and "no SCIP backend" still prove you
consulted codesearch for this file. Run the call, then retry the SAME
edit — it stays allowed for 5 minutes.

Why: find_impact (C#/TS) / find kind="usages" (other code) before edits
keeps refactors caller-aware; this guard makes that protocol structural
for codesearch-registered repos — code files only. Docs, config and data
files, unregistered repos, non-git paths and unparseable events fail open
and are never blocked.
EOF
)

jq -n --arg msg "$msg" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: $msg
  }
}'
exit 0
