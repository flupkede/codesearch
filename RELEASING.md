# Release Workflow

## Branch model

```
feature/fix branches  →  develop  →  master (tagged = release)
```

## Pre-commit hook

Install once:
```bash
cp scripts/pre-commit .git/hooks/pre-commit
```

Behavior:
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

Create PR → `develop`. **Squash merge.**

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

- **Version bumps are manual + deliberate** — edit `version` in `Cargo.toml`
  when it's meaningful (typically when cutting a release branch), then
  `cargo update --workspace` to sync `Cargo.lock`. There is no per-commit
  auto-bump; per-commit uniqueness comes from `build.rs`'s `+<commit_count>`.
- **No manual CHANGELOG.md edits** — GitHub Releases auto-generate release notes
- **Squash merge** all PRs to keep history linear
- **Tag format**: `v1.0.X` on master HEAD
