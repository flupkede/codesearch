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

- **`grep-guard`** — a `PreToolUse` hook on `Grep`. Blocks every `Grep`
  call against an indexed repo *for as long as the codesearch serve hub
  is reachable*, with a message telling the model exactly how to load and
  call codesearch instead. The guarded repo is resolved from the **grep
  target itself** — the git root of the path being searched, not the
  hook's working directory — so an absolute-path Grep into a different
  indexed repo is guarded too (a cwd-based check used to let those
  through, and until #199 the absolute-path test itself only recognized
  Windows-style roots, so POSIX absolute paths were still resolved
  against the cwd). Coverage is decided by **registration**: the target's
  git root must be one of the repos in the hub's `~/.codesearch/repos.json`
  (honoring `CODESEARCH_REPOS_CONFIG`), which also carves out nested
  repos for free — an unregistered clone inside a registered repo
  resolves to its own git root and is treated as uncovered. Grep is
  auto-allowed **only** when codesearch is genuinely
  down: the hook probes the unauthenticated `/healthz` liveness endpoint
  and lets Grep through only when that probe fails. A low-confidence or
  empty codesearch *result* is a successful call ("reformulate"), not a
  dead server, so it does **not** unblock Grep. One exception: when the
  target repo is live but **mid-reindex** (the serve watcher's full
  refresh, e.g. right after a branch switch), the hook denies Grep with a
  **wait-and-retry** instruction (sleep 15-30s, then re-run the
  codesearch call) — searching a mid-rebuild index returns stale/empty
  results and must not degrade into a manual grep approval on every
  routine checkout. The freshness probe (`GET /indexing?path=<repo
  root>`) is skipped silently on serves that predate the endpoint, so
  hook and server versions can be mixed freely. Grep against paths
  outside any git repo, or against repos codesearch does not cover, is
  never blocked; grep is right there in those cases.

- **`subagent-preamble`** — a `PreToolUse` hook on `Agent` (the subagent-spawn
  tool). Prepends a short preamble to every subagent prompt explaining that
  codesearch exists, that its tools are deferred and need `ToolSearch` first,
  and when to prefer it over Grep/Glob. This is the only way to reach
  subagents at all, since they don't inherit `AGENTS.md` or MCP instructions.

Both hooks fail open: if they can't parse their input, or codesearch isn't
running/indexed, they get out of the way and let Grep proceed untouched. They
never block targets outside any git repo, or repos codesearch does not cover.

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

- `grep-guard` resolves the guarded repo from the **grep target**, never
  from its own cwd: empty/relative paths resolve against the cwd repo
  (they are relative to it by definition), absolute paths resolve against
  the git root of the path being searched. Coverage is then decided by
  **registration with the serve hub**: the target's git root must be one
  of the repos listed in `~/.codesearch/repos.json` (the same
  registration list the hub itself resolves queries by, honoring the
  `CODESEARCH_REPOS_CONFIG` override), or an explicit `CODESEARCH_SERVER`
  env var for pure remote-serve setups with no local registration. A
  local `.codesearch.db` directory is deliberately **not** a coverage
  signal anymore (#199): a stale db from a since-unregistered repo used
  to deny Grep even though the hub could not answer for that repo
  (unknown alias), and a registered repo whose db directory was gone
  slipped through uncovered. Because the git *root* must equal a
  registration, nested repos are carved out correctly: an unregistered
  clone inside a registered repo resolves to its own root and is treated
  as uncovered. Missing, unreadable or malformed `repos.json` (or a
  missing `jq`) fails **open** — a guard that cannot resolve coverage
  must allow, never deny. It deliberately does **not** treat "a
  `codesearch` process is running" as sufficient — `codesearch serve`
  commonly runs as a persistent background hub covering many registered
  repos (`codesearch index list`), so that process is alive on a dev
  machine almost all the time regardless of whether the searched repo is
  one of the repos it actually indexes. Checking process presence alone
  made the hook fire in every directory on the machine, including
  unindexed ones — this was found and fixed after exactly that
  false-positive showed up in real use. (The older cwd-based resolution
  had the mirror-image defect: an absolute-path Grep into a *different*
  indexed repo looked "external" and slipped the guard — also fixed,
  same release. Until #199 the absolute-path test itself only matched
  Windows-style roots — `C:\`, `C:/`, MSYS `/c/`, UNC `//server` — so
  POSIX absolute paths like `/home/...` were still resolved against the
  cwd; with a session cwd that is a plain parent directory the guard
  silently allowed everything.)
  If your setup connects to a remote `codesearch serve` instance with no
  local `repos.json` registration, set `CODESEARCH_SERVER` to opt back
  into enforcement for that repo. Note: the PowerShell twin
  (`grep-guard.ps1`) still uses the older `.codesearch.db` coverage
  signal and Windows-only absolute-path detection; porting it to the
  registration-based resolution is tracked in #199.
- Both hooks are per-machine, not per-repo: install once at user scope and
  every project benefits, including ones not registered with the serve
  hub (the guard simply won't block Grep there, since coverage fails
  open).
- `grep-guard` decides "is codesearch down?" by probing the serve hub's
  unauthenticated `/healthz` endpoint (base URL from `CODESEARCH_SERVER`, else
  `http://127.0.0.1:$CODESEARCH_SERVE_PORT`, else the compiled default
  `http://127.0.0.1:39725`). Any HTTP response counts as up and keeps Grep
  blocked; only a connection-level failure (refused / timeout) counts as down
  and lets Grep through. The probe has a 2-second timeout, so a wedged server
  eventually fails open rather than stalling every Grep. The PowerShell hook
  needs no extra tools; the bash hook additionally requires `curl`.
