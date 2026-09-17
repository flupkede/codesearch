#!/usr/bin/env bash
# PreToolUse hook: steer WebSearch/WebFetch toward codesearch remote doc mounts.
#
# Why this exists: when codesearch has remote documentation projects mounted
# (e.g. cloud/inriver, cloud/example-dam), those indexes usually answer product /
# API / docs questions more precisely — and more currently — than an open web
# search. Nothing structurally stops the model from reaching for the always-on
# WebSearch/WebFetch tools first, so this hook makes the preference structural:
# the FIRST WebSearch/WebFetch *about a mounted product* is blocked with
# actionable guidance; a further call about that same mount within 5 minutes
# (i.e. the mounts didn't have the answer) is let through.
#
# TOPIC SCOPING (the important part): a mount only earns the right to intercept
# queries about ITS OWN subject. Mounting cloud/acme says "I have the Acme
# docs indexed" — it says nothing about Rust, Claude Code, or Azure CLI. So the
# hook first decides whether the query is plausibly about a mounted product, and
# stays out of the way entirely when it isn't. A mount's alias is its keyword
# (cloud/acme -> "acme"); anything else it should answer for is declared in
# repos.json under `.remote_mount_topics` — see README.
#
# Passes through (exit 0, no block) when:
#   - there are NO remote mounts to steer toward (nothing indexed to prefer)
#   - the query/URL doesn't mention any mounted product or its declared topics
#   - the same MOUNT was already steered for in the last 5 minutes. Keyed on the
#     matched mount, NOT on the exact query string, on purpose: the natural
#     follow-up after "the mirror had nothing" is a REFINED web query, and an
#     exact-string key treats every refinement as a fresh first attempt and
#     blocks it again — punishing precisely the correct behaviour.
#
# Bash/macOS/Linux twin of web-guard.ps1. Requires: jq
#
# Install: see ../README.md (or run `codesearch hooks claude install`).

set -euo pipefail

raw="$(cat)"
[ -z "$raw" ] && exit 0

tool=$(echo "$raw" | jq -r '.tool_name // empty')
case "$tool" in
    WebSearch | WebFetch) ;;
    *) exit 0 ;;
esac

# Query (WebSearch) or target URL (WebFetch) — used for matching, the cache key
# and the guidance text.
q=$(echo "$raw" | jq -r '.tool_input.query // .tool_input.url // empty')
[ -z "$q" ] && exit 0

# ------------------------------------------------------------------
# 1. Are there any remote doc mounts to steer toward?
#
# Mounts live in repos.json under `.remote_mounts` (canonical "<peer>/<alias>"
# names — the opt-in allowlist). No mounts -> nothing to prefer -> allow the
# web call unimpeded.
# ------------------------------------------------------------------
config="${CODESEARCH_REPOS_CONFIG:-$HOME/.codesearch/repos.json}"
[ -f "$config" ] || exit 0

# NOTE: strip CR. On Windows/Git Bash jq emits CRLF, and a trailing \r silently
# survives into the alias ("acme\r"), which then never matches anything —
# the hook degrades to "never block" without any visible error.
mapfile -t mount_list < <(jq -r '(.remote_mounts // [])[]' < "$config" 2>/dev/null | tr -d '\r' || true)
[ "${#mount_list[@]}" -eq 0 ] && exit 0

# ------------------------------------------------------------------
# 2. Is this query actually ABOUT one of the mounted products?
#
# Keywords per mount: the alias itself, plus any extra terms declared in
# `.remote_mount_topics` (keyed by the full "<peer>/<alias>" or the bare alias).
# Matching is case-insensitive on a word-ish boundary, so "acme" hits
# "Acme DAM API" and "help.acme.com" but not "acmexyz".
#
# No match -> this simply isn't the mirror's subject -> allow, silently.
# ------------------------------------------------------------------
hay=$(printf '%s' "$q" | tr '[:upper:]' '[:lower:]')

