# OpenCode integration: making codesearch structural

## The problem

codesearch publishes usage instructions to every MCP client on connect (the
`initialize` handshake, see main [README § Agent Guidance](../../README.md#agent-guidance-making-agents-use-codesearch-not-grep)).
OpenCode surfaces those instructions and the model follows them reasonably
well. But they are **advisory** and **connection-scoped**: they cannot see the
host state that decides whether the preference actually holds up in practice.

In OpenCode that shows up as several recurring failure modes:

1. **Scope friction.** In multi-repo serve mode every tool call needs
   `project=` or `group=`. The model has to guess the alias from the directory
   name; when it guesses wrong it burns a turn on `scope_required`, and when it
   guesses right it still may have picked the wrong project.
2. **Grep wins by default.** `grep`/`glob` are always loaded, zero-friction and
   local. The instruction "prefer codesearch" loses to convenience, and an
   **empty grep** (the exact case where the index would help) simply dead-ends
   the turn.
3. **Worktrees and clones are invisible.** A git worktree is a separate
   indexable repo. The model cannot tell that the current directory is
   unregistered, so it greps a tree codesearch has never seen.
4. **The hub can be down.** `codesearch serve` is a local daemon. If it is not
   running (or an MCP connect failed), the model keeps trying codesearch tools
   and gets transport errors instead of falling back gracefully.
5. **Fresh edits lag the index.** The file watcher reindexes shortly after a
   write; a query made in that window can miss the change and send the model
   back to grep.
6. **Compaction loses structure.** A long session compacts, and the file
   outlines that made navigation cheap are summarised away.

The plugin below fixes these by adding what only the host knows: the working
directory, the session lifecycle, tool calls, and file edits.

## The fix

A self-contained [OpenCode v2](https://opencode.ai/docs/plugins/) plugin
(`codesearch.ts`, no imports, no runtime dependencies) that turns the
preference into behaviour:

| Feature | Default | What it does |
|---|---|---|
| **Scope resolution** | on | Maps the session directory to a registered project alias (and its groups), from `~/.codesearch/repos.json` (relocatable via `CODESEARCH_HOME`) or a live `status` snapshot. |
| **Scope-aware guidance** | once per session | Injects a compact strategy block plus the resolved scope into system context, so the first tool call already knows `project=`. |
| **Zero-hit rescue** | on | When `grep`/`glob` returns nothing in an indexed repo, runs a literal codesearch query and appends the index hits to the tool result. |
| **Code-search-first guards** | `nudge` | `off`, `nudge` (one tip per session), `prune` (hide `grep`/`glob` from the model), or `block` (deny with the exact codesearch call to use). Raw shell `grep`/`rg` through the bash tool is denied in every mode except `off` (`guards.bash`, `CODESEARCH_PLUGIN_BASH_GUARD`). |
| **Hub health** | on | Probes `/healthz`, asks OpenCode to reload failed MCP servers, and can auto-start a local `codesearch serve` (opt-in). |
| **Unindexed warning** | on | Flags a registered repo with an empty or model-less index, so "no results" is not mistaken for "not there". |
| **Freshness note** | on | Tracks `file.edited` events and reminds the model that a just-edited file may lag the watcher's refresh. |
| **Compaction outlines** | on | Before compaction, captures `explore` outlines of recently edited files and injects them into the summary prompt. |
| **Commands** | on | `/codesearch <query>`, `/codesearch-status`, `/codesearch-index`. |
| **`codesearch_scope` tool** | on | Lets the model ask which project/groups the current directory maps to, and whether the index is healthy. |
| **Skill** | on | Registers a `codesearch` skill with the tool-routing playbook and worked examples. |
| **Auto-recall** | off | Opt-in: runs a semantic search for the user message and injects the top chunks as background context. |

Everything is **fail-open**: every hook is wrapped, guards only engage while
the hub is confirmed reachable and the repo is registered, and a plugin or
network failure degrades to plain OpenCode behaviour. Nothing here can block a
session or wedge the model.

## Requirements

- **OpenCode v2** (tested with 2.0.22). V1 is not supported.
- **A codesearch serve hub** reachable over HTTP (streamable MCP). The default
  is `http://127.0.0.1:39725/mcp`; remote serve is supported with an API key.
- Nothing else: the plugin has no npm dependencies and does not need Node or
  Bun on PATH (it uses the host's runtime and, when enabled, spawns the same
  `codesearch` binary that runs your hub).

Stdio-only setups (`codesearch mcp` without `serve`) do not expose the HTTP
endpoint this plugin needs. The plugin stays installed and inert there (and
says so in its logs); guards never fire, and the MCP server's own instructions
remain in effect.

## Install

The plugin is a single file. Drop it into OpenCode's plugin directory and
restart, or point at it from `opencode.jsonc`.

### User scope (applies to every project)

```bash
# macOS / Linux
mkdir -p ~/.config/opencode/plugins
cp integrations/opencode/codesearch.ts ~/.config/opencode/plugins/
```

```powershell
# Windows / PowerShell
New-Item -ItemType Directory -Force "$env:USERPROFILE\.config\opencode\plugins"
Copy-Item integrations\opencode\codesearch.ts "$env:USERPROFILE\.config\opencode\plugins\"
```

### Project scope (this repository only)

```bash
mkdir -p .opencode/plugins
cp integrations/opencode/codesearch.ts .opencode/plugins/
```

### Or reference the path from config

```jsonc
// ~/.config/opencode/opencode.jsonc
{
  "plugins": ["/absolute/path/to/codesearch.ts"]
}
```

OpenCode watches the plugin directories and reloads on change. If the plugin
does not appear, restart the service:

```sh
opencode service restart
```

### Verify

```sh
opencode plugin list        # should list: codesearch  local  .../codesearch.ts
```

Then, inside a session, run `/codesearch-status`. A loaded plugin answers with
the hub URL, health, the directory's resolved project and its index state.

### Uninstall

Delete `codesearch.ts` (and optionally `~/.config/opencode/codesearch.json`)
and restart OpenCode. The plugin never installs or edits anything else.

## Configuration

Defaults live under `~/.config/opencode/codesearch.json` (override the path
with `CODESEARCH_CONFIG`). Values may use `{env:NAME}` placeholders.
Precedence, lowest to highest:

1. built-in defaults
2. plugin options in `opencode.jsonc` (object form)
3. the config file
4. environment variables

```jsonc
{
  // Optional. Usually auto-resolved (see below).
  "url": "http://127.0.0.1:39725/mcp",
  // Optional bearer token for a remote / authenticated serve.
  "token": "{env:CODESEARCH_API_KEY}",
  // Name of the MCP server entry in opencode.jsonc to mirror.
  "mcpServer": "codesearch",
  // Optional override for the registry (default: ~/.codesearch/repos.json,
  // relocated wholesale by CODESEARCH_HOME).
  "reposConfig": "",
  "debug": false,

  "scope": {
    "enabled": true,
    // How often the scope+guidance block is injected:
    // "session" (first message), "message" (every message), "off".
    "inject": "session",
    "includeGroups": true,
    "siblings": true,
    "maxSiblings": 12,
    "warnUnindexed": true
  },

  "guidance": {
    "enabled": true,
    // Optional Markdown file (or inline text) replacing the built-in guidance.
    "file": "",
    "text": ""
  },

  "guards": {
    // "off" | "nudge" | "prune" | "block" (see the guard section below).
    "mode": "nudge",
    "tools": ["grep", "glob"],
    // Deny raw shell grep/rg through the bash tool (every mode except "off").
    "bash": true,
    "rescue": true,
    "rescueLimit": 5,
    "rescueTimeoutMs": 3000
  },

  "health": {
    "enabled": true,
    "intervalMs": 90000,
    "initialDelayMs": 60000,
    "maxBackoffMs": 900000,
    "reconnect": true,
    // Opt-in: start a local serve hub when it is not running.
    "autoStart": false,
    "startCommand": "codesearch serve --quiet"
  },

  "freshness": { "enabled": true, "maxFiles": 8 },
  "compaction": { "enabled": true, "maxFiles": 3, "timeoutMs": 4000, "budgetChars": 2400 },

  // Opt-in auto-recall.
  "recall": { "enabled": false, "limit": 3, "timeoutMs": 2500, "budgetChars": 1800 },

  "commands": { "enabled": true, "indexCommand": "codesearch index" },
  "tools": { "scope": true },
  "skill": { "enabled": true, "autoinvoke": true },

  "log": { "scope": false, "guidance": false, "guards": true, "rescue": true, "health": true, "recall": false, "commands": true }
}
```

### Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `CODESEARCH_URL` | – | Full MCP URL (`.../mcp`). Wins over every discovery step. |
| `CODESEARCH_SERVER` | – | Serve **base** URL (e.g. `http://127.0.0.1:39725`), same meaning as the Claude Code guard hooks use. `/mcp` is appended. |
| `CODESEARCH_API_KEY` | – | Bearer token for an authenticated/remote serve. |
| `CODESEARCH_CONFIG` | `~/.config/opencode/codesearch.json` | Config file path. |
| `CODESEARCH_REPOS_CONFIG` | `$CODESEARCH_HOME/repos.json` (`~/.codesearch` when unset) | Registry path (shared with the hub and the Claude Code hooks). |
| `CODESEARCH_MCP_SERVER` | `codesearch` | MCP entry name to mirror for URL/token. |
| `CODESEARCH_PLUGIN_GUARDS` | `nudge` | `off` / `nudge` / `prune` / `block`. |
| `CODESEARCH_PLUGIN_BASH_GUARD` | `true` | Shell-grep denial through the bash tool (`guards.bash`). `0` re-allows raw shell grep. |
| `CODESEARCH_PLUGIN_SCOPE` | `session` | `session` / `message` / `off`. |
| `CODESEARCH_PLUGIN_RESCUE` | `true` | Zero-hit rescue on/off. |
| `CODESEARCH_PLUGIN_RECALL` | `false` | Auto-recall on/off. |
| `CODESEARCH_PLUGIN_COMPACTION` | `true` | Compaction outlines on/off. |
| `CODESEARCH_PLUGIN_COMMANDS` | `true` | Slash commands on/off. |
| `CODESEARCH_PLUGIN_SKILL` | `true` | Skill registration on/off. |
| `CODESEARCH_PLUGIN_HEALTH` | `true` | Health probing on/off. |
| `CODESEARCH_PLUGIN_DEBUG` | `false` | Verbose plugin logs. |

Settings are resolved **once, at plugin setup** (the host calls `setup()` when
the plugin loads). Editing `codesearch.json` or a `CODESEARCH_*` variable
mid-session therefore does not take effect until OpenCode restarts or the
plugin is reloaded.

### Hub discovery order

The plugin resolves the MCP endpoint in this order, first non-empty wins:

1. `url` from config / `CODESEARCH_URL`
2. `CODESEARCH_SERVER`
3. the `url` of the MCP entry named by `mcpServer` in `opencode.jsonc`
   (its `Authorization` header is used as the token, minus `Bearer`)
4. `~/.codesearch/serve_url`, or `$CODESEARCH_HOME/serve_url` when the global root is relocated (written by `codesearch serve`)
5. `http://127.0.0.1:39725/mcp`

A base URL without `/mcp` is normalised by appending it, so pointing
`CODESEARCH_SERVER` at the hub root is enough.

## Guards and rescue in detail

`guards.mode` controls how strongly codesearch replaces `grep`/`glob` in a
**covered** directory (one whose path maps to a registered, indexed repo):

- **`off`** — hooks are not even registered. Nothing changes.
- **`nudge`** (default) — greps work normally. The first successful
  `grep`/`glob` in a session gets a one-line tip appended to its result, and an
  **empty** grep/glob is rescued (below).
- **`prune`** — `grep` and `glob` are removed from the tool list for covered
  sessions, so the model must use the codesearch tools. The scope block says
  so explicitly. Everything else is untouched, and reading/editing files is
  unaffected.
- **`block`** — `grep`/`glob` calls are denied with a message naming the exact
  `project=` value and the codesearch calls to use instead. Denial happens
  **only** while the hub is confirmed reachable (`/healthz` answered) and the
  repo is registered; in any other state the call proceeds. This mirrors the
  Claude Code `grep-guard` semantics and fails open the same way.

**Shell-grep closure** (independent of `guards.mode`, kill switch
`guards.bash: false` / `CODESEARCH_PLUGIN_BASH_GUARD=0`): a bash tool command
that invokes `grep`/`egrep`/`fgrep`/`zgrep`/`rg` is denied on covered, live
repos in every mode except `off` — the shell tool is the escape hatch around
the native-tool guards, and using it to grep is the workaround the guards
exist to stop. Quoted mentions (`echo "grep"`) are not invocations. If a shell
grep ran anyway (hub down at deny time), `execute.after` appends the one-time
tip instead.

**Zero-hit rescue** (part of `nudge` and `prune`, and independently
controlled by `guards.rescue`) is the highest-value, lowest-risk piece: it
only ever fires when a tool already returned nothing. For a plain-text pattern
it runs `search` with `mode="literal", phrase=true`; for a regex-shaped
pattern it uses `regex=true`. Hits are appended as metadata-style lines
(path/line/kind/signature), not full code, and the model is pointed at
`get_chunk` for the body:

```text
[codesearch] grep returned nothing, but the index has 2 match(es) for "retryUpload" in project "my-service":
  - src/uploader.ts:42 (Function) function retryUpload(job: Job, attempt: number)
  - src/queue.ts:13 (Class) class UploadQueue
Use search/find/get_chunk for the full result.
```

Glob patterns that are pure filesystem wildcards (`**/*.ts`) are skipped;
there is nothing semantic to look up.

## Commands

| Command | What it does |
|---|---|
| `/codesearch <query>` | Runs a semantic search in the resolved project and injects the top hits as a prompt, with a pointer to `get_chunk`. Falls back to guidance if the directory is unregistered. |
| `/codesearch-status` | Probes health, refreshes the status snapshot, and reports hub URL, health, directory, resolved project, chunk/file counts, model and lock state, groups and hub repo count. |
| `/codesearch-index` | Runs `commands.indexCommand` (`codesearch index` by default) in the current directory, detached, then reports that indexing started. Use this to register an unregistered clone or worktree. |

## The `codesearch_scope` tool and the skill

`codesearch_scope` returns JSON for the current directory:

```json
{
  "directory": "/work/my-service",
  "project": "my-service",
  "groups": ["platform"],
  "registered": true,
  "siblings": [],
  "indexed": true,
  "chunks": 4123,
  "model": "embeddinggemma-q4",
  "lockStatus": "warm",
  "serveUrl": "http://127.0.0.1:39725/mcp",
  "serveHealth": "ok"
}
```

Use it when the directory is unusual (a worktree, a multi-repo parent) or when
you want to confirm the index health before trusting a result. It is the one
piece of state the MCP server itself cannot see.

The plugin also registers a **`codesearch` skill** containing the same routing
playbook as the guidance plus worked examples, so a session can pull the
strategy in deliberately. Disable with `skill.enabled: false`.

## Health and auto-start

The health runner probes the serve hub's unauthenticated `/healthz` endpoint
(any HTTP response counts as reachable; only connection-level failures count
as down), with exponential backoff while down. On failure it asks OpenCode to
reload its MCP servers (which re-establishes a failed `codesearch` MCP entry).
With `health.autoStart: true` and a loopback URL it also runs
`codesearch serve --quiet` once, detached, and keeps probing.

While the hub is down, the injected scope block says so and explicitly allows
`grep` as a fallback; rescue skips its lookup (no timeout cost).

## Compaction outlines (and optional recall)

Before a session is compacted, the plugin collects the recently edited files
(tracked from `file.edited` events), fetches `explore kind="outline"` for up
to `compaction.maxFiles`, and appends a compact symbol list to the
compaction prompt, so file structure survives the summary:

```text
# codesearch outlines (captured before compaction)
- src/uploader.ts: function retryUpload(job: Job, attempt: number):41; class UploadManager:88
- src/queue.ts: class UploadQueue:13; function enqueue(job: Job):55
```

**Auto-recall** (`recall.enabled`, default `false`) is the opt-in mirror of
the guidance injection: for each new user message that looks like a code
question, it runs a semantic search and injects the top `recall.limit` chunks,
truncated to `budgetChars`. It costs a lookup plus context on every turn, which
is why it is off by default. Label it as background context; it is code from
the index, not instructions.

## Examples

**1. Multi-repo scope, no detour.** A workspace registers `my-service`,
`billing-api` and `docs`; the session opens in `my-service`.

```text
user> where does upload retry backoff live?
# plugin injects: project "my-service", group "platform" (cross-repo fan-out)
model> search(project:"my-service", query:"upload retry backoff") -> get_chunk(...)
```

Without the plugin, the first call is a guess: `project:"my_service"` or no
scope at all, and the turn pays for a `scope_required` round-trip.

**2. Empty grep becomes a hit.** The model greps for an old function name:

```text
model> grep(pattern:"retryUpload")
tool> [codesearch] grep returned nothing, but the index has 2 match(es) for "retryUpload" in project "my-service": ...
model> get_chunk(chunk_id: 7)
```

**3. Guard modes.** `prune` removes `grep`/`glob` from the tool list in
covered sessions; `block` returns:

```text
codesearch: grep is disabled here — this repository is indexed as project "my-service".
Use the codesearch MCP tools instead:
  - search { project: "my-service", query: "<concept>" } ...
```

Both fail open when the hub is down or the directory is unregistered.

**4. A dead hub recovers.** `codesearch serve` stops or an MCP connect fails:

```text
[codesearch] hub unreachable at http://127.0.0.1:39725/mcp: fetch failed
[codesearch] asked OpenCode to reload MCP servers
```

With `health.autoStart: true` the plugin starts the hub and logs when it comes
back; the scope block tells the model grep is acceptable until then.

**5. A new worktree.** The session opens in an unregistered worktree of a
registered repo. The scope block says it is unregistered and points at
`/codesearch-index`; one command registers and indexes it (`codesearch index`,
which produces a separate index per worktree by design).

**6. Compaction keeps the map.** After a long refactor, the compaction summary
carries the outline block above instead of losing every symbol location.

## Why this exists alongside the MCP instructions

The MCP instructions tell the model *what* to do. The plugin makes it
*true* in the host:

- The server cannot know the session directory, so it cannot resolve
  `project=`. The plugin can, because it runs inside the host.
- The server cannot see a `grep` call, let alone an empty one.
- The server cannot recover its own MCP connection or restart itself.
- The server cannot see file edits or pending compaction.

Together they behave like the Claude Code hooks from
[`integrations/claude-code/`](../claude-code/README.md), but implemented in
the host's own plugin API instead of shell guards: no `jq`, no `curl`, no
per-call process spawn, and coverage decisions come from the same registry the
hub uses.

## Troubleshooting

- **Plugin not listed** — confirm the file is `~/.config/opencode/plugins/codesearch.ts`
  (or referenced in `opencode.jsonc`), then `opencode service restart`. Look
  for `failed to load plugin` and `codesearch` in OpenCode's log
  (`~/.local/share/opencode/log/opencode.log`).
- **No scope / `project=null`** — the directory is not registered. `/codesearch-index`
  runs `codesearch index`; verify with `codesearch index list` and inspect
  `~/.codesearch/repos.json` (or your `CODESEARCH_REPOS_CONFIG`; `CODESEARCH_HOME` relocates the default).
- **Rescue never fires** — check `guards.rescue`, `guards.tools`, and health
  (`/codesearch-status`). Rescue is skipped while the hub is down.
- **Block mode too strict** — `guards.mode: "nudge"` (or `prune`) is the
  intended middle ground. Blocking deliberately requires a live hub.
- **Top-level `scope` tool missing / other plugins conflict** — the tool is
  registered as `codesearch_scope` precisely to avoid generic names.
- **Remote serve** — set `url` (or `CODESEARCH_SERVER`) and `token` (or
  `CODESEARCH_API_KEY`). `/healthz` stays probeable behind auth.
- **Skill/commands not visible** — they need an OpenCode build with skill and
  command transforms (2.x). Registration failures are logged and non-fatal;
  everything else keeps working.

## Security notes

- The plugin reads `opencode.jsonc`, its own config, and `repos.json`. It
  sends network requests only to the resolved serve URL (`/mcp`, `/healthz`).
- **Auto-recall sends the current user message to the serve hub** and injects
  retrieved code into the model context. It is off by default; enable it only
  where that is acceptable. Treat injected chunks as data, not instructions.
- **Auto-start** is off by default. When enabled it runs
  `health.startCommand` with your privileges; keep it pointed at
  `codesearch serve`.
- Guards fail open. `block` mode never denies while the hub is unreachable or
  the repo is unregistered, so a misconfigured guard cannot trap a session.
- No secrets are stored by the plugin; use `{env:...}` placeholders or the
  environment for tokens, and keep the config file readable only by you.

## Development

The plugin is dependency-free at runtime. For type checking and the tests
(unit tests for the pure helpers and the health runner, plus an end-to-end
smoke test against a mock OpenCode host and MCP server):

```bash
cd integrations/opencode
npm install          # dev-only: typescript + @types/node
npm run typecheck    # tsc --noEmit
npm run unit         # node test/unit.ts  — stripJsonc + HealthRunner (Node >= 23.6)
npm run smoke        # node test/smoke.ts — end-to-end vs mock host/MCP (or: bun)
npm test             # typecheck + unit + smoke
```

`test/unit.ts` covers the JSONC scanner (comments, string escapes, trailing
commas including comment-separated ones) and the health runner (probe
transitions, backoff/reconnect, recovery callback, stop, disabled mode).
`test/smoke.ts` covers scope resolution, guidance injection and deduplication,
zero-hit rescue, the one-time nudge, prune and block modes (including
fail-open for unregistered directories and an unreachable hub), compaction
outlines, the commands, the scope tool and the skill registration.
