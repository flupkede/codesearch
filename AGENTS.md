# Setup git flow: feature → develop → master

**Branch:** `chore/setup-develop-branch`
**Status:** Wachten tot `feature/serve-tui` gemerged is
**Eigenaar:** OpenCode (of handmatig)

---

## 1. Doel

Overstappen van een single-branch flow (alle PRs → `master`) naar een
develop-based flow:

```
feature/xxx ─PR→ develop ─PR→ master (release)
                    │
                    └─► CI runs hier
                    
master ─tag v1.x.x→ GitHub Release
```

`master` blijft de release branch (geen rename naar `main`). `develop` wordt de
actieve dev branch waar alle CI op draait. Releases gebeuren via PR
`develop → master` + tag.

---

## 2. Voorwaarde (plan A — clean cut)

**Voordat deze branch wordt uitgevoerd:** alle open feature branches die nog
bezig zijn moeten eerst gemerged of gesloten worden. Concreet:

- `feature/serve-tui` — wachten tot OpenCode klaar is en gemerged
- Eventueel andere actieve branches verifiëren met `git branch -r`

Stale branches die niet meer relevant zijn worden verwijderd voor de
overstap (zie sectie 6).

---

## 3. Stappen

### 3.1 Maak `develop` branch vanuit master

```bash
git checkout master
git pull origin master
git checkout -b develop
git push -u origin develop
```

### 3.2 Update GitHub default branch naar develop

Via REST API met PAT (vermijd gh CLI vanwege bedrijfsnetwerk traagheid):

```powershell
$t = (Get-Content "$env:APPDATA\Claude\claude_desktop_config.json" | ConvertFrom-Json).mcpServers.github.env.GITHUB_PERSONAL_ACCESS_TOKEN
Invoke-RestMethod -Uri "https://api.github.com/repos/flupkede/codesearch" -Method PATCH `
  -Headers @{ Authorization = "Bearer $t"; "Content-Type" = "application/json" } `
  -Body (@{ default_branch = "develop" } | ConvertTo-Json)
```

Verifieer:
```powershell
(Invoke-RestMethod -Uri "https://api.github.com/repos/flupkede/codesearch" -Headers @{ Authorization = "Bearer $t" }).default_branch
# → "develop"
```

### 3.3 Branch protection rules

Voor `master` (release branch — strikter):
- Required: PR before merge
- Required: status checks (build, test) als die bestaan
- Allowed source: alleen `develop`
- Geen direct push

Voor `develop` (active dev — minder strikt):
- Required: PR before merge
- Status checks aanbevolen, niet verplicht in v1
- Direct push uit voorzichtigheid disabled

API call (master):
```powershell
$rules = @{
  required_status_checks = $null
  enforce_admins = $false
  required_pull_request_reviews = @{
    required_approving_review_count = 0
    dismiss_stale_reviews = $false
  }
  restrictions = $null
  allow_deletions = $false
  allow_force_pushes = $false
} | ConvertTo-Json -Depth 5

Invoke-RestMethod -Uri "https://api.github.com/repos/flupkede/codesearch/branches/master/protection" `
  -Method PUT -Headers @{ Authorization = "Bearer $t"; "Content-Type" = "application/json" } -Body $rules
```

Hetzelfde voor `develop` met aangepaste regels.

### 3.4 CI workflow update

Check `.github/workflows/`:
- Als er een `ci.yml` of `build.yml` bestaat met trigger op `master`,
  vervang door `develop` (of beide) in de `on:` sectie:

```yaml
on:
  push:
    branches: [develop, master]
  pull_request:
    branches: [develop]
```

Geen wijziging als er nog geen workflows bestaan — dan slaan we deze stap over.

### 3.5 Release proces documenteren

Voeg toe aan `README.md` of een nieuwe `RELEASE.md`:

```markdown
## Release Process

1. PR `develop → master` aanmaken
2. Review en merge
3. Tag op master:
   git checkout master && git pull
   git tag -a v1.x.x -m "Release v1.x.x"
   git push origin v1.x.x
4. GitHub Release aanmaken op de tag
```

### 3.6 Update CONTRIBUTING / docs

In `README.md` (of nieuwe `CONTRIBUTING.md`) een sectie toevoegen:

```markdown
## Development workflow

- Maak feature branches vanuit `develop`: `git checkout -b feature/xxx develop`
- PR naar `develop`
- `master` is de release branch — alleen via PR `develop → master`
- Branch naming: `feature/xxx`, `fix/xxx`, `chore/xxx`, `docs/xxx`
```

---

## 4. Verifieer na uitvoer

- [ ] `develop` branch bestaat lokaal en op GitHub
- [ ] GitHub repo settings tonen `develop` als default branch
- [ ] Nieuwe PR via web UI defaultent naar `develop` als target
- [ ] `master` branch protection actief — direct push faalt
- [ ] CI draait op push naar `develop` (als CI bestaat)
- [ ] README documenteert de nieuwe flow

---

## 5. Stale branches opruimen

Voor de overstap: identificeer en verwijder branches die niet meer relevant zijn.
Lijst van branches die mogelijk stale zijn (op basis van naam en eerdere PR's):

```
feat/mcp-literal-search-tool          (vervangen door eerdere fixes)
feat/mcp-rebrand-hybrid-search        (gemerged via #15)
feature/LMDBResilience_GitAware_IndexCompact.md
feature/auto-regex-confidence
feature/branch_switch_failing_index
feature/cleanup
feature/fix-get-chunk-collision       (gemerged via #28)
feature/fix-serve-multi-repo          (gepruned)
feature/fix-serve-multi-repo-2        (gemerged via #25)
feature/fix-serve-shutdown
feature/improve_search_results
feature/mcp-navigation-extras
feature/post-pr8-fixes
feature/resolve_git_worktree_correction
feature/strict-scope-and-schema-version
feature/update_readme
feature/upgrade_tree_sitter
features/5-quiet-actually-quiet
```

Aanpak:
1. Voor elke branch: check of laatste commit in master zit (`git log master --grep="<branch-naam>"`
   of `git branch --merged master`)
2. Als gemerged: `git branch -d <name>` lokaal + `git push origin --delete <name>`
3. Als niet gemerged en niet meer relevant: bevestig met user voor verwijderen

Dit is **handmatig werk**, niet automatiseren — risico om recent werk te verliezen.

---

## 6. Niet in scope

- Conventional Commits enforcement (commitlint) — apart traject
- Semantic versioning automation (release-please) — apart traject
- Changelog generation — apart traject
- Rename `master` → `main` — bewust niet, conform user voorkeur
- Verplichte CI status checks — wachten tot CI workflow zelf gestabiliseerd is

---

## 7. Risico's

| Risico | Mitigatie |
|--------|-----------|
| Bestaande feature branches gerebaset op verouderde master | Plan A: alle open branches eerst mergen naar master, dan develop opzetten |
| Open PRs richten naar master ipv develop na switch | Bestaande PRs handmatig her-targeten via GitHub UI of API (`PATCH /repos/.../pulls/N` met `base: develop`) |
| Branch protection blokkeert legitieme master commits | Admin override blijft mogelijk; protection geldt voor PR flow |
| Lokale clones bij andere users hebben oude default | `git remote set-head origin -a` om het bij te werken |

---

## 8. Commit message voorstel

```
chore: setup develop branch and git flow

- Add develop branch as default for new feature work
- master remains release branch (no rename to main)
- Branch protection on master: PR only, source must be develop
- Branch protection on develop: PR required
- Update CI triggers (if applicable) to develop
- Document release process: develop → master + tag
```
