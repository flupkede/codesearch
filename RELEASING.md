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
- **Always**: runs `cargo fmt`, stages formatted files
- **Feature branches** (`fix/*`, `feature/*`, `features/*`): auto-bumps patch version + rebuilds binary
- **develop / master / release/***: only fmt, no version bump

## Step-by-step

### 1. Feature branch

```bash
git checkout -b fix/my-fix origin/develop
# ... make code changes ...
git commit -m "fix: describe the change"
# pre-commit hook auto-bumps version + rebuilds
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

- **Version bumps** happen only on feature branches (pre-commit hook)
- **No manual CHANGELOG.md edits** — GitHub Releases auto-generate release notes
- **No version bumps** on develop, master, or release branches
- **Squash merge** all PRs to keep history linear
- **Tag format**: `v1.0.X` on master HEAD
