# DIAGNOSE — Waarom kiest de agent zelden `find_impact`?

> **Status:** DIAGNOSE-EERST. Dit document levert geen fix, maar een reproduceerbare
> analyse met gehard bewijs uit de broncode, een hypotheses-overzicht, een geïsoleerde
> oorzaak, en pas daarna gefaseerde fix-opties (geen blinde oplossing).
> **Symptoom:** de agent pakt voor "wie roept X aan / wat breekt als ik X hernoem"
> vrijwel altijd `find kind=usages` (BM25/tekst-benadering) of `search(semantic)`,
> zelden `find_impact` — terwijl `find_impact` het enige SCIP-backed call-graph-pad is.
> **Repo:** `codesearch-git`. Validatie: `cargo check` + `cargo clippy -D warnings`.

---

## 1. Doel & scope

**In scope**
- Vaststellen **waarom** de agent `find_impact` mijdt, met bewijs op 3 lagen:
  server-instructies, tool-descriptions, en deploy-realiteit.
- De keuze instrumenteerbaar maken (zowel server- als agent-kant).
- Gefundeerde fix-opties aandragen — niet blind één implementeren.

**Niet in scope (pas ná isolatie)**
- De daadwerkelijke code-fix. Die volgt uit de gekozen optie in §7.
- TS/andere-talen SCIP-backends (apart plan: `PLAN_TYPESCRIPT_SCIP.md`).

---

## 2. Symptoom & observatie

| Vraagtype | Verwachte tool | Werkelijk gekozen (observatie) |
|-----------|----------------|--------------------------------|
| "wie roept `foo()` aan?" | `find_impact` | `find kind=usages` of `search` |
| "wat breekt als ik `Bar` hernoem?" | `find_impact` | `find kind=usages` |
| "toon call-graph van `X`" | `find_impact` | `search(semantic)` of `find` |

Het gedrag is **consistent reproduceerbaar**: stel de vraag in een willekeurige
agent-sessie die codesearch-MCP gebruikt → agent kiest `find`/`search`, niet `find_impact`.

---

## 3. Bewijsmateriaal uit de broncode (hard evidence)

De oorzaak is niet verborgen — ze staat letterlijk in wat de server aan de agent
voert. Drie lagen, allemaal in `src/mcp/mod.rs`:

### 3.1 Server-instructies (worden in de agent system-prompt geïnjecteerd)
`INSTRUCTIONS_TEMPLATE` (`src/mcp/mod.rs:7915-7953`) — exacte regels die de agent ziet:

```
PICK THE RIGHT TOOL FOR THE TASK:
  "who calls X?" / "what breaks if I rename X?"
    → find_impact (C# via SCIP; other languages: use find kind="usages")   ← 7931
RULES:
  - search(semantic) is the DEFAULT for code lookup. Don't skip it.         ← 7944
  - find_impact for C# refactors; find(kind="usages") for other languages.  ← 7945
```

**Drie biases in deze tekst:**
1. Regel 7931 routeert "who calls X?" voor **elke niet-C# taal** expliciet naar `find kind=usages`.
2. Regel 7944 positioneert `search(semantic)` als de **DEFAULT** — alles wat niet expliciet anders is, valt terug op search.
3. Regel 7945 kadermt `find_impact` als "C# **refactors**" — smal, niet als algemene call-graph-tool.

### 3.2 `find_impact` tool-description (`src/mcp/mod.rs:6236`)
```
"Symbol impact analysis — find all references ... (SCIP).
 ... More accurate than text-based `find kind=\"usages\"` ...
 Languages: C# today (requires the `scip-csharp` helper ...).
 For Rust/Python/Go/etc., use `find` with `kind=\"usages\"` as a text-based fallback
 until SCIP backends for those languages ship."        ← ACTIEVE DOORVERWIJZING WEG
