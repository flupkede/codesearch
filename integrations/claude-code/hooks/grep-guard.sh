#!/usr/bin/env bash
# PreToolUse hook: enforce codesearch-first for Grep on internal repo paths.
# Bash/macOS/Linux twin of grep-guard.ps1 — see that file for full rationale.
# Requires: jq, curl
#
# Grep is auto-allowed ONLY when the codesearch serve hub is genuinely
# unreachable ("plat"). A low-confidence / empty codesearch *result* is a
# SUCCESSFUL call ("reformulate your query"), NOT "codesearch is down" — so it
# must never open the grep escape hatch. The previous version used a blind
# "same query retried within 5 min" proxy that could not tell those two apart
# and leaked grep on every low-confidence result. We now probe the
# unauthenticated /healthz liveness endpoint directly.
#
# The target repo is resolved from the GREP TARGET itself (its own git root),
# never from the hook's cwd — absolute paths into a different repo resolve
# against THAT repo (#54, and see the #199 note below: POSIX absolute paths
# used to slip through this very test). Coverage is decided by REGISTRATION
# with the local serve hub (repos.json), not by a .codesearch.db directory
# (#199). When the target repo is covered and LIVE but MID-REINDEX
# (branch-switch full refresh, #55), the hook WAITS out the rebuild — polling
# /indexing for up to CODESEARCH_GREP_GUARD_WAIT_MS (default 120s, 0 = legacy
# instant WAIT message) — then denies with "retry codesearch now"; past the
# cap grep opens. Denying grep while every codesearch call times out
# deadlocks the session (the DRM-29395 report).
#
# Install: see ../README.md (or run ../install.sh to wire this up automatically).

set -euo pipefail

# Shared coverage/target-resolution helpers (repos.json registration, path
# normalization) live in codesearch-common.sh, shared with edit-guard.
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/codesearch-common.sh"

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty' | jq_str)

# Bash escape hatch: a Bash tool command invoking the grep family is guarded
# exactly like the Grep tool — the shell tool is otherwise a clean way around
# the Grep denial. A shell grep has no `path` parameter, so coverage follows
# the command's OWN operands (`command_grep_targets`: absolute paths, relative
# paths against the shell's cwd, and a leading `cd DIR &&` base). With no
# operand at all, grep walks the cwd, and the cwd repo is the target.
path=""
target_root=""
if [ "$tool" = "Bash" ]; then
    shell_cmd=$(echo "$raw" | jq -r '.tool_input.command // empty' | jq_str)
    command_grep_family "$shell_cmd" || exit 0
    bash_targets="$(command_grep_targets "$shell_cmd")"
    if [ -n "$bash_targets" ]; then
        while IFS= read -r candidate; do
            [ -n "$candidate" ] || continue
            root="$(resolve_target_git_root "$candidate")"
            if [ -n "$root" ] && target_registered "$root"; then
                target_root="$root"
                break
            fi
        done <<EOF
$bash_targets
EOF
        # Every operand is outside every indexed repo (or not in git at all):
        # this grep is not a code search of an indexed project.
        [ -n "$target_root" ] || exit 0
    else
        target_root="$(resolve_target_git_root "")"
    fi
else
    [ "$tool" = "Grep" ] || exit 0
    path=$(echo "$raw" | jq -r '.tool_input.path // empty' | jq_str)
fi

# ------------------------------------------------------------------
# 1+2. Resolve the TARGET repo (the repo this Grep is aimed at) and check
#      codesearch coverage THERE — never in the hook's cwd.
#
# History (#54): coverage used to be resolved from the hook's cwd. An
# absolute-path Grep into a DIFFERENT indexed repo then failed the
# startswith(cwd-repo-root) test, looked "external", and was allowed even
# though its target repo was fully covered. The guard now follows the
# target: empty/relative paths resolve against the cwd repo (they are
# relative to it by definition), absolute paths resolve against the git
# root of the path itself.
#
# History (#199): the #54 rewrite still mis-binned POSIX absolute paths
# (/home/..., /tmp/...): its absolute-detection pattern only matched
# Windows-style roots (C:\, C:/, MSYS /c/, UNC //server), so a POSIX
# absolute target fell into the relative branch and resolved against the
# hook's cwd — with a session cwd that is not itself a git repo (a parent
# directory holding several repos) the target resolved to NOTHING, looked
# external, and the guard silently allowed every grep. `/*` now covers
# every absolute path.
#
# Coverage signal (#199): the target's git root is REGISTERED with the
# local serve hub (~/.codesearch/repos.json, honoring the
# CODESEARCH_REPOS_CONFIG / CODESEARCH_HOME overrides — the same
# registration list the hub itself resolves queries by). A `.codesearch.db` directory at
# the git root was only ever a proxy for that and is wrong in both
# directions: a stale db from a since-unregistered repo denied Grep while
# the hub could not actually answer for it (unknown alias), and a
# registered repo whose db directory was gone slipped through uncovered.
# Matching the git ROOT (never a path prefix) also carves out nested
# repos for free: an unregistered clone nested inside a registered repo
# resolves to its OWN git root, equals no registration, and is correctly
# treated as uncovered. The explicit CODESEARCH_SERVER opt-in stays for
# pure remote-serve setups with no local repos.json (#199 tracks that it
# is a URL override rather than a coverage signal — to be reworked
# separately).
# ------------------------------------------------------------------
# Grep tool: resolve coverage from the tool's own `path` input. The Bash
# branch above already resolved the command's operands (and exited when none
# of them is an indexed repo), so it must not be re-resolved from `path=""`.
[ "$tool" = "Bash" ] || target_root="$(resolve_target_git_root "$path")"

