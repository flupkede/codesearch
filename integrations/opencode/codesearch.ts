/**
 * codesearch — OpenCode plugin
 * ============================
 *
 * Makes the codesearch MCP server *structural* inside OpenCode instead of
 * advisory: resolves the current directory to an indexed project, injects the
 * usage guidance once per session, nudges (or prunes/blocks) grep/glob, rescues
 * empty greps with index results, keeps the serve hub healthy, and adds slash
 * commands, a scope tool, a skill and compaction assistance.
 *
 * The MCP server already publishes usage instructions on connect, but those are
 * advisory and connection-scoped. This plugin adds what only the host knows:
 * the working directory, the session lifecycle, tool calls and file edits.
 *
 * Design constraints (see README.md in this directory):
 *   - Self-contained: no imports, no runtime dependencies. The host executes
 *     this file as-is; fs/child_process come from `process.getBuiltinModule`.
 *   - Fail open: every hook is wrapped so a plugin or network failure can never
 *     break a session. Guards only block while the hub is confirmed reachable.
 *   - Generic: no assumption about repo names, groups or layout. Everything
 *     comes from `~/.codesearch/repos.json`, the serve hub, or the config file.
 *
 * Targets OpenCode v2 (`opencode` 2.x). Install: copy this file to
 * `~/.config/opencode/plugins/codesearch.ts` and restart OpenCode.
 */

/* ------------------------------------------------------------------ *
 * Types (structural views; no @opencode/plugin import so the file runs
 * as-is under the host's module loader)
 * ------------------------------------------------------------------ */

type Loose = Record<string, any>

interface Registration {
  dispose(): Promise<void>
}

interface SystemPart {
  type: "text"
  text: string
  [key: string]: unknown
}

interface PluginContext {
  app?: { version?: string }
  location?: {
    directory?: string
    workspaceID?: string
    project?: { id?: string; canonical?: string }
  }
  options?: Record<string, unknown>
  session?: {
    hook(
      name: string,
      callback: (event: Loose) => Promise<void> | void,
      options?: { providerID?: string },
    ): Promise<Registration>
    prompt?(input: Loose): Promise<unknown>
    context?(input: { sessionID: string }): Promise<unknown>
  }
  tool?: {
    hook(name: string, callback: (event: Loose) => Promise<void> | void): Promise<Registration>
    transform(callback: (editor: ToolEditor) => void): Promise<Registration>
  }
  command?: {
    transform(callback: (editor: CommandEditor) => void): Promise<Registration>
  }
  skill?: {
    transform(callback: (editor: SkillEditor) => void): Promise<Registration>
  }
  mcp?: {
    list(): Promise<unknown>
    reload?(): Promise<void>
  }
  event?: {
    subscribe(options?: { signal?: AbortSignal }): AsyncIterable<Loose>
  }
}

interface ToolEditor {
  namespace(namespace: { name: string; description: string }): void
  add(tool: Loose): void
  get?(id: string): Loose | undefined
  update?(id: string, update: (tool: Loose) => void): void
  remove?(id: string): void
}

interface CommandEditor {
  add(definition: Loose): void
}

interface SkillEditor {
  add(skill: Loose): void
  get?(id: string): Loose | undefined
  update?(id: string, update: (skill: Loose) => void): void
}

type GuardMode = "off" | "nudge" | "prune" | "block"

interface CodeSearchSettings {
  url: string
  token: string
  mcpServer: string
  reposConfig: string
  debug: boolean
  scope: {
    enabled: boolean
    inject: "session" | "message" | "off"
    includeGroups: boolean
    siblings: boolean
    maxSiblings: number
    warnUnindexed: boolean
  }
  guidance: { enabled: boolean; file: string; text: string }
  guards: {
    mode: GuardMode
    tools: string[]
    bash: boolean
    rescue: boolean
    rescueLimit: number
    rescueTimeoutMs: number
    rebuildWaitMs: number
  }
  health: {
    enabled: boolean
    intervalMs: number
    initialDelayMs: number
    maxBackoffMs: number
    reconnect: boolean
    autoStart: boolean
    startCommand: string
  }
  freshness: { enabled: boolean; maxFiles: number }
  compaction: { enabled: boolean; maxFiles: number; timeoutMs: number; budgetChars: number }
  recall: { enabled: boolean; limit: number; timeoutMs: number; budgetChars: number }
  commands: { enabled: boolean; indexCommand: string }
  tools: { scope: boolean }
  skill: { enabled: boolean; autoinvoke: boolean }
  log: {
    scope: boolean
    guidance: boolean
    guards: boolean
    rescue: boolean
    health: boolean
    recall: boolean
    commands: boolean
  }
}

interface ScopeInfo {
  directory: string
  project?: string
  projectPath?: string
  groups: string[]
  siblings: string[]
  registered: boolean
  hasGit: boolean
}

interface StatusRepo {
  alias: string
  project_path?: string
  total_chunks?: number
  total_files?: number
  model?: string
  lock_status?: string
}

/* ------------------------------------------------------------------ *
 * Constants
 * ------------------------------------------------------------------ */

const DEFAULTS: CodeSearchSettings = {
  url: "",
  token: "",
  mcpServer: "codesearch",
  reposConfig: "",
  debug: false,
  scope: {
    enabled: true,
    inject: "session",
    includeGroups: true,
    siblings: true,
    maxSiblings: 12,
    warnUnindexed: true,
  },
  guidance: { enabled: true, file: "", text: "" },
  guards: {
    mode: "nudge",
    tools: ["grep", "glob"],
    bash: true,
    rescue: true,
    rescueLimit: 5,
    rescueTimeoutMs: 3000,
    rebuildWaitMs: 120_000,
  },
  health: {
    enabled: true,
    intervalMs: 90_000,
    initialDelayMs: 60_000,
    maxBackoffMs: 900_000,
    reconnect: true,
    autoStart: false,
    startCommand: "codesearch serve --quiet",
  },
  freshness: { enabled: true, maxFiles: 8 },
  compaction: { enabled: true, maxFiles: 3, timeoutMs: 4000, budgetChars: 2400 },
  recall: { enabled: false, limit: 3, timeoutMs: 2500, budgetChars: 1800 },
  commands: { enabled: true, indexCommand: "codesearch index" },
  tools: { scope: true },
  skill: { enabled: true, autoinvoke: true },
  log: {
    scope: false,
    guidance: false,
    guards: true,
    rescue: true,
    health: true,
    recall: false,
    commands: true,
  },
}

const DEFAULT_SERVE_URL = "http://127.0.0.1:39725/mcp"
const PROTOCOL_VERSION = "2025-06-18"
const FALLBACK_PROTOCOL_VERSION = "2024-11-05"
const MAX_SEEN = 20_000

/**
 * Built-in guidance. Deliberately strategy-only: per-tool parameter detail
 * lives in the MCP tool schemas and is injected by the server's `initialize`
 * instructions, so it is not duplicated here.
 */
const DEFAULT_GUIDANCE = `codesearch indexes this workspace and serves semantic code search over MCP. Prefer it over grep/glob for anything conceptual, cross-file, or symbol-shaped.

Tool routing:
- search — concepts, unknown locations, mixed natural language + identifiers. Default mode is semantic (hybrid vector + BM25); mode="literal" for exact syntax — phrase=true only for true adjacent multi-word phrases, regex=true only within a single token (punctuation never reaches the token index; a separator-spanning pattern with zero hits auto-falls back to a raw content scan).
- find — where a symbol is defined (kind="definition"), its call-sites (kind="usages"), a file's imports (kind="imports"), or a module's dependents (kind="dependents").
- find_impact — precise transitive call sites before a rename or edit (SCIP: C#, TypeScript; on a "no backend" answer fall back to find kind="usages").
- explore — outline a file's top-level symbols (kind="outline") or find chunks similar to a known hit (kind="similar").
- get_chunk — read the code behind a search hit, without loading the whole file.
- status — index health and registered repos.

Rules:
- Multi-repo serve mode requires a scope on every call except status: pass project="<alias>" (returned paths are the server's paths; read content via get_chunk, not the local filesystem), or group="<name>" to fan out.
- Start broad with search, narrow with find/explore, read with get_chunk. Keep compact=true (the default) until you know which chunk you need.
- The serve hub watches files and refreshes indexes automatically; if a just-made change seems missing, retry the query once before falling back to grep.`

/** Skill playbook: the same strategy plus worked examples. */
const SKILL_CONTENT = `${DEFAULT_GUIDANCE}

Examples:
- "Where is the upload retried?" -> search(project=..., query="upload retry backoff") -> get_chunk(top hit).
- "Who calls validateToken?" in a TypeScript repo -> find_impact(project=..., symbol_name="validateToken") -> rename/edit with the full caller list.
- "Add a field to the settings struct" -> find(project=..., symbol="Settings", kind="definition") -> find(project=..., symbol="Settings", kind="usages").
- Exact syntax like "Vec<T>" or "foo = null" -> search(project=..., mode="literal", regex=true, query="Vec<").
- "How does this file relate to the rest?" -> explore(project=..., kind="outline", target=<file>) -> explore(kind="similar", target=<chunk_id>).
- Freshly edited file returning stale hits -> wait a moment, rerun the query; the watcher is reindexing.`

/* ------------------------------------------------------------------ *
 * Small utilities
 * ------------------------------------------------------------------ */

function bool(value: string | undefined, fallback: boolean): boolean {
  if (value === undefined || value === "") return fallback
  return !/^(0|false|no|off)$/i.test(value)
}

function truncate(input: string, max: number): string {
  if (input.length <= max) return input
  return input.slice(0, Math.max(0, max - 1)) + "…"
}

function shortHash(input: string): string {
  let h = 5381
  for (let i = 0; i < input.length; i++) h = ((h << 5) + h + input.charCodeAt(i)) | 0
  return (h >>> 0).toString(36)
}

