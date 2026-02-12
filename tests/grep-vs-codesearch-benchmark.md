# Grep vs Codesearch Benchmark Test Plan

## Scoring Methodology

Per query, beide tools scoren op:

| Metric | Formule | Meet wat |
|--------|---------|----------|
| **Precision@10** | relevante resultaten / totaal geretourneerde (max 10) | Geen rommel |
| **Recall** | gevonden relevante / totaal relevante in codebase | Niets gemist |
| **MRR** | 1 / positie van eerste correcte resultaat | Snelheid naar antwoord |
| **F1** | 2 × (P × R) / (P + R) | Balans P/R |
| **Effort** | 1-5 schaal (1=direct bruikbaar, 5=veel handwerk nodig) | Praktische bruikbaarheid |

**Gewogen eindscore per query:** `0.25×Precision + 0.25×Recall + 0.20×MRR + 0.15×F1 + 0.15×(1 - Effort/5)`

## Ground Truth Procedure

1. Evaluator (Filip) verifieert voor elke query handmatig het verwachte resultaat VOORDAT tools draaien
2. Noteer: welke files, welke regels, welke types (class/method/struct/etc) zijn de correcte antwoorden
3. Pas daarna beide tools uitvoeren en scoren tegen ground truth
4. Bij twijfel over relevantie: markeer als "partial" (0.5 score ipv 1.0)

## Tool Configuratie

**Grep commando's (Windows PowerShell):**
```powershell
# Basis text search
Select-String -Path "src\**\*.cs" -Pattern "<pattern>" -Recurse
# Met context
Select-String -Path "src\**\*.cs" -Pattern "<pattern>" -Recurse -Context 3,3
# Case insensitive (default)
Select-String -Path "src\**\*.cs" -Pattern "<pattern>" -Recurse -CaseSensitive:$false
```

**Codesearch commando's:**
```powershell
# Hybrid search (default)
codesearch search "<query>" -m 10 --scores --content
# FTS only via tantivy
codesearch search "<query>" -m 10 --scores --content --vector-only:$false
# Vector only
codesearch search "<query>" -m 10 --scores --content --vector-only
# Met reranking
codesearch search "<query>" -m 10 --scores --content --rerank
```

---

## CODEBASE 1: BOIN.Aprimo (C# — primaire test)

Path: `C:\Users\develterf\source\repos\BOIN.Aprimo`

### Categorie A: Exact Name Lookup (grep-voordeel verwacht)

**Q1: Vind de class `BaseRestClient`**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "class BaseRestClient" -Recurse`
- Codesearch: `codesearch search "BaseRestClient class definition" -m 10 --scores --content`
- Ground truth: `src\Dlw.Aprimo.Dam\BaseRestClient.cs` — exacte locatie + volledige class boundaries

**Q2: Vind alle referenties naar `ServicebusService`**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "ServicebusService" -Recurse`
- Codesearch: `codesearch search "ServicebusService" -m 10 --scores --content`
- Ground truth: declaratie in Core\Services\ + alle usages (DI registratie, constructor injection, method calls)

**Q3: Vind de interface `IWorkflowMessageHandler`**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "IWorkflowMessageHandler" -Recurse`
- Codesearch: `codesearch search "IWorkflowMessageHandler interface" -m 10 --scores --content`
- Ground truth: interface definitie + alle implementaties + alle usages

### Categorie B: Type-Filtered / Structural (codesearch-voordeel verwacht)

**Q4: Vind alle Controller classes in het project**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "class \w+Controller" -Recurse`
- Codesearch: `codesearch search "controller class" -m 25 --scores --compact`
- Ground truth: handmatig tellen — alle *Controller.cs files in Api\Controllers\ en Web\Controllers\
- Let op: grep vindt text match, codesearch zou ChunkKind::Class moeten gebruiken

**Q5: Vind alle classes die een interface implementeren in de Workflow folder**
- Grep: `Select-String -Path "src\Dlw.Aprimo.Dam\Workflow\**\*.cs" -Pattern "class \w+ :.*I\w+" -Recurse`
- Codesearch: `codesearch search "workflow interface implementation" -m 10 --scores --content --filter-path "src/Dlw.Aprimo.Dam/Workflow"`
- Ground truth: alle classes in Workflow\ die `: ISomething` implementeren

**Q6: Vind alle enum definities in het Domain model**
- Grep: `Select-String -Path "src\Dlw.Aprimo.Dam\Domain\**\*.cs" -Pattern "enum \w+" -Recurse`
- Codesearch: `codesearch search "enum definition domain" -m 15 --scores --compact --filter-path "src/Dlw.Aprimo.Dam/Domain"`
- Ground truth: alle enums in Domain\

### Categorie C: Semantisch / Conceptueel (codesearch-voordeel verwacht)

