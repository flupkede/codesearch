/**
 * Smoke test for the codesearch OpenCode plugin.
 *
 * Runs the plugin against a mock OpenCode context and a mock codesearch MCP
 * server (streamable HTTP) so scope resolution, guidance injection, zero-hit
 * rescue, the nudge/prune/block guards (including prune fail-open while the
 * hub is down), compaction outlines, commands, the scope tool and the skill
 * registration are exercised end-to-end without a real model session.
 *
 * Run (Node >= 23.6 strips types natively):
 *   node test/smoke.ts
 * or:
 *   bun test/smoke.ts
 */

import assert from "node:assert/strict"
import http from "node:http"
import { mkdtempSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import plugin from "../codesearch.ts"

type Loose = Record<string, any>
type Hook = (event: Loose) => any

const HITS = [
  {
    chunk_id: 7,
    path: "my-service/src/uploader.ts",
    start_line: 41,
    end_line: 63,
    kind: "Function",
    signature: "function retryUpload(job: Job, attempt: number)",
  },
  {
    chunk_id: 9,
    path: "my-service/src/queue.ts",
    start_line: 12,
    end_line: 30,
    kind: "Class",
    signature: "class UploadQueue",
  },
]

const STATUS = {
  repos: [
    {
      alias: "my-service",
      project_path: "/work/my-service",
      total_chunks: 120,
      total_files: 30,
      model: "minilm-l6-q",
      lock_status: "warm",
    },
    {
      alias: "sibling-api",
      project_path: "/work/sibling-api",
      total_chunks: 0,
      total_files: 0,
      model: "unknown",
      lock_status: "available",
    },
  ],
  groups: { platform: ["my-service", "sibling-api"] },
  hub: { indexed: true, status: "ready" },
}

function startMockServer(): Promise<{ url: string; close: () => Promise<void>; calls: Loose[] }> {
  const calls: Loose[] = []
  const server = http.createServer((req, res) => {
    let body = ""
    req.on("data", (chunk) => (body += chunk))
    req.on("end", () => {
      if (req.url === "/healthz") {
        res.writeHead(200, { "content-type": "application/json" })
        res.end('{"status":"ok"}')
        return
      }
      if (req.url !== "/mcp" || req.method !== "POST") {
        res.writeHead(404)
        res.end()
        return
      }
      let message: Loose
      try {
        message = JSON.parse(body) as Loose
      } catch {
        res.writeHead(400)
        res.end()
        return
      }
      const reply = (payload: Loose, session = false): void => {
        res.writeHead(200, {
          "content-type": "application/json",
          ...(session ? { "mcp-session-id": "smoke-session" } : {}),
        })
        res.end(JSON.stringify(payload))
      }
      if (message.method === "initialize") {
        reply(
          {
            jsonrpc: "2.0",
            id: message.id,
            result: {
              protocolVersion: message.params?.protocolVersion ?? "2025-06-18",
              capabilities: { tools: {} },
              serverInfo: { name: "mock-codesearch", version: "smoke" },
            },
          },
          true,
        )
        return
      }
      if (message.method === "notifications/initialized") {
        res.writeHead(202)
        res.end()
        return
      }
      if (message.method === "tools/call") {
        const name = String(message.params?.name ?? "")
        const args = (message.params?.arguments ?? {}) as Loose
        calls.push({ name, args })
        let payload: unknown = {}
        if (name === "status") {
          payload = STATUS
        } else if (name === "search") {
          const query = String(args.query ?? "").toLowerCase()
          payload = query.includes("none-at-all") ? { results: [] } : { results: HITS }
        } else if (name === "explore") {
          payload = [
            { chunk_id: 1, kind: "Function", signature: "function retryUpload(job: Job)", start_line: 40, end_line: 63 },
          ]
        }
        reply({
          jsonrpc: "2.0",
          id: message.id,
          result: { content: [{ type: "text", text: JSON.stringify(payload) }] },
        })
        return
      }
      reply({ jsonrpc: "2.0", id: message.id, result: {} })
    })
  })
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const address = server.address()
      const port = typeof address === "object" && address ? address.port : 0
      resolve({
        url: `http://127.0.0.1:${port}/mcp`,
        close: () =>
          new Promise((done) => {
            server.close(() => done(undefined))
          }),
        calls,
      })
    })
  })
}

