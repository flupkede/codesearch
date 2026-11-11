#!/usr/bin/env bash
# Shared helpers for the codesearch Claude Code guard hooks. Sourced (never
# executed) by grep-guard.sh, edit-guard.sh and edit-guard-post.sh:
#   . "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/codesearch-common.sh"
#
# Coverage model (shared with the Rust serve hub, src/db_discovery/repos.rs):
# a repo is covered when its git root is REGISTERED in
# ~/.codesearch/repos.json (CODESEARCH_REPOS_CONFIG overrides the location,
# CODESEARCH_HOME relocates the whole global root),
# or when CODESEARCH_SERVER opts a pure remote-serve setup in. Everything
# here fails open: an unresolvable coverage question must allow, never deny.
# Requires: jq

# jq.exe builds on Windows emit CRLF; command substitution strips only the
# \n, leaving a stray \r that breaks every following string comparison.
# Every jq -r read pipes through this.
jq_str() {
    tr -d '\r'
}

# True when a shell command line invokes grep/egrep/fgrep/zgrep/rg — the Bash
# tool escape hatch around the Grep-tool guard. Same detector contract as the
# opencode plugin's bashCommandGrep: quoted spans are stripped first ("echo
# 'grep is slow'" is not an invocation), separators become spaces so unquoted
# pipes still split, and tokens compare whole after lowercasing, .exe and
# path-prefix removal ("progress" and "rgx" never match; /usr/bin/grep does).
command_grep_family() {
    local cmd="$1" stripped token
    [ -n "$cmd" ] || return 1
    stripped=$(printf '%s' "$cmd" | sed -E 's/"(\\.|[^"\\])*"/ /g')
    stripped=$(printf '%s' "$stripped" | sed -E $'s/\'(\\\\.|[^\'\\\\])*\'/ /g')
    stripped=$(printf '%s' "$stripped" | tr ';|&()`' '      ')
    for token in $stripped; do
        token=$(printf '%s' "$token" | tr '[:upper:]' '[:lower:]')
        token="${token%.exe}"
        token="${token##*[\\/]}"
        case "$token" in
            grep|egrep|fgrep|zgrep|rg) return 0 ;;
        esac
    done
    return 1
}

