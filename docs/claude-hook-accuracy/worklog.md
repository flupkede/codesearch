# Worklog — Claude Code hooks: make the promises match the enforcement

| | |
|---|---|
| **Branch** | `chore/claude-hook-glob-and-docs` |
| **Base SHA** | `db3d3cf` (Merge PR #194: worklog — merge, deploy and production verification) |
| **Scope** | The Claude Code integration tells agents more than it actually enforces. Bring the injected subagent preamble, the integration README and the hook scripts in line with what the code really does — and add regression guards where the drift is mechanical. |
| **Status** | All four stages implemented and reviewed — every stage PASS, no Critical at any point. Branch-wide final review: ⚠️ PASS WITH REMARKS (3 Important) → all three fixed (see "Final branch review" below). The branch was then **squashed to a single commit** before pushing; see "Why there are no per-stage SHAs" below. |
| **Latest test result** | `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean; `cargo test --lib` 570 passed / 21 ignored, `--lib claude_hooks` 9 passed. Both preamble twins byte-identical across four scope cases and three fail-open cases, compared through a single parser with positive controls. |

## The requirement

This work has **no plan file and no tracker card** — it came out of a live
session question. Recording it here so reviews have a fetchable basis instead
of trusting criteria pasted into a prompt (round 1 of the stage-1 review was
capped at ⚠️ on exactly that ground).

The user asked whether gating `Glob` makes any sense at all:

> "heeft glob hoegenaamd zin ?"

and then, on the multi-repo group config:

> "wordt dit nu gerespecteerd door subagents ?" … "maar niet letterlijk toch ?
> via de groups definitie in kwestie ?"

Standing constraint for the whole branch, from earlier in the same session:

> "doe wat het beste is en KISS ! ik word moe van al dat git gedoe. Veel te
> ingewikkeld voor mij"

so low cognitive load is an acceptance criterion, not a nice-to-have. Prefer
deleting a promise over adding a mechanism.

### Acceptance criteria

1. The injected preamble must not tell agents to treat `Glob` like `Grep`. Only
   `Grep` is gated (by `grep-guard`); there is no Glob guard and deliberately
   won't be.
2. The `.sh` and `.ps1` twins must produce equivalent injected text, verified by
   diffing real output — not by reading them side by side.
3. Hook contracts unchanged: exit 0 with no output for a non-`Agent` tool;
   idempotent on the `[[CODESEARCH-PREAMBLE]]` marker; `updatedInput` carries the
   FULL original `tool_input` (Claude Code replaces rather than merges, so a
   dropped field is silently lost).
4. `integrations/claude-code/README.md` must describe all three shipped hooks
   accurately — including where they *differ*, not just a blanket rule.
5. Documentation claims must be checkable against code, not aspirational.

## Why Glob is not gated (the decision, so it isn't re-litigated)

`Glob` matches **filenames**. codesearch has no file-listing primitive to
replace it: `search`'s `file_glob` is a *filter* on a content search, not an
enumerator. So Glob is not redundant.

It is also the only reliable way to get a **negative** answer. An empty
codesearch result cannot distinguish "not present" from "not indexed" —
`.venv`, `node_modules`, build output and binary assets are never indexed.
Glob answers over the real filesystem. This is the same trap the repo's own
guidance calls out for `find_impact` vs. regex: an empty approximate result is
not proof of absence.

And a guard could not hold anyway. `ls`, `find` and `git ls-files` reach the
same answer through Bash, which is not gated, so the only effect would be
friction on the honest path while the bypass stays open. (The same honest
caveat applies to `grep-guard`: it *steers*, it is not a fence. That is fine —
as long as nobody mistakes it for one.)

Promising more than is enforced had a real cost: it taught subagents to avoid
Glob for exactly the questions it is best at.

## Why there are no per-stage SHAs

Each stage below was committed and reviewed separately, and this file originally
cited the SHA of each. Those commits no longer exist: the first `git push`
attempt was **blocked by the repo's own pre-push leak guard**
(`.githooks/pre-push`), because the worked examples in the stage-3 material used
live client repo aliases read out of `repos.json`. The guard scans the tracked
tree at the tip, but a push publishes every commit, so scrubbing the working
tree alone would have produced a *false* clean — the identifiers would still
have shipped in the eight commits behind it.

The branch was therefore squashed onto `db3d3cf` as one scrubbed commit, and the
aliases replaced with placeholders (`NWND.Acme`, `NWND.CONFIG.ACME`,
`NORTHWIND`, `acme`) chosen to preserve every property the examples turn on.
The stage-by-stage narrative, review verdicts and round counts below are
unchanged and remain the basis of record; only the SHA citations are gone,
because they would now point at nothing. `db3d3cf` is still valid — it is the
base, not part of the rewrite.

## Stage 1 — the preamble and the README

- **Review:** ⚠️ PASS WITH REMARKS (round 1, 0 Critical / 2 Important) → both
  fixed and amended into the same commit → ✅ **PASS** (round 2 of 2).
- Preamble now steers `Grep` only and states positively when `Glob` is correct.
- Fixed a twin divergence: `$(cat <<EOF)` strips the trailing newline, so the
  `---` separator ran into the first line of the original prompt. The PowerShell
  here-string version was already correct. Proven with a positive control
  against `db3d3cf`, not assumed.
- `integrations/claude-code/README.md` documented **two** hooks while **three**
  ship — `web-guard` (`WebSearch|WebFetch`) was missing from the bullet list, the
  manual-install JSON *and* the uninstall step, so following that README left an
  incomplete install and an orphaned registration. Cross-checked against all
  three registrars (`install.ps1`, `install.sh`, `GUARD_HOOKS` in
  `src/cli/claude_hooks.rs`).
- CHANGELOG heading was `[1.2.4]` while `Cargo.toml` on develop already read
  `1.2.6` — the per-merge auto-bump moved the version but not the heading.
  Corrected in place per the file's own convention note; no entries moved.

**Review fixes (round 1), amended in:**
- Caveats still said "**Both** hooks are per-machine" — the exact stale count the
  commit set out to remove, while the CHANGELOG already claimed it was fixed.
- The fail-open paragraph was generalised from `grep-guard` to all three, but
  `web-guard` satisfies **neither** half: it never probes the server (a mount
  that is configured but unreachable still denies the first call — what bounds
  it is the 5-minute retry cache, not a liveness check), and it blocks *only*
  paths outside the current repo, the inverse of `grep-guard`. Now stated per
  hook.

**Files:** `CHANGELOG.md`, `integrations/claude-code/README.md`,
`integrations/claude-code/install.ps1`,
`integrations/claude-code/hooks/subagent-preamble.ps1`,
`integrations/claude-code/hooks/subagent-preamble.sh`

## Stage 2 — ASCII-only hook bodies, and the CRLF class behind them

- **Review:** ⚠️ PASS WITH REMARKS (round 1, 0 Critical / 3 Important) → all
  three fixed and amended → ✅ **PASS** (round 2 of 2, no new findings).
- The six hook scripts carried 49 × U+2014 and 1 × U+2026. Claude Code spawns
  the `.ps1` via `pwsh -File`; with a non-UTF-8 console codepage Windows
  best-fit-maps `—` down to `-`, so the twins emitted **different** text for
  identical input. Replaced with `-` / `...` and pinned by a new test,
  `embedded_hook_bodies_are_ascii_only`, which walks every `GUARD_HOOKS` body —
  exhaustive by construction, since that static *is* the set of `include_str!`
  constants. Verified as a real guard, not a tautology: the pre-fix blobs of all
  six files contain `0xe2 0x80 0x94`, so the test would have failed on each.
- Recorded honestly: the em-dash issue was **pre-existing** at `db3d3cf` and
  caller-side (pwsh-from-pwsh preserves U+2014; only the Git Bash spawn degraded
  it). It was kept *out* of the stage-1 amend rather than smuggled into a review
  fix, and given its own stage and its own review.

**Review fixes (round 1), amended in:**
- **Fixed an instance, not a class.** The CRLF workaround landed only in
  `subagent-preamble.ps1`; `grep-guard.ps1` (1939 B / 31 CR against its twin's
  1908 B / 0) and `web-guard.ps1` still diverged. Root cause was the checkout,
  not the scripts: `include_str!` reads the **working tree**, so `core.autocrlf`
  baked CRLF into the shipped binary too. Fixed at the source with
  `*.ps1 text eol=lf` in `.gitattributes` and the runtime workaround **deleted**
  — one line of config instead of three copies of a fix.
- That deleted workaround was itself a contract violation: the `-replace` ran
  *after* `$prompt` interpolation, so it stripped CRLF from the **caller's own
  prompt**. Now verified byte-for-byte intact.
- A sentence I had just written in the README — "all three hooks fail open on
  unparseable input" — was **false for the `.sh` twins**: under
  `set -euo pipefail` a `jq` parse error aborts with exit 5, surfacing as a hook
  *execution failure*, the opposite of failing open. Fixed the **code**, not the
  doc: all three `.sh` now validate with `jq -e .` before any extraction.
  Confirmed rc=0 / empty stderr on four malformed shapes, against an exit-5
  control at BASE, and confirmed the guard does not swallow valid input.

**Carried, non-blocking:** `subagent-preamble.sh` loses the trailing newline of a
multi-line caller prompt (`$(cat <<EOF)` strips it) and native `jq.exe` doubles
LF→CRLF on Git Bash. Both pre-exist this branch and neither is reachable in
deployment — `hook_command` picks `pwsh` on Windows and `bash` elsewhere. Also
carried: `grep-guard.sh` treats a Windows-style `C:/…` path as internal, same
pre-existing, same unreachable.

**Files:** `.gitattributes` (new rule), `src/cli/claude_hooks.rs` (new test),
all six `integrations/claude-code/hooks/*.{sh,ps1}`,
`integrations/claude-code/install.sh`, `integrations/claude-code/README.md`,
`CHANGELOG.md`

## Stage 3 — inject the current repo's `project=` / `group=`

- **Review:** ⚠️ PASS WITH REMARKS (round 1, 0 Critical / 1 Important) → fixed →
  ✅ **PASS** (round 2 of 2).
- **Files:** `integrations/claude-code/hooks/subagent-preamble.sh`,
  `integrations/claude-code/hooks/subagent-preamble.ps1`,
  `integrations/claude-code/README.md`, `CHANGELOG.md`,
  `docs/claude-hook-accuracy/worklog.md`

A spawned subagent is told the *mechanism* for multi-repo scoping but not the
*value*: it learns which projects and groups exist only by making an unscoped
call, getting `scope_required`, and picking from the error's list.

Two consequences, and the second is the real one:

1. A wasted round-trip on every subagent.
2. Faced with a list and standing in `NWND.ACME`, it picks
   `project="NWND.Acme"`. Nothing tells it that `NWND.CONFIG.ACME` is a
   sibling holding that customer's *configuration*, or that `group="NORTHWIND"`
   unions the two. So "where is field X configured" searches the code repo,
   misses the config repo, and returns empty — and empty reads as "doesn't
   exist". Note the alias is not derivable from the folder name either: path
   `NWND.ACME`, alias `NWND.Acme`.

Design (per the user's steer — reference the definition, don't inline it): read
`~/.codesearch/repos.json` directly (no HTTP, so no auth, timeout or latency on
every Agent spawn), map CWD → alias via `.repos`, collect the group names
containing that alias via `.groups`, and inject **only those two facts**. Do
*not* enumerate group membership — that duplicates config that goes stale the
moment a repo joins a group, and the agent doesn't need it: the union happens
server-side, and `status(kind="projects")` reads the real source on demand.
Fail open (emit the existing generic text) when the file is missing or the CWD
isn't a registered repo.

**Implemented as designed.** CWD → alias is an **exact** match on
`git rev-parse --show-toplevel` against `.repos` (both sides normalised to
forward slashes, lowercased, trailing slash trimmed). Exact rather than prefix
matching on purpose: `repos.json` stores repo *roots* and `--show-toplevel`
returns one, so there is no boundary bug to get wrong (`…/DPS` vs `…/DPS-other`).
Consistent with `grep-guard`, which already derives its repo from the same call.

Group names are sorted **by codepoint in both twins** — jq's `sort` is ordinal,
whereas PowerShell's default `Sort-Object` is culture- and case-insensitive and
would order `("acme","NORTHWIND")` the other way, silently breaking criterion #2.
`[Array]::Sort(…, [StringComparer]::Ordinal)` pins it rather than relying on two
parsers happening to agree.

**Verified** — both twins, one decoder, `updatedInput.prompt` compared as bytes:

| CWD | alias | injected `group=` | sh/ps1 |
|---|---|---|---|
| `codesearch.git` | `codesearch-git` | *(none → "belongs to no group")* | byte-identical |
| `source/repos/NWND.ACME` | `NWND.Acme` | `"NORTHWIND" \| "acme"` | byte-identical |
| `source/repos/NWND.CONFIG.ACME` | `NWND.CONFIG.ACME` | `"NORTHWIND"` | byte-identical |
| `AppData/Local/Temp` (not a repo) | — | *generic fallback text* | byte-identical |

(Aliases here are placeholders — the real run used live client repo names, which
must not be committed. The properties the rows turn on are preserved exactly:
folder case ≠ alias case, a sibling config repo, and two group names whose
ordinal and case-insensitive orderings disagree.)

Row 2 is the case that motivated the stage: folder `NWND.ACME`, alias
`NWND.Acme`, and `NORTHWIND` as the group that unions in the config repo of row 3.

Controls, because a clean result has to be able to be dirty: comparing
`sh@codesearch-git` against `ps1@NWND.ACME` reports **False**, so the
comparator can detect a real difference through this exact pipeline. Fail-open
proven on three separate causes — `$HOME` with no `repos.json`, with a truncated
`repos.json`, and with a valid one not containing this CWD — all three rc=0,
empty stderr, generic text, twins identical. Idempotence re-checked (marker
present → 0 bytes out, both twins). Leak check: the `remotes.cloud.api_key` and
URL from the same file do **not** appear in the injected text, and neither does
the sibling alias `NWND.CONFIG.ACME` — group *definition* referenced, not
inlined, which is the constraint the user set.

**Review fixes (round 1):** ⚠️ PASS WITH REMARKS, 0 Critical / 1 Important —
CONFIRMED with a reproduction, and a real bug my own testing missed because every
case I built used consistent casing.

PowerShell's `-contains` is **case-insensitive**; jq's `index` in the `.sh` twin
and the server's own `HashMap::contains_key` are not. So a group member typed
from the *folder* name — `"NORTHWIND": ["NWND.ACME"]` against alias `NWND.Acme`,
exactly the confusion this stage exists to fix — matched in the `.ps1` and not in
the `.sh`. That is not merely a twin divergence: `ReposConfig::reconcile()`
(`src/db_discovery/repos.rs`) *prunes* the mis-cased member, so the server's
`NORTHWIND` no longer contains this repo at all. The `.ps1` would advertise
`group="NORTHWIND"`, the agent would search a group it isn't in, get nothing, and
conclude the code is absent — the precise silent miss the stage was written to
prevent, reintroduced by the fix for it. One word: `-ccontains`.

Verified on the reviewer's own scenario, with the correctly-cased group kept as a
control against over-correcting: mis-cased `NORTHWIND` now advertised by neither twin,
correctly-cased `acme` still advertised by both, outputs byte-identical,
zero stderr. The original four scope cases and three fail-open cases all still
pass, and the comparator still reports `False` on a genuine difference.

**Deviation from the usual squash-into-the-stage-commit rule, deliberate:** by
the time this fix landed, the stage commit was no longer HEAD — two
reviewed-PASS commits sat on top of it, so amending would have meant rebasing
over both. Rewriting commits a reviewer had already signed off, to save one
commit, was the wrong trade (and interactive rebase is unavailable in this
environment). It landed as its own fix commit instead. Moot in the final
history, which is a single squashed commit.

## Stage 4 — `/merge` was documenting a repo that no longer exists

- **Review:** ✅ **PASS** (round 1, 0 Critical / 0 Important). A follow-up commit
  fixed three accuracy items the reviewer surfaced but declined to raise as
  findings.

Same defect class as the rest of the branch, one layer up: instructions asserting
behaviour the code does not have. `.claude/commands/merge.md` carried three false
load-bearing claims, each verified against the repo rather than argued:

1. *"The pre-commit hook bumps the patch version (+1) and rebuilds the binary on
   feature branches only."* `.githooks/pre-commit` says the opposite in its own
   header — "This hook deliberately does NOT bump the version or build a binary"
   — as does `RELEASING.md`. The bump now happens in CI **after** the merge
   (`.github/workflows/bump-develop.yml`), visible in history as
   `chore: bump version to 1.2.6 (auto, PR #193 merged to develop)`. So step 3's
   arithmetic ("heading version = current `Cargo.toml` + 1", "the version
   advances once per commit") computed a number that will never exist. Rewritten
   to "no arithmetic — match `Cargo.toml`", which is also what the CHANGELOG's own
   convention note says, and which is precisely the drift stage 1 had to repair
   by hand (heading `[1.2.4]` against `Cargo.toml` `1.2.6`).
2. The guardrail **aborting on `chore/*`** rested entirely on that premise, in so
   many words: "on those the hook does not bump, so the version/CHANGELOG premise
   below would silently break". With no branch bumping at commit time the premise
   is void, and the guardrail would have refused **this very branch**. Inverted to
   abort only on the merge *targets* (`develop`, `master`, `release/*`).
3. *"This repo disallows merge commits — always use `--squash`. NEVER `--merge`
   (it fails with 'Merge commits are not allowed on this repository')."* Backwards
   for the PR this command opens. `AGENTS.md`: "Merge style = merge commits
   (`--merge`), not squash." `RELEASING.md`: "Create PR → `develop`. **Merge
   commit** (`--merge`)". And the history settles it — 16 of the last 60 commits
   on `develop` are two-parent merges, including this branch's own base `db3d3cf`.
   Squash belongs to the `develop`→`master` release PR, which is `/release`'s job.

Every file path the rewritten command cites was existence-checked, with a
positive control proving the check can report a miss.

**Not changed, deliberately:** no `CHANGELOG.md` entry — `.claude/commands/` is
local developer tooling, not shipped behaviour, and the changelog convention is
user-facing changes.

**Follow-up commit** — the stage-4 review passed but flagged three accuracy items
it declined to raise as findings. They are this branch's own defect class, so
they were fixed rather than carried:

- `gh repo view` settles the merge-style question directly: `allow_merge_commit:
  true` (so the deleted "merge commits are not allowed" claim was indeed the false
  one) but also **`allow_auto_merge: false`**. `--auto` therefore errors *every*
  time, which made the polling path the normal one while the text framed it as a
  fallback. Now stated as the expected path.
- `PR=$(gh pr view …)` does not match the command's own `Bash(gh:*)` grant — a
  shell assignment is not a `gh` invocation — so the pre-approved tool list
  silently didn't cover the one line that used it. Rewritten as a bare call.
- The CHANGELOG heading and `Cargo.toml` drift on **every** merge, not only when
  entries are omitted, since CI bumps unconditionally. Corrected; the instruction
  was already unconditional, so only the explanation was wrong.

Also verified by the reviewer, contradicting an assumption I had recorded: `git
add -A` **does** stage modifications to `.claude/commands/merge.md`. `.gitignore`
applies only to *untracked* paths, and this file is tracked. The `git add -f`
note in step 4 is still right, but for new files under `.claude/`, not edits.

**Files:** `.claude/commands/merge.md`

## Final branch review — whole branch against `db3d3cf`

⚠️ PASS WITH REMARKS: 0 Critical, 3 Important, all three fixed. Build gates were
green and both headline stages verified working;
what the per-stage reviews structurally could not see was the cross-stage view.

1. **The env override I said didn't exist.** Stage 3's comment claimed "there is
   no env override" for `repos.json` and cited `config_dir()` — the wrong
   function. `config_path()` (`src/db_discovery/repos.rs`) honours
   `CODESEARCH_REPOS_CONFIG` (`src/constants.rs`), the main README documents it,
   and **`web-guard`, the sibling hook reading the same file, already respects
   it**. So a user who set it got two hooks reading two different configs, and
   the preamble silently dropped the whole stage-3 feature. The nastier variant:
   with a stale `~/.codesearch/repos.json` also present, it injects an alias from
   the wrong config and the agent scopes to the wrong repo set — the exact silent
   miss stage 3 exists to prevent. Fixed by copying `web-guard`'s resolution into
   both twins and deleting the false claim from the comments, the integration
   README and the CHANGELOG. Verified: override honoured by both twins
   (`ALT.Alias` / `altgroup`, 2257 B identical), decoy `$HOME` not consulted, and
   a missing override path still fails open (rc=0, empty stderr, generic text).
   The no-override run is the negative control — generic text, proving the
   override is what changes the outcome.
2. **`/merge` step 8 still used `"$PR"`** — the variable the stage-4 follow-up had
   just forbidden defining (a shell assignment doesn't match the `Bash(gh:*)`
   grant). Lines 105-106 were updated to `<PR>`; line 108 was missed, so the
   admin-override path expanded to `gh pr merge "" …`. On this branch's own merge
   path. Aligned with the others.
3. **This file contradicted itself.** The header said "all four stages reviewed"
   while stages 3 and 4 still read `**Commit:** *(pending)*` and stage 1 said
   "round 2 pending" — and stage 3 had no `**Files:**` line. A basis of record
   that can't map half its stages to commits fails at the one job it has, since
   there is no card and no plan behind this branch. Placeholders filled.

Also noted by the reviewer, **pre-existing and left alone**: both twins advertise
`find(symbol, kind="usages") -- all call sites`, while this repo's own guidance is
that `find usages` is approximate and only `find_impact` is a precise call graph
(and `find_impact` isn't in the injected `ToolSearch` list). That is the same
"promises more than it delivers" shape stage 1 removed for Glob, so it belongs on
this branch's theme — but it is unchanged since `db3d3cf`, and widening scope at
the final-review gate is how a branch stops converging. Recorded below instead.

## Open follow-ups

- **The preamble oversells `find(kind="usages")`.** It calls it "all call sites";
  the repo's own rule is that BM25/regex results are approximate and an empty one
  means "found nothing", not "no callers" — only `find_impact` (SCIP) can say the
  latter, and it isn't in the `ToolSearch` select list the preamble hands out.
  Same defect class as the Glob promise stage 1 deleted. One-line fix when
  scheduled: add `find_impact` to the select list and soften the description.

- `.claude/commands/merge.md` step 4 still pins
  `Co-Authored-By: Claude Opus 4.8 (1M context)`. Left alone: it is stale rather
  than false-about-the-repo, and replacing it would assert a model version this
  branch has no way to verify.
- **The CWD→alias path comparison lowercases differently in the two twins.** The
  `.ps1` uses `.ToLowerInvariant()` (full Unicode); the `.sh` uses jq's
  `ascii_downcase` (ASCII only). A repo path containing a non-ASCII uppercase
  letter, registered in a different case than it appears on disk, would resolve
  an alias in the `.ps1` and fall back to the generic text in the `.sh`.
  Introduced by stage 3 and surfaced by the round-2 reviewer. Left as-is
  deliberately: jq has no Unicode-aware downcase, so closing it means
  hand-rolling an ASCII-only lowercase in PowerShell — a mechanism out of all
  proportion to the case it guards, on a branch whose stated constraint is to
  prefer deleting a promise over adding a mechanism. Recorded instead, which
  bounds the "byte-identical twins" claim honestly: it holds for ASCII paths.
  Failure mode if it ever fires is a fallback to the generic wording, not wrong
  advice.
- **`subagent-preamble.ps1` writes stdout in the console codepage, not UTF-8.**
  A prompt containing e.g. `café` comes back as CP850 bytes and the emitted JSON
  is then undecodable as UTF-8. Pre-existing — present identically at `db3d3cf`
  (no `[Console]::OutputEncoding` is set there either; the write is the same
  `ConvertTo-Json` pipeline), so the *caller's* prompt was already being
  corrupted before this branch. Stage 3 only adds a second
  possible non-ASCII source (alias and group names) into the same sink. It bounds
  the "byte-identical twins" claim to ASCII inputs, which is worth stating plainly.
  Fix when scheduled: `[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)`
  before writing. Left out of this branch as out-of-scope rather than smuggled
  into a review fix — the same call made for the em-dash issue in stage 2.
- `web-guard` was never installed on this machine (only `grep-guard` and
  `subagent-preamble` are registered in `~/.claude/settings.json`), and two of
  the installed copies are stale relative to source. A
  `codesearch hooks claude install` run is needed to pick up the stage-1 and
  stage-2 changes — it rewrites the global `~/.claude/settings.json`.
- Worth reconsidering whether `grep-guard`'s deny message should say plainly
  that it steers rather than fences, given Bash `grep` is one call away.

## Security note

No auth, network-exposure or data-handling surface changed. Stage 3 adds no
network call.

It does, however, newly **read a file that holds a secret**: `~/.codesearch/
repos.json` contains `remotes.<peer>.api_key` alongside the `.repos` and
`.groups` maps the hook wants. The hook reads only those two maps and injects
only an alias and group *names*, but "we only read the safe fields" is a claim
worth testing rather than asserting, so it was: the stored `api_key` and remote
URL were searched for in the injected prompt and are absent, with a positive
control proving the search finds a string that *is* present. Re-verified
independently by the stage-3 reviewer. Anything future that widens what this
hook injects should re-run that check — the file it reads is not innocuous.

<!-- sync
trello-card-id: (none — no linked card for this work)
azure-devops: (none — not tracked as a work item)
-->
