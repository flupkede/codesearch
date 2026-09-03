#!/usr/bin/env bash
# Print `Co-authored-by:` trailers for everyone who authored work that a
# develop → master release squash is about to flatten away.
#
# Why: the contributors list is computed from the DEFAULT branch (master).
# Release PRs are squash-merged (one commit per release, authored by whoever
# clicks merge), so individual authorship would never reach master. GitHub
# counts `Co-authored-by:` trailers on the default branch, so carrying these
# trailers in the squash commit message credits every real contributor.
#
# Usage:
#   scripts/release-coauthors.sh [BASE] [HEAD]     # defaults: origin/master origin/develop
#
# Wire into the release (step 2 of RELEASING.md):
#   gh pr merge --squash --admin \
#     --subject "release: v1.3.11" \
#     --body "$(scripts/release-coauthors.sh)"
#
# Bots and merge commits are excluded (merge commits carry no unique
# authorship). One line per distinct author email; the same human committing
# under two emails produces two lines — harmless.

set -euo pipefail

BASE="${1:-origin/master}"
HEAD="${2:-origin/develop}"

emails=$(git log --no-merges --format='%an <%ae>' "$BASE..$HEAD" \
  | sort -u \
  | grep -viE 'github-actions\[bot\]|dependabot\[bot\]|41898282\+github-actions|test@example\.com' \
  || true)

# Optional local blocklist (.docs/ is gitignored): one grep -E pattern per
# line, e.g. a device identity you never want credited. NEVER commit it.
BLOCKLIST=".docs/coauthors-blocklist"
if [ -f "$BLOCKLIST" ]; then
  while IFS= read -r pattern; do
    [ -z "$pattern" ] && continue
    case "$pattern" in \#*) continue ;; esac
    emails=$(printf '%s\n' "$emails" | grep -viE "$pattern" || true)
  done < "$BLOCKLIST"
fi

if [ -z "$emails" ]; then
  echo "(no human commits between $BASE and $HEAD — nothing to credit)" >&2
  exit 0
fi

echo "$emails" | sed 's/^/Co-authored-by: /'
echo "(review the list before pasting — drop device/test identities you don't want credited)" >&2
