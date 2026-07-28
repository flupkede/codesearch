# PLAN — TypeScript SCIP-indexering (find_impact + call-graph voor TS)

> **Status:** PLANNING — geen code geschreven. Dit document is het oppakpunt voor de implementatie.
> **Doel:** `find_impact` en de call-graph voor TypeScript (.ts/.tsx) laten werken zoals nu voor C#,
> door de bestaande C#-SCIP-pijplijn te spiegelen met Sourcegraph `scip-typescript`.
> **Branch-target:** PRs tegen `develop` (zie AGENTS.md gitflow).

---

## 1. Doel & scope

**In scope**
- `TypeScriptSymbolIndexer` implementeert het bestaande `SymbolIndexer`-trait, gevoed door `scip-typescript` (Sourcegraph, npm CLI).
- `find_impact` (MCP-tool) routeert `.ts`/`.tsx`/`.mts`/`.cts` bestanden naar de TS-indexer.
- Single-pass indexering: `rebuild()` schrijft defs **en** refs in één run naar LMDB (geen two-phase lazy model nodig — zie §3).
- File-watcher pakt `.ts`/`.tsx`-wijzigingen op en triggert een TS-debounced rebuild.
- Tests bewijzen dat `find_impact` op een TS-symbool alle call-sites teruggeeft.

