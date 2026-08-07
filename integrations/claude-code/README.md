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

Three [Claude Code hooks](https://docs.claude.com/en/docs/claude-code/hooks)
that make the preference *structural* instead of advisory:

- **`grep-guard`** — a `PreToolUse` hook on `Grep`. Blocks every `Grep`
  call against an internal repo path *for as long as the codesearch serve hub
  is reachable*, with a message telling the model exactly how to load and call
  codesearch instead. Grep is auto-allowed **only** when codesearch is
  genuinely down: the hook probes the unauthenticated `/healthz` liveness
  endpoint and lets Grep through only when that probe fails. A low-confidence
  or empty codesearch *result* is a successful call ("reformulate"), not a dead
  server, so it does **not** unblock Grep. Grep against paths outside the
  current repo is never blocked; codesearch doesn't cover arbitrary external
  paths well, grep is right there.

- **`subagent-preamble`** — a `PreToolUse` hook on `Agent` (the subagent-spawn
  tool). Prepends a short preamble to every subagent prompt explaining that
  codesearch exists, that its tools are deferred and need `ToolSearch` first,
  and when to prefer it over Grep. This is the only way to reach subagents at
  all, since they don't inherit `AGENTS.md` or MCP instructions.

  It also resolves the **multi-repo scope for the current directory** and
  injects it concretely: `project="<alias>"` plus the `group=` values that
  alias belongs to, read from `repos.json` — `$CODESEARCH_REPOS_CONFIG` if set,
  else `~/.codesearch/repos.json`, the same resolution `web-guard` and the
  binary's own `config_path()` use. Without this a
  subagent can only learn its scope *reactively*, from a failed call's
  `scope_required` error — and the alias is not guessable from the folder
  name (a directory `NWND.ACME` is registered as `NWND.Acme`). The
  costlier failure is the silent one: an agent that guesses a plausible
  `project=` never sees the sibling repos a `group=` would union in, so
  configuration kept in its own repo simply reads as absent. Only the alias
  and the group *names* are injected — not the group's membership, which is
  the group definition's business and what `status(kind="projects")`
  reports. Everything about this is fail-open: no git root, no config file,
  malformed config, or an unregistered directory all fall back to the
  generic "read it off the error" wording.

- **`web-guard`** — a `PreToolUse` hook on `WebSearch`/`WebFetch`. When remote
  documentation projects are mounted (`codesearch remote mount`), it blocks the
  first web call with guidance to search those indexed mounts first — usually
  more precise and current than the open web, and the only thing that works at
  all for login-gated vendor docs. When no mounts are configured it does
  nothing.

All three hooks fail open on unparseable input, in both shells: they validate
the incoming JSON up front and exit 0 on anything malformed, letting the
original tool proceed untouched. (The `.sh` twins need that explicit check —
they run under `set -euo pipefail`, so without it a `jq` parse error would
abort with exit 5 and surface as a hook *execution failure* rather than a
silent pass-through.) Beyond that the three differ, and the differences matter
more than the shared rule.

`grep-guard` is the only one that checks whether codesearch is actually
available. It gets out of the way when the repo has no index, and when the
serve hub fails a `/healthz` probe — and it never blocks a path outside the
current repo.

`subagent-preamble` has **no** availability check at all: it rewrites the
prompt on every `Agent` spawn regardless of whether codesearch is running or
this repo is indexed. That is deliberate — the preamble tells the subagent how
to *load* the deferred tools and explicitly says what to do if `ToolSearch`
returns nothing, so it stays useful in a repo codesearch doesn't cover. It also
means the hook is never the reason a subagent fails to spawn: it only ever
prepends text.

`web-guard` inverts `grep-guard` on both counts, and this is the difference
worth knowing before you rely on any general rule. It blocks *only* external
calls — every `WebSearch`/`WebFetch` target is outside the repo by definition —
and it never probes the server, so a mount that is configured but unreachable
still denies the first call. What bounds it instead is the retry cache: the
same call repeated within 5 minutes is always allowed, so an unreachable mount
costs one retry rather than trapping you. With no remote mounts configured in
`repos.json` it does nothing at all.

**`Glob` is deliberately not gated.** It matches *filenames*, and codesearch has
no file-listing primitive to replace it — `search`'s `file_glob` is a filter on
a content search, not an enumerator. Nor could a guard hold: `ls`, `find` and
`git ls-files` reach the same answer through Bash, which isn't gated either, so
the only effect would be friction on the honest path. Glob is also the reliable
way to get a *negative* answer, since an empty codesearch result cannot
distinguish "not present" from "not indexed".

## Install

Preferred — the native command, which embeds the hook scripts in the binary and
needs no source tree:

```bash
codesearch hooks claude install            # user scope (~/.claude), all projects
codesearch hooks claude install --project  # project scope (./.claude)
```

The from-source installers below do the same thing from a checkout:

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
2. merges three `PreToolUse` registrations into `<claude-dir>/settings.json`
   (backing up the existing file first)
3. is idempotent — re-running it skips hooks already registered and never
   duplicates or clobbers unrelated settings

Restart Claude Code (or start a new session) after installing.

## Manual install

If you'd rather wire it up by hand, or already have a `PreToolUse.Grep` /
`PreToolUse.Agent` / `PreToolUse.WebSearch` hook and want to merge manually,
add to `~/.claude/settings.json` (or `.claude/settings.json` for project scope):

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
      },
      {
        "matcher": "WebSearch|WebFetch",
        "hooks": [
          { "type": "command", "command": "pwsh -NoProfile -NonInteractive -File \"<path>/web-guard.ps1\"" }
        ]
      }
    ]
  }
}
```

Use the `.sh` scripts with a `bash "<path>/..."` command instead on
macOS/Linux. Point `<path>` at wherever you copy `hooks/*.ps1` / `hooks/*.sh`.

## Uninstall

Remove the three `PreToolUse` entries (matcher `Grep`, `Agent` and
`WebSearch|WebFetch` whose command points at `hooks/codesearch/`) from
`settings.json`, and delete `<claude-dir>/hooks/codesearch/`.

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
- All three hooks are per-machine, not per-repo: install once at user scope and
  every project benefits, including ones without a local `.codesearch.db`
  (the guard simply won't block Grep there, since step 2 fails open).
- `grep-guard` decides "is codesearch down?" by probing the serve hub's
  unauthenticated `/healthz` endpoint (base URL from `CODESEARCH_SERVER`, else
  `http://127.0.0.1:$CODESEARCH_SERVE_PORT`, else the compiled default
  `http://127.0.0.1:39725`). Any HTTP response counts as up and keeps Grep
  blocked; only a connection-level failure (refused / timeout) counts as down
  and lets Grep through. The probe has a 2-second timeout, so a wedged server
  eventually fails open rather than stalling every Grep. The PowerShell hook
  needs no extra tools; the bash hook additionally requires `curl`.
