# Worklog — federated peer woken with no query behind it

| | |
|---|---|
| **Branch** | `fix/federated-silent-poll-diagnosis` |
| **Base SHA** | `55fa36b` (🐛 fix: log keep-warm pings + warn when target isn't self) |
| **Scope** | Stop a scale-to-zero federated cloud peer being woken, and kept warm, with no federated query behind it. Local repo polling must stay untouched. |
| **Status** | Complete — 3 commits, all reviewed and passed. Not pushed. |
| **Latest test result** | `cargo test --lib --bins` → **1134 passed, 42 ignored**; `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean |

## The requirement

Background polling of **local** repos is fine. Background polling of a
**federated peer** must never happen — an explicit design constraint from the
original federation design, restated by the user as:

> "hij mag die repos pollen LOKAAL maar niet federated !!! dat had ik nochthans
> in de specs effectief gezegd bij het ontwerp"

A peer staying warm for an hour *after real use* is **correct** and was
explicitly confirmed as such. The defect was the peer being **woken** with no
query, and then staying warm off that spurious wake.

## Stage 0 — diagnosis (no commit)

The pre-existing `DIAGNOSE_FEDERATED_KEEP_WARM.md` blamed a misconfigured local
`CODESEARCH_KEEP_WARM_URL`. **Disproven** on four independent grounds: the env
var is set nowhere locally (process env, HKCU, HKLM, every shell profile); the
one-time `🔥 keep-warm enabled` line appears in zero logs from 2026-04-26 on;
that absence is meaningful because `init_serve_logger` is always file-only in
serve mode and those logs do carry other INFO lines; and no local process held
a `:443` connection. `Watch-CodesearchServeReplicas.ps1` was also eliminated
(polls `/status` every 20s, but last ran 2026-07-05).

Azure Log Analytics ground truth, over a window with **zero** federated
searches: wakes **120 / 121 / 120 minutes** apart, each warm period **~67 min**,
nightly sleeps exactly 2h00m30s apart → **≈13.4h warm/day, ~56% duty cycle**.
The 120-minute spacing is the tell: it is the **local** host's 2h default, not
any value configured on the peer.

Two defects, one triggering and one amplifying:

1. **Trigger** — the TUI's `spawn_remote_discovery` used
   `state.idle_suspend_secs()` as a baseline poll interval and ran a `JoinSet`
   `/status` fan-out to every peer. The poll *itself* was the ingress traffic.
2. **Amplifier** — keep-warm's `most_recent_tool_call().unwrap_or(start)`.
   `/status` and `/healthz` never call `record_tool_call`, so any non-tool-call
   wake self-warmed for the full idle window. Unreachable in the case it was
   written for, so its only practical effect was rewarding spurious wakes
   (~11×).

## Stage 1 — remove the federated timer poll

- **Commit:** `6f1d1c5` · **Review:** ⚠️ PASS WITH REMARKS (round 1) →
  fixes amended → **PASS, zero code defects** (round 2, cap reached).
- `spawn_remote_discovery` no longer polls on any cadence; the tick is
  **config-only** (`REMOTE_ROW_REFRESH_SECS` = 5s, zero HTTP) so mount/unmount
  edits and `l` reloads still surface. Contact is activity-poke (single peer,
  never a fan-out) or the `i` keypress only.
- Removed `ServeState::idle_suspend_secs` (field, env init, getter,
  `--idle-suspend-secs` override) — write-only once the cadence went.
  Keep-warm resolves flag > env > default itself, so the flag still works.
- Round-1 fixes amended in: `tui_common.rs` `activity_stale` doc; and the
  snapshot emit, previously gated on a non-empty peer list, which left rows on
  screen forever after the last peer was removed.

**Files:** `src/constants.rs`, `src/serve/mod.rs`, `src/serve/tui.rs`,
`src/serve/tui_common.rs`

## Stage 2 — keep-warm requires a real tool call

- **Commit:** `12edcf2` · **Review:** ✅ **PASS, zero findings.**
- The `unwrap_or(start)` fallback is gone; with no recorded tool call the loop
  does not ping and lets the host suspend the replica.
- Reviewer independently confirmed warm-after-real-use does **not** regress: an
  inbound federated search forces `project=<alias>`, reaching
  `record_tool_call`; and `last_tool_call` is **insert-only** (no
  `remove`/`clear`/`retain`, untouched by repo idle-eviction), so once one real
  query lands the old behaviour holds for the process lifetime.
- Also fixed the `55fa36b` startup warning, which false-positived on the only
  correct deployment: Azure binds `0.0.0.0` while the target is the ingress
  FQDN. Wildcard bind ⇒ external host unknown ⇒ stay silent. Rule extracted to
  the testable `keep_warm_foreign_target` helper (5 new tests).

**Files:** `src/serve/mod.rs`, `src/serve/tests.rs`

## Stage 3 — documentation

- **Commit:** `3bcf153` · **Review:** ✅ **PASS** (final full-branch review,
  `55fa36b...3bcf153`) — "Nothing further is owed on this branch."
- `DIAGNOSE_FEDERATED_KEEP_WARM.md` rewritten (moved from `docs/`): confirmed
  root cause, ground truth, the violated requirement, and the **rejected
  reasoning** recorded so it is not re-litigated a fourth time.
- `AGENTS.md` bullet rewritten as an explicit design constraint; it had
  asserted the removed cadence as current and named a deleted field — the very
  mechanism by which this behaviour was re-introduced twice.
- `CHANGELOG.md`: fix entry under `[1.2.4] (unreleased)`. An earlier draft had
  put it under `[1.2.0]` — a **real tag** that shipped #181/#184; the reviewer
  caught this, and both original entries were restored **verbatim** (confirmed
  by diff) and marked superseded.
- `README.md`: grep-guard bullet corrected to the `/healthz` liveness probe.

**Files:** `AGENTS.md`, `CHANGELOG.md`, `README.md`,
`DIAGNOSE_FEDERATED_KEEP_WARM.md` *(new)*, `docs/diagnose-federated-keep-warm.md` *(deleted)*

## Why this took three attempts across three PRs

PR #181 introduced the cadence on the reasoning that polling no faster than the
host's suspend term is harmless; PR #184 named "waking the scale-to-zero cloud
peer for no real reason" as the defect and then explicitly sanctioned that same
cadence. The flaw: **not keeping a peer awake past its suspend term is strictly
weaker than not waking it**, and the two windows were unrelated values anyway
(local host vs. remote peer). Both PRs correctly said "local repos unaffected",
confirming the local/federated split was real — but honoured it in only one
direction.

## Open follow-ups

- **Not pushed.** Three commits sit locally on `fix/federated-silent-poll-diagnosis`.
  Per project workflow a PR targets `develop`.
- **Residual, known and not currently exploitable:** the MCP `status` **tool**,
  when project-scoped, *does* record a tool call (`allow_unscoped=true` reduces
  the guard to `!is_multi`), so an automated poller of that tool would still buy
  a warm window. No such poller exists — both `Watch-CodesearchServeReplicas.ps1`
  and `FederationClient::list_repos` use the HTTP `/status` endpoint, which does
  not record. First place to look if the symptom recurs.
- **Unverified in production:** the fix is validated by tests and review, not yet
  by observing the deployed peer stay asleep. Worth re-running the Log Analytics
  query after this reaches the cloud replica — expected: no wakes without a
  federated query.

## Security note

None. No auth, network-exposure or data-handling surface changed; the branch
strictly *reduces* outbound traffic.

<!-- sync
trello-card-id: (none — no linked card for this work)
azure-devops: (none — not tracked as a work item)
-->