**Out of scope (follow-up)**
- TS in de release-bundel shippen (`-with-ts` archive / `helpers/typescript/`) — optioneel, scip-typescript is een npm-package dus `npx` volstaat op de host.
- Incrementele `RebuildScope::Files` voor TS (single-pass maakt full-rebuild op kleine/ middelgrote repo's al snel genoeg; incrementeel is een latere optimalisatie).

---

## 2. Hoe C#-SCIP nu werkt (baseline voor spiegeling)

De TS-feature moet dezelfde raakvlakken gebruiken. Dit is de C#-status quo:

### 2.1 Trait + registry (`src/symbols/mod.rs`)
- **Trait `SymbolIndexer`** (regels 113-168): `language()`, `rebuild(repo_path, db_path, RebuildScope)`, `find_references(db_path, symbol)`, `find_references_by_position(db_path, file, line)`, `index_age()`, `is_available()`, `has_index()`, `applies_to(repo_path)`, `as_any()`.
- **`SymbolIndexerRegistry`** (regels 172-232): houdt `Vec<Box<dyn SymbolIndexer>>`. `new()` (regel 181) registreert **uitsluitend** `CSharpSymbolIndexer::new()`. `get(language)` is case-insensitive. Methodes: `available_languages()`, `installed_languages()` (filter op `is_available()`), `has_index_for()`, `indexed_languages()`.
- **Gedeelde types:** `SymbolReference{file,start_line,end_line,kind}`, `FindImpactResult`, `SymbolIndexError`, `RebuildScope` (Full | Project(PathBuf) | Files{changed,deleted}), `RebuildSummary`, `PrewarmSummary`.

### 2.2 C#-adapter (`src/symbols/csharp.rs`, 1740 regels)
`struct CSharpSymbolIndexer` implementeert het trait:
- `detect_helper()` / `resolve_helper_path()` / `validate_helper_path()` (239-353) — zoekt `scip-csharp` via env `CODESEARCH_SCIP_CSHARP` of `helpers/csharp/`.
- `find_solution(repo)` / `find_csproj_for_file(repo, file)` (355-391) — entrypoint-detectie (.sln/.csproj).
- `open_scip_env(db_path)` (398-429) — opent LMDB env in `db_path/scip/`, pre-createert 5 named DBs.
- `invoke_index_helper(...)` (433-504) — spawnt `scip-csharp index --solution X --output Y [--filter-project Z]`.
- `invoke_find_refs_helper()` (509-609), `invoke_batch_find_refs_helper()` (954-1033) — **lazy ref-resolutie** subcommands.
- Trait-impl (1124-1641): `language()`="csharp", `applies_to()` checkt .sln/.csproj, `is_available()` checkt `detect_helper()`.

### 2.3 Two-phase lazy reference model (C#-specifiek — TS doet dit ANDERS)
1. `rebuild()` → `scip-csharp index` emit **alleen definities** → snel.
2. `find_references()` resolvet refs on-demand: defs uit LMDB → cache-check → cache-miss → `scip-csharp find-refs` voor dat symbool → cache resultaat.
3. Pre-warm: `scip-csharp batch-find-refs` resolvet alle refs in één workspace-sessie.
- **C# helper output = custom JSON** (NIET standaard SCIP protobuf), geparseerd door `parse_json_index` in `scip_parse.rs` (regels 136-197).

### 2.4 LMDB-schema (`db_path/scip/`, 5 named DBs — keys namespaced door SCIP-symbol-scheme taal-prefix)
| DB | key | value |
|----|-----|-------|
| `scip_symbols` | full SCIP symbol | bincode `Vec<StoredReference>` |
| `scip_meta` | `"last_rebuild_ts"` | timestamp — **let op: NIET per-taal!** |
| `scip_positions` | `"file:line"` | `Vec<symbol_keys>` |
| `scip_simple_names` | simple name | `Vec<full_keys>` |
| `scip_ref_cache` | symbol | bincode `Vec<StoredReference>` |

### 2.5 Dispatch-punten die vandaag HARDCODED op C# staan (moeten generaliseren of een TS-tak krijgen)
| Locatie | Regel | Wat het doet | Voor TS |
|---------|-------|--------------|---------|
| `src/mcp/mod.rs` find_impact | 6296 | file-ext → language: alleen `"cs"` | voeg `"ts"/"tsx"/"mts"/"cts"` → LANG_TYPESCRIPT |
| `src/index/manager.rs` tracking | 1124, 1138, 1152 | trackt `.cs` modified/deleted/rename | voeg `.ts`/`.tsx` tracking toe |
| `src/index/manager.rs` debounce-flush | 1236 | `reg.get(LANG_CSHARP)` (hardcoded) | dispatch generiek over registry OF parallelle TS-tak |
| `src/index/manager.rs` notifier-type | — | `CSharpRebuildNotifier` callback | generaliseer of `TsRebuildNotifier` |
| `src/serve/mod.rs` Phase-3 pre-warm | 1045 | `symbol_registry.get(LANG_CSHARP)` | pre-warm loop over alle registry-talen |
| `src/serve/mod.rs` status | — | `CSharpIndexStatus::None/Ready` | generaliseer naar per-taal status-map |

---

## 3. Het TS-pad: spiegelen met scip-typescript

### 3.1 Kritieke verschillen met C#
| Aspect | C# (scip-csharp) | TS (scip-typescript) |
|--------|------------------|----------------------|
| **Output-formaat** | custom JSON | **standaard SCIP protobuf `.scip`** |
| **Referentiemodel** | two-phase lazy (defs dan refs) | **single-pass** (defs + refs samen) |
| **Entrypoint** | `.sln` / `.csproj` | `tsconfig.json` |
| **Runtime** | self-contained .NET exe | Node CLI: `npx scip-typescript index` |
| **find_references** | on-demand subprocess + cache | **alleen LMDB-lees** (geen subprocess) |

### 3.2 Consequentie voor de implementatie
1. **Protobuf-parse nodig.** scip-typescript is fixed binary-formaat → optie B (eigen TS-helper die JSON emit) is niet haalbaar. Keuze: de `scip` Rust-crate (Sourcegraph) toevoegen + een parser in nieuw `src/symbols/scip_proto.rs` die `.scip` → zelfde `ScipIndex`-shape mapt als `scip_parse.rs` nu voor JSON doet. Daarna is alle storage/resolution-code herbruikbaar.
2. **Geen two-phase.** TS `rebuild()` vult in één pass `scip_symbols` + `scip_positions` + `scip_simple_names` én de refs. `find_references()` leest alleen LMDB (snel, geen subprocess). `scip_ref_cache` is voor TS leeg/overbodig — schrijven kan geen kwaad (keys namespaced).
3. **`is_available()`** voor TS = detecteer of `scip-typescript` oplosbaar is via env `CODESEARCH_SCIP_TYPESCRIPT` (pad naar binary) of via `npx` op PATH + Node aanwezig.
4. **`applies_to()`** voor TS = zoek een `tsconfig.json` in `repo_path` (root of één niveau diep).

---

## 4. Betrokken files & functies (concreet)

### 4.1 Nieuwe files
| File | Inhoud |
|------|--------|
| `src/symbols/scip_proto.rs` | `parse_scip_protobuf(bytes) -> ScipIndex` via `scip` crate. Herbruikt `ScipReference`/`ScipIndex` uit `scip_parse.rs`. |
| `src/symbols/typescript.rs` | `struct TypeScriptSymbolIndexer` impl `SymbolIndexer`. Mirrot van `csharp.rs` structuur: `detect_helper()`, `find_tsconfig(repo)`, `open_scip_env()` (hergebruik), `invoke_index_helper()`, trait-impl. |
| `tests/symbols_typescript_test.rs` | Gated integratie-test (zelfde gate-patroon als `symbols_csharp_test.rs`), TS-fixture. |
| `tests/fixtures/ts-sample/` | Klein TS-project: `tsconfig.json` + 2-3 `.ts` files met een functie + call-sites. |

### 4.2 Te wijzigen files (exacte raakvlakken)
| File | Wijziging |
|------|-----------|
| `Cargo.toml` | voeg `scip` dependency toe (Sourcegraph crate) |
| `src/symbols/mod.rs` regel 181 | `SymbolIndexerRegistry::new()` registreer óók `typescript::TypeScriptSymbolIndexer::new()` |
| `src/symbols/mod.rs` | voeg `pub mod typescript;` + `pub mod scip_proto;` toe |
| `src/constants.rs` | `LANG_TYPESCRIPT="typescript"`, `SCIP_TYPESCRIPT_HELPER_ENV="CODESEARCH_SCIP_TYPESCRIPT"`, `SCIP_TYPESCRIPT_HELPER_NAME="scip-typescript"`, `TS_DEBOUNCE_MS` |
| `src/mcp/mod.rs` regel 6296 | find_impact auto-detect: map `"ts"/"tsx"/"mts"/"cts"` → `LANG_TYPESCRIPT` |
| `src/mcp/mod.rs` regel 6236 | update tool-description (nu: "C# today") → voeg TS toe |
| `src/index/manager.rs` regels 1124/1138/1152 | voeg `.ts`/`.tsx`-tracking velden toe (`ts_files_modified/deleted/last_event_time`) |
| `src/index/manager.rs` regel 1236 | dispatch: óf registry-loop, óf parallelle TS-tak na C#-tak |
| `src/index/manager.rs` notifier | generaliseer `CSharpRebuildNotifier` naar generieke `SymbolRebuildNotifier` (boxed callback) |
| `src/serve/mod.rs` regel 1045 | Phase-3 pre-warm: itereren over registry in plaats van hardcoded `LANG_CSHARP` |
| `src/serve/mod.rs` | vervang `CSharpIndexStatus` door `HashMap<String, IndexStatus>` (per-taal) |

### 4.3 Niet-wijzigen (herbruikbaar)
- `src/symbols/scip_parse.rs` structs (`ScipReference`, `ScipIndex`) — de protobuf-parser mapped hiernaartoe.
- LMDB-schema (de 5 named DBs) — keys zijn namespaced door SCIP-symbol-scheme, dus C# en TS co-existeren in dezelfde `db_path/scip/`.
- `RebuildScope`, `RebuildSummary`, `PrewarmSummary`, `SymbolReference`, `FindImpactResult` types.

---

## 5. Per-taal indexer-selectie (hoe taal-bepaling werkt)

Twee routes die beide TS moeten ondersteunen:

### 5.1 Expliciet (MCP find_impact `request.language`)
`SymbolIndexerRegistry::get(language)` is case-insensitive en retourneert de indexer waarvan `language()` overeenkomt. `LANG_TYPESCRIPT="typescript"` → `registry.get("typescript")` werkt automatisch zodra geregistreerd.

### 5.2 Auto-detect (file-extensie)
`src/mcp/mod.rs:6296` — huidige map is **enkel** `"cs" → LANG_CSHARP`, else fallback naar eerste `installed_languages()`. **Toevoegen:**
```rust
match ext { "cs" => LANG_CSHARP, "ts"|"tsx"|"mts"|"cts" => LANG_TYPESCRIPT, _ => /* fallback */ }
```
Fallback = huidig gedrag (eerste installed language) — ongewijzigd.

### 5.3 Applicability (welke indexer pakt een repo op?)
- `applies_to(repo_path)` per indexer: C# checkt `.sln`/`.csproj`, TS checkt `tsconfig.json`.
- `installed_languages()` filtert op `is_available()` (helper gevonden). Een host zonder Node/scip-typescript ziet TS simpelweg niet — geen crash.

---

## 6. Implementatie-stages (volgorde voor PR(s))

| # | Stage | Doel | Validering |
|---|-------|------|------------|
| 1 | Protobuf-binding | `scip` crate + `scip_proto.rs::parse_scip_protobuf()` | unit-test: fixture `.scip` file → `ScipIndex` met verwacht # defs/refs |
| 2 | TypeScriptSymbolIndexer | nieuw `typescript.rs`, implementeert trait | `cargo check` + `cargo clippy -D warnings` |
| 3 | Registratie + constants | `mod.rs:181` registreer TS; `constants.rs` lang/env | `installed_languages()` bevat "typescript" als Node aanwezig |
| 4 | find_impact auto-detect | `mcp/mod.rs:6296` map TS-extensies | handmatige smoke: find_impact op een TS-file |
| 5 | File-watcher TS-tracking | `manager.rs` `.ts`/`.tsx` + dispatch | bewerk een `.ts` → debounce-flush triggert rebuild |
| 6 | Tests | `tests/symbols_typescript_test.rs` + fixture | `cargo test --test symbols_typescript_test` groen |
| 7 | Pre-warm + status generaliseren | `serve/mod.rs` registry-loop | startup log toont TS pre-warm |
| 8 | (optioneel) Release-bundling | `release.yml` `-with-ts` | archive bevat scip-typescript binary |

Stages 1-6 zijn de MVP (find_impact werkt op TS). 7-8 zijn afronding.

---

## 7. Test-strategie: bewijs dat find_impact op een TS-symbool alle call-sites teruggeeft

### 7.1 Fixture-ontwerp (`tests/fixtures/ts-sample/`)
```
ts-sample/
  tsconfig.json          # compilerOptions, minimal
  src/
    math.ts              # export function add(a, b)  ← TARGET definitie
    consumer.ts          # import { add }; add(1,2); add(3,4)  ← 2 call-sites
    other.ts             # import { add }; const r = add(5,6)  ← 1 call-site
```
Doel: `add` heeft 1 definitie + 3 call-sites verdeeld over 2 files.

### 7.2 Integratie-test (`tests/symbols_typescript_test.rs`)
Gated (zelfde patroon als `symbols_csharp_test.rs`: skip als `scip-typescript`/Node niet oplosbaar, geen real embedding nodig). Test-flow:
1. `TypeScriptSymbolIndexer::new()`
2. `.rebuild(&fixture_root, db_path, RebuildScope::Full)` → `assert!(summary.ok)`
3. `.has_index(db_path)` → `true`
4. `.find_references(db_path, "add")` (via simple_name) → `assert_eq!(refs.len(), 4)` (1 def + 3 calls) OF via full SCIP-symbol key
5. `.find_references_by_position(db_path, "src/math.ts", <def-line>)` → retourneert de `add`-symbol key
6. Cross-check: voor elke call-site file komt deze voor in `refs.iter().map(|r| r.file)`

### 7.3 find_impact end-to-end (optioneel, handmatig)
Na opstarten van `codesearch serve` met de TS-fixture als project: roep de `find_impact` MCP-tool aan met `{file:"src/math.ts", line:<def-line>}` en verifieer dat het resultaat overeenkomt met de integratie-test (4 occurrences over 3 files).

### 7.4 Negative tests
- `find_references` op een onbekend symbool → lege `Vec`, geen panic.
- `.is_available()` op een host zonder Node → `false`; `installed_languages()` bevat geen "typescript".

---

## 8. Review — openstaande ontwerpkeuzes (beslissen vóór/ten tijde van implementatie)

### 8.1 `scip_meta` is NIET per-taal (design-issue)
`scip_meta` gebruikt key `"last_rebuild_ts"` zonder taal-prefix. Bij twee talen in dezelfde `db_path/scip/` overschrijven C# en TS elkaars timestamp. **Optie:** key namespacen `"last_rebuild_ts:csharp"` / `"last_rebuild_ts:typescript"`. Niet-breaking voor lezers die via `index_age()` gaan. **Beslissing:** namespacen — lokaal in `typescript.rs` een eigen key gebruiken, en later C# migreren.

### 8.2 File-watcher dispatch: generaliseren vs. parallelle tak
- **Optie A (generiek):** vervang hardcoded `reg.get(LANG_CSHARP)` (manager.rs:1236) door een loop `for lang in registry.indexed_languages()`. Schoon, schaalbaar naar meer talen, maar raakt `CSharpRebuildNotifier`-type (moet generiek `SymbolRebuildNotifier` worden) — grotere refactor.
- **Optie B (parallelle tak):** voeg een tweede `if`-blok voor TS toe, spiegelend het C#-blok. Minder netjes, lokaal, lager risico.
- **Beslissing:** start met Optie B (snel MVP), refactor naar A zodra er een derde taal komt. Documenteer als TODO.

### 8.3 scip-typescript distributie: `npx` vs. gebundelde binary
- C# shipt een self-contained exe in `helpers/csharp/` + `-with-csharp` release-archives.
- scip-typescript is een npm-package: `npx scip-typescript` werkt als Node op PATH staat. Geen bundling nodig voor development. Voor offline/air-gapped deploy: optie om `npm pack`-tarball te bundelen (follow-up, niet MVP).
- **Beslissing:** MVP = `npx` (env `CODESEARCH_SCIP_TYPESCRIPT` voor override-pad). Bundling = out of scope (§1).

### 8.4 Incrementele rebuild (RebuildScope::Files) voor TS
C# ondersteunt `Files{changed,deleted}` via csproj-groepering + `--filter-project`. scip-typescript heeft geen file-filter flag — herbouwt steeds de hele tsconfig-projectroot. **Beslissing:** MVP ondersteunt alleen `Full`; `Files` valt terug op `Full` (log + proceed). Voor grote monorepo's is dit later te optimaliseren (per-tsconfig groeperen, zie §8.5).

### 8.5 Monorepo met meerdere tsconfig.json
`applies_to()` zoekt nu één `tsconfig.json`. Een monorepo met `packages/*/tsconfig.json` vereist het C#-equivalent van `find_csproj_for_file` → een `find_tsconfig_for_file(repo, file)`. **Beslissing:** MVP pakt root-tsconfig; per-file-tsconfig-resolutie = follow-up. In scope zetten als de test-fixture dat meteen nodig maakt.

### 8.6 Pre-warm: heeft TS het nodig?
TS heeft geen two-phase lazy model → `rebuild()` populate direct alle refs → geen `batch-find-refs` pre-warm nodig. De registry-loop in `serve/mod.rs:1045` mag TS dus overslaan of gewoon `rebuild` aanroepen als index ontbreekt/stale is. **Beslissing:** registry-loop roept per indexer een `prewarm()`-methode aan; C# doet zijn batch-find-refs, TS is no-op (of `rebuild` als index koud). Voeg optionele default-methode `prewarm()` toe aan het trait.

### 8.7 `scip` crate keuze
Sourcegraph publiceert een `scip` Rust-crate (protobuf bindings + helpers). Alternatief: handmatige `prost`-build tegen de `.proto`. **Beslissing:** gebruik de `scip` crate (onderhouden, zelfde schema als scip-typescript output). Lock versie in `Cargo.toml`; als de crate afwijkt, val terug op `prost`-build.

---

## 9. Acceptatie-criteria (MVP = stages 1-6)
- [ ] `cargo check` + `cargo clippy -D warnings` groen.
- [ ] `cargo test --test symbols_typescript_test` groen op een host met Node + scip-typescript.
- [ ] `find_impact` MCP-tool retourneert voor een TS-functie alle call-sites (≥3 over 2 files in de fixture).
- [ ] `find_impact` auto-detect routeert `.ts`/`.tsx` naar TS-indexer (geen C#-fallback).
- [ ] `installed_languages()` bevat "typescript" als Node aanwezig, niet anders.
- [ ] Host zonder Node: geen crash, TS-indexer gewoon afwezig.
- [ ] C#-pijplijn ongewijzigd werken (geen regressie — bestaande C#-tests groen).

---

## 10. Verwijzingen
- C# trait + registry: `src/symbols/mod.rs:113-232`
- C# adapter (referentie-impl): `src/symbols/csharp.rs`
- JSON-parser (shape om naartoe te mappen): `src/symbols/scip_parse.rs:136-197`
- find_impact MCP-tool: `src/mcp/mod.rs:6238-6369` (taal-detect 6291-6329)
- File-watcher dispatch: `src/index/manager.rs:1124-1340`
- Startup pre-warm: `src/serve/mod.rs:1045`
- Constants: `src/constants.rs` (LANG_CSHARP, SCIP_CSHARP_*, HELPERS_SUBDIR)
- Bestaande tests: `tests/symbols_csharp_test.rs`, `helpers/csharp/tests/IndexerTests.cs`
- scip-typescript (Sourcegraph): https://github.com/sourcegraph/scip-typescript
- scip Rust-crate: https://github.com/sourcegraph/scip-rust
