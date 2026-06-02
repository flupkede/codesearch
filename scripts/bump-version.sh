#!/usr/bin/env bash
# scripts/bump-version.sh — Manual version bump for feature branches
#
# Bash equivalent of scripts/bump-version.ps1 — runs on Linux/macOS.
#
# Usage:
#   bash scripts/bump-version.sh                    # patch bump (default)
#   bash scripts/bump-version.sh --type minor       # minor bump
#   bash scripts/bump-version.sh --type major       # major bump
#   bash scripts/bump-version.sh --desc "new feature"
#
# This bumps Cargo.toml + Cargo.lock and optionally updates AGENTS.md.
# It does NOT commit — that's your decision (the script asks).
#

set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────────
BUMP_TYPE="patch"
DESCRIPTION=""
REPO_ROOT="$(git rev-parse --show-toplevel)"

# ── Parse arguments ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --type|-t)
            BUMP_TYPE="$2"; shift 2 ;;
        --desc|-d)
            DESCRIPTION="$2"; shift 2 ;;
        --help|-h)
            echo "Usage: $0 [--type major|minor|patch] [--desc \"description\"]"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# Validate bump type
case "$BUMP_TYPE" in
    major|minor|patch) ;;
    *) echo "Invalid type: $BUMP_TYPE (use major, minor, or patch)" >&2; exit 1 ;;
esac

# ── Colors ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

# ── Read current version ─────────────────────────────────────────────────────
cd "$REPO_ROOT"

CARGO_TOML="Cargo.toml"
CURRENT=$(grep -m1 '^version = ' "$CARGO_TOML" | sed 's/version = "\(.*\)"/\1/')

if [[ -z "$CURRENT" ]]; then
    echo -e "${RED}Error: cannot read version from $CARGO_TOML${NC}" >&2
    exit 1
fi

echo -e "${CYAN}Current version: $CURRENT${NC}"

# ── Calculate new version ────────────────────────────────────────────────────
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "$BUMP_TYPE" in
    major)
        NEW_VERSION="$((MAJOR + 1)).0.0"
        CHANGE_TYPE="Major"
        ;;
    minor)
        NEW_VERSION="$MAJOR.$((MINOR + 1)).0"
        CHANGE_TYPE="Minor"
        ;;
    patch)
        NEW_VERSION="$MAJOR.$MINOR.$((PATCH + 1))"
        CHANGE_TYPE="Patch"
        ;;
esac

echo -e "${GREEN}New version: $NEW_VERSION ($CHANGE_TYPE)${NC}"

# ── Update Cargo.toml ────────────────────────────────────────────────────────
echo -e "${CYAN}→ Updating Cargo.toml...${NC}"
sed -i "0,/^version = \"$CURRENT\"/s//version = \"$NEW_VERSION\"/" "$CARGO_TOML"

# Keep Cargo.lock in sync
cargo update --workspace --quiet 2>/dev/null || true

# ── Update AGENTS.md ─────────────────────────────────────────────────────────
echo -e "${CYAN}→ Updating AGENTS.md...${NC}"
TODAY=$(date +%Y-%m-%d)
AGENTS_MD="AGENTS.md"

if [[ -f "$AGENTS_MD" ]]; then
    # Build the new section
    NEW_SECTION=$'\n'"## [$NEW_VERSION] - $TODAY"$'\n\n'"### $CHANGE_TYPE"
    if [[ -n "$DESCRIPTION" ]]; then
        NEW_SECTION+=$'\n\n'"$DESCRIPTION"
    fi
    NEW_SECTION+=$'\n\n'"---"

    # Insert after the second "---" marker (after the header block)
    # Use awk to find the second "---" and insert after it
    TEMP_FILE=$(mktemp)
    awk -v section="$NEW_SECTION" '
        BEGIN { count = 0; inserted = 0 }
        /^---$/ { count++ }
        /^---$/ && count == 2 && !inserted {
            print $0
            print section
            inserted = 1
            next
        }
        { print }
    ' "$AGENTS_MD" > "$TEMP_FILE"
    mv "$TEMP_FILE" "$AGENTS_MD"
else
    # Create new AGENTS.md
    {
        echo "# CodeSearch - Agent Changelog"
        echo ""
        echo "---"
        echo ""
        echo "## [$NEW_VERSION] - $TODAY"
        echo ""
        echo "### $CHANGE_TYPE"
        if [[ -n "$DESCRIPTION" ]]; then
            echo ""
            echo "$DESCRIPTION"
        fi
        echo ""
        echo "---"
    } > "$AGENTS_MD"
fi

# ── Show git status ──────────────────────────────────────────────────────────
echo -e "${CYAN}→ Git status:${NC}"
git status --short

# ── Ask if user wants to commit ──────────────────────────────────────────────
echo ""
read -rp "Commit these changes? [y/N] " COMMIT

if [[ "$COMMIT" =~ ^[YyJj]$ ]]; then
    BRANCH=$(git branch --show-current)
    echo -e "${GREEN}→ Committing to branch: $BRANCH${NC}"

    COMMIT_MSG="chore: Bump version to $NEW_VERSION

- $CHANGE_TYPE update
- $NEW_VERSION"

    if [[ -n "$DESCRIPTION" ]]; then
        COMMIT_MSG+=$'\n\n'"$DESCRIPTION"
    fi

    git add "$CARGO_TOML" Cargo.lock "$AGENTS_MD"
    git commit -m "$COMMIT_MSG"

    echo -e "${GREEN}✅ Version bumped to $NEW_VERSION and committed!${NC}"
    echo ""
    echo "Next steps:"
    echo "  1. Push: git push"
    echo "  2. Or create a PR: gh pr create"
else
    echo -e "${YELLOW}⚠️  Changes not committed${NC}"
    echo "  Cargo.toml, Cargo.lock, and AGENTS.md have been modified"
fi
