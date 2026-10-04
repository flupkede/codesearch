/**
 * Unit tests for the codesearch OpenCode plugin's pure helpers and the health
 * runner. Complements test/smoke.ts, which exercises the plugin end-to-end
 * against a mock OpenCode host and a mock codesearch MCP server.
 *
 * Run (Node >= 23.6 strips types natively):
 *   node test/unit.ts
 * or:
 *   npm run unit
 */

import assert from "node:assert/strict"
import http from "node:http"
import type { AddressInfo } from "node:net"
import { HealthRunner, astGrepToolNames, stripJsonc } from "../codesearch.ts"

type Loose = Record<string, any>

/* ------------------------------------------------------------------ *
 * stripJsonc
 * ------------------------------------------------------------------ */

function testStripJsonc(): void {
  // Plain JSON passes through unchanged and still parses.
  const plain = '{"a":1,"b":[1,2],"c":"x"}'
  assert.equal(stripJsonc(plain), plain, "plain JSON is unchanged")
  assert.deepEqual(JSON.parse(stripJsonc(plain)), JSON.parse(plain))

  // Line comments (leading, trailing, at EOF) are removed.
  assert.deepEqual(JSON.parse(stripJsonc('{\n  // note\n  "a": 1 // tail\n}\n')), { a: 1 })
  assert.deepEqual(JSON.parse(stripJsonc('{"a":1} // done')), { a: 1 }, "comment at EOF without newline")

  // Block comments, including inline and multi-line.
  assert.deepEqual(JSON.parse(stripJsonc('{/* one */"a":/* two\nlines */1}')), { a: 1 })

  // Comment markers inside strings are data, not comments.
  assert.deepEqual(
    JSON.parse(stripJsonc('{"url":"http://example.com/a//b","glob":"/* literal */"}')),
    { url: "http://example.com/a//b", glob: "/* literal */" },
  )

  // Escaped quotes do not terminate the string early.
  assert.deepEqual(JSON.parse(stripJsonc('{"s":"a\\"//not-a-comment"}')), { s: 'a"//not-a-comment' })

  // Trailing commas in objects and arrays are dropped.
  assert.deepEqual(JSON.parse(stripJsonc('{\n "a": [1, 2,],\n "b": {"c": 3,},\n}')), {
    a: [1, 2],
    b: { c: 3 },
  })

  // Regression: a trailing comma may be followed by a comment before the
  // closing bracket ("a": 1, // last member \n }). Comments must be skipped
  // while looking ahead, not mistaken for the next member.
  assert.deepEqual(JSON.parse(stripJsonc('{"a": 1, // last member\n}')), { a: 1 })
  assert.deepEqual(JSON.parse(stripJsonc('{"a": [1, /* c */ ]}')), { a: [1] })

  // A comma followed by a real member is preserved.
  assert.deepEqual(JSON.parse(stripJsonc('{"a":1, "b":2}')), { a: 1, b: 2 })

  // CRLF line comments terminate at the newline, not at a literal \r.
  assert.deepEqual(JSON.parse(stripJsonc('{\r\n  // note\r\n  "a": 1\r\n}\r\n')), { a: 1 })
}

/* ------------------------------------------------------------------ *
 * astGrepToolNames
 * ------------------------------------------------------------------ */

function testAstGrepToolNames(): void {
  assert.deepEqual(astGrepToolNames(undefined), [], "no tool map is silent")
  assert.deepEqual(astGrepToolNames(null), [], "null tool map is silent")
  assert.deepEqual(astGrepToolNames("ast_grep_search"), [], "non-object maps are ignored")
  assert.deepEqual(astGrepToolNames({ grep: {}, glob: {}, read: {} }), [], "non-ast-grep tools are ignored")
  assert.deepEqual(
    astGrepToolNames({ read: {}, ast_grep_search: {}, ast_grep_edit: {}, "ast-grep_search": {} }),
    ["ast_grep_search", "ast_grep_edit", "ast-grep_search"],
    "ast-grep-shaped names are returned in tool-map order",
  )
}

/* ------------------------------------------------------------------ *
 * HealthRunner
 * ------------------------------------------------------------------ */

function healthSettings(overrides: Loose = {}): Loose {
  return {
    health: {
      enabled: true,
      intervalMs: 40,
      initialDelayMs: 10,
      maxBackoffMs: 80,
      reconnect: true,
      autoStart: false,
      startCommand: "",
      ...overrides,
    },
  }
}

