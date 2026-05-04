# AGENTS.md — features/symbol-references

## Goal

Add symbol-aware reference lookups to codesearch. New MCP tool `find_impact(symbol)` should return file/line-precise references so agents can plan refactors with IDE-level accuracy instead of grep heuristics.

MVP scope is **C# only**. The architecture stays language-agnostic through a per-language `SymbolIndexer` adapter so future languages can plug in without redesigning.

## Todo

# Symbol-references merge-preparation — 7-fase blocker-fix

**Status:** ✅ Alle 7 fases geïmplementeerd, reviews gepasseerd ( resterende DoD-items vereisen push/handmatige test)  
**Prioriteit:** Vóór PR-merge naar develop  
**Spec:** [`REVIEW_features-symbol-references.md`](./REVIEW_features-symbol-references.md) — Eindverdict + top-down validatie zijn de bron-van-waarheid  
**Eigenaar:** OpenCode (uitvoerder) / Claude (planner)  
**Branch:** `features/symbol-references` (geen sub-branch — fixes direct op de feature branch zodat PR-history coherent blijft)

---

## 1. Probleem

De feature branch `features/symbol-references` voegt C# semantic search toe via een SCIP-pijplijn (Roslyn helper → JSON → LMDB → MCP). De review is gedaan in 9 chunks plus een onafhankelijke top-down validatie. Resultaat van de top-down pass: de blocker-telling van 13 oversimplificeert — feitelijk zijn er **~6-7 onafhankelijke onderwerpen**, omdat de "4 cache-blockers" eigenlijk **2 onderliggende issues op 2 problematische sites** zijn (site A in IndexManager is correct geïmplementeerd).

### 1.1 Blocker-clusters (na top-down validatie)

| Cluster | # blockers | Hoofdprobleem |
|---|---|---|
| Cache-architectuur (Issue α) | 2 (was: 4) | Sites B en C in `serve/mod.rs` + `mcp/mod.rs` maken eigen `SymbolIndexerRegistry` ipv IndexManager's bestaande gedeelde `Arc` te hergebruiken |
| `detect_helper` failure-cache (Issue β) | 1 | `Mutex<Option<PathBuf>>` cachet alleen `Some`, niet `None` → elke `is_available()` bij ontbrekende helper triggert verse PATH-lookup + subprocess |
| O(N) hot-paths | 2 | `find_references` (fuzzy fallback) + `find_references_by_position` doen full LMDB-scan + bincode-deserialize-all per query |
| JSON version-validatie | 1 | `metadata.version` in `scip_parse::JsonMetadata` heeft `#[allow(dead_code)]`, nooit gevalideerd → silent breakage bij helper-bump |
| Test-coverage C#-pijplijn | 6 | `tests/symbols_csharp_test.rs` zijn unit-tests vermomd als integration; `IndexerTests.cs` test `SymbolIndexer` niet; `OutputWriter`-test gebruikt nooit `OutputWriter`; lege test-body; misleidende namen; fixture ongebruikt |

Plus één 🟠 die als blocker behandeld wordt:

| Cluster | # majors | Hoofdprobleem |
|---|---|---|
| Bincode schema-versie | 1 | Eén refactor van `StoredReference` corrumpeert bestaande LMDB-state stilletjes (geen version byte) |

### 1.2 Bewijs

Direct geverifieerd in de top-down inspectie van het review:

- **Issue α**: `git show features/symbol-references:src/mcp/mod.rs` toont `let registry = crate::symbols::SymbolIndexerRegistry::new();` in `find_impact`. `git show features/symbol-references:src/serve/mod.rs` toont `let bg_reg = Arc::new(SymbolIndexerRegistry::new());` in `trigger_symbol_rebuild`. IndexManager doet het correct: `symbol_registry: Arc::new(SymbolIndexerRegistry::new())` éénmalig in constructor, gedeeld via `Arc::clone`.

- **Issue β**: in `csharp.rs::detect_helper`, regel `if resolved.is_some() { ... cache }` — bij `None` blijft de mutex leeg, dus volgende call doet opnieuw `resolve_helper_path()`.

- **O(N) hot-paths**: `csharp.rs::find_references` en `find_references_by_position` doen beide `let iter = symbols_db.iter(&rtxn)?; for result in iter { ... bincode::deserialize(value) ... }`. Position-variant deserialiseert élke entry zonder eerst op key te filteren — dus **duurder dan** de fuzzy-variant.

- **JSON version**: `scip_parse.rs::JsonMetadata` heeft `#[allow(dead_code)] version: String` en `parse_json_index()` raakt het veld nooit aan.

- **Test-coverage**: `tests/symbols_csharp_test.rs` invoket nooit `scip-csharp` als subprocess, schrijft nooit naar LMDB. `helpers/csharp/tests/IndexerTests.cs` bevat alleen `ScipModelTests` (test models, niet `SymbolIndexer`). De fixture `helpers/csharp/tests/Fixtures/SmallSolution/` bestaat maar wordt door geen enkele test geladen.

## 2. Oplossing

Een **gefaseerde aanpak** waarbij elke fase een geïsoleerd, testbaar onderwerp is. De volgorde is gekozen op:

- **Effort/risk-ratio**: laagste effort + hoogste risk-reduction eerst (fases 1-3, samen ~1u)
- **Architectural impact**: refactors die meerdere callsites raken vroeg (fase 4)
- **Hot-path optimization**: pas na correctheid (fases 5-6)
- **Test-infrastructuur**: laatste, dekt alle voorgaande fases impliciet (fase 7)

Eén commit per fase voor reviewability. Geen sub-branch — fixes direct op `features/symbol-references` om de PR-history coherent te houden.