function isPlainObject(value: unknown): value is Loose {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

/** Deep-merge plain objects; arrays and scalars replace. */
function mergeDeep<T>(base: T, override: unknown): T {
  if (!isPlainObject(override)) return base
  const out: Loose = { ...(base as unknown as Loose) }
  for (const [key, value] of Object.entries(override)) {
    const current = out[key]
    if (isPlainObject(current) && isPlainObject(value)) out[key] = mergeDeep(current, value)
    else out[key] = value
  }
  return out as T
}

function expandEnv(value: string): string {
  return value.replace(/\{env:([A-Za-z_][A-Za-z0-9_]*)\}/g, (_m, name: string) => process.env[name] ?? "")
}

function expandEnvDeep<T>(value: T): T {
  if (typeof value === "string") return expandEnv(value) as unknown as T
  if (Array.isArray(value)) return value.map((v) => expandEnvDeep(v)) as unknown as T
  if (isPlainObject(value)) {
    const out: Loose = {}
    for (const [k, v] of Object.entries(value)) out[k] = expandEnvDeep(v)
    return out as unknown as T
  }
  return value
}

/** Inline builtin loader: works under Node (getBuiltinModule) and Bun (require). */
function builtin(name: string): any {
  const p = process as unknown as { getBuiltinModule?: (n: string) => unknown }
  if (typeof p.getBuiltinModule === "function") {
    try {
      const mod = p.getBuiltinModule(name)
      if (mod) return mod
    } catch {
      /* fall through */
    }
  }
  const g = globalThis as unknown as { require?: (n: string) => unknown }
  if (typeof g.require === "function") {
    try {
      return g.require(name)
    } catch {
      /* fall through */
    }
  }
  return undefined
}

function homeDir(): string {
  return process.env.HOME || process.env.USERPROFILE || ""
}

function configDir(): string {
  return process.env.XDG_CONFIG_HOME || `${homeDir()}/.config`
}

/** Global codesearch root: $CODESEARCH_HOME, else ~/.codesearch (mirrors constants.rs `codesearch_home()`). */
function codesearchHome(): string {
  return process.env.CODESEARCH_HOME || `${homeDir()}/.codesearch`
}

/**
 * Strip `//` and block comments plus trailing commas from JSONC, honouring
 * string literals. Enough for OpenCode's config files.
 */
/**
 * Strip JSONC comments and trailing commas. Exported for `test/unit.ts`; the
 * host consumes the default export only.
 */
export function stripJsonc(text: string): string {
  let out = ""
  let inString = false
  let escaped = false
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]
    const next = text[i + 1]
    if (inString) {
      out += ch
      if (escaped) escaped = false
      else if (ch === "\\") escaped = true
      else if (ch === '"') inString = false
      continue
    }
    if (ch === '"') {
      inString = true
      out += ch
      continue
    }
    if (ch === "/" && next === "/") {
      while (i < text.length && text[i] !== "\n") i++
      out += "\n"
      continue
    }
    if (ch === "/" && next === "*") {
      i += 2
      while (i < text.length && !(text[i] === "*" && text[i + 1] === "/")) i++
      i++
      continue
    }
    if (ch === ",") {
      // A trailing comma may sit before whitespace AND comments, e.g.
      //   "a": 1, // keep this member last
      // }
      // Skip both while looking ahead for the closing bracket.
      let j = i + 1
      for (;;) {
        while (j < text.length && /\s/.test(text[j])) j++
        if (text[j] === "/" && text[j + 1] === "/") {
          while (j < text.length && text[j] !== "\n") j++
          continue
        }
        if (text[j] === "/" && text[j + 1] === "*") {
          j += 2
          while (j < text.length && !(text[j] === "*" && text[j + 1] === "/")) j++
          j += 2 // skip the closing */
          continue
        }
        break
      }
      if (text[j] === "}" || text[j] === "]") continue // drop trailing comma
    }
    out += ch
  }
  return out
}

function readJsonFile(path: string): Loose | undefined {
  const fs = builtin("node:fs")
  if (!fs || !path) return undefined
  try {
    const raw = fs.readFileSync(path, "utf8")
    const parsed = JSON.parse(stripJsonc(raw))
    return isPlainObject(parsed) ? parsed : undefined
  } catch {
    return undefined
  }
}

function fileExists(path: string): boolean {
  const fs = builtin("node:fs")
  if (!fs || !path) return false
  try {
    fs.statSync(path)
    return true
  } catch {
    return false
  }
}

function readTextFile(path: string): string | undefined {
  const fs = builtin("node:fs")
  if (!fs || !path) return undefined
  try {
    return fs.readFileSync(path, "utf8")
  } catch {
    return undefined
  }
}

/* ------------------------------------------------------------------ *
 * Path helpers
 * ------------------------------------------------------------------ */

function normalizePath(input: string): string {
  let p = (input || "").trim()
  if (!p) return ""
  // Windows UNC prefix (\\?\C:\...) and backslashes.
  p = p.replace(/^\\\\\?\\/, "")
  p = p.replace(/\\/g, "/")
  p = p.replace(/\/+$/, "")
  // Collapse duplicate slashes (keep the leading two for UNC-style roots).
  p = p.replace(/([^:])\/{2,}/g, "$1/")
  if (process.platform === "win32") p = p.toLowerCase()
  return p
}

/** True when `child` is `parent` or lives under it. */
function isUnder(child: string, parent: string): boolean {
  const c = normalizePath(child)
  const p = normalizePath(parent)
  if (!c || !p) return false
  return c === p || c.startsWith(p + "/")
}

/** Path relative to `root`, or the original when not under it. */
function relativeTo(path: string, root: string): string {
  const p = normalizePath(path)
  const r = normalizePath(root)
  if (!p || !r) return path
  if (p === r) return "."
  if (p.startsWith(r + "/")) return p.slice(r.length + 1)
  return path
}

/* ------------------------------------------------------------------ *
 * Configuration resolution
 * ------------------------------------------------------------------ */

function opencodeConfigPath(): string {
  const candidates = [
    `${configDir()}/opencode/opencode.jsonc`,
    `${configDir()}/opencode/opencode.json`,
  ]
  for (const candidate of candidates) if (fileExists(candidate)) return candidate
  return candidates[0]
}

/**
 * Read the `mcp` entry named `serverName` from the OpenCode config, supporting
 * both the v2 shape (`mcp.servers.<name>`) and the v1 shape (`mcp.<name>`).
 */
function readMcpEntry(serverName: string): { url: string; authorization: string } {
  const config = readJsonFile(opencodeConfigPath())
  if (!config) return { url: "", authorization: "" }
  const mcp = isPlainObject(config.mcp) ? config.mcp : undefined
  if (!mcp) return { url: "", authorization: "" }
  const servers = isPlainObject(mcp.servers) ? mcp.servers : mcp
  const entry = isPlainObject(servers[serverName]) ? servers[serverName] : undefined
  if (!entry) return { url: "", authorization: "" }
  const url = typeof entry.url === "string" ? expandEnv(entry.url) : ""
  const headers = isPlainObject(entry.headers) ? entry.headers : undefined
  const rawAuth = headers && typeof headers.Authorization === "string" ? expandEnv(headers.Authorization) : ""
  const authorization = rawAuth.replace(/^Bearer\s+/i, "")
  return { url, authorization }
}

function resolveSettings(ctx: PluginContext): CodeSearchSettings {
  // Deep-clone DEFAULTS: mergeDeep copies the top level only, so a later
  // `settings.guards.mode = ...` env override would mutate DEFAULTS and leak
  // into every other resolveSettings call in the same process.
  let settings = mergeDeep(structuredClone(DEFAULTS), ctx.options ?? {})

  const configPath =
    process.env.CODESEARCH_CONFIG || `${configDir()}/opencode/codesearch.json`
  const fileConfig = readJsonFile(configPath)
  if (fileConfig) settings = mergeDeep(settings, expandEnvDeep(fileConfig))

  const env = process.env
  settings.url = env.CODESEARCH_URL ?? settings.url
  settings.token = env.CODESEARCH_API_KEY ?? settings.token
  settings.mcpServer = env.CODESEARCH_MCP_SERVER ?? settings.mcpServer
  settings.reposConfig = env.CODESEARCH_REPOS_CONFIG ?? settings.reposConfig
  settings.debug = bool(env.CODESEARCH_PLUGIN_DEBUG, settings.debug)

  if (env.CODESEARCH_PLUGIN_GUARDS) settings.guards.mode = env.CODESEARCH_PLUGIN_GUARDS as GuardMode
  if (env.CODESEARCH_PLUGIN_BASH_GUARD) settings.guards.bash = bool(env.CODESEARCH_PLUGIN_BASH_GUARD, settings.guards.bash)
  if (env.CODESEARCH_PLUGIN_SCOPE) settings.scope.inject = env.CODESEARCH_PLUGIN_SCOPE as CodeSearchSettings["scope"]["inject"]
  if (env.CODESEARCH_PLUGIN_RESCUE) settings.guards.rescue = bool(env.CODESEARCH_PLUGIN_RESCUE, settings.guards.rescue)
  if (env.CODESEARCH_PLUGIN_RECALL) settings.recall.enabled = bool(env.CODESEARCH_PLUGIN_RECALL, settings.recall.enabled)
  if (env.CODESEARCH_PLUGIN_COMPACTION) settings.compaction.enabled = bool(env.CODESEARCH_PLUGIN_COMPACTION, settings.compaction.enabled)
  if (env.CODESEARCH_PLUGIN_COMMANDS) settings.commands.enabled = bool(env.CODESEARCH_PLUGIN_COMMANDS, settings.commands.enabled)
  if (env.CODESEARCH_PLUGIN_SKILL) settings.skill.enabled = bool(env.CODESEARCH_PLUGIN_SKILL, settings.skill.enabled)
  if (env.CODESEARCH_PLUGIN_HEALTH) settings.health.enabled = bool(env.CODESEARCH_PLUGIN_HEALTH, settings.health.enabled)

  const rebuildWaitEnv = env.CODESEARCH_PLUGIN_REBUILD_WAIT_MS ?? env.CODESEARCH_GREP_GUARD_WAIT_MS
  if (rebuildWaitEnv) {
    const parsed = Number.parseInt(rebuildWaitEnv, 10)
    if (Number.isInteger(parsed) && parsed >= 0) settings.guards.rebuildWaitMs = parsed
  }

  if (!["off", "nudge", "prune", "block"].includes(settings.guards.mode)) settings.guards.mode = "nudge"
  if (!["session", "message", "off"].includes(settings.scope.inject)) settings.scope.inject = "session"
  settings.guards.tools = (settings.guards.tools || []).map((t) => String(t).toLowerCase())

  return settings
}