**Q7: "Hoe wordt authenticatie afgehandeld?"**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "auth|oauth|token|login|credential" -Recurse`
- Codesearch: `codesearch search "authentication handling oauth token" -m 10 --scores --content`
- Ground truth: AuthenticationResponse.cs, OAuthResponse.cs, relevante middleware, token handling code

**Q8: "Waar worden Azure blob storage operaties uitgevoerd?"**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "blob|BlobStorage|CloudBlob|BlobClient" -Recurse`
- Codesearch: `codesearch search "azure blob storage operations upload download" -m 10 --scores --content`
- Ground truth: Core\Infrastructure\BlobStorage\ + alle referenties in andere projecten

**Q9: "Hoe werkt de caching strategie?"**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "cache|Cache|ICach" -Recurse`
- Codesearch: `codesearch search "caching strategy implementation" -m 10 --scores --content`
- Ground truth: Core\Caching\ + Dam\Caches\ + alle cache-gerelateerde code

**Q10: "Welke code handelt Veeva integratie af?"**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "Veeva|veeva" -Recurse`
- Codesearch: `codesearch search "Veeva vault integration" -m 10 --scores --content`
- Ground truth: VeevaLastService.cs, VeevaController.cs, Domain\Vault\, Domain\VeevaDocument\, Domain\VeevaObjects\, Domain\VeevaReference\, Workflow\SendToVault\

### Categorie D: Cross-Cutting Concerns

**Q11: "Vind alle error handling / retry logica"**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "retry|Retry|catch|exception" -Recurse`
- Codesearch: `codesearch search "error handling retry logic exception" -m 10 --scores --content`
- Ground truth: Core\Infrastructure\Retryer.cs + try/catch patterns in services

**Q12: "Waar wordt dependency injection geconfigureerd?"**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "AddScoped|AddTransient|AddSingleton|services\.Add" -Recurse`
- Codesearch: `codesearch search "dependency injection service registration configuration" -m 10 --scores --content`
- Ground truth: Startup.cs files, Container.cs, Program.cs — alle DI registraties

### Categorie E: Ambigue Queries (stress test)

**Q13: Zoek naar "search" in de codebase**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "search" -Recurse -CaseSensitive:$false`
- Codesearch: `codesearch search "search" -m 10 --scores --content`
- Ground truth: MoSearch.cs, SearchResult.cs, SearchIndex\, + alle search-gerelateerde code
- Verwachting: grep geeft honderden hits, codesearch gerankte subset — wat is bruikbaarder?

**Q14: Zoek naar "import" (ambigue: C# import of DAM import feature?)**
- Grep: `Select-String -Path "src\**\*.cs" -Pattern "import" -Recurse -CaseSensitive:$false`
- Codesearch: `codesearch search "import data processing" -m 10 --scores --content`
- Ground truth: Dam\Import\, Dam.Import project, Core\Import\ — domein-specifieke import functionaliteit

---

## CODEBASE 2: Codesearch (Rust — secundaire test, circulair caveat)

Path: `C:\WorkArea\AI\codesearch\codesearch.git`

⚠️ **Let op:** codesearch zoekt in zichzelf. Parsing bugs worden niet gedetecteerd maar gereproduceerd.

### Categorie F: Structural Rust Queries

**Q15: Vind de struct `Chunk` en al zijn velden**
- Grep: `Select-String -Path "src\**\*.rs" -Pattern "struct Chunk" -Recurse`
- Codesearch: `codesearch search "Chunk struct definition fields" -m 10 --scores --content`
- Ground truth: chunker\mod.rs — Chunk struct met alle velden + impl block

**Q16: Vind alle implementaties van de `Chunker` trait**
- Grep: `Select-String -Path "src\**\*.rs" -Pattern "impl Chunker" -Recurse`
- Codesearch: `codesearch search "Chunker trait implementation" -m 10 --scores --content`
- Ground truth: alle files die `impl Chunker for X` bevatten

**Q17: Vind het `ChunkKind` enum en waar elke variant gebruikt wordt**
- Grep stap 1: `Select-String -Path "src\**\*.rs" -Pattern "enum ChunkKind" -Recurse`
- Grep stap 2: `Select-String -Path "src\**\*.rs" -Pattern "ChunkKind::" -Recurse`
- Codesearch: `codesearch search "ChunkKind enum variants usage" -m 15 --scores --content`
- Ground truth: enum definitie in chunker\mod.rs + alle ChunkKind:: usages
- Let op: grep heeft 2 stappen nodig, codesearch potentieel 1

### Categorie G: Conceptueel Rust

**Q18: "Hoe werkt de embedding pipeline?"**
- Grep: `Select-String -Path "src\**\*.rs" -Pattern "embed|Embed|embedding" -Recurse`
- Codesearch: `codesearch search "embedding pipeline process flow" -m 10 --scores --content`
- Ground truth: embed\embedder.rs, embed\batch.rs, embed\cache.rs, embed\mod.rs

**Q19: "Hoe worden file system changes gedetecteerd?"**
- Grep: `Select-String -Path "src\**\*.rs" -Pattern "watch|notify|fsw|FileSystem" -Recurse`
- Codesearch: `codesearch search "file system watching change detection" -m 10 --scores --content`
- Ground truth: watch\mod.rs + gerelateerde event handling

**Q20: "Waar wordt de vector database aangestuurd?"**
- Grep: `Select-String -Path "src\**\*.rs" -Pattern "vectordb|VectorStore|qdrant|vector" -Recurse`
- Codesearch: `codesearch search "vector database store operations" -m 10 --scores --content`
- Ground truth: vectordb\store.rs, vectordb\mod.rs + alle aanroepen vanuit search\ en index\

---

## Scoresheet Template

Kopieer per query:

```
Query: Q[N]
Tool: grep / codesearch