### 2.1 Dataflow na de fix

```
WRITE-paden (rebuild → LMDB) — onveranderd:

  [1] Watcher (.cs change)              [2] HTTP /reindex?symbols=true
        ↓                                       ↓
   IndexManager.symbol_registry          serve handler reuses
   - Arc::new(Registry::new())           IndexManager.symbol_registry  ◄── FASE 4
     (single source of truth)              (geen eigen Registry::new())
   - debounce 60s
        ↓                                       ↓
        └────────► CSharpSymbolIndexer::rebuild() ◄────────┘
                    ├── detect_helper() — Mutex<Option<Option<PathBuf>>>  ◄── FASE 2
                    │   (cachet zowel success als failure)
                    ├── scip-csharp subprocess → JSON
                    ├── parse_json_index() — bail bij version != "1.0"  ◄── FASE 1
                    └── LMDB:
                         ├── symbols_db: Str → [v1 | bincode(refs)]  ◄── FASE 3
                         ├── scip_positions: Str → [v1 | bincode([keys])]  ◄── FASE 5
                         ├── scip_simple_names: Str → [v1 | bincode([keys])]  ◄── FASE 6
                         └── meta_db: ts + counts

READ-pad — onveranderd qua API, sneller intern:

  MCP find_impact request
        ↓
   CodesearchService.symbol_registry  ◄── FASE 4 (geen Registry::new() meer)
        ↓
   indexer.find_references()            indexer.find_references_by_position()
   - Exact match: O(1)                  - O(1) lookup in scip_positions  ◄── FASE 5
   - Fuzzy: O(1) via                    - Pick shortest match
     scip_simple_names  ◄── FASE 6      - Fall through naar find_references
```

## 3. Concrete wijzigingen per fase

### Fase 1: JSON version-validatie (~10 min)

**Bestand:** `src/symbols/scip_parse.rs`

Verwijder `#[allow(dead_code)]` op `JsonMetadata.version` en voeg validatie toe in `parse_json_index()`:

```rust
const SUPPORTED_INDEX_VERSION: &str = "1.0";

pub fn parse_json_index(data: &[u8]) -> Result<ScipIndex> {
    let index: JsonIndex = serde_json::from_slice(data)
        .with_context(|| "Failed to parse symbol index JSON")?;

    if index.metadata.version != SUPPORTED_INDEX_VERSION {
        bail!(
            "Unsupported scip-csharp index version: '{}' (expected '{}'). \
             The scip-csharp helper may need to be rebuilt.",
            index.metadata.version, SUPPORTED_INDEX_VERSION
        );
    }
    // ... bestaande logica blijft hetzelfde ...
}
```

**Test in `scip_parse.rs::tests`:**

```rust
#[test]
fn test_parse_json_index_rejects_unknown_version() {
    let json = r#"{"metadata":{"version":"2.0","tool_info":"x"},"documents":[],"external_symbols":[]}"#;
    let err = parse_json_index(json.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("Unsupported"), "got: {}", err);
}
```

### Fase 2: `detect_helper` failure-cache (~15 min)

**Bestand:** `src/symbols/csharp.rs`

Verander de cache-type zodat zowel `Some` als `None` resultaten gecached worden:

```rust
pub struct CSharpSymbolIndexer {
    /// Cached detection result.
    /// None = not yet attempted.
    /// Some(None) = attempted, helper not found.
    /// Some(Some(path)) = found at given path.
    helper_path: std::sync::Mutex<Option<Option<PathBuf>>>,
}

impl CSharpSymbolIndexer {
    pub fn new() -> Self {
        Self {
            helper_path: std::sync::Mutex::new(None),
        }
    }

    pub fn detect_helper(&self) -> Option<PathBuf> {
        {
            let lock = self.helper_path.lock().unwrap();
            if let Some(cached) = lock.as_ref() {
                return cached.clone();
            }
        }

        let resolved = self.resolve_helper_path();
        let mut lock = self.helper_path.lock().unwrap();
        *lock = Some(resolved.clone()); // cache zowel Some als None
        resolved
    }
    // resolve_helper_path() blijft onveranderd
}
```

**Geen breaking change** voor de trait. Geen extra unit-test nodig — observable effect is dat `tracing::debug!` precies één keer logt bij ontbrekende helper.

### Fase 3: Bincode schema-versie (~30 min)

**Bestand:** `src/symbols/csharp.rs`

Prefix elke LMDB-value met een version byte. Bij read: bail met heldere error inclusief rebuild-instructie als versie onbekend.

```rust
const STORED_REFERENCE_SCHEMA_VERSION: u8 = 1;

fn serialize_refs(refs: &[StoredReference]) -> Result<Vec<u8>> {
    let payload = bincode::serialize(refs)
        .with_context(|| "bincode serialize failed")?;
    let mut buf = Vec::with_capacity(1 + payload.len());
    buf.push(STORED_REFERENCE_SCHEMA_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

fn deserialize_refs(bytes: &[u8]) -> Result<Vec<StoredReference>> {
    if bytes.is_empty() {
        bail!("Empty stored value");
    }
    let version = bytes[0];
    if version != STORED_REFERENCE_SCHEMA_VERSION {
        bail!(
            "Unsupported stored reference schema version {} (expected {}). \
             Run `codesearch reindex --symbols` to rebuild.",
            version, STORED_REFERENCE_SCHEMA_VERSION
        );
    }
    bincode::deserialize(&bytes[1..])
        .with_context(|| "bincode deserialize failed")
}
```