async function waitFor(predicate: () => boolean, what: string, timeoutMs = 4000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
  throw new Error(`timed out waiting for ${what}`)
}

/** Minimal /healthz server that counts probes, can stop/restart in place, and
 * can hold responses so a probe can be kept in flight deterministically. */
async function startHealthServer(port = 0) {
  let count = 0
  let hold = false
  let pending: Array<() => void> = []
  const make = () =>
    http.createServer((_req, res) => {
      count += 1
      const respond = () => {
        res.writeHead(200, { "content-type": "application/json" })
        res.end('{"status":"ok"}')
      }
      if (hold) pending.push(respond)
      else respond()
    })
  let server = make()
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve))
  const actual = (server.address() as AddressInfo).port
  return {
    url: `http://127.0.0.1:${actual}/mcp`,
    hits: () => count,
    pendingProbes: () => pending.length,
    hold: (value: boolean) => {
      hold = value
    },
    release: () => {
      const waiting = pending
      pending = []
      for (const respond of waiting) respond()
    },
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
    restart: async () => {
      server = make()
      await new Promise<void>((resolve) => server.listen(actual, "127.0.0.1", resolve))
    },
  }
}

async function testHealthRunner(): Promise<void> {
  const hub = await startHealthServer()
  const logs: string[] = []
  const warnings: string[] = []
  const log = (...args: unknown[]) => logs.push(args.join(" "))
  const warn = (...args: unknown[]) => warnings.push(args.join(" "))

  // probe() reflects reachability in state and lastError.
  {
    const runner = new HealthRunner({} as any, hub.url, healthSettings() as any, () => {}, log, warn)
    await runner.probe()
    assert.equal(runner.state, "ok", "probe marks a reachable hub ok")
    assert.equal(runner.lastError, "", "ok state clears lastError")
    await hub.close()
    await runner.probe()
    assert.equal(runner.state, "down", "probe marks an unreachable hub down")
    assert.ok(runner.lastError.length > 0, "down state records the error")
    assert.ok(
      warnings.some((line) => /hub unreachable/.test(line)),
      "the first down transition is warned about",
    )
    runner.stop()
  }

  // start() probes after the initial delay, backs off while down, asks the
  // host to reload failed MCP servers, and reports recovery exactly once.
  {
    await hub.restart()
    let reloads = 0
    let recovered = 0
    const ctx = {
      mcp: {
        reload: async () => {
          reloads += 1
        },
      },
    }
    const runner = new HealthRunner(
      ctx as any,
      hub.url,
      healthSettings() as any,
      () => {
        recovered += 1
      },
      log,
      warn,
    )
    runner.start()
    await waitFor(() => runner.state === "ok", "the initial probe to mark the hub ok")
    await hub.close()
    await waitFor(() => runner.state === "down", "the hub to be detected down")
    await waitFor(() => reloads >= 1, "the reconnect reload call")
    await hub.restart()
    await waitFor(() => runner.state === "ok", "the hub to be detected recovered")
    await waitFor(() => recovered === 1, "the recovery callback")
    assert.ok(
      logs.some((line) => /hub recovered/.test(line)),
      "recovery is logged",
    )
    // stop() must latch: even a probe that is already in flight must not
    // reschedule another one afterwards.
    hub.hold(true)
    await waitFor(() => hub.pendingProbes() >= 1, "an in-flight probe to hold")
    const hitsAtStop = hub.hits()
    runner.stop()
    hub.release()
    await new Promise((resolve) => setTimeout(resolve, 250))
    assert.equal(hub.hits(), hitsAtStop, "stop() halts probing, including from an in-flight tick")
    hub.hold(false)
    hub.release()
  }

  // start() is a no-op when health probing is disabled.
  {
    const hits = hub.hits()
    const runner = new HealthRunner(
      {} as any,
      hub.url,
      healthSettings({ enabled: false }) as any,
      () => {},
      log,
      warn,
    )
    runner.start()
    await new Promise((resolve) => setTimeout(resolve, 120))
    assert.equal(hub.hits(), hits, "disabled health never probes")
    assert.equal(runner.state, "unknown", "state stays unknown while disabled")
    runner.stop()
  }

  await hub.close()
}

async function main(): Promise<void> {
  testStripJsonc()
  testAstGrepToolNames()
  await testHealthRunner()
  console.log("unit: OK")
}

await main()