function makeCtx(directory: string) {
  const hooks: Record<string, Hook[]> = {}
  const toolHooks: Record<string, Hook[]> = {}
  const tools: Loose[] = []
  const commands: Loose[] = []
  const skills: Loose[] = []
  const prompts: string[] = []
  const eventQueue: Loose[] = []
  let eventWake: (() => void) | undefined

  const context: Loose = {
    app: { version: "smoke" },
    location: { directory },
    session: {
      hook: async (name: string, callback: Hook) => {
        ;(hooks[name] ||= []).push(callback)
        return { dispose: async () => {} }
      },
      prompt: async (input: Loose) => {
        prompts.push(String(input.text ?? ""))
        return {}
      },
    },
    tool: {
      hook: async (name: string, callback: Hook) => {
        ;(toolHooks[name] ||= []).push(callback)
        return { dispose: async () => {} }
      },
      transform: async (callback: (editor: Loose) => void) => {
        callback({
          namespace: () => {},
          add: (tool: Loose) => tools.push(tool),
          get: () => undefined,
          update: () => {},
        })
        return { dispose: async () => {} }
      },
    },
    command: {
      transform: async (callback: (editor: Loose) => void) => {
        callback({ add: (command: Loose) => commands.push(command) })
        return { dispose: async () => {} }
      },
    },
    skill: {
      transform: async (callback: (editor: Loose) => void) => {
        callback({
          add: (skill: Loose) => skills.push(skill),
          get: () => undefined,
          update: () => {},
        })
        return { dispose: async () => {} }
      },
    },
    mcp: { list: async () => [] },
    event: {
      subscribe: async function* (options?: { signal?: AbortSignal }) {
        const signal = options?.signal
        while (!signal?.aborted) {
          if (!eventQueue.length) {
            await new Promise<void>((resolve) => {
              eventWake = resolve
              signal?.addEventListener("abort", () => resolve(), { once: true })
            })
          }
          while (eventQueue.length) yield eventQueue.shift() as Loose
        }
      },
    },
  }

  return {
    context,
    hooks,
    toolHooks,
    tools,
    commands,
    skills,
    prompts,
    emit: (event: Loose) => {
      eventQueue.push(event)
      const wake = eventWake
      eventWake = undefined
      wake?.()
    },
  }
}

async function setupPlugin(directory: string) {
  const app = makeCtx(directory)
  const cleanup = await plugin.setup(app.context as any)
  return { app, cleanup }
}

function userMessage(sessionID: string, messageID: string, text: string): Loose {
  return {
    sessionID,
    system: [],
    tools: { grep: {}, glob: {}, read: {} },
    messages: [{ info: { role: "user", id: messageID }, parts: [{ type: "text", text }] }],
  }
}