Vervang alle `bincode::serialize(&stored)` → `serialize_refs(&stored)?` en alle `bincode::deserialize(value)` → `deserialize_refs(value)?` in:
- `rebuild()` (write path)
- `find_references()` (read paths — exact + fuzzy)
- `find_references_by_position()` (read paths)

**Migratie:** bestaande LMDB-state is incompatibel maar feature is in review (geen productiegebruik). Eerste rebuild herstelt automatisch. Documenteer in CHANGELOG.

**Tests:** twee unit-tests in `csharp.rs::tests` (nieuw module aan het einde van de file):

```rust
#[test]
fn test_serialize_refs_includes_version_byte() {
    let refs = vec![StoredReference {
        file: PathBuf::from("a.cs"), start_line: 1, end_line: 1, kind: "definition".into()
    }];
    let bytes = serialize_refs(&refs).unwrap();
    assert_eq!(bytes[0], STORED_REFERENCE_SCHEMA_VERSION);
}

#[test]
fn test_deserialize_refs_rejects_unknown_version() {
    let bytes = vec![99u8, 0, 0, 0];
    let err = deserialize_refs(&bytes).unwrap_err();
    assert!(err.to_string().contains("Unsupported"));
}
```

### Fase 4: Cache-architectuur refactor — Issue α (~1u 30min)

**Bestanden:** `src/serve/mod.rs`, `src/serve/state.rs` (of waar `AppState` gedefinieerd staat), `src/mcp/mod.rs`.

**Aanpak:** maak `Arc<SymbolIndexerRegistry>` deel van de gedeelde service-state, zodat IndexManager's bestaande registry hergebruikt wordt door HTTP- en MCP-handlers.

**4.1 `src/serve/mod.rs` + state**

Voeg `symbol_registry: Arc<SymbolIndexerRegistry>` toe aan `AppState` (of de equivalente service-state struct). In `serve()`-startup: clone het uit de IndexManager:

```rust
// Bij service-state constructie:
let symbol_registry = Arc::clone(&index_manager.symbol_registry);
let state = AppState {
    // ... bestaande fields ...
    symbol_registry,
};
```

Pas `trigger_symbol_rebuild` aan om de registry als parameter te accepteren in plaats van een eigen instantie te maken:

```rust
async fn trigger_symbol_rebuild(
    alias: &str,
    project_path: &Path,
    db_path: &Path,
    registry: Arc<SymbolIndexerRegistry>,  // NEW: from AppState
) {
    // body unchanged, gebruik `registry` ipv `bg_reg`
}
```

Update HTTP handler-callsites om `state.symbol_registry.clone()` mee te geven.

**4.2 `src/mcp/mod.rs`**

`CodesearchService` moet de registry kennen. Voeg `symbol_registry: Arc<SymbolIndexerRegistry>` toe aan de service struct, zet bij constructie (de service heeft al een referentie naar `IndexManager` of equivalente shared state — gebruik dezelfde route).

In `find_impact`:

```rust
// VOOR:
let registry = crate::symbols::SymbolIndexerRegistry::new();
// NA:
let registry = &self.symbol_registry;
```

**4.3 `src/index/manager.rs`**

Geen wijziging aan de constructor (dat is site A en is correct). Eventueel een `pub` modifier op `symbol_registry` als die nu private is, om externe access toe te staan.

**4.4 Verwijder `#[allow(dead_code)]` op constants**

In `src/constants.rs`: na fase 4 worden `SCIP_CSHARP_HELPER_ENV`, `SCIP_CSHARP_HELPER_NAME`, `HELPERS_SUBDIR`, `SCIP_CSHARP_DEBOUNCE_MS`, `SCIP_SYMBOLS_DB_NAME`, `SCIP_REBUILD_TIMESTAMP_KEY` allemaal vanuit `csharp.rs` referenced via `crate::constants::...`. Verwijder de `#[allow(dead_code)]` attributes.

**Verificatie:** `grep -rn "SymbolIndexerRegistry::new" src/` moet **exact 4 hits** retourneren na de refactor — één per canonieke aanmaak-site: `IndexManager::new()`, `IndexManager::new_for_path()`, `ServeState::new()` (serve-modus), `CodesearchService::new()` (standalone MCP-modus). Per-request instantiatie in `find_impact` en `trigger_symbol_rebuild` moet verdwenen zijn.


### Fase 5: `find_references_by_position` keyset-filter (~2u)

**Bestanden:** `src/symbols/csharp.rs` (+ nieuwe LMDB-tabel constant in `constants.rs`).

**Probleem:** huidige implementatie deserialiseert élke entry zonder eerst op key te filteren — duurder dan de fuzzy-fallback van `find_references`.

**Oplossing:** secundaire LMDB-tabel `scip_positions` mappend `(file:line)` op `[symbol_keys]`. Tijdens `rebuild` wordt voor elke definition-occurrence een entry geschreven. Bij position-lookup: O(1) get + dan deserialize alléén voor matching keys.

**Schema:**

```rust
// In src/constants.rs
pub const SCIP_POSITION_DB_NAME: &str = "scip_positions";

// In csharp.rs:
// scip_positions: Str -> Bytes
//   key: "<file>:<line>" (forward-slash genormaliseerd, 1-based line)
//   value: serialize_refs-equivalent voor Vec<String> — symbol-keys op die positie
```

Tijdens `rebuild()` (na de bestaande symbols_db.put loop):

