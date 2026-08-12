# Diagnosis — a federated cloud peer waking up with nobody querying it

_Branch: `fix/federated-silent-poll-diagnosis` — 2026-08-05_

> **Status: root cause CONFIRMED against Azure Log Analytics ground truth.**
> An earlier revision of this document blamed a misconfigured local
> `CODESEARCH_KEEP_WARM_URL`. That hypothesis was **disproven** — see
> [What was ruled out](#what-was-ruled-out) §4. The confirmed cause is a
> two-part defect described in [Root cause](#root-cause). The corresponding
> fixes are listed in [Fixes](#fixes).

## The requirement being violated

Background polling of **local** repos is fine and expected. Background
polling of a **federated peer** must never happen — it was an explicit
design constraint from the original federation design, restated by the
reporting user as:

> "hij mag die repos pollen LOKAAL maar niet federated !!! dat had ik
> nochthans in de specs effectief gezegd bij het ontwerp"

Two things follow, and conflating them is what caused three round-trips on
this same behaviour:

- A peer staying warm for its full idle window **after real use** is
  *correct*. That is what keep-warm is for.
- A peer being **woken** with no federated query behind it is the defect —
  as is it then staying warm for an hour off that spurious wake.

"Cannot keep a peer awake past the host's own suspend term" is a strictly
**weaker** property than "never wakes it", and only the latter was ever the
requirement.

## Symptom reported

A local `codesearch serve` instance kept a mounted cloud federation peer (an
Azure Container Apps replica, `minReplicas: 0`) alive. Quitting the local
instance stopped it. The peer would wake, stay up ~1 hour, sleep, and wake
again — with no federated searches performed in between. Nothing appeared in
the local logs for any of it.

## Ground truth

From Log Analytics (`ContainerAppSystemLogs_CL` / `ContainerAppConsoleLogs_CL`)
on the deployed peer, over a period with **zero** federated searches:

| Observation | Value |
|---|---|
| Interval between wakes | **120, 121, 120 minutes** |
| Warm period per wake | **~67 min** (1h idle window + 5min KEDA `cooldownPeriod`) |
| Nightly sleeps | exactly **2h00m30s** apart |
| Resulting duty cycle | **≈13.4h warm/day, ~56%** — at zero searches |

The 120-minute spacing is the tell: it is the **local** host's
`DEFAULT_IDLE_SUSPEND_SECS` (2h), not any value configured on the peer.

Each wake additionally paid for an `azcopy sync` of the docs blob and a
`git pull` of the KB repo.

## Root cause

Two independent defects, one triggering and one amplifying.

### Defect 1 — the trigger: the TUI polled federated peers on a timer

`spawn_remote_discovery` in `src/serve/tui.rs` used
`Duration::from_secs(state.idle_suspend_secs())` as a baseline poll interval
and, on each elapse, ran a `JoinSet` `/status` fan-out to **every**
configured peer. On the local host that value is 2h — matching the observed
cadence exactly.

Each fan-out woke the peer's scale-to-zero replica. Nothing else was needed:
the poll *itself* was the ingress traffic.

The reasoning that shipped this — recorded here so it is not reintroduced a
fourth time — was that polling no faster than the host's own suspend term is
harmless. It is not, for two separate reasons:

1. Not keeping a peer awake *past* its suspend term says nothing about not
   *waking* it. The peer's warm time is bounded, but its wake **count** is
   not zero, and each wake costs a full warm window.
2. The two windows are unrelated values. `idle_suspend_secs` was read from
   the **local** process (2h default); the window the woken peer then
   honoured was the **peer's** (~1h). PR #181's description claimed the
   cadence was "1h on the cloud deploy" — it was reading the local value.

### Defect 2 — the amplifier: keep-warm rewarded spurious wakes

The cloud keep-warm loop in `src/serve/mod.rs` computed its idle check as:

```rust
let last = kw_state.most_recent_tool_call().unwrap_or(start);
```

`/status` and `/healthz` do **not** call `record_tool_call`. So a replica
woken by anything other than a genuine tool call found no recorded tool
call, fell back to the process start time, and self-pinged its own ingress
every `KEEP_WARM_INTERVAL_SECS` (120s) for the entire idle window.

The critical observation is that this fallback is **unreachable in the case
it was written for**: a real tool call always sets `last_tool_call`, so the
`unwrap_or` only ever fires when the wake was *not* real work. Its whole
practical effect was to convert a momentary spurious wake into a full warm
hour — roughly **11× amplification** (~67 min instead of the ~6 min a bare
wake would have cost).

### How they combine

Defect 1 wakes the peer every 2h. Defect 2 then holds it up for ~67 min per
wake. Neither alone produces the observed 56% duty cycle; together they do.

## What was ruled out

1. **Explicit federated tool calls** (`federated_search`,
   `federated_project_search`, `federated_get_chunk` in `src/mcp/mod.rs`) —
   the only callers of `record_remote_peer_activity`, and only reached when a
   project resolves to a federated alias. No federation-shaped log lines
   existed in a full day's logs for either the reporting instance or an
   unrelated local hub used to cross-check.
2. **`Watch-CodesearchServeReplicas.ps1`** — does poll `/status` every 20s,
   but last ran 2026-07-05, well before the observed window.
3. **A stale binary re-introducing an old bug** — the reported startup banner
   was `v1.2.1`. Worth upgrading, but the 2h cadence exists in that version
   too.
4. **A misconfigured local `CODESEARCH_KEEP_WARM_URL`** *(the earlier
   revision's stated root cause — disproven)*, on four independent grounds:
   - The env var is set **nowhere** locally: not in the process environment,
     not in `HKCU`, not in `HKLM`, not in any shell profile.
   - The one-time `🔥 keep-warm enabled` line appears in **zero** local logs
     from 2026-04-26 onward.
   - That absence is meaningful: `init_serve_logger` is *always* file-only in
     serve mode, unconditional on `--no-tui`, and those logs do carry other
     `INFO` lines — so the line would have been captured had it fired.
   - No local `codesearch` process held any connection on `:443`.

Note that the earlier revision also ruled out TUI federated polling, on the
grounds that `maybe_spawn_tui` is gated on `!no_tui && is_tty()`. That gating
is real, but the conclusion was wrong: the reporting user's *waking* instance
was a normal TTY serve with the TUI running. Only the separate `--no-tui`
cross-check instance was exempt.

## Fixes

### Shipped earlier on this branch (commit `55fa36b`)

Keep-warm observability, in `src/serve/mod.rs`:

1. **Per-ping logging** — success at `debug!`, failure at `warn!`. Previously
   `let _ = client.get(&ping_url)...send().await;` discarded both, leaving a
   single one-time "enabled" line as the feature's only trace.
2. **Startup misconfiguration warning** — `extract_host_from_url` (no new
   dependency) compares the keep-warm target host against the server's own
   bind host and warns when they differ.

Tests: `src/serve/tests.rs::keep_warm_host_extraction_tests`.

### Defect 1 — no timer poll of federated peers

`spawn_remote_discovery` no longer polls on any cadence. The periodic tick is
**config-only** (`REMOTE_ROW_REFRESH_SECS` = 5s, zero HTTP): it rebuilds
mounted-remote rows from the `remote_mounts` allowlist so mount/unmount edits
and `l` reloads surface promptly, and contacts nobody.

A peer is contacted only by:

- an **activity poke** — a real federated tool call just landed on that peer,
  so it is demonstrably already awake; only that peer is refreshed, never a
  fan-out, so an idle sibling peer is untouched;
- the explicit **`i`** info-overlay keypress on a remote row.

Consequences: an idle mount renders its activity as `-`, which is now the
correct steady state rather than a fault. `ServeState::idle_suspend_secs`
(field, env init, getter and `--idle-suspend-secs` override) is removed — it
existed only to feed the poll cadence and became write-only. The keep-warm
task resolves flag > env > default directly, so `--idle-suspend-secs` is
unchanged. The `initial_cycle` startup gate is gone: every cycle is now
config-only, so it had nothing left to gate.

Also fixed in passing: the snapshot emit was gated on a non-empty peer list,
so removing the *last* peer from `repos.json` left its rows on screen
forever. It is now unconditional.

### Defect 2 — keep-warm requires a real tool call

The `unwrap_or(start)` fallback is removed: with no tool call recorded there
is nothing to keep warm for, so the loop simply does not ping. A freshly
deployed replica now sleeps until first real use instead of self-warming for
an hour, which is the intended behaviour of scale-to-zero.

### Follow-up — the `55fa36b` warning false-positived on the correct deploy

The startup "target isn't self" warning fired on the **only deployment where
keep-warm is correct**: on Azure the process binds `0.0.0.0` while
`keep_warm_url` is the ingress FQDN, so `looks_like_self` was false and the
warning fired on every cold start. A wildcard bind means the
externally-visible host is genuinely unknown, so the comparison cannot
conclude anything and must stay silent — a check that cries wolf on the
correct configuration trains operators to ignore the case that matters.

Fixed alongside Defect 2. The rule now lives in a testable
`keep_warm_foreign_target(ping_url, self_host) -> Option<String>` helper
(`None` = do not warn), covered by tests for wildcard binds, a genuine
foreign host, a matching host, loopback targets, and an unparseable URL.

## Residual surface (known, not currently exploitable)

The MCP **`status` tool** passes `allow_unscoped = true`, but when it is
*project-scoped* (or the replica is single-repo) `is_multi` is false, so the
`!allow_unscoped || !is_multi` guard lets it through and it **does** record a
tool call. An automated poller calling the MCP `status` *tool* with
`project=<alias>` would therefore still buy a full warm window.

No such poller is known to exist: both `Watch-CodesearchServeReplicas.ps1`
and `FederationClient::list_repos` use the **HTTP** `/status` endpoint
(`status_handler`), which does not record. Noted here so that if the
symptom ever recurs, this is the first place to look.

## Local repos

Unaffected by all of the above, by design.