function normalizeMcpUrl(value: string): string {
  let v = (value || "").trim()
  if (!v) return ""
  if (!/^https?:\/\//i.test(v)) v = `http://${v}`
  v = v.replace(/\/+$/, "")
  if (!/\/mcp$/i.test(v)) v += "/mcp"
  return v
}

/** Resolve the serve URL: config/env > CODESEARCH_SERVER > MCP entry > serve_url file > default. */
function resolveServeUrl(settings: CodeSearchSettings): string {
  const mcpEntry = readMcpEntry(settings.mcpServer)
  const serveUrlFile = readTextFile(`${codesearchHome()}/serve_url`)?.trim() ?? ""
  const candidates = [
    settings.url,
    process.env.CODESEARCH_SERVER ?? "",
    mcpEntry.url,
    serveUrlFile,
    DEFAULT_SERVE_URL,
  ]
  for (const candidate of candidates) {
    const normalized = normalizeMcpUrl(candidate)
    if (normalized) return normalized
  }
  return DEFAULT_SERVE_URL
}

function resolveToken(settings: CodeSearchSettings): string {
  if (settings.token) return settings.token
  const mcpEntry = readMcpEntry(settings.mcpServer)
  return mcpEntry.authorization || ""
}

/* ------------------------------------------------------------------ *
 * MCP client (streamable HTTP, JSON-RPC)
 * ------------------------------------------------------------------ */

interface CallOptions {
  timeoutMs?: number
  retry?: boolean
}

class McpClient {
  private sessionId: string | null = null
  private connected = false
  private connecting: Promise<void> | null = null
  private idSeq = 0
  private chain: Promise<unknown> = Promise.resolve()
  private protocolVersion = PROTOCOL_VERSION
  private readonly url: string
  private readonly token: string
  private readonly debug: boolean
  private readonly label: string

  constructor(url: string, token: string, debug: boolean, label: string) {
    this.url = url
    this.token = token
    this.debug = debug
    this.label = label
  }

  private log(...args: unknown[]): void {
    if (this.debug) console.log(`[codesearch:${this.label}]`, ...args)
  }

  /** Serialise requests per client so reconnect/session state never races. */
  private serial<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.chain.then(fn, fn)
    this.chain = run.then(
      () => undefined,
      () => undefined,
    )
    return run
  }

  async call(tool: string, args: Record<string, unknown>, options: CallOptions = {}): Promise<unknown> {
    const timeoutMs = options.timeoutMs ?? 30_000
    const retry = options.retry ?? true
    return this.serial(async () => {
      await this.connect()
      const payload = { jsonrpc: "2.0", id: ++this.idSeq, method: "tools/call", params: { name: tool, arguments: args } }
      try {
        return this.resultOf(tool, await this.post(payload, timeoutMs))
      } catch (err) {
        if (!retry) throw err
        const message = err instanceof Error ? err.message : String(err)
        if (/network|fetch failed|session|ECONN|socket|timed? ?out|abort|4[0-9][0-9]/i.test(message)) {
          this.connected = false
          this.sessionId = null
          await this.connect()
          return this.resultOf(tool, await this.post(payload, timeoutMs))
        }
        throw err
      }
    })
  }

  private async connect(): Promise<void> {
    if (this.connected) return
    if (this.connecting) return this.connecting
    this.connecting = this.doConnect().finally(() => {
      this.connecting = null
    })
    return this.connecting
  }

  private async doConnect(): Promise<void> {
    const attempt = async (version: string): Promise<boolean> => {
      const init = {
        jsonrpc: "2.0",
        id: 0,
        method: "initialize",
        params: {
          protocolVersion: version,
          capabilities: {},
          clientInfo: { name: "opencode-codesearch-plugin", version: "1.0.0" },
        },
      }
      const { session, messages } = await this.postRaw(init, 15_000)
      const first = messages.find((m) => (m as Loose).id === 0) as Loose | undefined
      if (!first?.result) return false
      this.sessionId = session
      this.protocolVersion = version
      await this.postRaw({ jsonrpc: "2.0", method: "notifications/initialized", params: {} }, 15_000)
      return true
    }
    if (!(await attempt(this.protocolVersion)) && this.protocolVersion !== FALLBACK_PROTOCOL_VERSION) {
      if (!(await attempt(FALLBACK_PROTOCOL_VERSION))) {
        throw new Error(`codesearch initialize failed at ${this.url}`)
      }
    }
    this.connected = true
    this.log("connected", this.sessionId ? `session ${this.sessionId}` : "(stateless)")
  }

  private async post(payload: Record<string, unknown>, timeoutMs: number): Promise<Loose[]> {
    const { messages } = await this.postRaw(payload, timeoutMs)
    return messages
  }

  private async postRaw(
    payload: Record<string, unknown>,
    timeoutMs: number,
  ): Promise<{ session: string | null; messages: Loose[] }> {
    const headers: Record<string, string> = {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
    }
    if (this.token) headers.authorization = `Bearer ${this.token}`
    if (this.sessionId) headers["mcp-session-id"] = this.sessionId

    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), timeoutMs)
    // The timer (and its abort signal) must stay armed through the body read:
    // `response.text()` is as capable of hanging as the response headers are
    // (undici's ~300 s bodyTimeout), and clearing the timer early would leave
    // the serial call chain blocked far past the requested timeout.
    try {
      const response = await fetch(this.url, {
        method: "POST",
        headers,
        body: JSON.stringify(payload),
        signal: controller.signal,
      })
      if (!response.ok) throw new Error(`codesearch HTTP ${response.status} ${response.statusText}`)
      const session = response.headers.get("mcp-session-id")
      const body = await response.text()
      const messages = parseMcpBody(body) as Loose[]
      if (messages.length === 0 && payload.id !== undefined) {
        throw new Error(`codesearch returned no JSON-RPC message for ${String(payload.method)}`)
      }
      return { session, messages }
    } finally {
      clearTimeout(timer)
    }
  }

  private resultOf(tool: string, messages: Loose[]): unknown {
    const message = messages.find((m) => m.result || m.error)
    if (!message) throw new Error(`No result for ${tool}`)
    if (message.error) throw new Error(`codesearch ${tool}: ${message.error.message ?? message.error.code ?? "error"}`)
    const result = message.result as Loose | undefined
    if (result?.isError) {
      const text = result.content?.[0]?.text || "unknown tool error"
      throw new Error(`codesearch ${tool} failed: ${String(text).slice(0, 500)}`)
    }
    const text = (result?.content ?? [])
      .filter((c: Loose) => c.type === "text" && typeof c.text === "string")
      .map((c: Loose) => c.text as string)
      .join("\n")
    if (text.trim() === "") return undefined
    try {
      return JSON.parse(text)
    } catch {
      return text
    }
  }
}

/** Parse a streamable-HTTP body (plain JSON or SSE `data:` lines). */
function parseMcpBody(body: string): unknown[] {
  const out: unknown[] = []
  const trimmed = body.trim()
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      out.push(JSON.parse(trimmed))
      return out
    } catch {
      /* fall through to SSE parsing */
    }
  }
  for (const line of body.split(/\r?\n/)) {
    const text = line.trim()
    if (!text.startsWith("data:")) continue
    const raw = text.slice(5).trim()
    if (!raw) continue
    try {
      out.push(JSON.parse(raw))
    } catch {
      /* ignore malformed frames */
    }
  }
  return out
}

/* ------------------------------------------------------------------ *
 * Registry + status cache
 * ------------------------------------------------------------------ */

class RepoRegistry {
  private repos = new Map<string, string>()
  private groups = new Map<string, string[]>()
  private loadedAt = 0
  private signature = ""
  private readonly path: string

  constructor(path: string) {
    this.path = path
  }

  load(force = false): void {
    const now = Date.now()
    if (!force && now - this.loadedAt < 30_000) return
    this.loadedAt = now
    const data = readJsonFile(this.path)
    const signature = data ? JSON.stringify(data) : ""
    if (signature === this.signature && !force) return
    this.signature = signature
    const repos = new Map<string, string>()
    const groups = new Map<string, string[]>()
    if (data) {
      const rawRepos = isPlainObject(data.repos) ? data.repos : {}
      for (const [alias, path] of Object.entries(rawRepos)) {
        if (typeof path === "string" && path) repos.set(alias, normalizePath(path))
      }
      const rawGroups = isPlainObject(data.groups) ? data.groups : {}
      for (const [name, members] of Object.entries(rawGroups)) {
        if (Array.isArray(members)) groups.set(name, members.map(String))
      }
    }
    this.repos = repos
    this.groups = groups
  }

  get size(): number {
    return this.repos.size
  }

  /** Registered project whose path contains (or equals) `directory`. */
  projectFor(directory: string): { alias: string; path: string } | undefined {
    this.load()
    const dir = normalizePath(directory)
    if (!dir) return undefined
    let best: { alias: string; path: string } | undefined
    for (const [alias, path] of this.repos) {
      if (!isUnder(dir, path)) continue
      if (!best || path.length > best.path.length) best = { alias, path }
    }
    return best
  }

  /** Registered repos nested under `directory` (multi-repo workspace roots). */
  siblingsFor(directory: string): string[] {
    this.load()
    const dir = normalizePath(directory)
    if (!dir) return []
    const out: string[] = []
    for (const [alias, path] of this.repos) {
      if (isUnder(path, dir) && normalizePath(path) !== dir) out.push(alias)
    }
    return out.sort()
  }

  groupsFor(alias: string): string[] {
    this.load()
    const out: string[] = []
    for (const [name, members] of this.groups) if (members.includes(alias)) out.push(name)
    return out.sort()
  }

  findAliasByPath(path: string): string | undefined {
    this.load()
    const target = normalizePath(path)
    for (const [alias, repoPath] of this.repos) if (repoPath === target) return alias
    return undefined
  }
}

/** Cached `status(kind="projects")` payload, used for scope fallback and health. */
class StatusCache {
  private repos: StatusRepo[] = []
  private groups: Record<string, string[]> = {}
  private fetchedAt = 0

  async refresh(client: McpClient, timeoutMs = 10_000): Promise<void> {
    const result = (await client.call("status", { kind: "projects" }, { timeoutMs, retry: false })) as Loose | undefined
    if (!result) return
    if (Array.isArray(result.repos)) this.repos = result.repos as StatusRepo[]
    if (isPlainObject(result.groups)) this.groups = result.groups as Record<string, string[]>
    this.fetchedAt = Date.now()
  }

  get ageMs(): number {
    return Date.now() - this.fetchedAt
  }

  get isFresh(): boolean {
    return this.fetchedAt > 0
  }

  get all(): StatusRepo[] {
    return this.repos
  }

  repo(alias: string): StatusRepo | undefined {
    return this.repos.find((r) => r.alias === alias)
  }

  projectFor(directory: string): { alias: string; path: string } | undefined {
    const dir = normalizePath(directory)
    if (!dir) return undefined
    let best: { alias: string; path: string } | undefined
    for (const repo of this.repos) {
      if (!repo.project_path) continue
      const path = normalizePath(repo.project_path)
      if (!isUnder(dir, path)) continue
      if (!best || path.length > best.path.length) best = { alias: repo.alias, path }
    }
    return best
  }

  groupsFor(alias: string): string[] {
    const out: string[] = []
    for (const [name, members] of Object.entries(this.groups)) if (members.includes(alias)) out.push(name)
    return out.sort()
  }

  siblingsFor(directory: string): string[] {
    const dir = normalizePath(directory)
    if (!dir) return []
    const out: string[] = []
    for (const repo of this.repos) {
      if (!repo.project_path) continue
      const path = normalizePath(repo.project_path)
      if (isUnder(path, dir) && path !== dir) out.push(repo.alias)
    }
    return out.sort()
  }
}

/* ------------------------------------------------------------------ *
 * Message helpers (new-user-message detection)
 * ------------------------------------------------------------------ */

function messageText(message: Loose): string {
  const parts = message?.parts
  if (Array.isArray(parts)) {
    return parts
      .map((p: Loose) => (p && typeof p.text === "string" ? p.text : ""))
      .filter(Boolean)
      .join("\n")
  }
  if (typeof message?.content === "string") return message.content
  if (Array.isArray(message?.content)) {
    return message.content
      .map((c: Loose) => (c && typeof c.text === "string" ? c.text : ""))
      .filter(Boolean)
      .join("\n")
  }
  return ""
}

function roleOf(message: Loose): string {
  return String(message?.info?.role ?? message?.role ?? "")
}

function lastUserMessage(messages: Loose[]): { id: string; text: string } | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i]
    if (roleOf(message) !== "user") continue
    const id = String(message?.info?.id ?? message?.id ?? "")
    const text = messageText(message)
    if (!text.trim()) continue
    return { id: id || shortHash(text), text }
  }
  return null
}

