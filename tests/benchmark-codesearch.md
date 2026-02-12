# Codesearch Benchmark: Grep vs Codesearch

**Project Path:** `C:\WorkArea\AI\codesearch\codesearch.git`
**Test Date:** [FILL IN]
**Evaluator:** [FILL IN]

⚠️ **Let op:** codesearch zoekt in zichzelf. Parsing bugs worden niet gedetecteerd maar gereproduceerd.

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

**Grep commando's (Git Bash):**
```bash
# Basis text search
grep -r "pattern" src/**/*.rs
# Met context
grep -r -C 3 "pattern" src/**/*.rs
# Case insensitive
grep -ri "pattern" src/**/*.rs
```

**Codesearch commando's:**
```bash
# Hybrid search (default)
codesearch search "query" -m 10 --scores --content
# FTS only
codesearch search "query" -m 10 --scores --content --vector-only:$false
# Vector only
codesearch search "query" -m 10 --scores --content --vector-only
# Met reranking
codesearch search "query" -m 10 --scores --content --rerank
```

---

## Categorie F: Structural Rust Queries

### Q15: Vind de struct `Chunk` en al zijn velden

**Grep:**
```bash
grep -r "struct Chunk" src/**/*.rs
```

**Codesearch:**
```bash
codesearch search "Chunk struct definition fields" -m 10 --scores --content
```

**Ground truth:**
- `chunker\mod.rs` — Chunk struct met alle velden + impl block

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

### Q16: Vind alle implementaties van de `Chunker` trait

**Grep:**
```bash
grep -r "impl Chunker" src/**/*.rs
```

**Codesearch:**
```bash
codesearch search "Chunker trait implementation" -m 10 --scores --content
```

**Ground truth:**
- Alle files die `impl Chunker for X` bevatten

[Scoresheet template - duplicate from Q15]

---

### Q17: Vind het `ChunkKind` enum en waar elke variant gebruikt wordt

**Grep stap 1:**
```bash
grep -r "enum ChunkKind" src/**/*.rs
```

**Grep stap 2:**
```bash
grep -r "ChunkKind::" src/**/*.rs
```

**Codesearch:**
```bash
codesearch search "ChunkKind enum variants usage" -m 15 --scores --content
```

**Ground truth:**
- Enum definitie in chunker\mod.rs + alle ChunkKind:: usages
- Let op: grep heeft 2 stappen nodig, codesearch potentieel 1

[Scoresheet template - duplicate from Q15]

---

## Categorie G: Conceptueel Rust

### Q18: "Hoe werkt de embedding pipeline?"

**Grep:**
```bash
grep -r "embed|Embed|embedding" src/**/*.rs
```

**Codesearch:**
```bash
codesearch search "embedding pipeline process flow" -m 10 --scores --content
```

**Ground truth:**
- embed\embedder.rs, embed\batch.rs, embed\cache.rs, embed\mod.rs

[Scoresheet template - duplicate from Q15]

---

### Q19: "Hoe worden file system changes gedetecteerd?"

**Grep:**
```bash
grep -r "watch|notify|fsw|FileSystem" src/**/*.rs
```

**Codesearch:**
```bash
codesearch search "file system watching change detection" -m 10 --scores --content
```

**Ground truth:**
- watch\mod.rs + gerelateerde event handling

[Scoresheet template - duplicate from Q15]

---

### Q20: "Waar wordt de vector database aangestuurd?"

**Grep:**
```bash
grep -r "vectordb|VectorStore|qdrant|vector" src/**/*.rs
```

**Codesearch:**
```bash
codesearch search "vector database store operations" -m 10 --scores --content
```

**Ground truth:**
- vectordb\store.rs, vectordb\mod.rs + alle aanroepen vanuit search\ en index\

[Scoresheet template - duplicate from Q15]

---

## Samenvattingstabel

| Query | Cat | Grep P@10 | Grep R | Grep MRR | Grep Effort | Grep Total | CS P@10 | CS R | CS MRR | CS Effort | CS Total |
|-------|-----|-----------|--------|----------|-------------|------------|---------|------|--------|-----------|----------|
| Q15   | F   |           |        |          |             |            |         |      |        |           |          |
| Q16   | F   |           |        |          |             |            |         |      |        |           |          |
| Q17   | F   |           |        |          |             |            |         |      |        |           |          |
| Q18   | G   |           |        |          |             |            |         |      |        |           |          |
| Q19   | G   |           |        |          |             |            |         |      |        |           |          |
| Q20   | G   |           |        |          |             |            |         |      |        |           |          |
| **GEM** |   |           |        |          |             |            |         |      |        |           |          |

---

## Verwachte Uitkomst Hypotheses

- **Cat F (Rust structural):** Codesearch wint, maar caveat: circulaire test
- **Cat G (Rust semantic):** Codesearch wint, maar caveat: circulaire test

---

## Export Resultaten

Nadat alle queries voltooid zijn, exporteer de samenvattingstabel naar `testresult_codesearch.md`:

```powershell
# Copy alleen de samenvattingstabel en de gemiddelde scores
# Sla op als: tests/testresult_codesearch.md
```

---

## Eerlijkheidschecks

- [ ] Ground truth handmatig geverifieerd VOOR tool uitvoering
- [ ] Grep patterns zijn eerlijk geoptimaliseerd (niet opzettelijk slecht)
- [ ] Codesearch queries zijn eerlijk geformuleerd (niet opzettelijk vaag)
- [ ] Beide tools draaien op zelfde moment (index is up-to-date)
- [ ] Resultaten beoordeeld door evaluator, niet door LLM