```
De tool-description **zelf** zegt de agent om `find_impact` te vermijden voor niet-C#.
Dit is de sterkste bias: de tool die we willen promoten, ontmoedigt zichzelf.

### 3.3 `find` tool-description (`src/mcp/mod.rs:4611`)
```
"- `usages`: find all call-sites and references to a symbol"
```
Generiek, geen caveat, geen verwijzing dat `find_impact` preciezer is. `find` presenteert
zich als het algemene antwoord op "who calls X" — voor **alle** talen, zonder drempel.

### 3.4 README + zoekresultaat-meta (versterking)
- `README.md:307-321`: publieke docs framen `find_impact` als "Currently supports **C#**",
  "Requires the `-with-csharp` release variant".
- **Ironische meta-observatie:** de server emit bij zwakke zoekresultaten zelf een
  `suggested_tool: "find with kind=usages"` note — dus het systeem adviseert actief `find`,
  nooit `find_impact`.

### 3.5 Deploy-realiteit (de derde laag)
`find_impact` faalt als er geen `scip-csharp` helper is (`mcp/mod.rs:6332-6347`,
`is_available()` check → retourneert een error-JSON met `hint_for_agent`). Op een
serve-hub **zonder** `-with-csharp` variant faalt `find_impact` dus altijd. Een agent
die het één keer probeert en een error terugkrijgt, leert het daarna vermijden —
self-reinforcing. `find kind=usages` faalt nooit (puur tekst-index, altijd aan).

---

## 4. (a) Reproduceren & instrumenteren

Doel: **meetbaar** maken welke tool de agent kiest en waarom, bij welke queries.

### 4.1 Wat de server al logt (server-kant = "welke tool")
`tracing::info!` bij elk tool-call:
- `find_impact`: `mcp/mod.rs:6242` (symbol_name, file, line, language, project)
- `find`: `mcp/mod.rs:4622` (symbol, kind, project, group)
- `search`: aparte `📥 search` log

→ **De "welke tool" is al traceerbaar** via de serve-logs. Wat ontbreekt is aggregatie.

### 4.2 Wat de server NIET kan loggen (agent-kant = "waarom")
De keuze "find_impact vs find" wordt in het **LLM-hoofd** van de agent gemaakt, vóór de
tool-aanroep. De server ziet alleen de uitkomst. Om het "waarom" te vangen:

| Laag | Wat loggen | Hoe |
|------|-----------|-----|
| Server | tool-callfrequentie per type + per taal + outcome (ok/fout) | structured counter/metrics naast tracing; bv. `tool_calls{tool="find_impact",lang="csharp",outcome="ok"}` |
| Server | of `find_impact` faalde door `!is_available` vs `No symbol indexer` | aparte outcome-labels op de counter |
| Agent-harness (opencode/claude) | de tool-selectie-reasoning vóór de call | opencode-session-logs / een wrapper die de assistant-tekst vóór tool_usecapt met "find_impact\|find\|search" |
| Eval-set | 20 vaste queries → welke tool wordt gekozen | herhaalbare harness-run (zie 4.4) |

### 4.3 Instrumentatie-voorstel (klein, niet-invasief)
1. **Tally in serve-modus:** een in-memory `HashMap<(tool, language, outcome), u64>`,
   exposed via `status kind=index` of een nieuw `/metrics`-veld. Laag risico, lokaal in
   `CodesearchService`. Bewijst de frequentie-kloof kwantitatief.
2. **Outcome-differentiatie:** onderscheid `Ok` / `NoIndexer` / `HelperUnavailable` /
   `Empty` bij `find_impact` — toont aan of het falen (§3.5) de oorzaak is.

### 4.4 Repro-harness (deterministisch)
Een klein script/set prompts (20 stuks) met mixed intent:
- 8× "who calls / what breaks" (zou → find_impact)
- 6× "find code about X" (zou → search semantic)
- 6× "where is X defined / imports" (zou → find definition/imports)

Draaien tegen een C# repo **met** scip-csharp én een C# repo **zonder**. Tellen welk %
"who calls" naar find_impact gaat. Vóór fix = baseline, na fix = meting.

---

## 5. (b) Hypotheses (systematisch afgelopen)

| # | Hypothese | Bewijs nu | Status |
|---|-----------|-----------|--------|
| H1 | Tool-descriptions/afbakening onduidelijk: `find_impact` framt zichzelf als C#-only en raadt `find kind=usages` aan | §3.2 — tool-desc bevat actieve doorverwijzing weg | **Sterk ondersteund** |
| H2 | `find` presenteert zich als de algemene weg; geen caveat dat `find_impact` preciezer is | §3.3 — find-desc "find all call-sites" zonder drempel | **Sterk ondersteund** |
| H3 | Server-instructies routeren "who calls X?" voor niet-C# expliciet weg van find_impact | §3.1 — regels 7931/7945 | **Sterk ondersteund** |
| H4 | Overlappende affordances: zowel find_impact als find kind=usages beantwoorden "who calls X" → agent kiest de generiekere | §3.2+§3.3 combi | Ondersteund (gevolg van H1+H2) |
| H5 | Server-side routing/ranking verbergt find_impact | §4.1 — geen routering die find_impact verbergt; tool is altijd geregistreerd | **Verworpen** |
| H6 | Deploy-realiteit: zonder scip-csharp faalt find_impact → agent leert vermijden | §3.5 — is_available-error | Ondersteund (versterkt H1 voor niet-C#-deploy) |
| H7 | `search(semantic)` als DEFAULT schuift find_impact naar de marge | §3.1 regel 7944 | Ondersteund (zwakker, secundair) |

**Conclusie H1–H4+H6 zijn allemaal ondersteund en versterken elkaar** → de oorzaak is
multicausaal maar concentreert zich in **framing/afbakening** (beschrijvingen + instructies),
niet in server-routing (H5 verworpen).

---

## 6. (c) Vermoedelijke oorzaak — geïsoleerd

> **De agent mijdt `find_impact` niet ondanks, maar **door** de documentatie.**

Eén samengestelde oorzaak, drie dragers:

1. **Zelf-ontmoedigende tool-description** (`mcp/mod.rs:6236`): `find_impact` zegt letterlijk
   "For Rust/Python/Go/etc., use `find` with `kind=usages`". Een agent die deze tekst leest
   vóór tool-selectie, volgt die instructie op — correct gedrag, foute uitkomst.
2. **Asymmetrische framing**: `find kind=usages` (4611) claimt zonder voorbehoud "find all
   call-sites and references"; `find_impact` geeft zichzelf een taal-drempel. De generiekere
   tool wint bij ambiguity.
3. **Server-instructies versterken** (7915-7953): routeert "who calls X?" voor niet-C#
   expliciet naar `find kind=usages`, en positioneert `search(semantic)` als default.

**Dus: het probleem zit in de tekstlaag (descriptions + INSTRUCTIONS_TEMPLATE), niet in
code-logica of routing.** Dat maakt het goed te fixen, maar ook makkelijk te onderschatten
— de "fix" is bewerken van strings, geen refactor. H6 (deploy-falen) is een versterker:
zelfs als de tekst is herzien, blijft `find_impact` falen op een serve-hub zonder scip-csharp;
dat moet via §7-optie B (delegatie) of de losse TS-SCIP-track worden opgelost.

---

## 7. (d) Fix-opties (gefaseerd, niet blind — kies na diagnose-bevestiging)

### Optie A — Tool-descriptions + instructies herzien (kleinste, eerste stap)
**Wat:**
- `find_impact`-desc (6236): verwijder de actieve doorverwijzing "use find kind=usages".
  Hernoem naar taal-neutraal: "Precision symbol impact via SCIP where available; falls back
  to lexical matching for languages without a SCIP backend." Maak van SCIP een bonus, niet
  een voorwaarde in de framing.
- `find`-desc (4611): voeg bij `usages` een caveat — "lexical/text-based; for IDE-precise
  call-graphs use `find_impact`".
- `INSTRUCTIONS_TEMPLATE` (7931/7945): routeer "who calls X?" → `find_impact` als **default**,
  niet als C#-uitzondering. `find kind=usages` als fallback alleen als find_impact geen index heeft.
- README (307-321): maak find_impact de aanbevolen call-graph-tool, scip-csharp als
  "precision boost" i.p.v. harde vereiste in de framing.

**Voorspeld effect:** bij de repro-harness (§4.4) stijgt het find_impact-aandeel voor
"who calls X" aanzienlijk — mits een SCIP-index aanwezig is (want anders faalt hij, H6).
**Risico:** op niet-C# repos zonder backend blijft hij falen → agent ziet errors → A alleen
is onvoldoende; combineer met B of de TS-track.

### Optie B — `find kind=usages` transparant delegatie naar SCIP (middel, structureel)
**Wat:** in `find_usages` (achter `find kind=usages`), detecteer of er een
`SymbolIndexer` voor de betreffende taal/repo beschikbaar + has_index is. Zo ja: roep
`indexer.find_references()` aan (het SCIP-pad) en voeg die resultaten bovenop/ipv de
lexicale match. Zo nee: huidige tekst-based fallback.

**Effect:** de agent hoeft niets te kiezen — `find kind=usages` wordt automatisch precies
waar SCIP beschikbaar is._lost de asymmetrie (H2) op zonder de agent te belasten. Houdt
`find_impact` als expliciete "geef me alleen SCIP"-tool voor agents die dat willen forceren.
**Risico:** "transparente" upgrade kan verrassingen geven (andere resultaat-volumen/
-volgorde); documenteer + feature-flag (`CODESEARCH_FIND_DELEGATES_TO_SCIP`, default aan).
Complexiteit: ~1 functie in `find_usages` + taal-detect per query (de file-ext logica uit
`find_impact` 6295 hergebruiken, maar dan generiek).

### Optie C — Tools samenvoegen (grootst, breekend)
**Wat:** één `find_references`-tool (of `find_impact` hernoemen) die altijd SCIP-voorrang
geeft en valt terug op lexicaal. `find kind=usages` afschaffen of als alias behouden.
**Effect:** elimineert de ambiguity volledig (H4 weg). Maar: breaking voor agents/harnesses
die `find kind=usages` aanroepen; migratiekosten; grotere review.
**Risico:** backward-compat, alias-beheer. **Alleen kiezen als A+B onvoldoende blijken.**

### Optie D — Language-aware routing binnen `find` (klein, complementair)
**Wat:** de `suggested_tool`-note die de server nu emit (§3.4 meta) uitbreiden: bij een
"who calls"-aardige query op een C# repo, suggesteer `find_impact` i.p.v. `find kind=usages`.
**Effect:** nudges de agent in-session, zonder tool-schema's te raken.
**Risico:** klein; louter aanvullend op A/B.

### Aanbevolen volgorde
1. **A eerst** (tekst-laag, goedkoop, direct meetbaar in repro-harness).
2. **B als structurele oplossing** (lost H6 op: ook zonder find_impact-aanroep krijgt de
   agent SCIP-kwaliteit via de vertrouwde `find`-tool).
3. C alleen als A+B in de eval niet voldoen.
4. D als finishing touch.
De losse TS-SCIP-track (`PLAN_TYPESCRIPT_SCIP.md`) breidt de **dekking** van find_impact uit
(meer talen met écht SCIP); dit diagnose-plan los de **keuze**-bias op. Beide zijn
complementair.

---

## 8. Review-sectie — open ontwerpkeuzes & risico's

| Keuze | Opties | Risico / afweging |
|-------|--------|-------------------|
| Verwijderen vs. verzachten van "use find kind=usages" in find_impact-desc | hard verwijderen kan agent in niet-C# zonder backend op een falende tool zetten | combineer altijd met B (delegatie) of een duidelijke runtime-foutmelding die wéér naar find_impact... → nee: naar `find kind=usages` als echte fallback (geen cirkel) |
| Delegatie default aan/uit | default AAN = transparante upgrade; default UIT = backward-compat | feature-flag, default aan na evaluatieperiode |
| Meten vóór/na | repro-harness is handmatig vandaag | overweeg een klein geautomatiseerd eval-script in `tests/` of `eval/` |
| `search(semantic)` als DEFAULT-handhaving | verwijderen verzwakt de grep-guard die search beschermt | behouden, maar herformuleer zodat find_impact niet onder "code lookup" valt maar onder "impact/call-graph" als eigen categorie |
| find_impact op repo zonder index | vandaag: error → agent vermijdt | bij delegatie (B) wordt dit onzichtbaar goed; zonder B: betere foutmelding die de agent niet de hele tool laat vermijden |
| Backward-compat van tool-schema's | samenvoegen (C) breekt callers | alleen bij voldoende wins; anders A+B behouden beide tools |

**Belangrijkste review-waarschuwing:** niet de server-logica is kapot (H5 verworpen) —
de agent volgt de instructies correct. Een "fix" die alleen code-logica aanraakt zonder de
tekstlaag (descriptions/instructies) raakt het hoofdbewijs niet.

---

## 9. Acceptatiecriteria voor de diagnose (waneer is "oorzaak bewezen"?)
- [ ] Repro-harness (§4.4) levert een baseline: % "who calls X" → find_impact vóór fix.
- [ ] Server-tally (§4.3) toont kwantitatief de kloof (find_impact vs find kind=usages).
- [ ] H1–H4+H6 bevestigd, H5 verworpen — met code-citaten uit §3.
- [ ] Eén gekozen fix-optie (A en/of B) geïmplementeerd → repro-harness na fix toont
      meetbare stijging van find_impact-aandeel (bij A) of SCIP-kwaliteit bij find (bij B).
- [ ] Geen regressie: bestaande `find kind=usages` op niet-C# repo's blijft werken.

---

## 10. Verwijzingen
- Server-instructies (agent system-prompt bron): `src/mcp/mod.rs:7915-7953` (`INSTRUCTIONS_TEMPLATE`)
- `find_impact`-description + handler: `src/mcp/mod.rs:6236-6380` (taal-detect 6291-6329, is_available 6332-6347)
- `find`-description + dispatch: `src/mcp/mod.rs:4611-4670`
- `suggested_tool`-meta (search-result nudge): emit in zoekresultaat-output
- Publieke docs: `README.md:307-321` (find_impact C#-only framing), `README.md:241-329` (tool reference)
- Instructie-test guard: `src/mcp/mod.rs:407-433` (`test_no_deprecated_tool_aliases_in_instructions`)
- Complementair plan: `PLAN_TYPESCRIPT_SCIP.md` (dekking-uitbreiding, niet keuze-bias)