```rust
let positions_db: Database<Str, Bytes> = env
    .create_database(&mut wtxn, Some(crate::constants::SCIP_POSITION_DB_NAME))?;
positions_db.clear(&mut wtxn)?;

// Build position index
let mut positions: HashMap<String, Vec<String>> = HashMap::new();
for (symbol_name, references) in index.iter() {
    for r in references.iter().filter(|r| r.kind == "definition") {
        let pos_key = format!(
            "{}:{}",
            r.file.to_string_lossy().replace('\\', "/"),
            r.start_line
        );
        positions.entry(pos_key).or_default().push(symbol_name.clone());
    }
}

for (key, keys) in &positions {
    let bytes = serialize_keys_v1(keys)?;  // simpele version-byte + bincode wrapper
    positions_db.put(&mut wtxn, key, &bytes)?;
}
```

Update `find_references_by_position()`:

```rust
fn find_references_by_position(&self, db_path: &Path, file: &Path, line: u32) -> Result<Vec<SymbolReference>> {
    let env = self.open_scip_env(db_path)?;
    let rtxn = env.read_txn()?;

    let positions_db: Database<Str, Bytes> = env
        .open_database(&rtxn, Some(crate::constants::SCIP_POSITION_DB_NAME))?
        .ok_or_else(|| anyhow::anyhow!("Position index not found. Rebuild required."))?;

    let pos_key = format!(
        "{}:{}",
        file.to_string_lossy().replace('\\', "/"),
        line
    );
    let candidate_keys: Vec<String> = match positions_db.get(&rtxn, &pos_key)? {
        Some(b) => deserialize_keys_v1(b)?,
        None => return Ok(vec![]),
    };

    // Pick shortest (most specific) symbol on this line
    let chosen = candidate_keys.iter().min_by_key(|k| k.len()).cloned();
    drop(rtxn);
    match chosen {
        Some(k) => self.find_references(db_path, &k),
        None => Ok(vec![]),
    }
}
```

### Fase 6: `find_references` fuzzy secondaire index (~2u 30min)

**Bestanden:** `src/symbols/csharp.rs` (+ nieuwe LMDB-tabel constant).

**Probleem:** fuzzy fallback doet O(N) full-table scan + bincode-deserialize-all.

**Oplossing:** secundaire `scip_simple_names: simple_name -> [full_keys]` waar `simple_name` is afgeleid van het canonieke SCIP symbool — laatste segment na `#` of `.`, e.g. `Validate` voor `csharp App . FieldDefinition#Validate().`.

**Schema:**

```rust
// In src/constants.rs
pub const SCIP_SIMPLE_NAMES_DB_NAME: &str = "scip_simple_names";
```

**Helper:**

```rust
fn extract_simple_name(scip_symbol: &str) -> String {
    // Strip trailing parens (e.g. "Validate()." → "Validate")
    let cleaned = scip_symbol.trim_end_matches('.').trim_end_matches("()");
    // Take last segment after '#' or '.'
    cleaned
        .rsplit(|c| c == '#' || c == '.')
        .next()
        .unwrap_or(cleaned)
        .trim()
        .to_string()
}
```

Vul tijdens `rebuild()` zoals fase 5: bouw een `HashMap<String, Vec<String>>` van simple_name → keys, schrijf naar `scip_simple_names`.

Vervang de O(N) iter-loop in `find_references()`:

```rust
fn find_references(&self, db_path: &Path, symbol: &str) -> Result<Vec<SymbolReference>> {
    let env = self.open_scip_env(db_path)?;
    let rtxn = env.read_txn()?;

    let symbols_db: Database<Str, Bytes> = env
        .open_database(&rtxn, Some(SCIP_DB_NAME))?
        .ok_or_else(|| anyhow::anyhow!("SCIP symbol database not found. Run a rebuild first."))?;

    // Exact match (fast path)
    if let Some(bytes) = symbols_db.get(&rtxn, symbol)? {
        let stored = deserialize_refs(bytes)?;
        return Ok(stored.into_iter().map(into_symbol_ref).collect());
    }

    // Fuzzy via simple-name index
    let simple_names_db: Database<Str, Bytes> = match env.open_database(&rtxn, Some(crate::constants::SCIP_SIMPLE_NAMES_DB_NAME))? {
        Some(db) => db,
        None => return Ok(vec![]),
    };

    let simple = extract_simple_name(symbol);
    let candidates: Vec<String> = match simple_names_db.get(&rtxn, &simple)? {
        Some(b) => deserialize_keys_v1(b)?,
        None => return Ok(vec![]),
    };

    let chosen = candidates.iter()
        .filter(|k| fuzzy_symbol_match(symbol, k))
        .min_by_key(|k| k.len())
        .cloned();

    drop(rtxn);
    match chosen {
        Some(k) => self.find_references(db_path, &k),
        None => Ok(vec![]),
    }
}
```

### Fase 7: Integration test + CI (~1d)

**Bestanden:** `tests/symbols_csharp_test.rs`, `Cargo.toml`, `.github/workflows/release.yml` (of dedicated test workflow).

**Stappen:**

1. **Cargo feature toevoegen** in `Cargo.toml`:

   ```toml
   [features]
   default = []
   csharp_helper_integration = []
   ```