async function main(): Promise<void> {
  const mock = await startMockServer()
  const tmp = mkdtempSync(join(tmpdir(), "codesearch-plugin-smoke-"))

  const reposPath = join(tmp, "repos.json")
  writeFileSync(
    reposPath,
    JSON.stringify({
      repos: { "my-service": "/work/my-service", "sibling-api": "/work/sibling-api" },
      groups: { platform: ["my-service", "sibling-api"] },
    }),
  )
  const configPath = join(tmp, "codesearch.json")
  writeFileSync(
    configPath,
    JSON.stringify({
      url: "",
      log: { guards: false, rescue: false, health: false, commands: false, scope: false, recall: false },
    }),
  )

  process.env.CODESEARCH_CONFIG = configPath
  process.env.CODESEARCH_REPOS_CONFIG = reposPath
  process.env.CODESEARCH_URL = mock.url
  process.env.CODESEARCH_PLUGIN_HEALTH = "0"
  delete process.env.CODESEARCH_PLUGIN_GUARDS

  /* ---------------- nudge mode (default) ---------------- */

  const one = await setupPlugin("/work/my-service")
  assert.ok(one.app.hooks.context?.length === 1, "context hook registered")
  assert.ok(one.app.toolHooks["execute.before"]?.length === 1, "execute.before hook registered")
  assert.ok(one.app.toolHooks["execute.after"]?.length === 1, "execute.after hook registered")
  assert.ok(one.app.hooks.compaction?.length === 1, "compaction hook registered")
  assert.deepEqual(
    one.app.commands.map((c) => c.name).sort(),
    ["codesearch", "codesearch-index", "codesearch-status"],
    "commands registered",
  )
  assert.equal(one.app.tools.length, 1, "scope tool registered")
  assert.equal(one.app.tools[0].name, "codesearch_scope")
  assert.equal(one.app.skills.length, 1, "skill registered")
  assert.equal(one.app.skills[0].id, "codesearch")

  // Guidance + scope injection, once per session.
  const first = userMessage("s1", "m1", "Where is the upload retried after a failure?")
  await one.app.hooks.context[0](first)
  const injected = first.system.map((p: Loose) => p.text).join("\n")
  assert.match(injected, /project "my-service"/, "scope line names the resolved project")
  assert.match(injected, /group: "platform"/, "scope line lists groups")
  assert.match(injected, /codesearch/, "guidance is injected")
  assert.ok(first.tools.grep, "nudge mode keeps grep visible")

  const second = userMessage("s1", "m2", "And where is it queued?")
  await one.app.hooks.context[0](second)
  assert.equal(second.system.length, 0, "session mode injects once per session")

  const duplicate = userMessage("s1", "m1", "Where is the upload retried after a failure?")
  await one.app.hooks.context[0](duplicate)
  assert.equal(duplicate.system.length, 0, "the same user message never injects twice")

  // ast-grep routing is gated on the request's tool map: absent tools stay
  // silent, exposed tools get one routing line.
  assert.doesNotMatch(injected, /ast[-_]grep/, "no ast-grep mention without ast-grep tools")
  const withAstGrep = userMessage("s1-ast", "ag1", "Migrate the call sites")
  withAstGrep.tools.ast_grep_search = {}
  withAstGrep.tools.ast_grep_edit = {}
  await one.app.hooks.context[0](withAstGrep)
  const astGrepGuidance = withAstGrep.system.map((p: Loose) => p.text).join("\n")
  assert.match(astGrepGuidance, /ast_grep_search/, "guidance names ast-grep tools when the host exposes them")
  assert.match(astGrepGuidance, /ast_grep_edit/, "guidance names the rewrite tool too")

  // Zero-hit rescue turns an empty grep into codesearch hits.
  const rescueEvent: Loose = {
    tool: "grep",
    status: "completed",
    sessionID: "s1",
    callID: "c1",
    input: { pattern: "retryUpload" },
    result: { output: "" },
  }
  await one.app.toolHooks["execute.after"][0](rescueEvent)
  assert.match(String(rescueEvent.result.output), /codesearch/, "rescue appends a codesearch section")
  assert.match(String(rescueEvent.result.output), /uploader\.ts/, "rescue includes index hits")

  // A successful grep gets a one-time nudge per session.
  const nudgeEvent: Loose = {
    tool: "grep",
    status: "completed",
    sessionID: "s1",
    callID: "c2",
    input: { pattern: "queue" },
    result: { output: "src/queue.ts:1:const queue = []", metadata: { count: 1 } },
  }
  await one.app.toolHooks["execute.after"][0](nudgeEvent)
  assert.match(String(nudgeEvent.result.output), /tip:/, "first successful grep is nudged")
  const nudgeAgain: Loose = { ...nudgeEvent, callID: "c3", result: { output: "hit", metadata: { count: 1 } } }
  await one.app.toolHooks["execute.after"][0](nudgeAgain)
  assert.doesNotMatch(String(nudgeAgain.result.output), /tip:/, "the nudge fires once per session")

  // Compaction assist: recently edited files become explore outlines, and the
  // target sent to the server must be project-relative. Regression guard: an
  // alias-prefix strip left absolute targets that could never match on
  // Windows, where the plugin lowercases paths but the server strips them
  // case-sensitively — a silent compaction no-op.
  one.app.emit({ type: "file.edited", data: { file: "/work/my-service/src/uploader.ts" } })
  await new Promise((resolve) => setTimeout(resolve, 150))
  const compactionEvent: Loose = { system: [] }
  await one.app.hooks.compaction[0](compactionEvent)
  const compacted = compactionEvent.system.map((p: Loose) => p.text).join("\n")
  assert.match(compacted, /src\/uploader\.ts/, "compaction outline names the relative file")
  const exploreCall = mock.calls.filter((c) => c.name === "explore").at(-1)
  assert.ok(exploreCall, "compaction asked the server for an outline")
  assert.equal(
    exploreCall!.args.target,
    "src/uploader.ts",
    "compaction passes the project-relative target, not an absolute/alias-prefixed path",
  )

  // Scope tool reports the resolved project and index state.
  const scopeResult = (await one.app.tools[0].execute({}, {})) as { content: string }
  const scopeInfo = JSON.parse(scopeResult.content)
  assert.equal(scopeInfo.project, "my-service")
  assert.equal(scopeInfo.indexed, true)
  assert.equal(scopeInfo.chunks, 120)

  // Commands resolve and deliver results through session.prompt.
  const searchCommand = one.app.commands.find((c) => c.name === "codesearch")
  assert.ok(searchCommand)
  await searchCommand.execute({ sessionID: "s1", prompt: { text: "upload retry" } })
  assert.match(one.app.prompts.at(-1) ?? "", /uploader\.ts/, "/codesearch injects hits")

  const statusCommand = one.app.commands.find((c) => c.name === "codesearch-status")
  assert.ok(statusCommand)
  await statusCommand.execute({ sessionID: "s1", prompt: { text: "" } })
  assert.match(one.app.prompts.at(-1) ?? "", /project: my-service/, "/codesearch-status reports scope")

  await one.cleanup()

  /* ---------------- prune mode ---------------- */

  process.env.CODESEARCH_PLUGIN_GUARDS = "prune"
  const pruned = await setupPlugin("/work/my-service")
  // Prune only engages once the startup health probe has confirmed the hub is
  // reachable; poll with fresh sessions instead of racing the probe (the
  // guidance is injected once per session, so each attempt needs its own).
  let prunedEvent: Loose | undefined
  for (let i = 0; i < 30 && !prunedEvent; i++) {
    const candidate = userMessage(`s2-${i}`, `p${i}`, "How does the uploader work?")
    await pruned.app.hooks.context[0](candidate)
    if (candidate.tools.grep === undefined) prunedEvent = candidate
    else await new Promise((r) => setTimeout(r, 100))
  }
  assert.ok(prunedEvent, "prune engages once the hub is reachable")
  assert.equal(prunedEvent!.tools.grep, undefined, "prune removes grep")
  assert.equal(prunedEvent!.tools.glob, undefined, "prune removes glob")
  assert.ok(prunedEvent!.tools.read, "prune keeps unrelated tools")
  assert.match(prunedEvent!.system.map((p: Loose) => p.text).join("\n"), /intentionally unavailable/)
  await pruned.cleanup()

  /* ---------------- block mode (fail-open everywhere else) ---------------- */

  process.env.CODESEARCH_PLUGIN_GUARDS = "block"
  const blocked = await setupPlugin("/work/my-service")
  const before = blocked.app.toolHooks["execute.before"][0]
  let threw = ""
  for (let i = 0; i < 30 && !threw; i++) {
    try {
      await before({ tool: "grep", input: { pattern: "x" }, sessionID: "s3" })
    } catch (err) {
      threw = String(err)
    }
    if (!threw) await new Promise((r) => setTimeout(r, 100))
  }
  assert.match(threw, /codesearch/, "block mode denies grep once the hub is reachable")
  assert.match(threw, /project "my-service"/, "denial names the project scope to use")
  assert.doesNotMatch(threw, /ast[-_]grep/, "denial omits ast-grep when the session never exposed it")

  // The same denial names ast-grep once the session's tool map has it.
  const astBlocked = await setupPlugin("/work/my-service")
  const astContext = userMessage("s6", "agb1", "Migrate the call sites")
  astContext.tools.ast_grep_search = {}
  astContext.tools.ast_grep_edit = {}
  await astBlocked.app.hooks.context[0](astContext)
  let astThrew = ""
  for (let i = 0; i < 30 && !astThrew; i++) {
    try {
      await astBlocked.app.toolHooks["execute.before"][0]({ tool: "grep", input: { pattern: "x" }, sessionID: "s6" })
    } catch (err) {
      astThrew = String(err)
    }
    if (!astThrew) await new Promise((r) => setTimeout(r, 100))
  }
  assert.match(astThrew, /ast_grep_search/, "denial names ast-grep tools when the host exposes them")
  await astBlocked.cleanup()

  const uncovered = await setupPlugin("/work/unregistered")
  await uncovered.app.toolHooks["execute.before"][0]({ tool: "grep", input: {}, sessionID: "s4" })
  await uncovered.cleanup()
  await blocked.cleanup()

  /* ---------------- prune fails open while the hub is down ---------------- */

  process.env.CODESEARCH_PLUGIN_GUARDS = "prune"
  process.env.CODESEARCH_URL = "http://127.0.0.1:1/mcp"
  const downPrune = await setupPlugin("/work/my-service")
  // Let the startup probe fail (connection refused) so health settles on down.
  await new Promise((resolve) => setTimeout(resolve, 300))
  const downEvent = userMessage("s5", "d1", "Where is the uploader?")
  await downPrune.app.hooks.context[0](downEvent)
  assert.ok(downEvent.tools.grep, "prune keeps grep visible while the hub is down")
  assert.ok(downEvent.tools.glob, "prune keeps glob visible while the hub is down")
  assert.match(
    downEvent.system.map((p: Loose) => p.text).join("\n"),
    /unreachable|acceptable fallback/,
    "guidance names the grep/glob fallback while the hub is down",
  )
  await downPrune.cleanup()
  process.env.CODESEARCH_URL = mock.url

  delete process.env.CODESEARCH_PLUGIN_GUARDS
  // Give any in-flight startup probe a moment before the mock hub disappears.
  await new Promise((resolve) => setTimeout(resolve, 200))
  await mock.close()
  console.log("smoke: OK")
}

await main()
