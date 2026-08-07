#!/usr/bin/env bash
# PreToolUse hook: inject a codesearch-first preamble into every Claude Code
# subagent prompt (the `Agent` tool). Bash/macOS/Linux twin of
# subagent-preamble.ps1 - see that file for full rationale.
# Requires: jq
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

# Fail open on malformed input. Every extraction below runs under
# `set -euo pipefail`, so a jq parse error would abort the hook with exit 5
# and surface to Claude Code as a hook EXECUTION FAILURE rather than the
# silent pass-through these guards promise. Validating once up front keeps
# the .sh twins matching the .ps1 twins, which already exit 0 on bad JSON.
printf '%s' "$raw" | jq -e . >/dev/null 2>&1 || exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
[ "$tool" != "Agent" ] && exit 0

has_prompt=$(echo "$raw" | jq -r 'if (.tool_input | has("prompt")) then "yes" else "no" end')
[ "$has_prompt" != "yes" ] && exit 0

prompt=$(echo "$raw" | jq -r '.tool_input.prompt')

marker='[[CODESEARCH-PREAMBLE]]'
case "$prompt" in
    *"$marker"*) exit 0 ;;  # already injected, idempotent
esac

# ------------------------------------------------------------------
# Resolve the codesearch scope for THIS working directory.
#
# In multi-repo serve mode every call needs project= or group=, and a fresh
# subagent has no way to know either. Telling it to "read the alias off the
# scope_required error" only works AFTER a failed call, and the alias is not
# guessable from the folder name (observed: directory NWND.ACME is
# registered as alias NWND.Acme). Worse, an agent that guesses a project=
# silently misses sibling repos: configuration commonly lives in its own
# repo, so the union that group= would have searched never gets searched and
# the agent concludes the code does not exist.
#
# We resolve it up front from repos.json, resolved the way
# db_discovery::repos::config_path() does it: the CODESEARCH_REPOS_CONFIG
# override first, else config_dir()/repos.json = dirs::home_dir()/.codesearch.
# web-guard reads the same file and honours the same override, so both hooks
# must agree - otherwise one session reads two different configs.
# No HTTP, no auth, no latency. Only the alias and the
# names of the groups it belongs to are injected - NOT the group's member
# list, which is the group definition's job to hold and status() to report,
# and NOT anything else from that file (it also stores a remote API key).
#
# Group names are sorted by codepoint so the .ps1 twin can produce byte-
# identical text (its default Sort-Object is culture- and case-insensitive,
# hence the explicit ordinal sort there).
#
# Fail-open: no git root, no config file, unparseable config, or a directory
# that is not a registered repo all fall through to the generic text below.
# ------------------------------------------------------------------
scope_block='Multi-repo serve mode: if a call returns "scope_required" or "Unknown alias",
add project="<repo-alias>" (single repo) or group="<group>" (cross-repo). The
error response lists the valid available_projects / available_groups - pick
from that list; the alias may differ from the folder name.'

cfg="${CODESEARCH_REPOS_CONFIG:-$HOME/.codesearch/repos.json}"
git_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
if [ -n "$git_root" ] && [ -f "$cfg" ]; then
    # Redirect the file in via the shell rather than passing a path argument:
    # on Git Bash a native jq.exe would not translate an MSYS-style $HOME.
    resolved=$(jq -r --arg root "$git_root" '
        def norm: gsub("\\\\"; "/") | ascii_downcase | sub("/+$"; "");
        ($root | norm) as $r
        | (.repos // {} | to_entries | map(select((.value | norm) == $r)) | .[0].key) as $alias
        | if $alias == null then empty
          else ([(.groups // {}) | to_entries[] | select(.value | index($alias)) | .key] | sort) as $g
            | "\($alias)\t\(if ($g | length) == 0 then "" else "group=\"" + ($g | join("\" | group=\"")) + "\"" end)"
          end' < "$cfg" 2>/dev/null || true)
    if [ -n "$resolved" ]; then
        tab=$(printf '\t')
        alias_name=${resolved%%${tab}*}
        group_clause=${resolved#*${tab}}
        if [ -n "$group_clause" ]; then
            scope_block="Multi-repo serve mode - the scope for THIS working directory is already
resolved; do not guess it from the folder name, the alias often differs:
  project=\"$alias_name\"
  $group_clause
Pass project= to search this repo alone, or one of the group= values above to
search it together with its siblings. A sibling repo often holds what this one
does not (configuration is commonly kept in a separate repo), so try group=
before concluding something is absent. Call status(kind=\"projects\") to see
which repos a group covers."
        else
            scope_block="Multi-repo serve mode - the scope for THIS working directory is already
resolved; do not guess it from the folder name, the alias often differs:
  project=\"$alias_name\"
It belongs to no group, so project= is the scope to pass."
        fi
    fi
fi

preamble=$(cat <<EOF
$marker
SEARCH RULE - read before doing anything:
codesearch is the preferred search tool for code in the current repo.
codesearch tools are DEFERRED: they do NOT appear in your tool list until you load them.

Load them first (before any Grep):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Then use:
  mcp__codesearch__search(query, mode="semantic")             -- concepts, identifiers, cross-file lookup
  mcp__codesearch__search(query, mode="literal", regex=true)  -- exact patterns / regex
  mcp__codesearch__find(symbol, kind="definition")             -- where a symbol is defined
  mcp__codesearch__find(symbol, kind="usages")                 -- all call sites
  mcp__codesearch__explore(target, kind="outline")             -- file/class structure
  mcp__codesearch__get_chunk(chunk_id)                         -- read a specific code chunk

$scope_block

Fall back to Grep only after codesearch returns no useful results, or when the
path is outside the current repo (codesearch covers internal paths only unless
you're in multi-repo serve mode with an explicit group).

Glob is NOT a substitute for codesearch on content lookup, but it IS the right
tool for filename and existence questions, and nothing here discourages that
use: codesearch has no file-listing primitive, and an empty codesearch result
cannot distinguish "not present" from "not indexed" (.venv, node_modules,
build output and binary assets are never indexed). Glob answers over the real
filesystem, so it is the reliable way to get a NEGATIVE answer.

If ToolSearch returns no codesearch tools, codesearch is not active for this
session - proceed with Grep/Glob as normal.
---
EOF
)
# `$(cat <<EOF)` strips the trailing newline, so re-add it: without this the
# `---` separator runs into the first line of the original prompt. The .ps1
# twin gets this for free from its here-string.
preamble="${preamble}
${prompt}"

# updatedInput must carry the FULL tool_input (Claude Code replaces, not merges) -
# take the original object and only overwrite `prompt`, so subagent_type,
# description, model, isolation etc. survive untouched.
echo "$raw" | jq --arg p "$preamble" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "allow",
    updatedInput: (.tool_input + {prompt: $p}),
    permissionDecisionReason: "Injected codesearch-first preamble into subagent prompt (deferred MCP tools need an explicit ToolSearch load)."
  }
}'
exit 0