# Not inside any git repo (or git unusable) -> external target: grep is right.
[ -z "$target_root" ] && exit 0

# Covered = the target's git root is registered with the serve hub
# (repos.json), or CODESEARCH_SERVER opts a pure remote-serve setup in.
target_registered "$target_root" || exit 0

# ------------------------------------------------------------------
# 3. Is the codesearch serve hub actually UP right now? (Liveness probe.)
#
# This is the ONLY condition under which grep is auto-allowed: codesearch is
# genuinely unreachable ("plat"). We probe the unauthenticated /healthz
# liveness endpoint (fixed {"status":"ok"} body, no API key required). A
# reachable server -> DENY grep and force a codesearch reformulation, even when
# a previous codesearch call returned a low-confidence / empty result — an empty
# *result* is a SUCCESSFUL call, not a dead server, so it must NOT open the
# escape hatch. Only a connection-level failure means the server is down.
#
# Base URL resolution (mirrors codesearch src/constants.rs):
#   CODESEARCH_SERVER (full base URL) > http://127.0.0.1:$CODESEARCH_SERVE_PORT
#   > http://127.0.0.1:39725  (DEFAULT_SERVE_URL / DEFAULT_SERVE_PORT)
# ------------------------------------------------------------------
if [ -n "${CODESEARCH_SERVER:-}" ]; then
    base="${CODESEARCH_SERVER%/}"
elif [ -n "${CODESEARCH_SERVE_PORT:-}" ]; then
    base="http://127.0.0.1:${CODESEARCH_SERVE_PORT}"
else
    base="http://127.0.0.1:39725"
fi

# curl: -sS quiet (errors to stderr), --max-time 2 short timeout; the body is
# discarded via the shell redirect below. We deliberately do NOT use curl's
# `-o /dev/null` — on Windows/Git-Bash a native curl.exe fails writing to the
# translated /dev/null path (exit 23) even on a healthy 200, which would
# misreport an UP server as down. Shell-level `>/dev/null` avoids that.
# Without -f, any HTTP response (even 4xx/5xx) yields exit 0 = reachable/up;
# only a connection-level failure (exit 7/28/…) means it's down -> allow grep.
if ! curl -sS --max-time 2 "${base}/healthz" >/dev/null 2>&1; then
    exit 0  # codesearch is DOWN -> grep is genuinely all you have, let it through
fi

# Wait-cap for an in-flight rebuild (parity with the opencode grep-guard
# plugin and the ps1 twin). 0 = legacy behaviour: instant WAIT-AND-RETRY deny.
wait_cap_ms=120000
case "${CODESEARCH_GREP_GUARD_WAIT_MS:-}" in
    ''|*[!0-9]*) ;;  # not a plain non-negative integer: keep the default
    *) wait_cap_ms=$CODESEARCH_GREP_GUARD_WAIT_MS ;;
esac

# /indexing probe auth: a network-bound serve with require_auth_for_network
# needs the bearer key; a local hub ignores the header entirely. The empty-
# array guard keeps macOS's bash 3.2 + `set -u` from choking on it.
probe_auth=()
if [ -n "${CODESEARCH_API_KEY:-}" ]; then
    probe_auth=(-H "Authorization: Bearer $CODESEARCH_API_KEY")
fi

