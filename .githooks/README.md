# Git hooks

All hooks live here. Enable them once per clone:

```sh
git config core.hooksPath .githooks
```

That single setting is the whole install. Nothing is copied into `.git/hooks/`
— an unset `core.hooksPath` means **no hooks run at all**, so if a guard below
never seems to fire, check that setting first.

| Hook | What it does |
|---|---|
| `pre-commit` | Runs `cargo fmt` and stages the result, so CI's fmt check can't fail. Then runs the branch-aware root-md allowlist guard (see below) — on `develop`/`master`, `AGENTS.md`/`CLAUDE.md` are not allowlisted, so (re)introducing them via a direct commit or conflict resolution is blocked. |
| `pre-merge-commit` | Runs the same root-md allowlist guard on the merge result — a clean merge never invokes `pre-commit`, so this is the gate that stops `AGENTS.md`/`CLAUDE.md` riding a clean feature→`develop` merge onto the integration branch. |
| `pre-push` | Blocks direct pushes to `master`; runs the QC gate (`scripts/qc.ps1 -Fast`: CI's own fmt/clippy/lib-test commands, workspace-wide, plus a compile of the integration tests — `src/qc_gate_tests.rs` fails when the gate scripts drift from `.github/workflows/ci.yml`; skipped when the branch changes no Rust); scans tracked files for customer references. |
| `post-checkout` | Branch-local agent-file lifecycle: on a feature branch, creates `AGENTS.md` from `AGENTS.develop.md` plus the `CLAUDE.md` pointer when absent; on `develop`/`master`, removes untracked `AGENTS.md`/`CLAUDE.md` leftovers. No-op on detached HEAD. |

Any hook can be bypassed with `git push --no-verify` / `git commit --no-verify`.

## Root-md allowlist guard (`pre-commit`)

Agents love dropping `*.md` files (diagnoses, plans, worklogs, test scenarios)
at the repo root. The `pre-commit` hook rejects any commit that **introduces**
(added/copied/renamed) a root-level `*.md` outside this allowlist:

- `AGENTS.develop.md` — agent instructions reference (always allowed)
- `AGENTS.md`, `CLAUDE.md` — branch-local agent working files: allowlisted on
  feature branches only. They are dropped at feature finalization; on
  `develop`/`master` introducing them is rejected, merge commits included.
- `README.md`, `README_CSharp.md` — user-facing docs
- `CHANGELOG.md`, `RELEASING.md` — release infrastructure

Everything else belongs in **`.docs/`** (gitignored, local-only) — see
AGENTS.develop.md, section *Root file hygiene (markdown)*. The guard itself
lives in [`lib/root-md-guard.sh`](lib/root-md-guard.sh), shared by
`pre-commit` and `pre-merge-commit`; its behavior is pinned by
`scripts/test-githooks.sh`. Only introductions are
checked: modifying an already-tracked stray is only possible after a
deliberate `--no-verify` bypass, where the file itself — not the commit — is
the violation. Dot-folders (`.githooks/`, `.github/`, `.claude/`, …) are out
of scope: a root-level file cannot be inside one.

## `customer-patterns.local`

The `pre-push` leak scan reads its patterns from `.githooks/customer-patterns.local`
— one extended regex (ERE) per line. Blank lines and lines starting with `#` are
ignored; a `#` in the middle of a line stays part of the pattern.

**This file is gitignored and must stay that way.** The pattern list is a list of
customer names and project codes, so committing it would leak precisely what the
scan exists to prevent. It does not survive a fresh clone; recreate it from your
password manager.

One rule governs the scan: **configuration problems warn, actual leaks block.**
A missing file, an empty file, or a pattern that makes `git grep` fail (an
invalid regex exits 128) all print a loud `SKIPPED — nothing was checked` warning
and let the push through. Only a real match blocks it.

The warning is the point. A guard that stops running quietly is worse than no
guard, because it is still trusted — so the scan is never allowed to report
"clean" when it did not actually run.
