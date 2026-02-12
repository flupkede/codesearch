# BOIN.Aprimo Benchmark: Grep vs Codesearch

**Project Path:** `C:\Users\develterf\source\repos\BOIN.Aprimo`
**Test Date:** [FILL IN]
**Evaluator:** [FILL IN]

---

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

---

## Ground Truth Procedure

1. Evaluator verifieert voor elke query handmatig het verwachte resultaat VOORDAT tools draaien
2. Noteer: welke files, welke regels, welke types (class/method/struct/etc) zijn de correcte antwoorden
3. Pas daarna beide tools uitvoeren en scoren tegen ground truth
4. Bij twijfel over relevantie: markeer als "partial" (0.5 score ipv 1.0)

---

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
# FTS only
codesearch search "<query>" -m 10 --scores --content --vector-only:$false
# Vector only
codesearch search "<query>" -m 10 --scores --content --vector-only
# Met reranking
codesearch search "<query>" -m 10 --scores --content --rerank
```

---

## Categorie A: Exact Name Lookup (grep-voordeel verwacht)

### Q1: Vind de class `BaseRestClient`

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "class BaseRestClient" -Recurse
```

**Codesearch:**
```powershell
codesearch search "BaseRestClient class definition" -m 10 --scores --content
```

**Ground truth:**
- `src\Dlw.Aprimo.Dam\BaseRestClient.cs` — exacte locatie + volledige class boundaries

**Grep Results (top 10):**
```
1. [FILL IN] — relevant? ja/nee/partial
2. [FILL IN]
...
```

**Codesearch Results (top 10):**
```
1. [FILL IN] — relevant? ja/nee/partial
2. [FILL IN]
...
```

**Grep Scores:**
- Ground truth items totaal: [N]
- Gevonden relevant: [N]
- Niet-relevant in resultaten: [N]
- Precision@10: [gevonden relevant / totaal geretourneerd]
- Recall: [gevonden relevant / ground truth totaal]
- MRR: [1 / positie eerste correcte]
- F1: [2×P×R / (P+R)]
- Effort (1-5): [score + toelichting]
- Gewogen score: [berekening]

**Codesearch Scores:**
- Ground truth items totaal: [N]
- Gevonden relevant: [N]
- Niet-relevant in resultaten: [N]
- Precision@10: [gevonden relevant / totaal geretourneerd]
- Recall: [gevonden relevant / ground truth totaal]
- MRR: [1 / positie eerste correcte]
- F1: [2×P×R / (P+R)]
- Effort (1-5): [score + toelichting]
- Gewogen score: [berekening]

---

### Q2: Vind alle referenties naar `ServicebusService`

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "ServicebusService" -Recurse
```

**Codesearch:**
```powershell
codesearch search "ServicebusService" -m 10 --scores --content
```

**Ground truth:**
- Declaratie in Core\Services\ + alle usages (DI registratie, constructor injection, method calls)

[Scoresheet template - duplicate from Q1]

---

### Q3: Vind de interface `IWorkflowMessageHandler`

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "IWorkflowMessageHandler" -Recurse
```

**Codesearch:**
```powershell
codesearch search "IWorkflowMessageHandler interface" -m 10 --scores --content
```

**Ground truth:**
- Interface definitie + alle implementaties + alle usages

[Scoresheet template - duplicate from Q1]

---

## Categorie B: Type-Filtered / Structural (codesearch-voordeel verwacht)

### Q4: Vind alle Controller classes in het project

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "class \w+Controller" -Recurse
```

**Codesearch:**
```powershell
codesearch search "controller class" -m 25 --scores --compact
```

**Ground truth:**
- Handmatig tellen — alle *Controller.cs files in Api\Controllers\ en Web\Controllers\
- Let op: grep vindt text match, codesearch zou ChunkKind::Class moeten gebruiken

[Scoresheet template - duplicate from Q1]

---

### Q5: Vind alle classes die een interface implementeren in de Workflow folder

**Grep:**
```powershell
Select-String -Path "src\Dlw.Aprimo.Dam\Workflow\**\*.cs" -Pattern "class \w+ :.*I\w+" -Recurse
```

**Codesearch:**
```powershell
codesearch search "workflow interface implementation" -m 10 --scores --content --filter-path "src/Dlw.Aprimo.Dam/Workflow"
```

**Ground truth:**
- Alle classes in Workflow\ die `: ISomething` implementeren

[Scoresheet template - duplicate from Q1]

---

### Q6: Vind alle enum definities in het Domain model

**Grep:**
```powershell
Select-String -Path "src\Dlw.Aprimo.Dam\Domain\**\*.cs" -Pattern "enum \w+" -Recurse
```

**Codesearch:**
```powershell
codesearch search "enum definition domain" -m 15 --scores --compact --filter-path "src/Dlw.Aprimo.Dam/Domain"
```

**Ground truth:**
- Alle enums in Domain\

[Scoresheet template - duplicate from Q1]

---

## Categorie C: Semantisch / Conceptueel (codesearch-voordeel verwacht)

### Q7: "Hoe wordt authenticatie afgehandeld?"

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "auth|oauth|token|login|credential" -Recurse
```

