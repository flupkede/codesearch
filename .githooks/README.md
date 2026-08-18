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
| `pre-commit` | Runs `cargo fmt` and stages the result, so CI's fmt check can't fail. Then rejects any commit that introduces a root-level `*.md` outside the allowlist (see below). |
| `pre-push` | Blocks direct pushes to `master`; runs the QC gate (skipped when the branch changes no Rust); scans tracked files for customer references. |
| `post-checkout` | Creates `AGENTS.md` from `AGENTS.develop.md` on branch switch, if absent. |

Any hook can be bypassed with `git push --no-verify` / `git commit --no-verify`.

## Root-md allowlist guard (`pre-commit`)

Agents love dropping `*.md` files (diagnoses, plans, worklogs, test scenarios)
at the repo root. The `pre-commit` hook rejects any commit that **introduces**
(added/copied/renamed) a root-level `*.md` outside this allowlist:

- `AGENTS.md`, `AGENTS.develop.md` — agent instructions
- `CLAUDE.md` — one-line pointer to `AGENTS.md`
- `README.md`, `README_CSharp.md` — user-facing docs
- `CHANGELOG.md`, `RELEASING.md` — release infrastructure

Everything else belongs in **`.docs/`** (gitignored, local-only) — see
AGENTS.md, section *Root file hygiene (markdown)*. Only introductions are
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
