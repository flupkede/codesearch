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

Four [Claude Code hooks](https://docs.claude.com/en/docs/claude-code/hooks)
that make the preference *structural* instead of advisory:

- **`grep-guard`** — a `PreToolUse` hook on `Grep` **and `Bash`**. Blocks every
  `Grep`
  call against an indexed repo *for as long as the codesearch serve hub
  is reachable*, with a message telling the model exactly how to load and
  call codesearch instead. The `Bash` matcher closes the shell escape hatch:
  a Bash tool command that greps FILES inside an indexed repo is guarded
  identically (quoted mentions like `echo "grep"` are not invocations, and a
  pure stream filter such as `find . | grep -v '^./.git/'` reads no files, so
  it is not guarded). The guarded repo is resolved from the **grep target
  itself** — the git root of the path being searched, not the hook's working
  directory — so an absolute-path Grep into a different indexed repo is
  guarded too (a cwd-based check used to let those through, and until #199 the
  absolute-path test itself only recognized Windows-style roots, so POSIX
  absolute paths were still resolved against the cwd). For a Bash grep the
  target is the command's own operand (absolute, relative to the shell cwd, or
  after a leading `cd DIR &&`), so a grep aimed at `/tmp` or at an
  unregistered clone is allowed; that fixed the reported misfire where a
  session inside an indexed repo denied greps that never touched it.
  Coverage is decided by **registration**: the target's
  git root must be one of the repos in the hub's `~/.codesearch/repos.json`
  (honoring `CODESEARCH_REPOS_CONFIG`; `CODESEARCH_HOME` relocates the
  root), which also carves out nested
  repos for free — an unregistered clone inside a registered repo
  resolves to its own git root and is treated as uncovered. Grep is
  auto-allowed **only** when codesearch is genuinely
  down: the hook probes the unauthenticated `/healthz` liveness endpoint
  and lets Grep through only when that probe fails. A low-confidence or
  empty codesearch *result* is a successful call ("reformulate"), not a
  dead server, so it does **not** unblock Grep. One exception: when the
   target repo is live but **mid-reindex** (the serve watcher's full
   refresh, e.g. right after a branch switch), the hook waits the rebuild
   out itself — polling `GET /indexing?path=<repo root>` every few seconds
   (the endpoint also accepts `&wait=<s>`, which holds the request until the
   repo settles; the hooks can adopt that to drop this loop) for up to
   `CODESEARCH_GREP_GUARD_WAIT_MS` (default 120s; `0` restores
   the legacy instant wait-and-retry instruction) — and denies Grep with
   a retry-codesearch instruction once the rebuild lands, so the denial
   only fires when codesearch will actually answer. Past the cap Grep
   opens: a big-repo rebuild outlasts any polite wait message, and
   denying Grep while every codesearch call times out deadlocks the
   session. The freshness probe (`GET /indexing?path=<repo
   root>`, bearer key forwarded when `CODESEARCH_API_KEY` is set) is
   skipped silently on serves that predate the endpoint, so
  root>`) is skipped silently on serves that predate the endpoint, so
  hook and server versions can be mixed freely. Grep against paths
  outside any git repo, or against repos codesearch does not cover, is
  never blocked; grep is right there in those cases.

- **`subagent-preamble`** — a `PreToolUse` hook on `Agent` (the subagent-spawn
  tool). Prepends a short preamble to every subagent prompt explaining that
  codesearch exists, that its tools are deferred and need `ToolSearch` first,
  and when to prefer it over Grep/Glob. This is the only way to reach
  subagents at all, since they don't inherit `AGENTS.md` or MCP instructions.

- **`edit-guard`** — a `PreToolUse` hook on `Edit`/`Write`/`MultiEdit`.
  Installed as the native command `"…/codesearch" hook edit-guard` — the
  codesearch binary itself handles the event, one process, no shell, no
  child processes. Blocks an edit to a code file in a codesearch-registered
  repo until
  codesearch was consulted for that file within the last 5 minutes:
  `mcp__codesearch__find_impact` for SCIP-backed languages
  (`.cs .ts .tsx .mts .cts`), `mcp__codesearch__find(kind="usages")` for
  other source code. Docs, markup and config files (`.md`, `.json`, ...)
  are never gated — a usages lookup answers nothing there. Making the
  caller-aware-editing protocol structural.
  Coverage uses the same registration model as grep-guard (the shared
  `codesearch-common` helpers, ported into the binary): the file's git root
  must be listed in
  `~/.codesearch/repos.json` (relocatable via `CODESEARCH_HOME`; or
  `CODESEARCH_SERVER` is set); unregistered
  repos and non-git paths are never blocked. The git root is found by
  walking up for `.git` (a directory, or a file for worktrees) — never a
  spawned `git`.

- **`edit-guard-post`** — a `PostToolUse` hook on
  `mcp__codesearch__find_impact` and `mcp__codesearch__find`, installed as
  the native command `"…/codesearch" hook edit-guard-post`, recording the
  "consulted" markers `edit-guard` reads into a state file
  (`${TMPDIR:-/tmp}/.codesearch-edit-guard-state.json` on POSIX,
  `%TEMP%\.codesearch-edit-guard-state.json` on Windows — the native hook
  resolves the same locations and writes atomically via temp file + rename).
  It fires on
  every `find_impact` call and every `find(kind="usages")` (the kind check
  is done in the hook itself — matchers only see tool names). Markers are
  attributed from all available sources, all written: a file-ish input
  field (`file`/`path`/`file_path` — serves older schemas); a **path-like
  `symbol` or `symbol_name` input** — the exact call shapes `edit-guard`
  demands (`find(symbol=<path>, kind="usages")` for non-SCIP code,
  `find_impact(symbol_name=<path>)` for SCIP-backed languages) mark the
  path they name (bare code symbols never mark); every **path the response
  mentions** (`path`/`file`/`file_path` at any depth, including inside the
  JSON-stringified text payload MCP wraps results in — values must look
  like file paths, so a bare symbol string writes no junk key; paths join
  onto the registered root of the call's `project` alias or, as fallback,
  of the `cwd` repo — agents may pass both, so a stale alias still
  attributes, and both shape candidates are marked), so one consultation
  marks every file it listed; and a **repo-level marker** on the
  `project`'s registered root, else the `cwd` repo's root, written for
  **every** qualifying consult — `edit-guard` accepts it for any file in
  that repo. Per-file attribution cannot be exhaustive: `find_impact`
  answers with *callers*, so the consulted file itself is often absent
  from its response — attribution-only marking let a successful consult
  deny the very next edit anyway (the theme-2-merge wedge). Any outcome
  counts; without response-path and repo-level attribution a missing C#
  symbol index once wrote no marker at all and every `.cs` edit in a
  registered repo stayed denied — a permanent wedge. PostToolUse hooks cannot block anything; this one never emits a
  decision and always exits 0.

All hooks fail open: if they can't parse their input, or codesearch isn't
running/indexed, they get out of the way and let the tool call proceed
untouched. They never block targets outside any git repo, or repos
codesearch does not cover.

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

These shell installers are the **script-based path**: for the edit guards
they still copy and register the `edit-guard*.ps1`/`.sh` twins. The native
registration below is done by the binary itself — `codesearch hooks claude
install` (add `--project` for `./.claude` scope) — which also migrates any
pre-native edit-guard registration away on re-install.

The native installer:
1. copies only the script guards (grep-guard, subagent-preamble, web-guard)
   and the shared `codesearch-common` helpers into
   `<claude-dir>/hooks/codesearch/` — the edit-guard hooks need no scripts:
   they run natively as `"…/codesearch" hook edit-guard` /
   `"…/codesearch" hook edit-guard-post`, one process per call (the old
   ~7.4 s-per-edit pwsh spawn and the ~45 s bash twin are both gone; the
   sh/ps1 twins remain for the other guards)
2. merges the `PreToolUse` registrations (one per guard) and the
   `PostToolUse` registration (the companion) into
   `<claude-dir>/settings.json` (backing up the existing file first). A
   pre-native `edit-guard.ps1`/`edit-guard.sh` registration is removed
   automatically on re-install — old and new never run side by side
3. is idempotent — re-running it skips hooks already registered and never
   duplicates or clobbers unrelated settings

Restart Claude Code (or start a new session) after installing.

## Manual install

If you'd rather wire it up by hand, or already have hooks on these events and
want to merge manually, add to `~/.claude/settings.json` (or
`.claude/settings.json` for project scope):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Grep|Bash",
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
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [
          { "type": "command", "command": "\"<path-to-codesearch>\" hook edit-guard" }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "mcp__codesearch__find_impact|mcp__codesearch__find",
        "hooks": [
          { "type": "command", "command": "\"<path-to-codesearch>\" hook edit-guard-post" }
        ]
      }
    ]
  }
}
```

