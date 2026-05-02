# AGENTS.md — features/cleanup

## Goal

Remove stale planning documents and old benchmark results that have no value
for contributors or users of the released codebase. These directories were
useful during development but are now clutter.

## Scope

This branch touches **only** file deletions — no source code, no Cargo.toml,
no tests. `cargo check` is not required (no Rust changes).

---

## Tasks

### 1. Delete `.docs/` directory (entire tree)

Remove all files under `.docs/` including the `done/` subdirectory:

```
.docs/MCP_HELP_SYSTEM.md
.docs/opencode-reload-commands-pr.md
.docs/plan-implementation.md
.docs/plan-testing.md
.docs/done/benchmarks-improvement-plan.md
.docs/done/codesearch-improvement-plan.md
.docs/done/LMDBResilience_GitAware_IndexCompact.md
.docs/done/old-plan-review.md
.docs/done/old-testplan.md
.docs/done/plan-embedding-cache.md
.docs/done/plan-review.md
```

### 2. Delete `benchmarks/` directory (entire tree)

Remove all files under `benchmarks/`:

```
benchmarks/benchmark-20251124-232718.md
benchmarks/benchmark-20251124-234722.md
benchmarks/benchmark-20251125-103111.md
benchmarks/benchmark-20251125-103719.md
benchmarks/benchmark-20251125-104204.md
benchmarks/BGE-small-en-v1.5.md
benchmarks/demongrep_vs_osgrep.md
benchmarks/external_repo_bat.md
benchmarks/FULL_BENCHMARK_SUMMARY.md
benchmarks/improvement-plan.md
benchmarks/mcp-tool-description-improvements.md
benchmarks/test_external_repo.sh
```

### 3. Commit

```
git rm -r .docs benchmarks
git commit -m "chore: remove stale planning docs and old benchmark results"
git push origin features/cleanup
```

### 4. Update CHANGELOG.md

Add a line under a new `## [Unreleased]` section — or add to the next release
section if one already exists:

```markdown
### Removed

- Stale planning documents (`.docs/`) and old benchmark results (`benchmarks/`)
  removed from the repository. These were internal working documents with no
  value for contributors.
```

---

## Done when

- [ ] `.docs/` directory no longer exists in the repository
- [ ] `benchmarks/` directory no longer exists in the repository
- [ ] Commit on `features/cleanup` pushed to origin
- [ ] CHANGELOG.md updated
