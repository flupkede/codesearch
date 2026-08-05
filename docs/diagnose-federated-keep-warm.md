# Diagnosis — local serve silently keeping a federated cloud peer warm

_Branch: `fix/federated-silent-poll-diagnosis` — 2026-08-05_

## Symptom reported

A user's own, separately-launched local `codesearch serve` instance kept
sending periodic requests to a mounted cloud federation peer (an Azure
Container Apps replica), defeating that replica's scale-to-zero. Quitting
the local instance stopped the requests; restarting (including with
`--no-tui`) reproduced them. The user insisted — correctly, as it turned
out — that **nothing appeared in the logs for these requests**, and that
they were seeing a request roughly **every 2 minutes**.

## What was ruled out

1. **TUI federated `/status` polling** (`spawn_remote_discovery`,
   `src/serve/tui.rs`) — traced the full call chain
   (`spawn_remote_discovery` → `run_tui_loop` → `run_tui` →
   `maybe_spawn_tui` → `run_serve`). `maybe_spawn_tui` is only invoked when
   `!no_tui`, and it further gates on `is_tty()`. With `--no-tui`, this
   entire path never spawns — confirmed against current `develop`, which
   already carries the earlier "TUI federated /status polling now respects
   scale-to-zero" and "TUI no longer pokes federated peers on startup"
   fixes (see root `AGENTS.md`).
2. **Explicit federated tool calls** (`federated_search`,
   `federated_project_search`, `federated_get_chunk` in `src/mcp/mod.rs`) —
   these are the only callers of `record_remote_peer_activity`, and only
   fire when a project genuinely resolves to a federated/remote alias. Not
   the cause here — no federation-shaped log lines existed anywhere in a
   full day's log for either the reporting user's instance or an unrelated
   local hub instance used to cross-check.
3. **A stale binary re-introducing an old bug** — the user's reported
   startup banner was `v1.2.1`, predating the #189 LMDB mapsize fix
   (confirmed by seeing the expected `MDB_MAP_FULL` warning during that
   session's cache warmup) — worth upgrading regardless, but not the cause
   of the 2-minute cadence, since the TUI federated-poll gating traced above
   already existed at that version too.

## Root cause — `keep_warm_url`, not federation at all

`src/serve/mod.rs`'s cloud keep-warm task (`run_serve`, guarded by
`if let Some(base_url) = keep_warm_url`) is a `tokio::spawn`'d loop that,
once enabled, self-pings `{base_url}{HEALTHZ_PATH}` every
`KEEP_WARM_INTERVAL_SECS` (**120s — exactly the reported "every 2
minutes"**) while the most recent real tool call is younger than
`idle_suspend_secs` (default 2 hours). It exists so a scale-to-zero cloud
replica (Azure Container Apps) can keep **itself** warm by generating its
own ingress traffic.

Three properties of this task combine into the exact symptom reported:

1. **Not gated by `--no-tui` at all** — it is gated purely on
   `keep_warm_url` being non-empty (via `--keep-warm-url` CLI flag or the
   `CODESEARCH_KEEP_WARM_URL` env var). `--no-tui` has zero effect on it.
2. **Nothing restricts the target to "self"** — `keep_warm_url` is a bare
   string, validated only for non-emptiness. Nothing stops it from pointing
   at a *different* host — including another federation peer's URL.
3. **Every individual ping was completely silent** — before this fix, the
   ping body was `let _ = client.get(&ping_url)...send().await;`, discarding
   both success and failure with zero log line. The only log evidence was a
   single one-time `info!("🔥 keep-warm enabled: ...")` at task spawn —
   easy to miss, and the only trace this feature ever left behind.

**The failure mode:** if `CODESEARCH_KEEP_WARM_URL` ends up set in a local
shell profile or `.env` — e.g. copy-pasted from a cloud deployment's
environment while testing or configuring the federation mount — to the
**cloud peer's own URL**, a local `codesearch serve` process will silently
ping that cloud peer's `/healthz` every 2 minutes, indefinitely (as long as
the local process sees *any* real tool-call activity within the idle
window — which for an interactively-used local MCP hub is essentially
always). This is indistinguishable, from the outside, from the cloud
replica being kept warm by legitimate federated traffic — except that it
generates zero local log output, which is exactly why the user could not
find it in the logs.

## Fix shipped on this branch

`src/serve/mod.rs`:

1. **Per-ping logging** — success now logs at `debug!` (quiet by default,
   traceable with `RUST_LOG=debug`), failure always logs at `warn!`. No
   longer silent.
2. **Startup misconfiguration warning** — a new `extract_host_from_url`
   helper (no new dependency; the `url` crate is only transitive via
   `reqwest`, not a direct dependency) extracts the keep-warm target's
   host and compares it against this server's own effective bind host. If
   they don't match (and the target isn't `localhost` / `127.0.0.1` /
   `::1`), a loud `warn!` fires at startup naming both hosts and
   explicitly calling out that keep-warm exists to self-ping THIS replica,
   not another peer.

Tests added (`src/serve/tests.rs::keep_warm_host_extraction_tests`): host
extraction from plain `http`, `https` with a real hostname, no-scheme
input, IPv6 literal (bracket-preserving), query-string/fragment stripping,
and the empty-host edge case.

## Action for the user

Check whichever shell profile / `.env` launched the affected local
`codesearch serve` process for `CODESEARCH_KEEP_WARM_URL` (or a
`--keep-warm-url` flag baked into an alias/script). If it is set and
points at the cloud peer's URL, that is the root cause — remove it (or
point it at the local process's own ingress if self-keep-warm was
genuinely intended, which is unusual for a local dev instance). After
upgrading past this fix, the next occurrence would either be silent no
more (thanks to the new per-ping log lines) or caught immediately by the
new startup warning.
