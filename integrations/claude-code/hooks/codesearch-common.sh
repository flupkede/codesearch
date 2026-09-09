#!/usr/bin/env bash
# Shared helpers for the codesearch Claude Code guard hooks. Sourced (never
# executed) by grep-guard.sh, edit-guard.sh and edit-guard-post.sh:
#   . "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/codesearch-common.sh"
#
# Coverage model (shared with the Rust serve hub, src/db_discovery/repos.rs):
# a repo is covered when its git root is REGISTERED in
# ~/.codesearch/repos.json (CODESEARCH_REPOS_CONFIG overrides the location),
# or when CODESEARCH_SERVER opts a pure remote-serve setup in. Everything
# here fails open: an unresolvable coverage question must allow, never deny.
# Requires: jq

# jq.exe builds on Windows emit CRLF; command substitution strips only the
# \n, leaving a stray \r that breaks every following string comparison.
# Every jq -r read pipes through this.
jq_str() {
    tr -d '\r'
}

# repos.json location — mirrors src/db_discovery/repos.rs `config_path()`:
# CODESEARCH_REPOS_CONFIG override > ~/.codesearch/repos.json.
repos_config_file() {
    if [ -n "${CODESEARCH_REPOS_CONFIG:-}" ]; then
        printf '%s' "$CODESEARCH_REPOS_CONFIG"
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
    local p="$1" norm probe root=""
    norm="${p%/}"
    norm="${norm%\\}"
    case "$norm" in
        # `/*` matches every absolute path — Windows drive, UNC and POSIX
        # alike (a Windows-style-only pattern used to strand POSIX absolute
        # paths on the cwd branch, #199).
        [A-Za-z]:[\\/]*|/*)
            probe="$norm"
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
