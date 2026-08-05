# Worklog — federated peer woken with no query behind it

| | |
|---|---|
| **Branch** | `fix/federated-silent-poll-diagnosis` |
| **Base SHA** | `55fa36b` (🐛 fix: log keep-warm pings + warn when target isn't self) |
| **Scope** | Stop a scale-to-zero federated cloud peer being woken, and kept warm, with no federated query behind it. Local repo polling must stay untouched. |
| **Status** | **Shipped and verified in production.** Merged to `develop` via PR #192 (`b6cb48f`); deployed to the cloud peer as revision `codesearch-serve--0000024`. |
| **Latest test result** | CI green on PR #192 (test-linux, test-windows, csharp-integration-tests, CodeQL, Analyze). Locally: `cargo test --lib --bins` → **1134 passed, 42 ignored**; `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean |

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

## Stage 4 — the branch had no CI at all

- **Commit:** `4add0d1` · **Review:** covered by the final full-branch review.
- While preparing the PR it turned out `ci.yml`'s push trigger is a **prefix
  allowlist** that did not include `fix/**`. Every `fix/...` branch — the
  repo's own documented naming convention — had therefore merged into
  `develop` without ever running fmt, clippy or a single test. The PR still
  showed green because CodeQL is a separate `pull_request`-triggered workflow
  and was the only check present.
- Added `fix/**`, plus a comment explaining the footgun and how to verify
  (`gh pr checks <n>` must list the CI jobs, not just CodeQL).
- Proven by self-test: `08276de` → CodeQL only; `4add0d1` → CI + CodeQL.
- `chore/**` was added later, in PR #193.

**Files:** `.github/workflows/ci.yml`

## Stage 5 — deployment and production verification

- **Merged:** PR #192 → `develop` (`b6cb48f`), auto-bumped to 1.2.5.
- **Local instance:** deployed by the user via `copy-to-common`. This carries
  Defect 1 (the TUI timer poll), which only ever ran on the *local* side — so
  the trigger was removed first.
- **Cloud peer:** image `codesearch-serve:8d7261e6d` built from a clean
  `git archive` of `develop` and deployed as revision
  `codesearch-serve--0000024`. Config verified intact across the update: 12
  env vars, 4 secretRefs, `CODESEARCH_IDLE_SUSPEND_SECS=1800`. `/healthz`
  returned 200 in 147 ms. Registry size 160,647,888 B vs 160,629,714 B for the
  previous image — an 18 KB difference, so no size regression.
- **Idle window** was separately reduced 3600 → 1800 s at the user's request,
  halving the cost of any wake that does still occur.
- **Verified:** the replica scaled to 0 about 10 minutes after deploy and
  **stayed at 0 across six consecutive one-minute checks with no traffic**.
  This is positive evidence rather than mere absence of symptoms: under the old
  binary `most_recent_tool_call()` would have been `None`, fallen back to the
  process start time, and self-pinged every 120 s for the full 30-minute idle
  window — reaching 0 at 10 minutes was not possible.

**Build note:** two `az acr build` runs failed at the identical step
(`COPY --from=builder /models.tar.gz`) with
`failed to export image: ... layer does not exist`. Layer digests differed
between runs, so it was not a poisoned cache; the Dockerfile is byte-identical
to the one that built the previously deployed image. Root cause sits in the ACR
Tasks build agent (registry is Basic SKU), not in this repo. A local
`docker build` + `docker push` succeeded first time and was used instead.

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

- **Residual, known and not currently exploitable:** the MCP `status` **tool**,
  when project-scoped, *does* record a tool call (`allow_unscoped=true` reduces
  the guard to `!is_multi`), so an automated poller of that tool would still buy
  a warm window. No such poller exists — both `Watch-CodesearchServeReplicas.ps1`
  and `FederationClient::list_repos` use the HTTP `/status` endpoint, which does
  not record. First place to look if the symptom recurs.
- **Verified in production** (see Stage 5) over a ~6-minute window. Worth
  re-running the original Log Analytics query over a **full day** to confirm the
  duty cycle: was ≈13.4 h warm/day (~56%) at zero searches, expected now ≈0 with
  wakes only behind real federated queries. The short window proves the
  self-ping is gone; only a 24 h sample proves nothing else wakes it.
- **ACR Tasks cannot currently build this image** (Stage 5 build note). The
  local `docker build` path works, but a CI/automated deploy would hit the same
  failure. Worth a look before anyone automates the cloud deploy.

## Security note

None. No auth, network-exposure or data-handling surface changed; the branch
strictly *reduces* outbound traffic.

<!-- sync
trello-card-id: (none — no linked card for this work)
azure-devops: (none — not tracked as a work item)
-->