/** Heuristic for "this user message is a question about code" (recall gate). */
function isLikelyCodeQuestion(text: string): boolean {
  const trimmed = text.trim()
  if (trimmed.length < 20 || trimmed.length > 4000) return false
  if (/^\//.test(trimmed)) return false
  return (
    /\b(where|how|why|what|who|which|find|locate|implement|handle|call|use|flow|work|refactor|fix|add|change|rename|test)\b/i.test(trimmed) ||
    /[A-Za-z_][A-Za-z0-9_]*\(|::|->|\.\w+\b|_[a-z]/.test(trimmed)
  )
}

/* ------------------------------------------------------------------ *
 * Guidance and scope text
 * ------------------------------------------------------------------ */

function buildScopePrelude(scope: ScopeInfo, extra: string[]): string {
  const lines: string[] = []
  if (scope.project) {
    lines.push(`- Current directory is indexed by codesearch as project "${scope.project}"${scope.projectPath ? ` (${scope.projectPath})` : ""}.`)
    if (scope.groups.length) {
      lines.push(`- Project group${scope.groups.length > 1 ? "s" : ""}: ${scope.groups.map((g) => `"${g}"`).join(", ")} (use group= for cross-repo fan-out).`)
    }
    // The session sits in a NESTED git repo that is only covered as part of a
    // parent project: the index is the parent's, so the child's files may be
    // missing or stale and searches here will feel wrong (reported: kcharts
    // registered at the umbrella, sessions running in kcharts/druid).
    if (
      scope.hasGit &&
      scope.projectPath &&
      normalizePath(scope.directory) !== normalizePath(scope.projectPath)
    ) {
      lines.push(
        `- This directory is its own git repository, but codesearch only covers it as part of "${scope.project}" (${scope.projectPath}). ` +
          `For a complete, repo-local index run \`codesearch index add .\` here; add related repos to a group to search them together.`,
      )
    }
  } else if (scope.registered && scope.siblings.length) {
    lines.push(`- This workspace contains ${scope.siblings.length} registered codesearch project(s): ${scope.siblings.join(", ")}. Pass project= for one, or group= to fan out.`)
  } else if (scope.hasGit) {
    lines.push(`- This git repository is not registered with codesearch. Register it with \`codesearch index\` (or the /codesearch-index command) to enable semantic search here.`)
  }
  for (const line of extra) lines.push(line)
  return lines.length ? `codesearch scope:\n${lines.join("\n")}` : ""
}

/* ------------------------------------------------------------------ *
 * Health runner
 * ------------------------------------------------------------------ */

/**
 * Periodic `/healthz` probe with backoff and optional MCP reload. Exported for
 * `test/unit.ts`; the host consumes the default export only.
 */
export class HealthRunner {
  private timer: ReturnType<typeof setTimeout> | null = null
  private stopped = false
  private failures = 0
  state: "unknown" | "ok" | "down" = "unknown"
  lastError = ""
  private readonly ctx: PluginContext
  private readonly url: string
  private readonly settings: CodeSearchSettings
  private readonly onRecovered: () => void
  private readonly log: (...args: unknown[]) => void
  private readonly warn: (...args: unknown[]) => void

  constructor(
    ctx: PluginContext,
    url: string,
    settings: CodeSearchSettings,
    onRecovered: () => void,
    log: (...args: unknown[]) => void,
    warn: (...args: unknown[]) => void,
  ) {
    this.ctx = ctx
    this.url = url
    this.settings = settings
    this.onRecovered = onRecovered
    this.log = log
    this.warn = warn
  }

  start(): void {
    if (!this.settings.health.enabled) return
    this.schedule(this.settings.health.initialDelayMs)
  }

  stop(): void {
    this.stopped = true
    if (this.timer) clearTimeout(this.timer)
    this.timer = null
  }

  private schedule(ms: number): void {
    if (this.stopped) return
    const timer = (this.timer = setTimeout(() => void this.tick(), ms))
    ;(timer as unknown as { unref?: () => void }).unref?.()
  }

  private async tick(): Promise<void> {
    if (this.stopped) return
    const before = this.state
    await this.probe()
    if (before === "down" && this.state === "ok") {
      this.log(`hub recovered at ${this.url}`)
      this.onRecovered()
    }
    let delay = this.settings.health.intervalMs
    if (this.state === "down") {
      this.failures += 1
      delay = Math.min(this.settings.health.intervalMs * 2 ** (this.failures - 1), this.settings.health.maxBackoffMs)
      if (this.settings.health.reconnect) await this.tryReconnect()
      if (this.settings.health.autoStart && this.failures === 1) await this.tryAutoStart()
    } else {
      this.failures = 0
    }
    this.schedule(delay)
  }

  /** Liveness probe: any HTTP response means reachable; only network errors mean down. */
  async probe(): Promise<void> {
    const base = this.url.replace(/\/mcp\/?$/i, "")
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), 2000)
    try {
      await fetch(`${base}/healthz`, { signal: controller.signal })
      if (this.state !== "ok") this.log("hub reachable")
      this.state = "ok"
      this.lastError = ""
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err)
      if (this.state !== "down") this.warn(`hub unreachable at ${this.url}: ${message}`)
      this.state = "down"
      this.lastError = message
    } finally {
      clearTimeout(timer)
    }
  }

  private async tryReconnect(): Promise<void> {
    if (!this.settings.health.reconnect) return
    try {
      if (this.ctx.mcp && typeof this.ctx.mcp.reload === "function") {
        await this.ctx.mcp.reload()
        this.log("asked OpenCode to reload MCP servers")
      }
    } catch (err) {
      this.warn(`MCP reload failed: ${err instanceof Error ? err.message : err}`)
    }
  }

  private async tryAutoStart(): Promise<void> {
    if (!/^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])/i.test(this.url)) return
    const childProcess = builtin("node:child_process")
    if (!childProcess || !this.settings.health.startCommand.trim()) return
    try {
      const child = childProcess.spawn(this.settings.health.startCommand, {
        cwd: this.ctx.location?.directory || process.cwd(),
        shell: true,
        detached: true,
        stdio: "ignore",
      })
      child.unref?.()
      this.log(`started: ${this.settings.health.startCommand}`)
    } catch (err) {
      this.warn(`auto-start failed: ${err instanceof Error ? err.message : err}`)
    }
  }
}

/* ------------------------------------------------------------------ *
 * Guard helpers
 * ------------------------------------------------------------------ */

/**
 * Wait out a mid-rebuild index before the guard denies a grep. Polls
 * GET /indexing?path=<target> with a short deadline per probe and returns true
 * the moment the repo will answer codesearch queries (settled, unregistered,
 * or a 404 on an older serve without the endpoint); false when the rebuild is
 * still running at `waitMs` — grep is genuinely all you have. `waitMs <= 0`
 * settles immediately, preserving the pre-wait deny behavior. Probe failures
 * and non-404 HTTP errors do not settle: they mean "no fresh signal", so
 * polling continues until the cap (mirrors the Claude grep-guard hook twins).
 * Exported for test/unit.ts.
 */
export async function waitRebuildSettled(opts: {
  base: string
  target: string
  waitMs: number
  pollMs?: number
  token?: string
  fetchImpl?: typeof fetch
}): Promise<boolean> {
  const { base, target, waitMs, pollMs = 3000, token, fetchImpl = fetch } = opts
  if (waitMs <= 0 || !base || !target) return true
  const deadline = Date.now() + waitMs
  for (;;) {
    try {
      const controller = new AbortController()
      const timer = setTimeout(() => controller.abort(), 5000)
      let settled = false
      try {
        const headers: Record<string, string> = {}
        if (token) headers.authorization = `Bearer ${token}`
        const res = await fetchImpl(`${base}/indexing?path=${encodeURIComponent(target)}`, {
          signal: controller.signal,
          headers,
        })
        if (res.status === 404) settled = true
        else if (res.ok) {
          const info = (await res.json()) as Loose
          settled = info?.covered === false || !info?.indexing
        }
      } finally {
        clearTimeout(timer)
      }
      if (settled) return true
    } catch {
      /* transient probe failure mid-wait: keep polling until the cap */
    }
    const rest = deadline - Date.now()
    if (rest <= 0) return false
    await new Promise((resolve) => setTimeout(resolve, Math.min(pollMs, rest)))
  }
}

/** grep-family commands a bash tool call can smuggle past the native-tool guards. */
const BASH_GREP_WORDS = new Set(["grep", "egrep", "fgrep", "zgrep", "rg"])

/** Words between a separator and the real command (`do grep ...`, `! grep ...`). */
const SHELL_PREFIX_WORDS = new Set(["do", "then", "else", "elif", "if", "while", "until", "!", "{", "}", "time"])

/** Commands that run another command, so the wrapped word is the real command. */
const SHELL_WRAPPERS = new Set(["sudo", "doas", "env", "nohup", "nice", "command", "exec", "busybox", "stdbuf"])

/** Long grep options that take a value (`--include=X` or `--include X`). */
const GREP_LONG_VALUE_OPTS = new Set([
  "include", "exclude", "exclude-dir", "exclude-from", "regexp", "file", "label", "color", "colour",
  "binary-files", "directories", "devices", "max-count", "after-context", "before-context", "context",
  "group-separator", "type", "glob", "sort", "threads", "encoding", "engine", "pre", "pre-glob",
  "field-separator",
])

/** Short grep options that take a value (`-A3`, `-e PATTERN`, `-f FILE`). */
const GREP_SHORT_VALUE_OPTS = new Set(["e", "f", "m", "A", "B", "C", "d", "D", "t", "T", "j", "g"])

function bashToolCommand(event: Loose): string {
  const input = isPlainObject(event?.input) ? event.input : undefined
  return typeof input?.command === "string" ? input.command : ""
}

/**
 * Working directory a bash tool call runs in. Hosts that expose a per-call
 * `workdir`/`cwd` win; otherwise the command runs in the session directory.
 */
function bashToolWorkdir(event: Loose): string {
  const input = isPlainObject(event?.input) ? event.input : undefined
  for (const key of ["workdir", "cwd"]) {
    const value = input?.[key]
    if (typeof value === "string" && value.trim()) return value.trim()
  }
  return ""
}

/** Last path component, with a trailing `.exe` removed (Windows shells). */
function commandWord(raw: string): string {
  return (raw.replace(/\.exe$/i, "").split(/[\\/]/).pop() ?? "").toLowerCase()
}

interface ShellSegment {
  words: string[]
  /** Reads its stdin from the previous pipeline segment. */
  piped: boolean
}

/**
 * Split a shell command line into pipeline segments, one word list each.
 *
 * Quotes are resolved but preserved as single words, so argument POSITIONS
 * survive: in `grep -n "image:" file` the pattern stays a distinct word and
 * `file` is correctly the only path operand. Substitutions (`$(...)` and
 * backticks) are extracted into `substitutions` and parsed as their own
 * command lists by the caller.
 */