matches_kw() {
    local kw="$1"
    [ -z "$kw" ] && return 1
    [[ "$hay" =~ (^|[^a-z0-9])"$kw"([^a-z0-9]|$) ]]
}

matched=()
for mount in "${mount_list[@]}"; do
    [ -z "$mount" ] && continue
    alias_name="${mount##*/}"

    hit=0
    if matches_kw "$(printf '%s' "$alias_name" | tr '[:upper:]' '[:lower:]')"; then
        hit=1
    else
        while IFS= read -r topic; do
            [ -z "$topic" ] && continue
            if matches_kw "$(printf '%s' "$topic" | tr '[:upper:]' '[:lower:]')"; then
                hit=1
                break
            fi
        done < <(jq -r --arg m "$mount" --arg a "$alias_name" \
                    '((.remote_mount_topics // {}) | (.[$m] // .[$a] // []))[]' \
                    < "$config" 2>/dev/null | tr -d '\r' || true)
    fi

    [ "$hit" -eq 1 ] && matched+=("$mount")
done

[ "${#matched[@]}" -eq 0 ] && exit 0

relevant=$(printf '%s, ' "${matched[@]}")
relevant="${relevant%, }"
primary="${matched[0]}"

# ------------------------------------------------------------------
# 3. Retry cache: same mount steered for recently -> let it through.
#    Covers "tried the mounts, they had nothing, now use the web".
#
# NOTE: feed the cache file to jq via stdin redirection (`< file`), never as a
# positional path argument — see grep-guard.sh for the Windows/Git-Bash rationale.
# ------------------------------------------------------------------
cache_file="${TMPDIR:-/tmp}/.codesearch-web-guard.json"
cache_ttl=300
now=$(date +%s)
# Keyed on tool + primary matched mount, NOT the raw query: one steer per vendor
# per window. An exact-query key made every refinement of the search terms look
# like a first attempt and blocked it again, which is the opposite of the intent.
cache_key="$tool|$primary"

if [ -f "$cache_file" ]; then
    blocked_at=$(jq -r --arg k "$cache_key" '.[$k] // empty' < "$cache_file" 2>/dev/null || true)
    if [ -n "$blocked_at" ] && [ $((now - blocked_at)) -lt "$cache_ttl" ]; then
        exit 0
    fi
fi

# Prune stale entries and record this block.
if [ -f "$cache_file" ]; then
    tmp=$(mktemp)
    jq --arg k "$cache_key" --argjson now "$now" --argjson ttl "$cache_ttl" \
        'with_entries(select(($now - .value) < $ttl)) + {($k): $now}' \
        < "$cache_file" > "$tmp" 2>/dev/null && mv "$tmp" "$cache_file" || true
else
    jq -n --arg k "$cache_key" --argjson now "$now" '{($k): $now}' > "$cache_file" 2>/dev/null || true
fi

# ------------------------------------------------------------------
# 4. Block with actionable guidance — naming ONLY the mounts that matched.
# ------------------------------------------------------------------
msg=$(cat <<EOF
This looks like a question about a product whose documentation codesearch has
indexed — search that mirror before the web.
Relevant mount(s): ${relevant}

These indexed mounts often answer product/API/docs questions more precisely
(and more currently) than a web search, and they cover vendor sites that need a
login and would fail an anonymous fetch anyway.

Step 1 — load the deferred MCP tool schemas (one-time per conversation):
  ToolSearch("select:mcp__codesearch__search,mcp__codesearch__get_chunk")

Step 2 — search the relevant mount (compact=false reads matching content inline):
  mcp__codesearch__search(query="${q}", project="${primary}", compact=false)
  mcp__codesearch__get_chunk(chunk_ref="${primary}:<id from a result>")  # full context

For a canonical source link, read the doc's front-matter chunk (start_line 0)
and cite its \`url:\` field verbatim rather than reconstructing a URL.

If the mount does NOT have the answer, go straight to the web: any further
${tool} call about '${primary}' is allowed for the next 5 minutes. You do NOT
need to repeat this call verbatim — refining your search terms is fine and will
not be blocked again.
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
