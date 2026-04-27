# Fix: Multi-repo serve quality issues

**Branch:** `feature/fix-serve-multi-repo`
**Prioriteit:** Hoog — alle issues beïnvloeden dagelijks gebruik
**Eigenaar:** OpenCode

---

## 1. Overzicht

Vijf problemen geïdentificeerd na eerste multi-repo test op 2026-04-27:

| # | Probleem | Ernst |
|---|----------|-------|
| 1 | Dubbele zoekresultaten (FTS dedup werkt niet op content) | Hoog |
| 2 | `explore` path met project-alias prefix werkt niet | Medium |
| 3 | File watchers starten simultaan, blokkeren elkaar | Hoog |
| 4 | Alle repos laden tegelijk → LMDB map_full + memory spike | Hoog |
| 5 | `repos.json` wordt niet automatisch gevuld bij eerste gebruik | Medium |

Bewijs uit serve logs (2026-04-27T12:24):
```
INFO  Processing 436 changed files...
INFO  Embedding 6612 chunks...
WARN  MDB_MAP_FULL error in insert_chunks_with_ids(), resizing to 2048MB
ERROR Initial refresh for 'BOIN.Aprimo.refactor' failed: an environment is already opened with different options
```

---

## 2. Fix 1 — Dubbele resultaten in search (FTS + vector)

### Root cause

`search/mod.rs` dedupliceert op `chunk_id` (lijn ~657). Historische index-snapshots
produceren nieuwe chunk_ids voor dezelfde content (zelfde path + start_line + end_line
maar ander chunk_id). Die dubbels overleven de chunk_id-dedup en komen als
aparte resultaten terug.

`chunks_for_file` in `vectordb/store.rs` heeft al de correcte fix (dedup op
`(start_line, end_line)`, hoogste chunk_id wint). Die logica ontbreekt in het
search pad.

### Fix

In `src/search/mod.rs`, na het ophalen van resultaten maar voor het teruggeven:

```rust
// Bestaande dedup op chunk_id:
if seen_exact_ids.insert(exact_match.chunk_id) { ... }

// TOEVOEGEN: dedup op (path, start_line, end_line) — elimineert historische snapshots
// die verschillende chunk_ids hebben maar identieke locatie.
// Strategie: behoud hoogste chunk_id (meest recente snapshot).
```

Concrete implementatie:
```rust
// Na de chunk_id-dedup, voeg een tweede pass toe:
let mut seen_locations: HashMap<(String, u32, u32), u64> = HashMap::new();
// key = (path, start_line, end_line), value = chunk_id

results.retain(|r| {
    let key = (r.path.clone(), r.start_line, r.end_line);
    match seen_locations.entry(key) {
        Entry::Vacant(e) => { e.insert(r.chunk_id); true }
        Entry::Occupied(mut e) => {
            if r.chunk_id > *e.get() {
                *e.get_mut() = r.chunk_id;
                // verwijder eerder toegevoegd resultaat met lager chunk_id
                // (dit vereist twee-pass of een sort-first aanpak)
                false
            } else {
                false
            }
        }
    }
});
```

**Aanbevolen aanpak (eenvoudiger):** sort resultaten op chunk_id descending vóór
dedup, dan is de eerste encounter altijd de meest recente:

```rust
results.sort_by(|a, b| b.chunk_id.cmp(&a.chunk_id));
let mut seen_locations: HashSet<(String, u32, u32)> = HashSet::new();
results.retain(|r| seen_locations.insert((r.path.clone(), r.start_line, r.end_line)));
// Daarna re-sort op score voor output
results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
```

Dit geldt voor zowel literal als semantic search. Pas toe in alle zoekpaden die
een `Vec<SearchResult>` produceren.

### Locaties

- `src/search/mod.rs` — literal search resultaten (rond lijn 649-660)
- `src/search/mod.rs` — semantic/hybrid RRF resultaten (zelfde patroon)
- Controleer ook `src/fts/tantivy_store.rs` of FTS zelf al op locatie dedupliceert

---

## 3. Fix 2 — `explore` path met project-alias prefix

### Root cause

`explore(target="HUSQ.Aprimo/src/...", project="HUSQ.Aprimo")` geeft geen resultaten.
`explore(target="src/...", project="HUSQ.Aprimo")` werkt wel.

