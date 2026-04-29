# Strict scope routing + schema version foundation

**Branch:** `feature/strict-scope-and-schema-version`
**Status:** In planning
**Scope:** 4 samenhangende fixes voor multi-repo correctness

---

## 1. Achtergrond

Na PR #28 (`get_chunk` chunk_id collision) blijft een fundamenteel probleem:
**andere tools (`search`, `find`, `explore`) kunnen nog steeds zonder `project`/`group`
over alle repos zoeken**. Dat is een silent footgun:

```
search("pagehook validation")
→ resultaten van investing/, BOIN.refactor/, alle aprimo-repos door elkaar
→ agent kan niet onderscheiden welke repo relevant is
```

Voor agents (vooral Claude Desktop, die geen CWD-context heeft) is dit verwarrend
en leidt tot foute conclusies. De agent moet altijd **bewust** kiezen in welke
scope hij zoekt.

Daarnaast: chunk IDs blijven u64 lokaal-per-database. Een toekomstige migratie
naar UUIDs of een andere ID-strategie heeft een **schema version** in de database
metadata nodig om automatisch detecteren + rebuilden mogelijk te maken. Dit is
fundament dat we nu willen leggen, los van de collision fix.

---

## 2. Scope — 4 onderdelen

| # | Onderdeel | Impact |
|---|-----------|--------|
| 1 | Strict scope routing voor `search`, `find`, `explore` | Agent UX |
| 2 | Gestructureerde `scope_required` error response | Agent UX |
| 3 | Smart `available_projects` lijst voor `get_chunk` | Agent UX |
| 4 | Schema version in LMDB metadata + auto-detect | Toekomst-proof |

Niet in scope:
- UUID chunk IDs (separate v2 feature, deze branch legt enkel het fundament)
- Wijzigingen aan single-repo gedrag
- FTS schema wijzigingen

---

## 3. Onderdeel 1 — Strict scope routing

### Huidige logica (`resolve_routing` in `src/mcp/mod.rs`)

Vandaag: als `project` en `group` beide leeg zijn in serve_hub mode → fan-out
naar alle stores. Dat is de bron van het probleem.

### Nieuwe logica

```rust
async fn resolve_routing(&self, project: &Option<String>, group: &Option<String>, tool_name: &str) 
    -> Result<RoutingContext, ScopeError> 
{
    let in_multi_repo = self.serve_stores.len() > 1;
    
    match (project, group) {
        (Some(p), Some(_)) => {
            // Beide gespecificeerd: project wint, group genegeerd (of fout?)
            // Voorstel: fout — kies één
            return Err(ScopeError::Conflicting);
        }
        (Some(p), None) => {
            route_to_project(p)
        }
        (None, Some(g)) => {
            route_to_group(g)
        }
        (None, None) if !in_multi_repo => {
            // Single repo: route naar de enige repo
            route_to_single_store()
        }
        (None, None) if in_multi_repo => {
            // KERN VAN DE FIX: weiger fan-out, vraag om scope
            return Err(ScopeError::ScopeRequired {
                available_projects: self.list_available_projects(),
                available_groups: self.list_available_groups(),
            });
        }
    }
}
```

### Tools die dit krijgen

- `search` (semantic + literal modes)
- `find` (definition, usages, imports, dependents)
- `explore` (outline, similar)
- `get_chunk` — al voorzien in PR #28, krijgt enkel verfijnde error

### Tools die dit NIET krijgen

- `status` — moet altijd werken zonder scope (toont juist de scope opties)

---

## 4. Onderdeel 2 — Gestructureerde error response

### Format

Niet een platte tekst-error, maar JSON in het tool result zodat agents
programmatisch kunnen reageren:

```json
{
  "error_code": "scope_required",
  "message": "Specify project= for a single repository or group= for cross-repo search.",
  "available_projects": [
    "BAYR.Aprimo", "BOIN.Aprimo", "BRU.Aprimo", "DMNT.Aprimo",
    "FCPN.Aprimo", "HUSQ.Aprimo", "KRKA.Aprimo",
    "codesearch", "DPS", "investing"
  ],
  "available_groups": ["aprimo"],
  "hint_for_agent": "If the user has not indicated which repository to search, ask them to choose. Show available_projects and available_groups as options."
}
```

### Belangrijke velden