2. **Schrijf één integration-test** gemarkeerd met `#[cfg_attr(...)]`:

   ```rust
   #[test]
   #[cfg_attr(not(feature = "csharp_helper_integration"), ignore)]
   fn test_csharp_pipeline_smallsolution_roundtrip() {
       use codesearch::symbols::{SymbolIndexer, csharp::CSharpSymbolIndexer, RebuildScope};
       use std::path::PathBuf;

       // Locate fixture
       let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
           .join("helpers/csharp/tests/Fixtures/SmallSolution");
       assert!(fixture_root.join("SmallSolution.sln").exists(),
           "Fixture not found at {}", fixture_root.display());

       // Locate helper (CODESEARCH_SCIP_CSHARP env var or relative to target/)
       let helper = std::env::var("CODESEARCH_SCIP_CSHARP")
           .map(PathBuf::from)
           .or_else(|_| {
               let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                   .join("helpers/csharp/bin/Release/net10.0/scip-csharp");
               if candidate.exists() { Ok(candidate) } else { Err(()) }
           })
           .expect("scip-csharp helper not found. Set CODESEARCH_SCIP_CSHARP or build helper.");
       std::env::set_var("CODESEARCH_SCIP_CSHARP", &helper);

       // Setup tempdir for LMDB
       let tmp = tempfile::tempdir().unwrap();
       let db_path = tmp.path();

       // Rebuild
       let indexer = CSharpSymbolIndexer::new();
       assert!(indexer.is_available(), "Helper detection failed");
       let summary = indexer.rebuild(&fixture_root, db_path, RebuildScope::Full)
           .expect("rebuild failed");
       assert!(summary.symbols_indexed > 0, "No symbols indexed");

       // Query: Calculator.Add should have ≥2 occurrences (definition + 1 reference)
       let refs = indexer.find_references(
           db_path,
           "csharp SmallSolution.Library . Calculator#Add(int, int)."
       ).expect("find_references failed");
       assert!(refs.len() >= 2, "Expected ≥2 refs for Calculator.Add, got {}", refs.len());

       // Position-based lookup
       let pos_refs = indexer.find_references_by_position(
           db_path,
           &PathBuf::from("Library/Calculator.cs"),
           8  // line of Add definition (verify against fixture)
       ).expect("find_references_by_position failed");
       assert!(!pos_refs.is_empty(), "Position lookup returned empty");
   }
   ```

3. **Cleanup misleidende tests** in `tests/symbols_csharp_test.rs`:
   - Verwijder `test_parse_json_index_fuzzy_symbol_match` (lege body — heeft geen waarde)
   - Verwijder `test_symbol_reference_conversion` (struct-test zonder waarde)
   - Hernoem `test_lmdb_round_trip` → `test_indexer_returns_empty_when_db_missing` (eerlijke naam voor wat het test)

4. **CI-job toevoegen** voor de feature-flagged test. Aparte job in `.github/workflows/release.yml` (of nieuwe `.github/workflows/integration-tests.yml`):

   ```yaml
   csharp-integration-tests:
     runs-on: ubuntu-latest
     steps:
       - uses: actions/checkout@v4
       - uses: actions/setup-dotnet@v4
         with:
           dotnet-version: '10.0.x'
       - name: Build helper
         run: |
           cd helpers/csharp
           dotnet publish -c Release --no-self-contained -o bin/Release/net10.0
       - uses: dtolnay/rust-toolchain@stable
       - name: Run integration tests
         run: |
           export CODESEARCH_SCIP_CSHARP="$PWD/helpers/csharp/bin/Release/net10.0/scip-csharp"
           cargo test --features csharp_helper_integration -- --include-ignored
   ```

   Maakt een aparte job zodat kale Rust-CI niet trager wordt door dotnet-build (60-90s extra).

## 4. Schema / DTOs

Twee nieuwe LMDB-tabellen + één version-byte protocol op alle waarde-encodings:

```rust
// src/constants.rs
pub const SCIP_POSITION_DB_NAME: &str = "scip_positions";
pub const SCIP_SIMPLE_NAMES_DB_NAME: &str = "scip_simple_names";

// src/symbols/csharp.rs (private)
const STORED_REFERENCE_SCHEMA_VERSION: u8 = 1;
const KEYS_LIST_SCHEMA_VERSION: u8 = 1;
```

**Tabellen:**

| Tabel | Key | Value | Schrijver | Lezer |
|---|---|---|---|---|
| `scip_symbols` (bestaand) | full SCIP key | `[v=1, bincode(Vec<StoredReference>)]` | `rebuild` | `find_references` exact + fuzzy |
| `scip_positions` (nieuw) | `<file>:<line>` (forward-slash, 1-based) | `[v=1, bincode(Vec<String>)]` | `rebuild` | `find_references_by_position` |
| `scip_simple_names` (nieuw) | last segment of canonical symbol | `[v=1, bincode(Vec<String>)]` | `rebuild` | `find_references` fuzzy fallback |
| `scip_meta` (bestaand) | `last_rebuild_ts`, `symbol_count` | `Str` | `rebuild` | `index_age` |

Beide nieuwe tabellen worden vol-overschreven bij elke `rebuild()` (met `clear()` vóór de schrijf-loop).

## 5. Tests

**Unit tests** (`cargo test`, default features, snel):

- `scip_parse::tests::test_parse_json_index_rejects_unknown_version` — fase 1
- `csharp::tests::test_serialize_refs_includes_version_byte` — fase 3
- `csharp::tests::test_deserialize_refs_rejects_unknown_version` — fase 3
- `csharp::tests::test_extract_simple_name` — fase 6 (verifieert bv. `extract_simple_name("csharp App . Foo#Bar().")` == `"Bar"`)

**Integration tests** (`cargo test --features csharp_helper_integration -- --include-ignored`, langzaam, opt-in):

- `tests::test_csharp_pipeline_smallsolution_roundtrip` — fase 7 (subprocess + LMDB + alle drie de query-paths)

**Handmatige tests** (na voltooiing, op echte client repo):

1. Build feature branch + helper, `cargo build` (niet `--release`). Verifieer geen `#[allow(dead_code)]` warnings meer in `constants.rs`.
2. Run `codesearch serve` op een Aprimo client repo. Eerste MCP `find_impact`-call moet werken zonder PATH-spam in tracing logs.
3. Tweede en derde call: latency moet sub-100ms zijn (was: seconden bij O(N) fuzzy fallback).
4. Verifieer position-lookup: `{"file": "src/X.cs", "line": 42}` returnt direct, geen full scan in logs.
5. `grep -rn "SymbolIndexerRegistry::new" src/` moet exact 4 hits zijn (IndexManager::new, IndexManager::new_for_path, ServeState::new, CodesearchService::new).