Serve-logs bevestigen: beide calls worden ontvangen maar de eerste vindt geen chunks.
De path matching in `chunks_for_file` vergelijkt het opgegeven target met de opgeslagen
paden. In multi-repo mode slaat de index paden op met project-alias prefix
(`HUSQ.Aprimo/src/...`). De explore handler strip die prefix niet.

### Fix

In de `explore` MCP handler (`src/mcp/mod.rs`), voor de `chunks_for_file` call:

```rust
// Strip project alias prefix als die aanwezig is in het target pad.
// Bv. "HUSQ.Aprimo/src/foo.cs" met project="HUSQ.Aprimo" → "src/foo.cs"
let normalized_target = if let Some(alias) = &request.project {
    target.strip_prefix(&format!("{}/", alias))
           .unwrap_or(&target)
           .to_string()
} else {
    target
};
```

Alternatief: strip altijd de eerste path-component als die overeenkomt met een
bekende repo alias. Kies de eenvoudigste aanpak die de agent-UX verbetert.

### Locatie

`src/mcp/mod.rs` — explore tool handler

---

## 4. Fix 3 + 4 — Sequentiële FSW startup en geheugencontrole

### Root cause (3)

`codesearch serve` start voor alle geregistreerde repos tegelijk een `IndexManager`
met file watcher. Als één repo 436 gewijzigde bestanden heeft en 6612 chunks
moet embedden, blokkeert dat de initialisatie van alle volgende repos.

### Root cause (4)

Alle repos openen tegelijk hun LMDB environments. Als twee repos hun map_size
anders initialiseren (bv. na een resize op de ene), krijgt de tweede een
`already opened with different options` error omdat LMDB map-size globaal is
per proces.

Huidige logs:
```
WARN  MDB_MAP_FULL → resize to 2048MB (attempt 1/3)
ERROR BOIN.Aprimo.refactor failed: already opened with different options
```

### Fix

**Stap 1: Sequentiële initialisatie**

In `src/serve/mod.rs`, vervang parallelle repo initialisatie door sequentieel:

```rust
// Was (conceptueel — alle repos tegelijk):
let handles: Vec<_> = repos.iter().map(|r| tokio::spawn(init_repo(r))).collect();
join_all(handles).await;

// Wordt:
for repo in &repos {
    if let Err(e) = init_repo(repo).await {
        tracing::error!("Failed to init {}: {}", repo.alias, e);
        // Log en ga verder — één falende repo mag de rest niet blokkeren
    }
}
```

**Stap 2: Gestaffelde FSW startup**

Na sequentiële init, voeg een kleine vertraging toe tussen het starten van elke
file watcher (bv. 500ms) om burst I/O te vermijden:

```rust
for repo in &repos {
    start_file_watcher(repo).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}
```

**Stap 3: LMDB map-size consistentie**

Zorg dat alle repos bij initialisatie dezelfde initiële map-size gebruiken
(bv. altijd starten met `max(current_size, MIN_MAP_SIZE_MB)`). Als één repo
al geresize heeft, moet de volgende repo dat kunnen detecteren via de LMDB
info file voordat hij opent.

Check `src/vectordb/store.rs` — LMDB open logica — en zorg dat map-size
gelezen wordt uit de bestaande database voor hij geopend wordt.

### Locaties

- `src/serve/mod.rs` — repo initialisatie loop
- `src/vectordb/store.rs` — LMDB open/resize logica

---

## 5. Fix 5 — Auto-discovery van repos.json bij eerste gebruik

### Probleem

Gebruikers die codesearch al hadden vóór multi-repo support (voor v1.0) hebben
geen `repos.json`. Bij `codesearch serve` worden hun bestaande indexes niet
gevonden.

### Fix

Bij `codesearch serve` opstarten, **vóór** het laden van `repos.json`:

```rust
fn ensure_repos_json_populated(repos_path: &Path) -> Result<()> {
    let config = load_repos_json(repos_path)?;

    if config.repos.is_empty() {
        tracing::info!("repos.json is empty — scanning for existing indexes...");
        let discovered = discover_codesearch_databases()?;
        if !discovered.is_empty() {
            tracing::info!("Found {} existing indexes, populating repos.json", discovered.len());
            let mut updated = config;
            for (alias, path) in discovered {
                updated.repos.insert(alias, path);
            }
            save_repos_json(repos_path, &updated)?;
        }
    }
    Ok(())
}
```