- `error_code` — machine-readable, agents kunnen erop matchen
- `available_projects` — alfabetisch gesorteerd, alle aliassen uit `repos.json`
- `available_groups` — alle group keys uit `repos.json`
- `hint_for_agent` — expliciete instructie aan de LLM. Niet voor de gebruiker.

### Implementatie

Nieuwe functie `format_scope_error()` in `src/mcp/mod.rs`:

```rust
fn format_scope_error(&self, available_projects: Vec<String>, available_groups: Vec<String>) 
    -> CallToolResult 
{
    let payload = serde_json::json!({
        "error_code": "scope_required",
        "message": "Specify project= for a single repository or group= for cross-repo search.",
        "available_projects": available_projects,
        "available_groups": available_groups,
        "hint_for_agent": "If the user has not indicated which repository to search, ask them to choose. Show available_projects and available_groups as options."
    });
    CallToolResult::success(vec![Content::text(payload.to_string())])
}
```

---

## 5. Onderdeel 3 — Smart `available_projects` voor `get_chunk`

### Probleem

Vandaag (na PR #28) toont `get_chunk` zonder project een algemene fout met àlle
projecten. Maar de agent weet vaak ALLEEN dat hij dit `chunk_id` ergens heeft
gezien — niet welke. Hij zou het kunnen probéren, één voor één. Slechte UX.

### Fix

Voor `get_chunk` specifiek: scan kort over alle stores en geef in de error response
**alleen de projects die dit specifieke `chunk_id` hebben**:

```json
{
  "error_code": "ambiguous_chunk_id",
  "message": "chunk_id 433 exists in multiple repositories. Specify which one.",
  "candidate_projects": ["DPS", "HUSQ.Aprimo"],
  "hint_for_agent": "The chunk_id collision is a known limitation of multi-repo mode. Re-run get_chunk with one of the candidate_projects, or use search to identify the correct repository first."
}
```

Als slechts één project het chunk_id heeft → geen fout, route automatisch.
Als geen enkel project het heeft → fout `chunk_not_found`.

### Implementatie

Voor `get_chunk` zonder project, in plaats van directe error:

```rust
let candidates: Vec<String> = self.serve_stores.iter()
    .filter_map(|(alias, store)| {
        match store.vector_store.read().await.get_chunk(request.chunk_id) {
            Ok(Some(_)) => Some(alias.clone()),
            _ => None,
        }
    })
    .collect();

match candidates.len() {
    0 => return chunk_not_found_error(request.chunk_id),
    1 => proceed_with_single_match(candidates[0]),
    _ => return ambiguous_chunk_id_error(candidates),
}
```

---

## 6. Onderdeel 4 — Schema version in LMDB metadata

### Doel

Maak het mogelijk om in de toekomst (bv. UUID migratie, vector format change)
**automatisch te detecteren** dat een database verouderd is en dan een rebuild
te triggeren. Vandaag hebben we geen versioning — een toekomstige migratie zou
silent failures of crashes geven.

### Implementatie

In `src/vectordb/store.rs`, voeg toe aan de LMDB metadata tabel:

```rust
const SCHEMA_VERSION: u32 = 1;
const METADATA_KEY_SCHEMA_VERSION: &str = "schema_version";

impl VectorStore {
    fn ensure_schema_version(&mut self) -> Result<()> {
        let stored = self.read_metadata::<u32>(METADATA_KEY_SCHEMA_VERSION)?;
        match stored {
            None => {
                tracing::info!("Initializing schema_version = {}", SCHEMA_VERSION);
                self.write_metadata(METADATA_KEY_SCHEMA_VERSION, &SCHEMA_VERSION)?;
                Ok(())
            }
            Some(v) if v == SCHEMA_VERSION => Ok(()),
            Some(v) if v < SCHEMA_VERSION => {
                tracing::warn!(
                    "Database schema is v{}, current is v{}. Rebuild required.",
                    v, SCHEMA_VERSION
                );
                Err(StoreError::SchemaOutdated { 
                    current: v, 
                    required: SCHEMA_VERSION 
                })
            }
            Some(v) => {
                tracing::error!(
                    "Database schema v{} is newer than supported v{}. Upgrade codesearch.",
                    v, SCHEMA_VERSION
                );
                Err(StoreError::SchemaTooNew { 
                    found: v, 
                    supported: SCHEMA_VERSION 
                })
            }
        }
    }
}
```

Bij `VectorStore::open()` aanroep van `ensure_schema_version()`. Bij eerste keer
op een bestaande database: schema_version wordt op 1 gezet zonder rebuild
(bestaande indexen worden als v1 beschouwd, niet als "voor schema versioning").

### Auto-rebuild trigger (optioneel voor v1, sterk aanbevolen)

In `serve` startup: als `SchemaOutdated` error optreedt, log een warning en
markeer de repo als "needs_rebuild". Bij eerste tool call op die repo:
delegeer naar `POST /repos/{alias}/reindex` (Fix 7 uit `AGENTS_fix-serve-multi-repo.md`).

In v1 van deze branch: alleen detection + duidelijke error message, geen
auto-rebuild. Dat hoort bij Fix 7 wat in een andere branch zit.

---

## 7. Implementatievolgorde

1. **Onderdeel 4 eerst** — schema_version infrastructuur (laag risico, klein).
   Geen gedragswijziging, alleen toekomstvoorbereiding.
2. **Onderdeel 2** — `format_scope_error` helper functie.
3. **Onderdeel 1** — strict scope routing in `resolve_routing`.
   Toepassen op `search`, `find`, `explore` één voor één.
4. **Onderdeel 3** — `get_chunk` smart candidate detection.
   Vervangt de simpele error uit PR #28 met de smart variant.

Na elk onderdeel: `cargo check` (achtergrond), niet bouwen tot het einde.

---

## 8. Definition of Done

- [ ] `cargo check --all-targets` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test --lib` groen
- [ ] Test: `search` zonder project/group in multi-repo → JSON error met available_projects
- [ ] Test: `search(project="HUSQ.Aprimo", ...)` → alleen HUSQ resultaten (geen verandering)
- [ ] Test: `search(group="aprimo", ...)` → alle aprimo repos (geen verandering)
- [ ] Test: `search` in single-repo mode (één repo in repos.json) → werkt zonder project (geen verandering)
- [ ] Test: `find`, `explore` zelfde gedrag als `search`
- [ ] Test: `get_chunk(chunk_id=433)` zonder project → `candidate_projects` met alleen DPS en HUSQ.Aprimo
- [ ] Test: `get_chunk(chunk_id=999999999)` → `chunk_not_found`
- [ ] Test: nieuwe LMDB database krijgt `schema_version: 1` in metadata
- [ ] Test: bestaande database zonder schema_version krijgt v1 toegekend bij eerste open
- [ ] `cargo build --release` clean (allerlaatste stap)

---

## 9. Tool description updates

Alle tools die strict mode krijgen, hun description krijgt een toevoeging:

```
IMPORTANT (multi-repo): always specify either `project` (single repo) or 
`group` (cross-repo). Omitting both in multi-repo mode returns a 
`scope_required` error with the list of available projects and groups.
If the user has not indicated which repository to search, ask them to choose.
```

---

## 10. Risico's

| Risico | Mitigatie |
|--------|-----------|
| Bestaande agents breken die zonder scope zochten | Tool description maakt expliciet, error message is duidelijk en bevat alle keuzes |
| `default` repo concept gewenst voor sommige users | Out of scope — kan later via `"default_project": "X"` in repos.json worden toegevoegd |
| Schema versie 1 niet retro-actief op oude DB's | Bestaande DB's krijgen automatisch v1 bij eerste open, geen handmatige migratie nodig |

---

## 11. Commit message voorstel

```
fix(mcp): strict scope routing + schema version foundation

Multi-repo mode now requires explicit project= or group= in search,
find, explore, and get_chunk. Fan-out over all repos was confusing
agents and producing irrelevant results from unrelated repositories.

Returns structured scope_required error with available_projects,
available_groups, and hint_for_agent so the LLM knows to either
pick a scope or ask the user.

get_chunk: when chunk_id collision occurs, error now lists only the
projects that actually contain that chunk_id (typically 1-2), making
it actionable for agents.

Schema version foundation: LMDB metadata now stores schema_version
(v1 baseline). Future schema changes can detect outdated databases
and trigger rebuilds via the existing reindex delegation mechanism.

No UUID chunk_id migration in this branch — that requires a v2
schema bump and is deferred. The infrastructure is now in place.
```
