#!/usr/bin/env bash
# Installs the codesearch <-> Claude Code enforcement hooks.
# Bash/macOS/Linux twin of install.ps1 — see that file for full description.
# Requires: jq
#
# Usage:
#   bash integrations/claude-code/install.sh              # installs to ~/.claude
#   bash integrations/claude-code/install.sh --project     # installs to ./.claude

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOOKS_SRC="$SCRIPT_DIR/hooks"

SCOPE="user"
if [ "${1:-}" = "--project" ]; then
    SCOPE="project"
fi

if [ "$SCOPE" = "user" ]; then
    CLAUDE_DIR="$HOME/.claude"
else
    CLAUDE_DIR="$(pwd)/.claude"
fi

HOOKS_DEST="$CLAUDE_DIR/hooks/codesearch"
SETTINGS_PATH="$CLAUDE_DIR/settings.json"

mkdir -p "$HOOKS_DEST"
# NOTE: `codesearch hooks claude install` is the preferred, self-contained way
# to install these hooks (no source tree needed). This script remains as the
# from-source equivalent and must stay in sync with src/cli/claude_hooks.rs.
cp "$HOOKS_SRC/grep-guard.sh" "$HOOKS_DEST/"
cp "$HOOKS_SRC/subagent-preamble.sh" "$HOOKS_DEST/"
cp "$HOOKS_SRC/web-guard.sh" "$HOOKS_DEST/"
chmod +x "$HOOKS_DEST/grep-guard.sh" "$HOOKS_DEST/subagent-preamble.sh" "$HOOKS_DEST/web-guard.sh"

GREP_GUARD_CMD="bash \"$HOOKS_DEST/grep-guard.sh\""
PREAMBLE_CMD="bash \"$HOOKS_DEST/subagent-preamble.sh\""
WEB_GUARD_CMD="bash \"$HOOKS_DEST/web-guard.sh\""

mkdir -p "$CLAUDE_DIR"
if [ -f "$SETTINGS_PATH" ]; then
    backup="${SETTINGS_PATH}.bak-$(date +%Y%m%d-%H%M%S)"
    cp "$SETTINGS_PATH" "$backup"
    echo "Backed up existing settings to $backup"
    settings=$(cat "$SETTINGS_PATH")
else
    settings='{}'
fi

# Ensure hooks.PreToolUse exists as an array
settings=$(echo "$settings" | jq 'if has("hooks") then . else . + {hooks: {}} end
  | .hooks |= (if has("PreToolUse") then . else . + {PreToolUse: []} end)')

already_registered() {
    local cmd="$1"
    echo "$settings" | jq -e --arg cmd "$cmd" \
        '.hooks.PreToolUse[]?.hooks[]? | select(.command == $cmd)' >/dev/null 2>&1
}

add_matcher_hook() {
    local matcher="$1" cmd="$2"
    if already_registered "$cmd"; then
        echo "Already registered: $matcher -> $cmd (skipping)"
        return
    fi
    settings=$(echo "$settings" | jq --arg matcher "$matcher" --arg cmd "$cmd" \
        '.hooks.PreToolUse += [{matcher: $matcher, hooks: [{type: "command", command: $cmd}]}]')
    echo "Registered $matcher hook -> $cmd"
}

add_matcher_hook "Grep"             "$GREP_GUARD_CMD"
add_matcher_hook "Agent"            "$PREAMBLE_CMD"
add_matcher_hook "WebSearch|WebFetch" "$WEB_GUARD_CMD"

echo "$settings" | jq '.' > "$SETTINGS_PATH"

echo ""
echo "Done. Hooks installed to: $HOOKS_DEST"
echo "Settings updated: $SETTINGS_PATH"
echo ""
echo "Restart Claude Code (or start a new session) for the hooks to take effect."