`discover_codesearch_databases()` zoekt:
1. Bekende standaard locaties: `~/source/repos/`, `~/WorkArea/`, `~/projects/`
2. Huidige working directory en parents (git root detection)
3. Elke directory met een `.codesearch.db` subdir

Alias afleiding: gebruik de directory naam van de repo root.

**Scope beperking:** scan maximaal 3 directory niveaus diep vanuit elke
startlocatie. Geen volledige schijfscan.

**Versie check (optioneel):** als de `repos.json` een versie veld heeft
(`"version": 1`), kan de auto-discovery overgeslagen worden voor bestaande
geconfigureerde setups.

### Locatie

- `src/serve/mod.rs` — opstartlogica
- `src/db_discovery/mod.rs` — bestaande discovery module uitbreiden

---

## 6. Definition of Done

- [ ] `cargo check --all-targets` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test --lib` groen
- [ ] Test: literal search op HUSQ.Aprimo geeft geen dubbele resultaten meer
- [ ] Test: `explore(target="HUSQ.Aprimo/src/...", project="HUSQ.Aprimo")` werkt
- [ ] Test: `codesearch serve` met 10+ repos start zonder LMDB errors
- [ ] Test: `codesearch serve` op machine zonder repos.json ontdekt en vult auto
- [ ] Test: `get_chunk(chunk_id=X, project="HUSQ.Aprimo")` geeft correct HUSQ chunk terug
- [ ] Test: `get_chunk(chunk_id=X)` zonder project werkt nog (backwards compat)

---

## 9. Niet in scope

- Globale chunk ID coordinatie (te complex, optie A is voldoende)
- UI voor repos.json beheer

---

## 10. Fix 7 — `codesearch index --force` terwijl serve draait

### Root cause

`codesearch serve` houdt een `.writer.lock` én heeft LMDB/Tantivy bestanden
memory-mapped open. `codesearch index --force` op een actieve serve-repo faalt
om twee redenen:

1. `is_database_locked()` detecteert de lock → valt terug naar readonly → kan niet schrijven
2. Windows: memory-mapped files kunnen niet verwijderd worden → `--force` delete faalt met access denied

Resultaat: gebruiker krijgt een cryptische fout of stille mislukking.

### Fix — `index --force` delegeert naar serve via HTTP als serve actief is

**Nieuwe serve endpoint:**

```
POST /repos/{alias}/reindex
Body: { "force": true }
Response: { "job_id": "uuid", "status": "started" }

GET /repos/{alias}/reindex/{job_id}
Response: { "status": "running"|"done"|"failed", "progress": "...", "error": null }
```

Serve voert de reindex intern uit:
1. Pauzeer file watcher voor deze repo
2. Release writer lock tijdelijk (close LMDB/Tantivy handles)
3. Voer `index --force` uit op de repo path
4. Heropen database met verse index
5. Hervat file watcher

Tijdens reindex: serve beantwoordt queries op deze repo met een tijdelijke error:
`{ "error": "reindex in progress, retry in a moment" }`

**`codesearch index --force` aanpassing:**

```rust
// In src/index/mod.rs, index_with_options():
// Vóór de lock acquisitie, controleer of serve actief is voor deze repo.

if force {
    if let Some(serve_url) = detect_running_serve() {
        if let Some(alias) = find_repo_alias_in_serve(&serve_url, &db_path).await? {
            tracing::info!("codesearch serve is running — delegating --force reindex to serve");
            return delegate_reindex_to_serve(&serve_url, &alias).await;
        }
    }
    // Geen serve actief: normale --force rebuild
}
```

`detect_running_serve()` doet een snelle GET `/health` op de standaard poort
(39725). Als serve antwoordt, stuur de reindex opdracht door.

`delegate_reindex_to_serve()` poll GET `/repos/{alias}/reindex/{job_id}` elke
2 seconden en toont voortgang in de terminal totdat status `done` of `failed` is.

### Gebruikservaring na fix

```bash
# Serve draait, gebruiker voert uit:
$ codesearch index --force