**Codesearch:**
```powershell
codesearch search "authentication handling oauth token" -m 10 --scores --content
```

**Ground truth:**
- AuthenticationResponse.cs, OAuthResponse.cs, relevante middleware, token handling code

[Scoresheet template - duplicate from Q1]

---

### Q8: "Waar worden Azure blob storage operaties uitgevoerd?"

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "blob|BlobStorage|CloudBlob|BlobClient" -Recurse
```

**Codesearch:**
```powershell
codesearch search "azure blob storage operations upload download" -m 10 --scores --content
```

**Ground truth:**
- Core\Infrastructure\BlobStorage\ + alle referenties in andere projecten

[Scoresheet template - duplicate from Q1]

---

### Q9: "Hoe werkt de caching strategie?"

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "cache|Cache|ICach" -Recurse
```

**Codesearch:**
```powershell
codesearch search "caching strategy implementation" -m 10 --scores --content
```

**Ground truth:**
- Core\Caching\ + Dam\Caches\ + alle cache-gerelateerde code

[Scoresheet template - duplicate from Q1]

---

### Q10: "Welke code handelt Veeva integratie af?"

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "Veeva|veeva" -Recurse
```

**Codesearch:**
```powershell
codesearch search "Veeva vault integration" -m 10 --scores --content
```

**Ground truth:**
- VeevaLastService.cs, VeevaController.cs, Domain\Vault\, Domain\VeevaDocument\, Domain\VeevaObjects\, Domain\VeevaReference\, Workflow\SendToVault\

[Scoresheet template - duplicate from Q1]

---

## Categorie D: Cross-Cutting Concerns

### Q11: "Vind alle error handling / retry logica"

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "retry|Retry|catch|exception" -Recurse
```

**Codesearch:**
```powershell
codesearch search "error handling retry logic exception" -m 10 --scores --content
```

**Ground truth:**
- Core\Infrastructure\Retryer.cs + try/catch patterns in services

[Scoresheet template - duplicate from Q1]

---

### Q12: "Waar wordt dependency injection geconfigureerd?"

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "AddScoped|AddTransient|AddSingleton|services\.Add" -Recurse
```

**Codesearch:**
```powershell
codesearch search "dependency injection service registration configuration" -m 10 --scores --content
```

**Ground truth:**
- Startup.cs files, Container.cs, Program.cs — alle DI registraties

[Scoresheet template - duplicate from Q1]

---

## Categorie E: Ambigue Queries (stress test)

### Q13: Zoek naar "search" in de codebase

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "search" -Recurse -CaseSensitive:$false
```

**Codesearch:**
```powershell
codesearch search "search" -m 10 --scores --content
```

**Ground truth:**
- MoSearch.cs, SearchResult.cs, SearchIndex\, + alle search-gerelateerde code
- Verwachting: grep geeft honderden hits, codesearch gerankte subset — wat is bruikbaarder?

[Scoresheet template - duplicate from Q1]

---

### Q14: Zoek naar "import" (ambigue: C# import of DAM import feature?)

**Grep:**
```powershell
Select-String -Path "src\**\*.cs" -Pattern "import" -Recurse -CaseSensitive:$false
```

**Codesearch:**
```powershell
codesearch search "import data processing" -m 10 --scores --content
```

**Ground truth:**
- Dam\Import\, Dam.Import project, Core\Import\ — domein-specifieke import functionaliteit

[Scoresheet template - duplicate from Q1]

---

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
| **GEM** |   |           |        |          |             |            |         |      |        |           |          |

---

## Verwachte Uitkomst Hypotheses

- **Cat A (exact lookup):** Grep wint of gelijk — exacte string match is grep's kracht
- **Cat B (structural):** Codesearch wint — type-awareness geeft voorsprong
- **Cat C (semantic):** Codesearch wint significant — grep kan niet conceptueel zoeken
- **Cat D (cross-cutting):** Mixed — hangt af van hoe specifiek de grep patterns zijn
- **Cat E (ambigue):** Codesearch wint op precision, grep op recall

**Als codesearch NIET wint in Cat C en E, is dat een serieus probleem.**
**Als grep NIET wint of gelijkspel haalt in Cat A, is dat onverwacht.**

---

## Export Resultaten

Nadat alle queries voltooid zijn, exporteer de samenvattingstabel naar `testresult_BOIN.Aprimo.md`:

```powershell
# Copy alleen de samenvattingstabel en de gemiddelde scores
# Sla op als: tests/testresult_BOIN.Aprimo.md
```

---

## Eerlijkheidschecks

- [ ] Ground truth handmatig geverifieerd VOOR tool uitvoering
- [ ] Grep patterns zijn eerlijk geoptimaliseerd (niet opzettelijk slecht)
- [ ] Codesearch queries zijn eerlijk geformuleerd (niet opzettelijk vaag)
- [ ] Beide tools draaien op zelfde moment (index is up-to-date)
- [ ] Resultaten beoordeeld door evaluator, niet door LLM
