# Claude Code integration: enforcing codesearch over grep

## The problem

codesearch publishes usage instructions to every MCP client via the
`initialize` handshake (see main [README § Agent Guidance](../../README.md#agent-guidance-making-agents-use-codesearch-not-grep)).
OpenCode and Cursor surface these automatically and the model follows them
reasonably well.

Claude Code is different in two ways that combine to defeat the advisory
instructions:

1. **MCP tool schemas are deferred.** Claude Code doesn't load full parameter
   schemas for MCP tools (codesearch included) up front — only tool *names*
   appear in context. To actually call `mcp__codesearch__search`, the model
   must first call `ToolSearch` to pull in the schema. That's an extra step
   with no obvious payoff, so under any time pressure the model skips it.
2. **`Grep` and `Glob` are always fully loaded**, schema and all, and require
   zero extra steps. They're the path of least resistance.

Net effect: advisory instructions ("prefer codesearch") lose to structural
convenience ("Grep just works") more often than on other clients. This shows
up as codesearch sitting there indexed and unused while the model greps the
working tree — including in spawned subagents, which don't even inherit the
parent's `AGENTS.md` or the MCP `initialize` instructions at all.

## The fix

Two [Claude Code hooks](https://docs.claude.com/en/docs/claude-code/hooks)
that make the preference *structural* instead of advisory:

- **`grep-guard`** — a `PreToolUse` hook on `Grep`. Blocks the first `Grep`
  call against an internal repo path when codesearch looks available, with a
  message telling the model exactly how to load and call codesearch instead.
  If the *same* query is retried within 5 minutes, it's let through
  unblocked — that's the legitimate "codesearch found nothing, falling back"
  path. Grep against paths outside the current repo is never blocked;
  codesearch doesn't cover arbitrary external paths well, grep is right there.

- **`subagent-preamble`** — a `PreToolUse` hook on `Agent` (the subagent-spawn
  tool). Prepends a short preamble to every subagent prompt explaining that
  codesearch exists, that its tools are deferred and need `ToolSearch` first,
  and when to prefer it over Grep/Glob. This is the only way to reach
  subagents at all, since they don't inherit `AGENTS.md` or MCP instructions.

Both hooks fail open: if they can't parse their input, or codesearch isn't
running/indexed, they get out of the way and let Grep proceed untouched. They
never block anything outside the current repo.

## Install

```bash
# Windows / PowerShell — user-level (~/.claude), applies to all projects
pwsh -File integrations/claude-code/install.ps1

# Windows / PowerShell — project-level (./.claude), this repo only
pwsh -File integrations/claude-code/install.ps1 -Scope project

# macOS / Linux — user-level (~/.claude)
bash integrations/claude-code/install.sh

# macOS / Linux — project-level (./.claude)
bash integrations/claude-code/install.sh --project
```

The installer:
1. copies the hook scripts into `<claude-dir>/hooks/codesearch/`
2. merges two `PreToolUse` registrations into `<claude-dir>/settings.json`
   (backing up the existing file first)
3. is idempotent — re-running it skips hooks already registered and never
   duplicates or clobbers unrelated settings

Restart Claude Code (or start a new session) after installing.

## Manual install

If you'd rather wire it up by hand, or already have a `PreToolUse.Grep` /
`PreToolUse.Agent` hook and want to merge manually, add to
`~/.claude/settings.json` (or `.claude/settings.json` for project scope):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Grep",
        "hooks": [
          { "type": "command", "command": "pwsh -NoProfile -NonInteractive -File \"<path>/grep-guard.ps1\"" }
        ]
      },
      {
        "matcher": "Agent",
        "hooks": [
          { "type": "command", "command": "pwsh -NoProfile -NonInteractive -File \"<path>/subagent-preamble.ps1\"" }
        ]
      }
    ]
  }
}
```

Use the `.sh` scripts with a `bash "<path>/..."` command instead on
macOS/Linux. Point `<path>` at wherever you copy `hooks/*.ps1` / `hooks/*.sh`.

## Uninstall

Remove the two `PreToolUse` entries (matcher `Grep` and `Agent` whose command
points at `hooks/codesearch/`) from `settings.json`, and delete
`<claude-dir>/hooks/codesearch/`.

## Caveats

- `grep-guard` detects "codesearch is available **for the current repo**" via
  a local `.codesearch.db` at the git root, or an explicit `CODESEARCH_SERVER`
  env var for pure remote-serve setups with no local index. It deliberately
  does **not** treat "a `codesearch` process is running" as sufficient —
  `codesearch serve` commonly runs as a persistent background hub covering
  many registered repos (`codesearch index list`), so that process is alive
  on a dev machine almost all the time regardless of whether the current
  directory is one of the repos it actually indexes. Checking process
  presence alone made the hook fire in every directory on the machine,
  including unindexed ones — this was found and fixed after exactly that
  false-positive showed up in real use.
  If your setup connects to a remote `codesearch serve` instance with no
  local `.codesearch.db`, set `CODESEARCH_SERVER` to opt back into
  enforcement for that repo.
- Both hooks are per-machine, not per-repo: install once at user scope and
  every project benefits, including ones without a local `.codesearch.db`
  (the guard simply won't block Grep there, since step 2 fails open).
- The 5-minute retry-unblock window is a heuristic, not a guarantee the model
  actually called codesearch in between. It's deliberately permissive —
  the goal is nudging the *first* attempt, not adversarially trapping the
  model into an unusable state.