🔗 codesearch serve is running — delegating reindex to serve...
⏳ Reindexing HUSQ.Aprimo (436 files)...
📦 Embedding 6612 chunks...
✅ Reindex complete (23.4s) — serve is using fresh index
```

### Locaties

- `src/serve/mod.rs` — nieuw `/repos/{alias}/reindex` endpoint + job tracking
- `src/index/mod.rs` — detect_running_serve + delegate logica vóór lock acquisitie
- `src/constants.rs` — endpoint path constante `REINDEX_ENDPOINT_PATH`

### Review-opmerkingen

- Job tracking kan simpel in-memory zijn (HashMap<Uuid, ReindexJob>) —
  geen persistentie nodig want reindex duurt zelden langer dan de serve uptime
- Concurrent reindex requests voor dezelfde alias afwijzen met 409 Conflict
- Als serve tijdens reindex crasht, is de repo mogelijk in inconsistente staat.
  Mitigatie: werk op een tijdelijke database copy, swap atomisch na voltooiing.
  Dit is optioneel voor v1 maar aanbevolen voor v2.

---

## 11. Definitief Definition of Done

### Root cause

Chunk IDs zijn uniek per database maar niet globaal. In serve_hub mode:
```
search(project="HUSQ.Aprimo") → chunk_id 23246
get_chunk(23246)               → serve zoekt eerste repo met dat id → BOIN.Aprimo ❌
```

### Fix — `project` parameter op `get_chunk` (optioneel, backwards compatible)

In `src/mcp/mod.rs`, de `get_chunk` tool definitie:

```rust
#[derive(Deserialize, JsonSchema)]
struct GetChunkRequest {
    chunk_id: u64,
    context_lines: Option<u32>,
    // NIEUW: optionele project parameter voor multi-repo routing
    project: Option<String>,
}
```

In de handler: als `project` aanwezig is, route direct naar die repo's database.
Als `project` afwezig is, gedraag zoals vandaag (eerste match — backwards compat).

In de tool description toevoegen:
```
In multi-repo (serve) mode: always pass `project` to ensure the correct
repository is queried. chunk_ids are local to each repository database.
```

Dit is de eenvoudigste fix en backwards compatible.

### Migratiepad voor dubbels

Een `codesearch index --force` op elke repo verwijdert alle historische snapshots
en elimineert dubbele chunk IDs aan de bron. Dit is de aanbevolen migratie voor
bestaande indexes vóór de upgrade naar multi-repo serve:

```bash
# Voor elke repo:
codesearch index --force /path/to/repo
```

Fix 1 (dedup op locatie) blijft nuttig als vangnet maar is na een --force rebuild
niet meer strikt noodzakelijk.

### Locaties

- `src/mcp/mod.rs` — get_chunk tool handler en request struct
- `src/serve/mod.rs` — routing logica voor project-scoped chunk lookup

---

## 12. Definition of Done

- [ ] `cargo check --all-targets` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test --lib` groen
- [ ] `cargo build --release` clean
- [ ] Test: literal + semantic search op HUSQ.Aprimo geeft geen dubbele resultaten
- [ ] Test: `explore(target="HUSQ.Aprimo/src/...", project="HUSQ.Aprimo")` werkt
- [ ] Test: `codesearch serve` met 10+ repos start zonder LMDB errors
- [ ] Test: `codesearch serve` op machine zonder repos.json ontdekt en vult auto
- [ ] Test: `get_chunk(chunk_id=X, project="HUSQ.Aprimo")` geeft correct HUSQ chunk
- [ ] Test: `get_chunk(chunk_id=X)` zonder project werkt nog (backwards compat)
- [ ] Test: `codesearch index --force` terwijl serve draait → delegeert naar serve
- [ ] Test: serve toont voortgang tijdens reindex, andere repos blijven beschikbaar

---

## 13. Niet in scope

- Globale chunk ID coordinatie (te complex, optie A is voldoende)
- UI voor repos.json beheer
- Atomische database swap bij reindex crash (v2)

---

## 14. Commit message voorstel

```
fix(serve): multi-repo quality — dedup, explore paths, sequential init,
           auto-discovery, get_chunk routing, reindex delegation

1. search: dedup on (path, start_line, end_line) — kills historical snapshot dupes
2. explore: strip project-alias prefix from target path
3. serve: sequential FSW startup, LMDB map-size consistency
4. serve: auto-discover .codesearch.db on empty repos.json
5. get_chunk: optional project param for multi-repo routing
6. index --force: detect running serve and delegate via POST /repos/{alias}/reindex
```