Use the `.sh` scripts with a `bash "<path>/..."` command instead on
macOS/Linux. Point `<path>` at wherever you copy `hooks/*.ps1` / `hooks/*.sh`.
The script guards source `codesearch-common.ps1` / `codesearch-common.sh`
from their own directory, so that file must be copied alongside them; the
edit-guard commands above invoke the codesearch binary directly and need no
copied files.

## Uninstall

Remove the guard entries (matchers `Grep|Bash`, `Agent`, `WebSearch|WebFetch` and
`Edit|Write|MultiEdit` under `PreToolUse`, and
`mcp__codesearch__find_impact|mcp__codesearch__find` under `PostToolUse`)
from `settings.json` and delete `<claude-dir>/hooks/codesearch/`. The
grep/preamble/web commands always point at `hooks/codesearch/`; the
edit-guard commands invoke `"…/codesearch" hook …` after a native install,
but point at `hooks/codesearch/edit-guard*.ps1|.sh` when installed by the
shell installers.

## Caveats

- `grep-guard` resolves the guarded repo from the **grep target**, never
  from its own cwd: empty/relative paths resolve against the cwd repo
  (they are relative to it by definition), absolute paths resolve against
  the git root of the path being searched. Coverage is then decided by
  **registration with the serve hub**: the target's git root must be one
  of the repos listed in `~/.codesearch/repos.json` (the same
  registration list the hub itself resolves queries by, honoring the
  `CODESEARCH_REPOS_CONFIG` / `CODESEARCH_HOME` overrides), or an explicit `CODESEARCH_SERVER`
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
  into enforcement for that repo. Both the bash and the PowerShell twin
  now resolve coverage through the shared `codesearch-common` helpers,
  so the two shells behave identically (the PowerShell twin used to lag
  behind on the `.codesearch.db`/Windows-only-path signals — closed with
  the edit-guard work, #199).
- `edit-guard` is per-file, not per-repo: a marker for one file never
  unblocks an edit to another file — except the repo-level marker
  `edit-guard-post` writes when a consultation returned no attributable
  path (empty result / "no SCIP backend"), which unblocks every file in
  that repo for the window. The marker state lives in a temp-dir
  JSON file (`$TMPDIR/.codesearch-edit-guard-state.json` /
  `%TEMP%\.codesearch-edit-guard-state.json`), entries expire after
  5 minutes and are pruned on write; missing or corrupt state counts as
  "not consulted" (deny on covered repos, allow everywhere else). Like
  the other guards it is per-machine, not per-repo: it only ever fires
  on files whose git root is registered with the serve hub. The gate keys
  on extension: docs/markup/config (`.md`, `.json`, ...) pass without any
  marker; filename-mapped code files (`Dockerfile`, `Makefile`) pass too —
  rare refactor targets. Scope your consultation with the `project` alias:
  response paths resolve against that project's registered root, and the
  repo-level fallback needs it — a projectless `find_impact` that returns
  no paths writes no marker at all. Known limit: in a `CODESEARCH_SERVER`
  pure-remote setup there is no local registry, so neither the
  response-path resolution nor the repo fallback can attribute a marker —
  there the guard demands a file-ish input field and nothing else
  satisfies it.
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