# ------------------------------------------------------------------
# 3.5 Is the TARGET repo mid-reindex right now? (Freshness probe, #55.)
#
# Liveness (/healthz) says the server is UP; it says nothing about index
# FRESHNESS. Right after a branch switch the serve watcher fires a full
# refresh, and searches against the mid-rebuild index return stale/empty
# results — which used to push the agent into a manual grep approval on
# every routine checkout. GET /indexing?path=<repo root> resolves the
# target to its registered repo and reports an active reindex. When one is
# in flight, the hook waits it out — polling up to CODESEARCH_GREP_GUARD_WAIT_MS
# (default 120s, 0 = legacy instant WAIT deny) — then denies with a
# retry-codesearch instruction; past the cap grep opens.
#
# Backward compat: an older serve without this endpoint answers 404; the
# probe is skipped and behaviour is exactly the pre-#55 deny. The ?path=
# value is percent-encoded (jq @uri) — a repo root containing spaces, +,
# &, # or non-ASCII would otherwise make the probe fail or answer for the
# WRONG repo prefix, silently disabling the wait-and-retry path.
# ------------------------------------------------------------------
enc=$(printf '%s' "$target_root" | jq -sRr @uri)
if fresh_json=$(curl -sS --max-time 2 ${probe_auth[@]+"${probe_auth[@]}"} "${base}/indexing?path=${enc}" 2>/dev/null); then
    covered=$(echo "$fresh_json" | jq -r '.covered // false' 2>/dev/null || echo false)
    indexing=$(echo "$fresh_json" | jq -r '.indexing // false' 2>/dev/null || echo false)
    if [ "$covered" = "true" ] && [ "$indexing" = "true" ]; then
        if [ "$wait_cap_ms" -le 0 ]; then
            msg=$(cat <<'EOF'
codesearch is LIVE, but THIS repo's index is being rebuilt right now (a
branch switch or file change fired the serve watcher's full refresh).
Searching immediately would return stale or empty results — that is the
rebuild in progress, not a miss, and NOT a reason to grep.

WAIT 15-30 seconds (Bash: sleep 20), then RETRY your codesearch call — it
will answer normally once the rebuild lands. The working tree did not
change; only the index is catching up, so grep adds nothing here.

If the rebuild still has not landed after ~2 minutes, run your codesearch
call anyway (a partially fresh index still beats grep) or ask the user how
to proceed.
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
        fi
        # A big-repo rebuild outlasts any polite WAIT message, so the hook
        # waits itself and only denies once queries will actually answer —
        # or opens grep when the cap expires, the one state where grep is
        # genuinely all you have (parity with the ps1 twin / opencode plugin).
        # Second-granularity clock: millis are not portable (BSD date has no
        # %3N), and the cap only needs "at least this long" semantics.
        deadline=$(( $(date +%s) + (wait_cap_ms + 999) / 1000 ))
        while :; do
            now=$(date +%s)
            rest=$(( deadline - now ))
            [ "$rest" -le 0 ] && exit 0  # still rebuilding at the cap: grep is all you have
            if [ "$rest" -gt 3 ]; then sleep 3; else sleep 1; fi
            if fresh_json=$(curl -sS --max-time 5 ${probe_auth[@]+"${probe_auth[@]}"} "${base}/indexing?path=${enc}" 2>/dev/null); then
                covered=$(echo "$fresh_json" | jq -r '.covered // false' 2>/dev/null || echo false)
                indexing=$(echo "$fresh_json" | jq -r '.indexing // false' 2>/dev/null || echo false)
                if [ "$covered" != "true" ] || [ "$indexing" != "true" ]; then
                    msg="codesearch rebuild on this repo landed while the guard waited
(CODESEARCH_GREP_GUARD_WAIT_MS=${wait_cap_ms}ms). Retry your codesearch call
now — it will answer normally. Do NOT grep."
                    jq -n --arg msg "$msg" '{
                      hookSpecificOutput: {
                        hookEventName: "PreToolUse",
                        permissionDecision: "deny",
                        permissionDecisionReason: $msg
                      }
                    }'
                    exit 0
                fi
            fi
            # probe failure mid-wait: keep polling until the cap
        done
    fi
fi

# ------------------------------------------------------------------
# 4. Block with actionable guidance
# ------------------------------------------------------------------
msg=$(cat <<'EOF'
codesearch is LIVE for this repo (its /healthz probe just answered) — use it,
do NOT fall back to Grep, and do NOT work around this via a shell grep/rg in
the Bash tool: the same guard denies that too. Grep on an indexed internal
path is only auto-allowed when the hub is DOWN or this repo's index is stuck
mid-rebuild past the guard's wait cap — neither is the case right now.

IMPORTANT: a low-confidence or EMPTY codesearch result is a SUCCESSFUL call that
means "reformulate your query" — it does NOT mean codesearch is down and it will
NOT unblock Grep. Reformulate instead of grepping.

Step 1 — load the deferred MCP tool schemas (Claude Code defers all MCP tools;
this is a one-time step per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__find,mcp__codesearch__explore,mcp__codesearch__get_chunk")

Step 2 — pick the RIGHT tool (this is usually why a query came back empty):
  find(symbol="Name", kind="definition")   -- known symbol / type / function definition
  find(symbol="Name", kind="usages")       -- all call sites of a known symbol
  explore(kind="outline", target="path")   -- every symbol in one file
  search(query="concept", mode="semantic") -- concepts / cross-file, the DEFAULT
  search(query="exact", mode="literal", regex=true) -- exact syntax / pattern

Query hygiene (this is what produces "low_confidence: []"):
  * Do NOT paste grep-style multi-term alternations ("a|b|c", "::", "fn foo(")
    into search — BM25 tokenises on punctuation and the match scores below the
    relevance floor, so you get an empty result even though the string exists.
  * Use ONE clean term, or switch to find()/explore() for exact symbols.

Multi-repo serve mode: if the call returns a "scope_required" or "Unknown alias"
error, you MUST pass project="<repo-alias>" (single repo) or group="<group>"
(cross-repo). The error response LISTS the valid available_projects /
available_groups — pick from that list (the alias may differ from the folder
name).

Grep is always allowed for targets outside any git repo, for repos that are
not registered with the codesearch serve hub (repos.json), and for pure
remote-serve setups opted in via CODESEARCH_SERVER.
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