Resultaten (top 10):
1. [file:line] — relevant? ja/nee/partial
2. ...

Ground truth items totaal: [N]
Gevonden relevant: [N]
Niet-relevant in resultaten: [N]

Precision@10: [gevonden relevant / totaal geretourneerd]
Recall: [gevonden relevant / ground truth totaal]
MRR: [1 / positie eerste correcte]
F1: [2×P×R / (P+R)]
Effort (1-5): [score + toelichting]
Gewogen score: [berekening]
```

## Samenvattingstabel

| Query | Cat | Grep P@10 | Grep R | Grep MRR | Grep Effort | Grep Total | CS P@10 | CS R | CS MRR | CS Effort | CS Total |
|-------|-----|-----------|--------|----------|-------------|------------|---------|------|--------|-----------|----------|
| Q1    | A   |           |        |          |             |            |         |      |        |           |          |
| Q2    | A   |           |        |          |             |            |         |      |        |           |          |
| Q3    | A   |           |        |          |             |            |         |      |        |           |          |
| Q4    | B   |           |        |          |             |            |         |      |        |           |          |
| Q5    | B   |           |        |          |             |            |         |      |        |           |          |
| Q6    | B   |           |        |          |             |            |         |      |        |           |          |
| Q7    | C   |           |        |          |             |            |         |      |        |           |          |
| Q8    | C   |           |        |          |             |            |         |      |        |           |          |
| Q9    | C   |           |        |          |             |            |         |      |        |           |          |
| Q10   | C   |           |        |          |             |            |         |      |        |           |          |
| Q11   | D   |           |        |          |             |            |         |      |        |           |          |
| Q12   | D   |           |        |          |             |            |         |      |        |           |          |
| Q13   | E   |           |        |          |             |            |         |      |        |           |          |
| Q14   | E   |           |        |          |             |            |         |      |        |           |          |
| Q15   | F   |           |        |          |             |            |         |      |        |           |          |
| Q16   | F   |           |        |          |             |            |         |      |        |           |          |
| Q17   | F   |           |        |          |             |            |         |      |        |           |          |
| Q18   | G   |           |        |          |             |            |         |      |        |           |          |
| Q19   | G   |           |        |          |             |            |         |      |        |           |          |
| Q20   | G   |           |        |          |             |            |         |      |        |           |          |
| **GEM** |   |           |        |          |             |            |         |      |        |           |          |

## Verwachte Uitkomst Hypotheses (vooraf vastleggen)

- **Cat A (exact lookup):** Grep wint of gelijk — exacte string match is grep's kracht
- **Cat B (structural):** Codesearch wint — type-awareness geeft voorsprong
- **Cat C (semantic):** Codesearch wint significant — grep kan niet conceptueel zoeken
- **Cat D (cross-cutting):** Mixed — hangt af van hoe specifiek de grep patterns zijn
- **Cat E (ambigue):** Codesearch wint op precision, grep op recall
- **Cat F (Rust structural):** Codesearch wint, maar caveat: circulaire test
- **Cat G (Rust semantic):** Codesearch wint, maar caveat: circulaire test

**Als codesearch NIET wint in Cat C en E, is dat een serieus probleem.**
**Als grep NIET wint of gelijkspel haalt in Cat A, is dat onverwacht.**

## Eerlijkheidschecks

- [ ] Ground truth handmatig geverifieerd VOOR tool uitvoering
- [ ] Grep patterns zijn eerlijk geoptimaliseerd (niet opzettelijk slecht)
- [ ] Codesearch queries zijn eerlijk geformuleerd (niet opzettelijk vaag)
- [ ] Beide tools draaien op zelfde moment (index is up-to-date)
- [ ] Resultaten beoordeeld door evaluator, niet door LLM