export function tokenizeShellCommand(command: string, substitutions: string[]): ShellSegment[] {
  const text = String(command ?? "")
  const segments: ShellSegment[] = []
  let words: string[] = []
  let word = ""
  let inWord = false
  let pendingPipe = false
  let skipNextWord = false
  let readsStdin = false
  let quote: '"' | "'" | "" = ""

  const pushWord = (): void => {
    if (!inWord) return
    if (skipNextWord) {
      // The word is a redirection target (`> out.txt`), not an argument.
      skipNextWord = false
      word = ""
      inWord = false
      return
    }
    words.push(word)
    word = ""
    inWord = false
  }
  const endSegment = (pipeNext: boolean): void => {
    pushWord()
    if (words.length) segments.push({ words, piped: pendingPipe || readsStdin })
    words = []
    pendingPipe = pipeNext
    readsStdin = false
  }
  /** End of a quoted or balanced substitution body starting at `from`. */
  const scanBalanced = (from: number, open: string, close: string): number => {
    let depth = 1
    let q: '"' | "'" | "" = ""
    for (let k = from; k < text.length; k++) {
      const ch = text[k]
      if (q) {
        if (ch === "\\" && q === '"') k++
        else if (ch === q) q = ""
        continue
      }
      if (ch === "'" || ch === '"') q = ch
      else if (ch === "\\") k++
      else if (ch === open) depth++
      else if (ch === close && --depth === 0) return k
    }
    return text.length
  }

  for (let i = 0; i < text.length; i++) {
    const ch = text[i]
    if (quote) {
      // Command substitution is recognised INSIDE double quotes too
      // (`echo "$(grep -rn foo src/)"`), and inside backticks.
      if (quote === '"' && ch === "$" && text[i + 1] === "(") {
        const end = scanBalanced(i + 2, "(", ")")
        substitutions.push(text.slice(i + 2, end))
        i = end
        continue
      }
      if (quote === '"' && ch === "`") {
        const end = text.indexOf("`", i + 1)
        const stop = end < 0 ? text.length : end
        substitutions.push(text.slice(i + 1, stop))
        i = stop
        continue
      }
      if (ch === "\\" && quote === '"' && i + 1 < text.length) {
        word += text[++i]
        inWord = true
      } else if (ch === quote) {
        quote = ""
      } else {
        word += ch
        inWord = true
      }
      continue
    }
    if (ch === "'" || ch === '"') {
      quote = ch
      inWord = true // an empty quoted string is still an argument position
      continue
    }
    if (ch === "\\" && i + 1 < text.length) {
      word += text[++i]
      inWord = true
      continue
    }
    if (ch === "`") {
      const end = text.indexOf("`", i + 1)
      const stop = end < 0 ? text.length : end
      substitutions.push(text.slice(i + 1, stop))
      endSegment(false)
      i = stop
      continue
    }
    if (ch === "$" && text[i + 1] === "(") {
      const end = scanBalanced(i + 2, "(", ")")
      substitutions.push(text.slice(i + 2, end))
      endSegment(false)
      i = end
      continue
    }
    if (ch === "|") {
      // `||` is a command separator, not a pipe.
      endSegment(text[i + 1] !== "|")
      if (text[i + 1] === "|") i++
      continue
    }
    if (ch === ">" || ch === "<") {
      // Redirection, not an argument: `2>/dev/null`, `> out.txt`, `2>&1`,
      // `>>/tmp/log`, `< input`. The operator and its target are dropped, so
      // they can never be mistaken for grep's path operand (reported:
      // `.../druid/2>/dev/null` resolved as a project target).
      if (inWord && /^[0-9]+$/.test(word)) {
        word = "" // file-descriptor prefix (`2>`)
        inWord = false
      }
      pushWord()
      i++
      while (i < text.length && (text[i] === ">" || text[i] === "<")) i++
      if (text[i] === "&") {
        // fd duplication (`2>&1`, `>&2`): no file target follows.
        i++
        while (i < text.length && /[0-9-]/.test(text[i])) i++
      } else {
        // `< file` makes grep read a STREAM, exactly like `cat file | grep`:
        // nothing on disk is a code-search target.
        if (ch === "<") readsStdin = true
        skipNextWord = true
      }
      i-- // the for-loop increment lands on the character after the operator
      continue
    }
    if (ch === ";" || ch === "&" || ch === "\n" || ch === "(" || ch === ")") {
      endSegment(false)
      continue
    }
    if (/\s/.test(ch)) {
      pushWord()
      continue
    }
    word += ch
    inWord = true
  }
  endSegment(false)
  return segments
}

/** Index of the command word in a segment, skipping shell prefixes/wrappers. */
function shellCommandIndex(words: string[]): number {
  let i = 0
  while (i < words.length) {
    const word = words[i]
    if (SHELL_PREFIX_WORDS.has(word)) {
      i++
      continue
    }
    const cmd = commandWord(word)
    if (SHELL_WRAPPERS.has(cmd)) {
      i++
      // Wrapper arguments: `-u user`, `-n 5`, `VAR=value`.
      while (i < words.length && (/^-/.test(words[i]) || /^[A-Za-z_][A-Za-z0-9_]*=/.test(words[i]))) i++
      continue
    }
    if (cmd === "timeout" || cmd === "time") {
      i++
      while (i < words.length && (/^-/.test(words[i]) || /^\d+(\.\d+)?[smhd]?$/.test(words[i]))) i++
      continue
    }
    break
  }
  return i < words.length ? i : -1
}

/**
 * Absolute paths a shell command will grep FILES under, resolved against the
 * command's working directory (`cd X && ...` adjusts it).
 *
 * Deliberately empty for a pure stream filter (`git log | grep -i fix`,
 * `find . | grep -v '^./.git/'`): the guard exists to reroute code search, and
 * reading a pipe is not that workload. Anything that reads files — a path
 * operand, `-r`/`-R`, grep behind `xargs`, or grep with no stdin and no
 * operand — contributes its targets, and the caller decides whether those
 * targets are inside a covered index.
 */
export function shellGrepTargets(command: string, baseDir: string): string[] {
  const targets: string[] = []
  const pending: Array<{ text: string; base: string }> = [{ text: String(command ?? ""), base: baseDir }]
  let guard = 0
  while (pending.length && guard++ < 32) {
    const current = pending.shift()!
    const substitutions: string[] = []
    const segments = tokenizeShellCommand(current.text, substitutions)
    for (const sub of substitutions) pending.push({ text: sub, base: current.base })
    let base = current.base
    for (const segment of segments) {
      const start = shellCommandIndex(segment.words)
      if (start < 0) continue
      const cmd = commandWord(segment.words[start])
      if (cmd === "cd") {
        const dest = segment.words[start + 1]
        if (dest && dest !== "-") base = resolveShellPath(dest, base)
        continue
      }
      let index = -1
      let viaXargs = false
      if (BASH_GREP_WORDS.has(cmd)) {
        index = start
      } else if (cmd === "xargs") {
        const rest = segment.words.slice(start + 1)
        const at = rest.findIndex((w) => BASH_GREP_WORDS.has(commandWord(w)))
        if (at >= 0) {
          index = start + 1 + at
          viaXargs = true // stdin becomes file arguments: it is not a filter
        }
      } else if (cmd === "git") {
        // `git [-C dir] grep ...` — git grep always reads the work tree.
        const rest = segment.words.slice(start + 1)
        const at = rest.findIndex((w) => commandWord(w) === "grep")
        if (at >= 0) index = start + 1 + at
      }
      if (index < 0) continue
      const parsed = parseGrepArgs(segment.words.slice(index + 1))
      if (parsed.operands.length) {
        for (const operand of parsed.operands) targets.push(resolveShellPath(operand, base))
      } else if (!segment.piped || viaXargs) {
        // No operand and no stream: a recursive grep walks the base dir, a
        // bare `grep PATTERN` would read stdin, and `xargs` supplies the file
        // list. A grep that reads a pipe or a `< file` reads no repo files.
        targets.push(normalizePath(base))
      }
      // Otherwise: a stream filter. Nothing on disk is read.
    }
  }
  return targets
}

/** Split grep arguments into the recursive flag and the path operands. */
function parseGrepArgs(args: string[]): { recursive: boolean; operands: string[] } {
  const operands: string[] = []
  let recursive = false
  let patternSeen = false
  let endOfOptions = false
  for (let i = 0; i < args.length; i++) {
    const arg = args[i]
    if (endOfOptions) {
      if (!patternSeen) patternSeen = true
      else operands.push(arg)
      continue
    }
    if (arg === "--") {
      endOfOptions = true
      continue
    }
    if (arg.startsWith("--")) {
      const eq = arg.indexOf("=")
      const name = eq >= 0 ? arg.slice(2, eq) : arg.slice(2)
      if (name === "recursive" || name === "dereference-recursive") recursive = true
      if (GREP_LONG_VALUE_OPTS.has(name)) {
        if (name === "regexp" || name === "file") patternSeen = true
        if (eq < 0) i++ // consume the separate value token
      }
      continue
    }
    if (arg.length > 1 && arg[0] === "-") {
      for (let k = 1; k < arg.length; k++) {
        const flag = arg[k]
        // `-rg` / `-rn` bundles are boolean; a value option ends the cluster.
        if (flag === "r" || flag === "R") recursive = true
        if (GREP_SHORT_VALUE_OPTS.has(flag)) {
          if (flag === "e" || flag === "f") patternSeen = true
          if (k === arg.length - 1) i++ // value is the next token
          break
        }
      }
      continue
    }
    if (!patternSeen) patternSeen = true
    else operands.push(arg)
  }
  return { recursive, operands }
}

/**
 * Resolve a shell operand to an absolute, lexically normalized path.
 *
 * Case is preserved: `normalizePath` lowercases on win32 for comparisons, but
 * these targets are shown to the user and unit-asserted verbatim, so coverage
 * matching normalizes both sides again instead.
 */
function resolveShellPath(operand: string, baseDir: string): string {
  let value = operand.trim().replace(/^\\\\\?\\/, "")
  if (!value) return ""
  if (value === "~" || value.startsWith("~/")) {
    value = `${process.env.HOME ?? process.env.USERPROFILE ?? ""}${value.slice(1)}`
  }
  // Windows mount forms name the same files as `C:/...`: Git Bash writes
  // `/c/dir`, and WSL-style literals reach a Windows shell as `/mnt/c/dir`.
  // Left POSIX-absolute they never match a drive-letter registry root, which
  // is a fail-open hole in `guards.mode: "block"`. Only on win32 — on a POSIX
  // host both are real absolute paths that the registry also stores as such.
  if (process.platform === "win32") {
    const mount = /^\/mnt\/([A-Za-z])(\/|$)/.exec(value) ?? /^\/([A-Za-z])(\/|$)/.exec(value)
    if (mount) value = `${mount[1].toUpperCase()}:/${value.slice(mount[0].length)}`
  }
  const absolute = /^([A-Za-z]:[\\/]|[\\/])/.test(value)
  const joined = absolute ? value : `${baseDir}/${value}`
  const drive = /^([A-Za-z]:)[\\/]/.exec(joined)?.[1] ?? ""
  const unc = !drive && /^[\\/]{2}/.test(joined)
  const rest = drive ? joined.slice(2) : joined
  const out: string[] = []
  for (const part of rest.split(/[\\/]+/)) {
    if (!part || part === ".") continue
    if (part === "..") {
      out.pop()
      continue
    }
    out.push(part)
  }
  const root = drive ? `${drive}/` : unc ? "//" : "/"
  const tail = out.join("/")
  return tail ? `${root}${tail}` : root
}

function resultOfToolEvent(event: Loose): { output?: string; metadata?: Loose } {
  const result = isPlainObject(event?.result) ? event.result : undefined
  if (!result) return {}
  const output = typeof result.output === "string" ? result.output : undefined
  const metadata = isPlainObject(result.metadata) ? result.metadata : undefined
  return { output, metadata }
}

function looksEmptyResult(event: Loose): boolean {
  const { output, metadata } = resultOfToolEvent(event)
  if (metadata) {
    for (const key of ["matches", "count", "numMatches", "total"]) {
      if (typeof metadata[key] === "number" && metadata[key] === 0) return true
    }
  }
  if (output === undefined) return false
  const trimmed = output.trim()
  if (trimmed === "") return true
  return /^(no (files|matches|results) found|0 (matches|results)|no matches)\b/i.test(trimmed)
}

function extractPattern(event: Loose): string {
  const input = isPlainObject(event?.input) ? event.input : undefined
  if (!input) return ""
  for (const key of ["pattern", "query", "search"]) {
    const value = input[key]
    if (typeof value === "string" && value.trim()) return value.trim()
  }
  return ""
}

const REGEX_META = /[\\^$.*+?()[\]{}|]/

function formatSearchHits(result: Loose | undefined, limit: number): string[] {
  const results = Array.isArray(result?.results) ? (result!.results as Loose[]) : []
  const lines: string[] = []
  for (const hit of results.slice(0, limit)) {
    const path = typeof hit.path === "string" ? hit.path : "?"
    const line = typeof hit.start_line === "number" ? `:${hit.start_line + 1}` : ""
    const kind = typeof hit.kind === "string" ? ` (${hit.kind})` : ""
    const signature = typeof hit.signature === "string" && hit.signature ? ` ${truncate(hit.signature, 90)}` : ""
    lines.push(`  - ${path}${line}${kind}${signature}`)
  }
  return lines
}