## 6. Edge cases

| Scenario | Verwacht gedrag |
|---|---|
| Helper niet geïnstalleerd, eerste `is_available()` call | Returnt `false`, log één keer op `tracing::debug`, gecached. Vervolg-calls = 0 detectie-werk. |
| Helper geïnstalleerd later (dev-flow), restart codesearch nodig | Acceptabel voor v1. TTL-cache als follow-up. |
| LMDB bevat oude bincode (v0, geen version byte) | `deserialize_refs` faalt met heldere error met rebuild-instructie. User runt `?force=true&symbols=true`. |
| Rebuild parallel via watcher én HTTP `?symbols=true` | Geen guard nu. Issue α-fix verandert dit niet. Follow-up: per-repo mutex. |
| Helper schrijft `version: "2.0"` (toekomstige bump) | `parse_json_index` faalt met heldere error. User updatet codesearch. |
| Position-lookup op file zonder definities | Lege Vec, geen error. |
| Position-lookup op pathseparator-mismatch (Windows backslash input) | `replace('\\', "/")` op de query-kant. Test met expliciete backslash input. |
| Simple-name lookup met prefix-collision (bv. `Validate` matcht 5 symbolen) | Filter via `fuzzy_symbol_match`, kies shortest key. Documenteer in code-comment. |
| Helper crashes mid-rebuild | Bestaande `if !output.status.success()` warning + `read(output_path)` fail. Verbetering uit chunk 5 niet in scope hier (volg-up). |
| `extract_simple_name("")` | Returns `""`. Caller filtert lege simple-names tijdens index-build. |

## 7. Definition of Done

- [x] **Fase 1:** JSON version-validatie geïmplementeerd, unit-test groen
- [x] **Fase 2:** `detect_helper` cachet ook negatieve resultaten (handmatig getest met ontbrekende helper)
- [x] **Fase 3:** bincode payload heeft version byte, alle serialize/deserialize sites geüpdate, twee unit-tests groen
- [x] **Fase 4:** Sites B en C in `serve/mod.rs` en `mcp/mod.rs` hergebruiken `IndexManager.symbol_registry` via service-state
- [x] **Fase 4:** `grep -rn "SymbolIndexerRegistry::new" src/` returnt exact 4 hits (IndexManager×2, ServeState, CodesearchService)
- [x] **Fase 4:** `#[allow(dead_code)]` weg van alle 6 SCIP-constants in `constants.rs`
- [x] **Fase 5:** `scip_positions` LMDB-tabel werkt; `find_references_by_position` doet O(1) lookup (verifieer met tracing op fixture-repo)
- [x] **Fase 6:** `scip_simple_names` LMDB-tabel werkt; `find_references` fuzzy fallback doet O(1) lookup
- [x] **Fase 7:** Cargo feature `csharp_helper_integration` toegevoegd; integration-test groen onder die feature lokaal
- [ ] **Fase 7:** CI-job voor de feature toegevoegd, eerste run groen op GitHub Actions *(na push te verifiëren)*
- [x] **Fase 7:** 3 misleidende/lege tests in `tests/symbols_csharp_test.rs` opgeruimd
- [x] `cargo check` zonder warnings
- [x] `cargo clippy` zonder warnings (of expliciet gemotiveerde `#[allow]`)
- [x] `cargo test` (default features) groen
- [x] CHANGELOG.md bijgewerkt onder `[Unreleased]` met de 7 blocker-fixes
- [x] AGENTS.md status-sectie bijgewerkt: alle fases ✅
- [ ] `REVIEW_features-symbol-references.md` heeft een afsluitende sectie "Fixes toegepast" die per fase linkt naar de commit-SHA *(nog te doen)*
- [ ] Handmatige eindtest op een echte Aprimo client repo: 2e en 3e MCP `find_impact` call < 100ms

## 8. Niet in scope

Deze plan-doc dekt alléén de blockers + de bincode-major. Niet meegenomen:

- 49 minors uit de review — apart in volgende `AGENTS_*.md`
- Andere majors: rebuild scope altijd `Full`, sequentiele text+symbol rebuild, git rev-parse subprocess per poll, LMDB `map_size` hardcoded → apart follow-up
- `scip_parse.rs` hernoemen naar `helper_output.rs` — cosmetisch, na merge
- AGENTS.md merge-strategie naar develop (`merge=ours` hook) — apart proces-onderwerp
- Performance-tuning van de C# helper zelf (parallel `FindReferencesAsync`) — hoort in helper-code, geen Rust-side fix
- Multi-language support (Python/TS adapters) — toekomstige feature
- ONNX arena allocator memory bloat — bekende upstream limitatie, niet deze branch
- Rate-limiting op `?symbols=true` — apart correctness-en-DOS-onderwerp
- `find_csproj_for_file` redundant condition fix — minor uit chunk 5, post-merge

Reden voor scope-discipline: deze branch is in PR-review. Scope-creep zou de PR oneindig vertragen. Eerst blockers weg, dan mergen, dan follow-ups in volgende iteratie.

## 9. Implementatietijd (schatting)