# Absolute paths a Bash grep will read files from, one per line. Empty when
# the command has no path operand at all (grep then walks the Bash tool's cwd).
#
# Why this exists: the Bash tool has no `path` parameter, so the guard used to
# resolve coverage from the SESSION cwd. A grep aimed at /tmp or at a sibling
# repo was then denied and blamed on the session's project (reported: a
# kcharts session grepping /tmp docs and platform/audits). The Bash tool does
# have a per-call cwd, and `cd X && grep ...` moves the operands with it, so
# resolve the operands instead: a grep whose operands are all outside every
# indexed repo is legitimate and must not be denied.
#
# Quoted spans are stripped first, exactly like command_grep_family, so a
# pattern such as "^./.git/" is never mistaken for a path operand.
command_grep_targets() {
    local cmd="$1" base="${PWD}" first stripped seg tok i w
    local -a words
    [ -n "$cmd" ] || return 0
    case "$cmd" in
        cd\ *)
            first="${cmd#cd }"
            first="${first%%&&*}"
            first="${first%%;*}"
            first="${first%%|*}"
            first="${first# }"
            first="${first%"${first##*[! ]}"}"
            case "$first" in
                \"*\") first="${first#\"}"; first="${first%\"}" ;;
                \'*\') first="${first#\'}"; first="${first%\'}" ;;
            esac
            case "$first" in
                ''|-) ;;
                *) base="$first" ;;
            esac
            ;;
    esac
    case "$base" in
        [A-Za-z]:[\\/]*|/*|'~'*) ;;
        *) base="${PWD%/}/$base" ;;
    esac
    # Quoted spans first (a pattern like "^./.git/" is not a path), then one
    # pipeline SEGMENT per line so the pattern/operand split is per grep. The
    # placeholder keeps the pattern's ARGUMENT POSITION: replacing it with
    # whitespace would promote the first path operand to "pattern" and hide it.
    stripped=$(printf '%s' "$cmd" | sed -E 's/"(\\.|[^"\\])*"/__CS_Q__/g')
    stripped=$(printf '%s' "$stripped" | sed -E $'s/\'(\\\\.|[^\'\\\\])*\'/__CS_Q__/g')
    stripped=$(printf '%s' "$stripped" | tr ';|&()`' '\n\n\n\n\n')
    while IFS= read -r seg; do
        [ -n "$seg" ] || continue
        read -r -a words <<<"$seg"
        local gidx=-1 pattern_seen=0
        for ((i = 0; i < ${#words[@]}; i++)); do
            w=$(printf '%s' "${words[i]}" | tr '[:upper:]' '[:lower:]')
            w="${w%.exe}"
            w="${w##*[\\/]}"
            case "$w" in
                grep|egrep|fgrep|zgrep|rg) gidx=$i; break ;;
            esac
        done
        [ "$gidx" -ge 0 ] || continue
        for ((i = gidx + 1; i < ${#words[@]}; i++)); do
            tok="${words[i]}"
            case "$tok" in
                -*) continue ;;  # flag (and its inline value)
            esac
            # The first bare word is grep's PATTERN; the rest are paths.
            if [ "$pattern_seen" -eq 0 ]; then
                pattern_seen=1
                continue
            fi
            case "$tok" in
                [A-Za-z]:[\\/]*|/*) ;;
                '~'*) tok="${HOME:-}${tok#\~}" ;;
                .) tok="$base" ;;
                *) tok="${base%/}/${tok#./}" ;;
            esac
            [ "$tok" = "/" ] || tok="${tok%/}"
            [ -n "$tok" ] && printf '%s\n' "$tok"
        done
    done <<EOF
$stripped
EOF
}

# repos.json location — mirrors src/db_discovery/repos.rs `config_path()`
# and constants.rs `codesearch_home()`:
# CODESEARCH_REPOS_CONFIG > $CODESEARCH_HOME/repos.json > ~/.codesearch/repos.json.
repos_config_file() {
    if [ -n "${CODESEARCH_REPOS_CONFIG:-}" ]; then
        printf '%s' "$CODESEARCH_REPOS_CONFIG"
    elif [ -n "${CODESEARCH_HOME:-}" ]; then
        printf '%s' "${CODESEARCH_HOME%/}/repos.json"
    else
        printf '%s' "${HOME:-}/.codesearch/repos.json"
    fi
}

# Normalize one path for comparison: drop a Windows extended-length prefix
# (\\?\ — exactly 4 chars), unify backslashes to forward slashes (Git-Bash
# reports C:/x/y while repos.json records "C:\\x\\y"), and trim trailing
# separators. Registration canonicalizes paths before writing them
# (safe_canonicalize), so after this both sides agree byte-for-byte on
# POSIX and component-wise on Windows.
norm_repo_path() {
    local p="$1"
    p="${p%$'\r'}"
    case "$p" in
        '\\?\'*) p="${p:4}" ;;
    esac
    p="${p//\\//}"
    while [ "$p" != "/" ] && [ "${p%/}" != "$p" ]; do
        p="${p%/}"
    done
    printf '%s' "$p"
}

# Path equality: exact on POSIX, case-insensitive for Windows drive-letter
# paths (NTFS is case-insensitive throughout) — mirrors the serve hub's own
# /indexing resolver, which folds case on Windows only.
repo_path_eq() {
    local a b
    a="$(norm_repo_path "$1")"
    b="$(norm_repo_path "$2")"
    case "$a" in [A-Za-z]:/*) a="$(printf '%s' "$a" | tr '[:upper:]' '[:lower:]')" ;; esac
    case "$b" in [A-Za-z]:/*) b="$(printf '%s' "$b" | tr '[:upper:]' '[:lower:]')" ;; esac
    [ "$a" = "$b" ]
}

# Is this git root covered by codesearch — registered with the local serve
# hub (repos.json), or a CODESEARCH_SERVER opt-in for pure remote-serve
# setups? Fails OPEN on any resolver problem (missing/unreadable/malformed
# repos.json, missing jq): a guard that cannot resolve coverage must allow,
# never deny.
target_registered() {
    local root="$1" cfg reg
    [ -n "${CODESEARCH_SERVER:-}" ] && return 0
    cfg="$(repos_config_file)"
    [ -n "$cfg" ] || return 1
    [ -r "$cfg" ] || return 1
    while IFS= read -r reg; do
        [ -n "$reg" ] || continue
        if repo_path_eq "$reg" "$root"; then
            return 0
        fi
    # jq gets the config via stdin redirection, never as a positional path
    # argument — native jq.exe builds mangle POSIX-style paths (see the
    # same note in web-guard.sh).
    done < <(jq -r '(.repos // {}) | to_entries[] | .value | tostring' < "$cfg" 2>/dev/null)
    return 1
}

# Git root of the repo a target path lives in — the TARGET's repo, never the
# hook's cwd one, for absolute paths; empty/relative paths are relative to
# the cwd by definition and resolve against it. Prints the root; empty when
# the path is outside any git repo (or git is unusable).
resolve_target_git_root() {
    local p="$1" norm probe prev="" root=""
    norm="${p%/}"
    norm="${norm%\\}"
    case "$norm" in
        # `/*` matches every absolute path — Windows drive, UNC and POSIX
        # alike (a Windows-style-only pattern used to strand POSIX absolute
        # paths on the cwd branch, #199).
        [A-Za-z]:[\\/]*|/*)
            probe="$norm"
            # Walk up to the nearest EXISTING ancestor before asking git: a
            # glob operand (`src/*.rs`) or a not-yet-created path has no git
            # root of its own, and treating it as "not in a repo" would let a
            # grep of an indexed repo through (the Bash guard now passes
            # operand paths here).
            while [ -n "$probe" ] && [ ! -e "$probe" ] && [ "$probe" != "$prev" ]; do
                prev="$probe"
                probe="$(dirname "$probe")"
            done
            [ -f "$probe" ] && probe="$(dirname "$probe")"
            root=$(git -C "$probe" rev-parse --show-toplevel 2>/dev/null || true)
            ;;
        *)
            root=$(git rev-parse --show-toplevel 2>/dev/null || true)
            ;;
    esac
    printf '%s' "$root"
}

# Canonical state-file key for a file path: edit-guard reads the state by
# this key and edit-guard-post writes it, so both sides must derive the SAME
# key from whatever path form the agent happened to pass. Absolute-izes
# relative paths against the cwd, then reuses the repos.json normalization
# plus repo_path_eq's Windows drive-letter casefold.
norm_state_key() {
    local p="$1" n
    [ -n "$p" ] || return 0
    case "$p" in
        [A-Za-z]:[\\/]*|/*) ;;
        *) p="$PWD/${p#./}" ;;
    esac
    n="$(norm_repo_path "$p")"
    case "$n" in [A-Za-z]:/*) n="$(printf '%s' "$n" | tr '[:upper:]' '[:lower:]')" ;; esac
    printf '%s' "$n"
}
