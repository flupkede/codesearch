# Release Workflow

## Branch model

```
feature/fix branches  →  develop  →  master (tagged = release)
```

## Git hooks

Install once per clone — this single setting enables all hooks:
```bash
git config core.hooksPath .githooks
```

Nothing is copied into `.git/hooks/`; with `core.hooksPath` set, git ignores that
directory entirely, so anything placed there would look installed and never run.
See [`.githooks/README.md`](.githooks/README.md) for what each hook does.

Pre-commit behavior:
- Runs `cargo fmt` and stages any reformatting (keeps CI's fmt-check green).
- Does **not** bump the version or build a binary. Every build already gets a
  unique `+<commit_count>` suffix from `build.rs` (`git rev-list --count HEAD`),
  so a per-commit auto-bump added churn (and a slow debug rebuild that blocked
  each commit) for no traceability gain. The base version is bumped deliberately
  — see **Version bumps** under Rules.

## Step-by-step

### 1. Feature branch

```bash
git checkout -b fix/my-fix origin/develop
# ... make code changes ...
git commit -m "fix: describe the change"
# pre-commit hook runs cargo fmt only (fast; no version bump, no build)
git push -u origin fix/my-fix
```

Create PR → `develop`. **Merge commit** (`--merge`) — feature history stays full of
`Merge pull request #N`, not squash.

### 2. Develop → master (when requested)

```bash
git checkout -b release/v1.0.X origin/develop
git push -u origin release/v1.0.X
```

Create PR → `master`. **Squash merge.**

### 3. Tag release

```bash
git checkout master && git pull
git tag v1.0.X
git push origin v1.0.X
```

CI (`release.yml`) builds binaries and creates a GitHub Release with auto-generated notes from PR titles.

## Rules

- **Version scheme `Major.Minor.Patch`** (semver):
  - **Patch** auto-bumps +1 on every PR merged to `develop` — CI does this via
    `.github/workflows/bump-develop.yml` (edits `Cargo.toml` + syncs `Cargo.lock`,
    no rebuild). No per-commit bump; per-commit uniqueness still comes from
    `build.rs`'s `+<commit_count>` suffix.
  - **Minor** bumps manually at release via `scripts/bump-version.sh --type minor`
    (resets patch→0). **Major** on breaking changes.
- **CHANGELOG.md** — no `[Unreleased]` staging section; entries are added directly
  under the heading for the current pending version (see the convention note at
  the top of `CHANGELOG.md`) and that section is finalized with a date once the
  release is tagged.
- **Merge style:** feature→`develop` = **merge commit** (`--merge`); `develop`→`master`
  release PR = **squash** (one commit per release on master)
- **Tag format**: `v1.0.X` on master HEAD