/**
 * Names of tools in a request's tool map that look like an ast-grep
 * integration (`ast_grep_search`, `ast-grep_edit`, ...). Every ast-grep
 * mention in this plugin is gated on this: codesearch users are not expected
 * to have ast-grep installed, so guidance must never name tools the host does
 * not expose. Unknown or absent maps stay silent.
 */
export function astGrepToolNames(tools: unknown): string[] {
  if (!isPlainObject(tools)) return []
  return Object.keys(tools as Loose).filter((name) => /ast[-_]grep/i.test(name))
}

/* ------------------------------------------------------------------ *
 * Plugin
 * ------------------------------------------------------------------ */

export default {
  id: "codesearch",

  async setup(ctx: PluginContext) {
    const settings = resolveSettings(ctx)
    const log = (...args: unknown[]) => console.log("[codesearch]", ...args)
    const warn = (...args: unknown[]) => console.warn("[codesearch]", ...args)
    const directory = normalizePath(ctx.location?.directory ?? "")

    const url = resolveServeUrl(settings)
    const token = resolveToken(settings)
    const registry = new RepoRegistry(
      settings.reposConfig || `${codesearchHome()}/repos.json`,
    )
    registry.load()
    const status = new StatusCache()

    // Interactive client (rescue/commands/recall) and a background client
    // (health/status/compaction assistance) so a slow background call can
    // never queue ahead of an interactive one.
    const client = new McpClient(url, token, settings.debug, "client")
    const background = new McpClient(url, token, settings.debug, "background")

    const disposables: Registration[] = []
    const timers: Array<ReturnType<typeof setTimeout>> = []
    const controller = new AbortController()

    const logGuard = (...args: unknown[]) => {
      if (settings.log.guards) log(...args)
    }
    const logRescue = (...args: unknown[]) => {
      if (settings.log.rescue) log(...args)
    }
    const logScope = (...args: unknown[]) => {
      if (settings.log.scope) log(...args)
    }

    if (settings.debug) log(`loaded in ${directory || "(unknown directory)"}; serve ${url}`)

    /* ---------------- scope resolution ---------------- */

    const scopeCache = { at: 0, value: undefined as ScopeInfo | undefined }

    function resolveScope(): ScopeInfo {
      const now = Date.now()
      if (scopeCache.value && now - scopeCache.at < 15_000) return scopeCache.value

      registry.load()
      const dir = directory
      let project = dir ? registry.projectFor(dir) : undefined
      if (!project && status.isFresh && dir) project = status.projectFor(dir)

      let groups: string[] = []
      if (project) {
        groups = registry.groupsFor(project.alias)
        if (!groups.length && status.isFresh) groups = status.groupsFor(project.alias)
      }

      let siblings: string[] = []
      if (!project && dir) {
        siblings = registry.siblingsFor(dir)
        if (!siblings.length && status.isFresh) siblings = status.siblingsFor(dir)
        siblings = siblings.slice(0, settings.scope.maxSiblings)
      }

      const hasGit = dir ? fileExists(`${dir}/.git`) : false
      const value: ScopeInfo = {
        directory: dir,
        project: project?.alias,
        projectPath: project?.path,
        groups: settings.scope.includeGroups ? groups : [],
        siblings: settings.scope.siblings ? siblings : [],
        registered: !!project,
        hasGit,
      }
      scopeCache.at = now
      scopeCache.value = value
      return value
    }

    /** Repo alias covering `path`, from repos.json then the live status snapshot. */
    function projectForPath(path: string): { alias: string; path: string } | undefined {
      const target = normalizePath(path)
      if (!target) return undefined
      const fromRegistry = registry.projectFor(target)
      if (fromRegistry) return fromRegistry
      return status.isFresh ? status.projectFor(target) : undefined
    }

    /**
     * Repo alias a native grep/glob call targets: its own `path` input when
     * given (a Grep aimed into another indexed repo), else the session dir.
     */
    function projectForTool(event: Loose): { alias: string; path: string } | undefined {
      const input = isPlainObject(event?.input) ? event.input : undefined
      const target = typeof input?.path === "string" ? input.path.trim() : ""
      if (target) return projectForPath(resolveShellPath(target, directory))
      return projectForPath(directory)
    }

    /* ---------------- health + status refresh ---------------- */

    const refreshStatus = async (): Promise<void> => {
      try {
        await status.refresh(background)
        scopeCache.at = 0 // pick up registry changes discovered via status
      } catch (err) {
        if (settings.debug) warn(`status refresh failed: ${err instanceof Error ? err.message : err}`)
      }
    }

    const health = new HealthRunner(
      ctx,
      url,
      settings,
      () => {
        void refreshStatus()
      },
      log,
      warn,
    )

    // Startup probe + first status refresh, off the critical path.
    void health.probe().then(() => void refreshStatus())

    /* ---------------- rebuild-wait (guard gap fix) ---------------- */

    // A rebuild keeps /healthz green while codesearch queries time out, so
    // whether to wait is decided here, never by the model.
    async function guardRebuildSettled(target?: string): Promise<boolean> {
      const settled = await waitRebuildSettled({
        base: url.replace(/\/mcp\/?$/i, ""),
        target: target || resolveScope().projectPath || resolveScope().directory,
        waitMs: settings.guards.rebuildWaitMs,
        token,
      })
      if (!settled) {
        logGuard(`rebuild still running after ${settings.guards.rebuildWaitMs}ms; opening the grep escape hatch`)
      }
      return settled
    }

    /* ---------------- freshness (file edits) ---------------- */

    const editedPaths = new Map<string, number>()
    const recordEdited = (path: string): void => {
      const normalized = normalizePath(path)
      if (!normalized) return
      if (!/\./.test(normalized)) return // skip directories / odd values
      editedPaths.set(normalized, Date.now())
      while (editedPaths.size > 64) {
        const oldest = editedPaths.keys().next().value as string | undefined
        if (oldest === undefined) break
        editedPaths.delete(oldest)
      }
    }

    const collectEditedFromEvent = (event: Loose): void => {
      const data = isPlainObject(event?.data) ? event.data : event
      for (const key of ["file", "path", "filePath", "filename"]) {
        const value = data?.[key]
        if (typeof value === "string") recordEdited(value)
      }
    }

    if (settings.freshness.enabled && ctx.event) {
      const loop = (async () => {
        try {
          for await (const event of ctx.event!.subscribe({ signal: controller.signal })) {
            try {
              const type = String((event as Loose)?.type ?? "")
              if (type.startsWith("file.")) collectEditedFromEvent(event as Loose)
            } catch {
              /* ignore malformed events */
            }
          }
        } catch (err) {
          if (!(err instanceof Error && err.name === "AbortError")) {
            if (settings.debug) warn(`event loop ended: ${err instanceof Error ? err.message : err}`)
          }
        }
      })()
      void loop
    }

    function recentEdits(max: number): string[] {
      const entries = [...editedPaths.entries()].sort((a, b) => b[1] - a[1])
      return entries.map(([path]) => path).slice(0, max)
    }

    /* ---------------- guidance text ---------------- */

    function loadGuidance(): string {
      if (!settings.guidance.enabled) return ""
      const inline = settings.guidance.text.trim()
      if (inline) return inline
      const path = expandEnv(settings.guidance.file).trim()
      if (path) {
        const text = readTextFile(path)?.trim()
        if (text) return text
      }
      return DEFAULT_GUIDANCE
    }

    const guidanceText = loadGuidance()

    /* ---------------- context hook (scope, guidance, prune, recall) ---------------- */

    const seenMessages = new Set<string>()
    const seenSessions = new Set<string>()
    const nudged = new Set<string>()
    /** Sessions whose request tool map exposed ast-grep tools, set by the
     *  context hook and read by block mode so its denial only ever names
     *  callable tools. An absent entry means "do not mention ast-grep". */
    const astGrepBySession = new Map<string, string[]>()

    function markSeen(set: Set<string>, key: string): boolean {
      if (set.has(key)) return false
      set.add(key)
      if (set.size > MAX_SEEN) set.clear()
      return true
    }

    async function onContext(event: Loose): Promise<void> {
      try {
        const sessionID = String(event?.sessionID ?? "unknown")
        const userMessage = lastUserMessage((event?.messages ?? []) as Loose[])
        if (!userMessage || !userMessage.text.trim()) return
        const messageKey = `${sessionID}:${userMessage.id}`
        if (!markSeen(seenMessages, messageKey)) return
        const firstForSession = !seenSessions.has(sessionID)
        seenSessions.add(sessionID)
        if (seenSessions.size > MAX_SEEN) seenSessions.clear()

        const scope = resolveScope()
        const system = Array.isArray(event?.system) ? (event.system as SystemPart[]) : undefined
        if (!system) return

        // Tool-map snapshot: this is the only place the assembled tool map is
        // visible, and every ast-grep mention is gated on it so environments
        // without an ast-grep integration see unchanged guidance.
        const astGrep = astGrepToolNames(event?.tools)
        astGrepBySession.set(sessionID, astGrep)
        if (astGrepBySession.size > MAX_SEEN) astGrepBySession.clear()

        // 1. Opt-in auto-recall: inject index context for the user message.
        if (settings.recall.enabled && scope.project && isLikelyCodeQuestion(userMessage.text)) {
          await injectRecall(system, sessionID, userMessage.text, scope)
        }

        // 2. Prune guard: hide grep/glob from the model when enforcement is
        //    structural, so the first move is a codesearch call. Like block
        //    mode, only engage while the hub is confirmed reachable —
        //    otherwise the removed tools would contradict the "grep/glob is an
        //    acceptable fallback" guidance injected below.
        const pruneActive =
          settings.guards.mode === "prune" && !!scope.project && health.state === "ok"
        if (pruneActive && isPlainObject(event.tools)) {
          for (const tool of settings.guards.tools) delete (event.tools as Loose)[tool]
        }

        // 3. Guidance + scope, once per session (or per message).
        const injectGuidance = settings.scope.enabled || guidanceText
        const wantsThisMessage =
          settings.scope.inject === "message" || (settings.scope.inject === "session" && firstForSession)
        if (injectGuidance && wantsThisMessage) {
          const extra: string[] = []
          if (pruneActive) {
            extra.push(`- grep and glob are intentionally unavailable in this project (codesearch guard mode "prune"); use the codesearch tools for discovery.`)
          }
          if (health.state === "down" && !pruneActive) {
            extra.push(`- The codesearch serve hub is unreachable right now; grep/glob is an acceptable fallback until it recovers.`)
          }
          if (astGrep.length > 0) {
            extra.push(
              `- For syntax-shaped queries and multi-file mechanical rewrites, prefer the ast-grep tools (${astGrep.join(" / ")}); codesearch and grep match text, not structure.`,
            )
          }
          if (freshUnindexedWarning(scope)) {
            extra.push(`- The registered index for "${scope.project}" is empty or was built with an unknown model; searches will miss until \`codesearch index\` refreshes it.`)
          }
          if (settings.freshness.enabled) {
            const edits = recentEdits(settings.freshness.maxFiles)
            if (edits.length) {
              const shown = edits.map((p) => relativeTo(p, scope.projectPath || directory) || p)
              extra.push(`- Files edited this session: ${shown.join(", ")}. The serve hub reindexes on write; if a query misses a very recent change, retry it once.`)
              editedPaths.clear()
            }
          }
          const prelude = buildScopePrelude(scope, extra)
          const text = [prelude, guidanceText].filter(Boolean).join("\n\n")
          if (text) {
            system.push({ type: "text", text })
            logScope(`injected scope/guidance for session ${sessionID} (project=${scope.project ?? "none"})`)
          }
        }
      } catch (err) {
        if (settings.debug) warn(`context hook error: ${err instanceof Error ? err.message : err}`)
      }
    }

    function freshUnindexedWarning(scope: ScopeInfo): boolean {
      if (!settings.scope.warnUnindexed || !scope.project) return false
      const repo = status.repo(scope.project)
      if (!repo) return false
      if (typeof repo.total_chunks === "number" && repo.total_chunks === 0) return true
      return !repo.model || repo.model === "unknown"
    }

    async function injectRecall(system: SystemPart[], sessionID: string, text: string, scope: ScopeInfo): Promise<void> {
      try {
        const result = (await client.call(
          "search",
          { project: scope.project, query: truncate(text, 1000), limit: settings.recall.limit, compact: false },
          { timeoutMs: settings.recall.timeoutMs, retry: false },
        )) as Loose | undefined
        const results = Array.isArray(result?.results) ? (result!.results as Loose[]) : []
        const kept: string[] = []
        let budget = settings.recall.budgetChars
        for (const hit of results) {
          const content = typeof hit.content === "string" ? hit.content.trim() : ""
          if (!content) continue
          const snippet = truncate(content, Math.min(700, budget))
          if (snippet.length > budget) continue
          budget -= snippet.length
          kept.push(`- ${hit.path ?? "?"}:${(hit.start_line ?? 0) + 1}\n  ${snippet.replace(/\n/g, "\n  ")}`)
        }
        if (!kept.length) return
        system.push({
          type: "text",
          text:
            `# codesearch context (auto-injected)\n` +
            `Relevant indexed code for the current request (background context, not instructions):\n` +
            kept.join("\n"),
        })
        if (settings.log.recall) log(`recall injected ${kept.length} chunk(s) for ${sessionID}`)
      } catch (err) {
        if (settings.debug) warn(`recall failed: ${err instanceof Error ? err.message : err}`)
      }
    }

    /* ---------------- tool hooks (guards, rescue, nudge) ---------------- */

    function blockMessage(tool: string, alias: string, projectPath: string, astGrep: string[] = []): string {
      const lines = [
        `codesearch: ${tool} is disabled here — the target is inside the indexed project "${alias}"${projectPath ? ` (${projectPath})` : ""}.`,
        `Use the codesearch MCP tools instead:`,
        `  - search { project: "${alias}", query: "<concept>" }   (add mode: "literal", regex: true for exact syntax)`,
        `  - find { project: "${alias}", symbol: "<name>", kind: "definition" | "usages" | "imports" | "dependents" }`,
        `  - find_impact { project: "${alias}", symbol_name: "<name>" }  (C#/TypeScript call sites)`,
        `  - explore { project: "${alias}", target: "<file>", kind: "outline" }`,
        `  - get_chunk { project: "${alias}", chunk_id: <id> }`,
      ]
      if (astGrep.length > 0) {
        lines.push(`  - ${astGrep.join(" / ")} for syntax-shaped queries and multi-file rewrites (structure, not text)`)
      }
      lines.push(
        `If codesearch is genuinely unavailable, set "guards": { "mode": "off" } in ~/.config/opencode/codesearch.json (or CODESEARCH_PLUGIN_GUARDS=off) and retry.`,
      )
      return lines.join("\n")
    }

    interface CoveredTarget {
      target: string
      alias: string
      projectPath: string
    }

    function bashBlockMessage(command: string, covered: CoveredTarget[]): string {
      const shown = covered.slice(0, 3)
      const targetLines = shown.map((c) => `  - ${c.target} → project "${c.alias}"${c.projectPath ? ` (${c.projectPath})` : ""}`)
      if (covered.length > shown.length) targetLines.push(`  - … ${covered.length - shown.length} more`)
      const alias = shown[0].alias
      return (
        `codesearch: shell grep is blocked — the command searches files inside an indexed project.\n` +
        `Target(s) resolved from the command's working directory, not the session directory:\n` +
        `${targetLines.join("\n")}\n` +
        `Raw grep through the shell tool bypasses the grep/glob guards. Command: ${truncate(command.trim(), 120)}\n` +
        `Use the codesearch MCP tools instead:\n` +
        `  - search { project: "${alias}", query: "<concept>" }   (add mode: "literal", regex: true for exact syntax)\n` +
        `  - find { project: "${alias}", symbol: "<name>", kind: "definition" | "usages" | "imports" | "dependents" }\n` +
        `  - get_chunk { project: "${alias}", chunk_id: <id> }\n` +
        `Stream filters (git log | grep -i fix) and greps outside every indexed project are not blocked.\n` +
        `If codesearch is genuinely unavailable, set "guards": { "bash": false } in ~/.config/opencode/codesearch.json (or CODESEARCH_PLUGIN_BASH_GUARD=0) and retry.`
      )
    }

    /**
     * Map shell-grep targets to the registered repos that cover them. Empty
     * when no target is inside an index, which is the "grep is right here"
     * case: another repo, /tmp, or a pure stream filter.
     */
    function coveredGrepTargets(command: string, workdir: string): CoveredTarget[] {
      const seen = new Set<string>()
      const covered: CoveredTarget[] = []
      for (const target of shellGrepTargets(command, workdir)) {
        if (!target || seen.has(target)) continue
        seen.add(target)
        const repo = projectForPath(target)
        if (repo) covered.push({ target, alias: repo.alias, projectPath: repo.path })
      }
      return covered
    }

    async function onToolBefore(event: Loose): Promise<void> {
      let blocked: Error | null = null
      try {
        if (settings.guards.mode === "off") return
        const tool = String(event?.tool ?? "").toLowerCase()
        if (tool === "bash" || tool === "shell") {
          // Shell grep is the escape hatch around the native grep/glob guards.
          // In `nudge` (the default) it runs and the after-hook appends the
          // one-time tip: the goal is "try codesearch first", not "block grep".
          // `prune` and `block` still deny it, because allowing the shell
          // bypass there would make hiding the native tools pointless.
          // Kill switch: guards.bash=false.
          if (!settings.guards.bash) return
          const command = bashToolCommand(event)
          if (!command) return
          // Scope follows the COMMAND, not the session: a grep in /tmp or in a
          // sibling repo must not be denied because the session happens to sit
          // in an indexed one. This was the reported misfire (kcharts session,
          // /tmp target, audits target) — see CHANGELOG.
          const workdir = bashToolWorkdir(event) || directory
          const covered = coveredGrepTargets(command, workdir)
          if (!covered.length) return
          if (settings.guards.mode === "nudge") return
          if (health.state !== "ok") return // fail open unless the hub is confirmed up
          if (!(await guardRebuildSettled(covered[0].target))) return // still rebuilding at the cap: grep is all you have
          blocked = new Error(bashBlockMessage(command, covered))
        } else {
          if (settings.guards.mode !== "block") return
          if (!settings.guards.tools.includes(tool)) return
          const repo = projectForTool(event)
          if (!repo) return
          if (health.state !== "ok") return // fail open unless the hub is confirmed up
          if (!(await guardRebuildSettled(repo.path))) return // still rebuilding at the cap: grep is all you have
          const astGrep = astGrepBySession.get(String(event?.sessionID ?? "")) ?? []
          blocked = new Error(blockMessage(tool, repo.alias, repo.path, astGrep))
        }
      } catch (err) {
        if (settings.debug) warn(`execute.before error: ${err instanceof Error ? err.message : err}`)
      }
      if (blocked) throw blocked
    }

    async function onToolAfter(event: Loose): Promise<void> {
      try {
        if (event?.status && event.status !== "completed") return
        const tool = String(event?.tool ?? "").toLowerCase()
        const result = isPlainObject(event?.result) ? event.result : undefined
        if (!result) return
        if (tool === "bash" || tool === "shell") {
          maybeNudgeBashGrep(event, result)
          return
        }
        if (!settings.guards.tools.includes(tool)) return
        const scope = resolveScope()
        if (!scope.project) return

        if (looksEmptyResult(event)) {
          await rescueEmptyResult(event, result, scope)
        } else {
          nudgeOnce(event, result, scope)
        }
      } catch (err) {
        if (settings.debug) warn(`execute.after error: ${err instanceof Error ? err.message : err}`)
      }
    }

    /** A shell grep that ran anyway (hub down at before-time) still gets the one-time tip. */
    function maybeNudgeBashGrep(event: Loose, result: Loose): void {
      if (settings.guards.mode === "off") return
      if (!settings.guards.bash) return
      if (health.state === "down") return // "grep is an acceptable fallback" while the hub is down
      const command = bashToolCommand(event)
      if (!command) return
      const covered = coveredGrepTargets(command, bashToolWorkdir(event) || directory)
      if (!covered.length) return
      const sessionID = String(event?.sessionID ?? "unknown")
      if (!markSeen(nudged, sessionID)) return
      const existing = typeof result.output === "string" ? result.output : ""
      result.output =
        existing +
        `\n[codesearch] tip: raw shell grep bypasses the codesearch guards — ${covered[0].target} is indexed as project "${covered[0].alias}". ` +
        `Prefer search (mode: "literal" for exact syntax) or find/get_chunk.`
      logGuard(`nudged session ${sessionID} after shell grep (bash tool)`)
    }

    async function rescueEmptyResult(event: Loose, result: Loose, scope: ScopeInfo): Promise<void> {
      if (!settings.guards.rescue) return
      if (health.state === "down") return
      const pattern = extractPattern(event)
      if (!pattern || pattern.length > 500) return
      // Glob patterns are not search queries: skip when the pattern is just a
      // filesystem wildcard ("**/*.ts") — there is nothing semantic to look up.
      if (String(event?.tool ?? "").toLowerCase() === "glob" && /[*?[\]{}]/.test(pattern)) return
      const plain = !REGEX_META.test(pattern)
      try {
        const hits = (await client.call(
          "search",
          {
            project: scope.project,
            query: pattern,
            mode: "literal",
            ...(plain ? { phrase: true } : { regex: true }),
            limit: settings.guards.rescueLimit,
            compact: true,
          },
          { timeoutMs: settings.guards.rescueTimeoutMs, retry: false },
        )) as Loose | undefined
        const lines = formatSearchHits(hits, settings.guards.rescueLimit)
        if (!lines.length) return
        const notice =
          `\n[codesearch] ${String(event?.tool ?? "grep")} returned nothing, but the index has ` +
          `${lines.length >= settings.guards.rescueLimit ? `${settings.guards.rescueLimit}+` : lines.length} ` +
          `match(es) for ${plain ? `"${pattern}"` : `/${pattern}/`} in project "${scope.project}":\n` +
          lines.join("\n") +
          `\nUse search/find/get_chunk for the full result.`
        const existing = typeof result.output === "string" ? result.output : ""
        result.output = existing + notice
        logRescue(`rescued empty ${String(event?.tool ?? "grep")} (${scope.project})`)
      } catch (err) {
        if (settings.debug) warn(`rescue failed: ${err instanceof Error ? err.message : err}`)
      }
    }

    function nudgeOnce(event: Loose, result: Loose, scope: ScopeInfo): void {
      if (settings.guards.mode === "off") return
      const sessionID = String(event?.sessionID ?? "unknown")
      if (!markSeen(nudged, sessionID)) return
      const existing = typeof result.output === "string" ? result.output : ""
      result.output =
        existing +
        `\n[codesearch] tip: this repository is indexed as project "${scope.project}" — ` +
        `codesearch search is faster and more token-efficient than ${String(event?.tool ?? "grep")} for cross-file or symbol lookups.`
      logGuard(`nudged session ${sessionID} after ${String(event?.tool ?? "grep")}`)
    }

    /* ---------------- commands ---------------- */

    async function promptSession(sessionID: string, prompt: Loose, text: string): Promise<void> {
      if (!ctx.session?.prompt) {
        warn("session.prompt is unavailable; cannot deliver command output")
        return
      }
      await ctx.session.prompt({ ...prompt, sessionID, text, delivery: (prompt as Loose)?.delivery ?? "queue" })
    }

    function formatStatusText(scope: ScopeInfo): string {
      const lines: string[] = []
      lines.push(`codesearch serve: ${url}`)
      lines.push(`health: ${health.state}${health.lastError ? ` (${health.lastError})` : ""}`)
      lines.push(`directory: ${directory || "?"}`)
      if (scope.project) {
        const repo = status.repo(scope.project)
        lines.push(
          `project: ${scope.project}` +
            (repo
              ? ` — ${repo.total_chunks ?? "?"} chunks, ${repo.total_files ?? "?"} files, model=${repo.model ?? "?"}, state=${repo.lock_status ?? "?"}`
              : " (not in the latest status snapshot)"),
        )
      } else if (scope.siblings.length) {
        lines.push(`workspace projects: ${scope.siblings.join(", ")}`)
      } else {
        lines.push(`project: not registered${scope.hasGit ? " (git repo — run /codesearch-index to register)" : ""}`)
      }
      if (scope.groups.length) lines.push(`groups: ${scope.groups.join(", ")}`)
      if (status.isFresh) lines.push(`hub repos: ${status.all.length}`)
      return lines.join("\n")
    }

    async function registerCommands(): Promise<void> {
      if (!settings.commands.enabled || !ctx.command?.transform) return
      const registration = await ctx.command.transform((editor) => {
        editor.add({
          name: "codesearch",
          description: "Semantic code search (codesearch) — query the index and inject the top hits",
          execute: async (invocation: Loose) => {
            const sessionID = String(invocation?.sessionID ?? "")
            const prompt = isPlainObject(invocation?.prompt) ? invocation.prompt : {}
            const query = String(prompt.text ?? invocation?.arguments ?? "").trim()
            const scope = resolveScope()
            if (!query) {
              await promptSession(sessionID, prompt, "Usage: /codesearch <query>")
              return
            }
            if (!scope.project) {
              await promptSession(
                sessionID,
                prompt,
                `codesearch: this directory is not registered with a codesearch project, so there is nothing to search yet. Run /codesearch-index first.`,
              )
              return
            }
            try {
              const hits = (await client.call(
                "search",
                { project: scope.project, query, limit: 8 },
                { timeoutMs: 8_000, retry: false },
              )) as Loose | undefined
              const lines = formatSearchHits(hits, 8)
              await promptSession(
                sessionID,
                prompt,
                lines.length
                  ? `codesearch results for "${query}" (project "${scope.project}"):\n${lines.join("\n")}\n\nFetch full code with get_chunk, or refine with find/explore.`
                  : `codesearch returned no results for "${query}" in project "${scope.project}". Try different wording, or mode literal with regex=true for exact syntax.`,
              )
              if (settings.log.commands) log(`/codesearch "${truncate(query, 80)}" -> ${lines.length} hit(s)`)
            } catch (err) {
              await promptSession(
                sessionID,
                prompt,
                `codesearch search failed: ${err instanceof Error ? err.message : String(err)}`,
              )
            }
          },
        })

        editor.add({
          name: "codesearch-status",
          description: "Show codesearch serve health, current project scope and index state",
          execute: async (invocation: Loose) => {
            const sessionID = String(invocation?.sessionID ?? "")
            const prompt = isPlainObject(invocation?.prompt) ? invocation.prompt : {}
            await health.probe()
            await refreshStatus()
            scopeCache.at = 0
            await promptSession(sessionID, prompt, formatStatusText(resolveScope()))
          },
        })

        editor.add({
          name: "codesearch-index",
          description: "Register and index the current directory with codesearch (runs in the background)",
          execute: async (invocation: Loose) => {
            const sessionID = String(invocation?.sessionID ?? "")
            const prompt = isPlainObject(invocation?.prompt) ? invocation.prompt : {}
            const childProcess = builtin("node:child_process")
            const command = settings.commands.indexCommand.trim()
            if (!command) {
              await promptSession(sessionID, prompt, "codesearch: commands.indexCommand is empty; nothing to run.")
              return
            }
            if (!childProcess) {
              await promptSession(sessionID, prompt, `codesearch: cannot spawn processes here. Run \`${command}\` in ${directory} manually.`)
              return
            }
            try {
              const child = childProcess.spawn(command, {
                cwd: directory || process.cwd(),
                shell: true,
                detached: true,
                stdio: "ignore",
              })
              child.unref?.()
              await promptSession(
                sessionID,
                prompt,
                `codesearch indexing started in ${directory} (\`${command}\`). It runs in the background; check /codesearch-status in a minute (large repos can take longer).`,
              )
              if (settings.log.commands) log(`/codesearch-index started: ${command}`)
            } catch (err) {
              await promptSession(sessionID, prompt, `codesearch index failed to start: ${err instanceof Error ? err.message : String(err)}`)
            }
          },
        })
      })
      disposables.push(registration)
    }

    /* ---------------- scope tool ---------------- */

    async function registerScopeTool(): Promise<void> {
      if (!settings.tools.scope || !ctx.tool?.transform) return
      const registration = await ctx.tool.transform((editor) => {
        editor.add({
          name: "codesearch_scope",
          description:
            "Return the codesearch scope for the current directory: project alias, groups, index state, serve health. " +
            "Use it when you are unsure which project= value to pass to codesearch MCP tools.",
          input: { type: "object", properties: {}, additionalProperties: false },
          execute: async () => {
            await refreshStatus().catch(() => undefined)
            scopeCache.at = 0
            const scope = resolveScope()
            const repo = scope.project ? status.repo(scope.project) : undefined
            const info = {
              directory: scope.directory,
              project: scope.project ?? null,
              groups: scope.groups,
              registered: scope.registered,
              siblings: scope.siblings,
              indexed: repo ? (repo.total_chunks ?? 0) > 0 : undefined,
              chunks: repo?.total_chunks,
              model: repo?.model,
              lockStatus: repo?.lock_status,
              serveUrl: url,
              serveHealth: health.state,
            }
            return { content: JSON.stringify(info, null, 2) }
          },
        })
      })
      disposables.push(registration)
    }

    /* ---------------- skill ---------------- */

    async function registerSkill(): Promise<void> {
      if (!settings.skill.enabled || !ctx.skill?.transform) return
      const registration = await ctx.skill.transform((editor) => {
        editor.add({
          id: "codesearch",
          name: "codesearch navigation",
          description:
            "Use the codesearch MCP tools (search, find, find_impact, explore, get_chunk) to navigate code: " +
            "project scope rules, tool routing, and exact-syntax patterns.",
          path: "codesearch-plugin/skill.md",
          content: SKILL_CONTENT,
        })
        if (settings.skill.autoinvoke && typeof editor.get === "function" && typeof editor.update === "function") {
          const existing = editor.get("codesearch")
          if (existing) editor.update("codesearch", (skill: Loose) => void (skill.autoinvoke = true))
        }
      })
      disposables.push(registration)
    }

    /* ---------------- compaction assist ---------------- */

    function formatOutline(target: string, result: unknown): string {
      const entries = Array.isArray(result) ? (result as Loose[]) : []
      const symbols = entries
        .filter((entry) => entry && typeof entry.signature === "string" && entry.signature)
        .filter((entry) => !/^imports\b/i.test(String(entry.signature)))
        .slice(0, 12)
        .map((entry) => `${truncate(String(entry.signature), 100)}:${(entry.start_line ?? 0) + 1}`)
      const label = target.replace(/\\/g, "/")
      return symbols.length ? `- ${label}: ${symbols.join("; ")}` : ""
    }

    async function onCompaction(event: Loose): Promise<void> {
      try {
        if (!settings.compaction.enabled) return
        const scope = resolveScope()
        if (!scope.project || !Array.isArray(event?.system)) return
        const candidates: string[] = []
        for (const path of recentEdits(settings.compaction.maxFiles * 2)) {
          if (!candidates.includes(path)) candidates.push(path)
        }
        if (!candidates.length) return
        const deadline = Date.now() + settings.compaction.timeoutMs
        const outlines: string[] = []
        let budget = settings.compaction.budgetChars
        for (const path of candidates) {
          if (outlines.length >= settings.compaction.maxFiles || Date.now() >= deadline || budget <= 0) break
          try {
            // Strip the project root the same way `relativeTo` does for the
            // freshness notice: it normalizes both sides (including the
            // win32 lowercase rule), so absolute paths, alias-prefixed paths
            // and already-relative paths all resolve consistently. Stripping
            // a literal alias prefix here silently produced absolute targets
            // on Windows, where the case-normalized path never matched.
            const target = relativeTo(path, scope.projectPath || directory) || path
            const result = await background.call(
              "explore",
              { project: scope.project, target, kind: "outline" },
              { timeoutMs: Math.max(500, deadline - Date.now()), retry: false },
            )
            const line = formatOutline(target, result)
            if (line) {
              outlines.push(line)
              budget -= line.length
            }
          } catch {
            /* best effort */
          }
        }
        if (!outlines.length) return
        ;(event.system as SystemPart[]).push({
          type: "text",
          text:
            `# codesearch outlines (captured before compaction)\n` +
            `Pre-compaction structure of recently edited files (from the code index):\n` +
            outlines.join("\n"),
        })
        if (settings.log.scope) log(`compaction outlines: ${outlines.length} file(s)`)
      } catch (err) {
        if (settings.debug) warn(`compaction hook error: ${err instanceof Error ? err.message : err}`)
      }
    }

    /* ---------------- register everything (fail soft) ---------------- */

    const safeRegister = async (name: string, fn: () => Promise<void>): Promise<void> => {
      try {
        await fn()
        if (settings.debug) log(`registered: ${name}`)
      } catch (err) {
        warn(`could not register ${name}: ${err instanceof Error ? err.message : err}`)
      }
    }

    if (ctx.session?.hook) {
      await safeRegister("session context hook", async () => {
        disposables.push(await ctx.session!.hook("context", onContext))
      })
    }
    if (ctx.tool?.hook && settings.guards.mode !== "off") {
      await safeRegister("tool execute.before hook", async () => {
        disposables.push(await ctx.tool!.hook("execute.before", onToolBefore))
      })
      await safeRegister("tool execute.after hook", async () => {
        disposables.push(await ctx.tool!.hook("execute.after", onToolAfter))
      })
    }
    if (settings.compaction.enabled && ctx.session?.hook) {
      await safeRegister("compaction hook", async () => {
        disposables.push(await ctx.session!.hook("compaction", onCompaction))
      })
    }
    await safeRegister("commands", registerCommands)
    await safeRegister("scope tool", registerScopeTool)
    await safeRegister("skill", registerSkill)

    /* ---------------- timers ---------------- */

    if (settings.health.enabled) {
      health.start()
      const statusTimer = setInterval(() => void refreshStatus(), 10 * 60_000)
      ;(statusTimer as unknown as { unref?: () => void }).unref?.()
      timers.push(statusTimer)
    }

    log(`ready (project=${resolveScope().project ?? "none"}, guards=${settings.guards.mode}, serve=${url})`)

    return async () => {
      controller.abort()
      health.stop()
      for (const timer of timers) clearTimeout(timer)
      for (const registration of disposables) await registration.dispose().catch(() => undefined)
    }
  },
}