| Fase | Schatting | Cumulatief |
|---|---|---|
| Fase 1 — JSON version-validatie | 10 min | 0u 10min |
| Fase 2 — detect_helper failure-cache | 15 min | 0u 25min |
| Fase 3 — Bincode schema-versie | 30 min | 0u 55min |
| Fase 4 — Cache-architectuur refactor | 1u 30min | 2u 25min |
| Fase 5 — Position keyset-filter | 2u | 4u 25min |
| Fase 6 — Simple-name secondaire index | 2u 30min | 6u 55min |
| Fase 7 — Integration test + CI | 1d (8u) | 14u 55min |
| **Totaal** | **~15 uur (1.5-2 werkdagen)** |  |

Schatting includeert tests-schrijven, niet uitgebreide code-review. CI-tweaks voor fase 7 kunnen 1-2 uur extra kosten afhankelijk van .NET 10 setup-snelheid.

## 10. Review-opmerkingen

- **Issue α was eerst geclassificeerd als 4 blockers, niet 2.** OpenCode kan in twijfel raken bij de discrepantie met het review-document. Dit plan reflecteert de **top-down validatie** (sectie "Top-down architecturale inspectie" in REVIEW), niet de eerste chunked telling.
- **Site A in `IndexManager` is correct — niet aanraken.** Het patroon `Arc::new(SymbolIndexerRegistry::new())` éénmalig in de constructor, gedeeld via `Arc::clone`, is goed. Sites B en C moeten dat patroon volgen, niet hun eigen instantie maken.
- **Fase 4 raakt service-architectuur** — verifieer met sanity-check na refactor: `grep -rn "SymbolIndexerRegistry::new" src/` moet exact 4 hits retourneren (IndexManager::new, IndexManager::new_for_path, ServeState::new, CodesearchService::new). Per-request instantiatie moet verdwenen zijn.
- **Fase 5 en 6 zijn LMDB-schema-wijzigingen** — gecombineerd met fase 3's version byte zal eerste run na fix automatisch een full rebuild forceren omdat oude bincode-values geen version byte hebben. Documenteer dat in CHANGELOG zodat users niet schrikken.
- **Fase 6's `extract_simple_name`** is heuristisch — voor gewone C#-symbolen werkt "laatste segment na `#` of `.`" goed, maar generic types (`Container<T>`) en explicit interface implementations (`Foo.IBar.Method`) kunnen vreemd zijn. Begin met de simpele heuristiek; itereer als test fixtures iets onthullen. Voeg in de fuzzy-fallback altijd een `fuzzy_symbol_match`-filter toe als safety net.
- **Fase 7's `dotnet publish` in CI** kan tot 60-90s extra builddtijd kosten. Aparte job (parallel) zodat kale Rust-CI niet trager wordt — zie de YAML-snippet in fase 7.
- **Niet alle blockers vereisen aparte commits.** Fase 1+2+3 kunnen één commit zijn ("fix: address review blockers — version validation, helper failure cache, schema versioning"). Fase 4-7 elk hun eigen commit voor reviewability.
- **Branch-strategie:** als de review-PR al open is op GitHub, push na elke fase voor incrementele review-feedback. Anders alle fases lokaal en in één keer pushen.
- **Het is OK om fase 5 en 6 te swappen** als test-data laat zien dat fuzzy-lookups in de praktijk vaker voorkomen dan position-lookups. De gekozen volgorde (5 voor 6) komt uit het feit dat position-lookup correctness-kritischer is (chunk 5's bevinding dat het duurder is dan fuzzy is verrassend en suggereert dat het minder vaak gebruikt is dan ontworpen).

## 11. Commit messages (per fase)

**Fase 1:**

```
fix(symbols): validate scip-csharp index JSON version

Adds explicit check for metadata.version == "1.0" in parse_json_index.
Previously the field was parsed but #[allow(dead_code)], silently
accepting any version including breaking-change futures.

Refs: REVIEW_features-symbol-references.md (blocker chunk 5)
```

**Fase 2:**

```
fix(symbols): cache detect_helper negative results

CSharpSymbolIndexer.detect_helper now caches both Some and None.
Previously failures triggered fresh PATH-lookup + subprocess on every
is_available() call — degrading every MCP find_impact request when
helper is missing.

Refs: REVIEW_features-symbol-references.md (blocker chunk 5)
```

**Fase 3:**

```
fix(symbols): add schema version byte to stored references

Prefixes bincode payload with u8 version. Reads bail with rebuild
instruction on unknown version. Prevents silent LMDB corruption when
StoredReference shape evolves.

BREAKING (internal): existing scip LMDB state requires rebuild.
First reindex after upgrade triggers automatic rebuild.

Refs: REVIEW_features-symbol-references.md (major chunk 5, escalated to blocker)
```

**Fase 4:**

```
refactor(symbols): share SymbolIndexerRegistry across services

Move Arc<SymbolIndexerRegistry> ownership from per-call to
service-state. find_impact (mcp/mod.rs) and trigger_symbol_rebuild
(serve/mod.rs) now reuse IndexManager's registry instead of creating
fresh instances per request, restoring helper-detection cache effectiveness.

Removes #[allow(dead_code)] on six constants in constants.rs that are
now actively referenced from csharp.rs.

Refs: REVIEW_features-symbol-references.md (blockers chunks 6, 7 — top-down clustered)
```

**Fase 5:**

```
perf(symbols): O(1) position lookup via secondary LMDB index

Adds scip_positions table mapping (file, line) -> [symbol_keys].
find_references_by_position now does a direct lookup instead of
iterating all symbols and deserializing every value.

Refs: REVIEW_features-symbol-references.md (blocker chunk 5)
```

**Fase 6:**

```
perf(symbols): O(1) fuzzy lookup via simple-name index

Adds scip_simple_names table mapping last-segment identifier ->
[full_keys]. find_references fuzzy fallback now consults this index
instead of iterating all symbols.

Refs: REVIEW_features-symbol-references.md (blocker chunk 5)
```

**Fase 7:**

```
test(symbols): add gated integration test for C# pipeline

Adds tests/symbols_csharp_test.rs::test_csharp_pipeline_smallsolution_roundtrip
behind cargo feature `csharp_helper_integration`. Exercises full pipeline:
scip-csharp subprocess → JSON → LMDB → find_references + find_references_by_position.

CI runs feature-flagged tests in a separate job that builds the helper
via dotnet publish on Linux.

Removes 3 misleading tests from prior commits (empty body, struct-only
construction test, "lmdb_round_trip" that did no roundtrip).

Refs: REVIEW_features-symbol-references.md (blockers chunks 3, 9)
```

---

**Bron:** [`REVIEW_features-symbol-references.md`](./REVIEW_features-symbol-references.md) — sectie "Eindverdict" + "Top-down architecturale inspectie".


## Implemented Features

- None yet — this branch is still the C# symbol-references implementation plan.

## Architecture

### Per-language adapter pattern

`src/symbols/` hosts the adapter layer:

```rust
trait SymbolIndexer: Send + Sync {
    async fn rebuild(&self, repo: &Repo, scope: RebuildScope) -> Result<RebuildSummary>;
    fn find_references(&self, repo: &Repo, symbol: &CanonicalSymbol) -> Result<Vec<Reference>>;
}

enum RebuildScope {
    Full,
    Project(PathBuf),
    Files(Vec<PathBuf>),
}
```

`mod.rs` owns dispatch, `csharp.rs` owns the C# adapter, and `scip_parse.rs` wraps the `scip` crate.

### C# helper

`helpers/csharp/` is a small stateless CLI wrapper around Roslyn `SymbolFinder.FindReferencesAsync()`. It runs as a subprocess and writes SCIP output.

CLI shape:

```text
scip-csharp index --solution path\to\X.sln --output path\to\index.scip [--project path\to\Y.csproj]
```

Behavior:
- register MSBuild with `MSBuildLocator`
- load the solution or project with `MSBuildWorkspace`
- collect methods, properties, classes, interfaces, and fields
- serialize results to SCIP
- exit 0 even on partial compilation; warnings are acceptable, crashes are not

### Storage and query flow

- LMDB table: `scip_symbols`
- key: canonical SCIP symbol string
- value: serialized references (`file_path`, `start_line`, `end_line`, `kind`)
- query input can be `FieldDefinition.Validate` or `file:line`; resolve to canonical SCIP symbol first, then look up references

### Rebuild trigger

The watcher should debounce `.cs` changes for 60 seconds, then rebuild via the helper using `RebuildScope::Project` when possible, otherwise `RebuildScope::Full`.

Rebuilds run in the background. BM25/vector search keep working, and `find_impact` may return the older index with `index_age_seconds` while a rebuild is in progress.

### MCP tool: `find_impact`

Inputs:
- `{ "symbol_name": "FieldDefinition.Validate", "project": "boin-aprimo" }`
- `{ "file": "src/Validation/FieldDefinition.cs", "line": 42, "project": "boin-aprimo" }`

Response shape:

```json
{
  "symbol": "csharp . . . FieldDefinition#Validate().",
  "references": [{ "file": "...", "start_line": 87, "end_line": 87, "kind": "call" }],
  "index_age_seconds": 12,
  "language": "csharp",
  "scope": "project:boin-aprimo"
}
```

If the helper is missing, the index is unavailable, or the language is unsupported, return a structured error with `available_languages` and `hint_for_agent`.


## CI / Release changes

Release output grows from 3 archives to 6: the existing platform archives plus `-with-csharp` variants for Windows, Linux, and macOS.

Each platform job should:
1. install .NET 10 (`actions/setup-dotnet@v4`)
2. publish `helpers/csharp` framework-dependent (`--no-self-contained`)
3. stage the Rust binary plus helper output into `helpers/csharp/`
4. pack the `-with-csharp` archive next to the existing one

macOS arm64 stays opt-in via `include_macos: false`.

## Helper detection at runtime

Lookup order:
1. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]`
2. `PATH` lookup for `scip-csharp`
3. `CODESEARCH_SCIP_CSHARP`

If nothing is found and a C# repo is registered, log one warning and keep the rest of codesearch working.

## Quality gates

- `cargo check` clean
- `cargo clippy --all-targets --all-features -- -D warnings` clean
- `cargo test --lib --bins` pass
- `dotnet test helpers/csharp/` pass
- CI produces all 6 archives
- manual `find_impact` validation against Visual Studio references
- manual rebuild trigger validation after `.cs` edits
- manual warning-path validation on the kale variant

## Out of scope

- languages other than C#
- LSP-based live mode
- Stack Graphs, Kythe, or Glean
- interactive HTML graph viewer
- per-symbol incremental SCIP merge
- native AOT for the helper
- cross-language reference graphs

## Branch flow

```powershell
# already on features/symbol-references (branched from develop)
# implement, test, commit incrementally
git push origin features/symbol-references

# when done: PR features/symbol-references → develop
```

## Notes for OpenCode

- This is a Rust + C# change; `cargo` and `dotnet` do not interfere.
- Rust uses the Sourcegraph `scip` crate for protobuf decoding.
- C# uses `Microsoft.CodeAnalysis` >= 4.13, `Microsoft.Build.Locator`, and `Google.Protobuf`.
- Roslyn may yield partial output on compilation failures; that is expected.
- `scip-csharp` is stateless and runs once per indexing request.
- Symbol resolution should try exact match first, then a pragmatic fuzzy match.

When in doubt, prefer the simpler ship-ready path.
