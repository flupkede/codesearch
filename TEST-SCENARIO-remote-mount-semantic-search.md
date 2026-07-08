# Test Scenario — Semantic findability across remote-mounted doc projects

Acceptance test for the `features/remote-mount-selection` work: does codesearch
**find the right content in the right mounted doc project — and *not* surface it
where it doesn't belong** — and does the federated `get_chunk` round-trip work
end-to-end (the Stage A `ambiguous_chunk_id` fix).

The corpus is six product-documentation indexes mounted from the `cloud` peer, a
natural mix of **PIM** and **DAM** products. That overlap (two PIMs, three DAMs)
is exactly what makes findability testable: a PIM concept *should* surface in a
PIM index and *should not* have a genuine match in a DAM index, and vice-versa.

---

## 0. Preconditions

| # | Check | How |
|---|-------|-----|
| P1 | `serve` is active and the six mounts are present | `status(kind="projects")` → `remote_projects[]` lists `cloud/akeneo`, `cloud/example-dam`, `cloud/bynder`, `cloud/custom-kb`, `cloud/digizuite`, `cloud/inriver` |
| P2 | The `docs` group federates the peer | `status(kind="projects")` → `groups.docs == ["@cloud"]` |
| P3 | **The serve process runs the Stage-A binary** | A remote search result's `chunk_ref` is namespaced `"cloud/inriver:<id>"`, **not** the legacy `"cloud:<id>"`. See ⚠️ below. |

> ⚠️ **Known state at time of writing:** the live serve still runs a **pre-Stage-A
> binary** — remote results come back with legacy `chunk_ref` `"cloud:<id>"` (no
> alias). Section **D** is therefore the gating regression test: it is expected to
> reproduce the old `ambiguous_chunk_id` bug on the current binary and to pass only
> after serve is rebuilt/restarted with the fixed binary. Redeploy, then re-run.

### The mounted products (concept map)

| Mount | Product | Domain | Signature concepts (owns) |
|-------|---------|--------|---------------------------|
| `cloud/inriver` | inriver | **PIM** | entity/link model, variants, channels, syndication, Enrich, Control Center |
| `cloud/akeneo` | Akeneo | **PIM** | product families, attribute groups, categories, reference entities, connectors |
| `cloud/example-dam` | example-dam | **DAM + MO** | Marketing Operations, workflow designer, DAM records, classifications, review/approval |
| `cloud/bynder` | Bynder | **DAM** | asset portal, brand guidelines, Studio, collections, asset workflow |
| `cloud/digizuite` | Digizuite | **DAM** | DAM Center, media renditions, transformations, publishing destinations |
| `cloud/custom-kb` | custom KB | mixed | wildcard — no assumption |

---

## ⚖️ Scoring caveat — READ THIS BEFORE JUDGING RESULTS

Result `score` is **RRF (Reciprocal Rank Fusion)** — a *rank-based* number, not an
absolute similarity. In calibration the **top hit scored ~0.0476 in every index**,
including one where the query had no genuine match. So:

- **Never judge findability by the score number.** The top score is ~0.0476 whether
  the match is perfect or garbage.
- **Judge findability by the returned content**: does the top-ranked chunk's
  `path` + body actually address the queried concept?
- A **true positive** = the top 1–3 chunks are *on-topic* docs for the concept.
- A **true negative** = the top chunks are *off-topic* (release notes, unrelated
  features) — the concept simply isn't documented in that product.

---

## A. True positives — semantic recall in the owning product

Each query is phrased in **different words** than the docs use, so a plain keyword
match would miss it. Semantic search must still surface the right doc.
Run with `search(mode="semantic", project="<mount>", query="…", limit=5)`.

| # | Project | Query (natural language) | PASS = top 1–3 chunks are about… | Calibrated? |
|---|---------|--------------------------|----------------------------------|-------------|
| A1 | `cloud/inriver` | "how are product entities linked to variants and sales channels" | inriver **entity / elastic data model** (e.g. `…/What-is-an-entity.md`, channel/link docs) | ✅ verified — hit `getting-started/elastic-data-model-common-terminology/…What-is-an-entity.md` |
| A2 | `cloud/akeneo` | "grouping product attributes into families and attribute groups" | Akeneo **families / attribute groups** docs | ⬜ to verify |
| A3 | `cloud/example-dam` | "digital asset review and approval workflow" | example-dam **Marketing Operations workflow** (e.g. `…/workflow_admin/workflow_designer_concepts…`) | ✅ verified — hit `Marketing_Operations_Help/workflow_admin/workflow_designer_concepts.html.md` |
| A4 | `cloud/bynder` | "set up an asset approval workflow and organize assets into collections" | Bynder **Asset-Workflow / collections** (e.g. `…/Asset-Workflow/…Asset-Workflow.md`) | ⬜ to verify (Asset-Workflow.md already appeared as a side hit under B1) |
| A5 | `cloud/digizuite` | "generate media renditions and publish them to a destination" | Digizuite **renditions / transformation / publishing** docs | ⬜ to verify |

**Expected:** all five PASS. Record the top chunk `path` + `chunk_ref` for each in
the results table.

---

## B. True negatives — a concept that lives in the *other* domain

Take a concept a product genuinely **does not have** and query the product that
lacks it. Semantic search will still return *something* (it always ranks the
top-k), so PASS is defined by **off-topic** content, not an empty result.

| # | Project | Query (from the *wrong* domain) | PASS = top chunks are OFF-topic (concept absent) | Result |
|---|---------|--------------------------------|--------------------------------------------------|--------|
| B1 | `cloud/bynder` (DAM) | "how are product entities linked to variants and sales channels" (PIM) | No PIM entity/link model; hits are generic DAM articles | ✅ **clean negative** — `Product-Feedback…`, `…AI-Agents…`, Studio; no PIM model |
| B2 | `cloud/example-dam` (DAM) | "how are product entities linked to variants and sales channels" (PIM) | No PIM entity model in a DAM/MO product | ✅ **clean negative** — `system_types_reference`, DAM `RecordLink` field, `clients_associated_programs`; no PIM entity/variant/channel model |
| B3 | `cloud/inriver` (PIM) | "automatically generate cropped image renditions and file derivatives from a master asset" (DAM) | No rendition/derivative engine in a PIM | ✅ **clean negative** — top hits are release notes / product announcements; inriver has no image-rendition transformation |
| B4 | `cloud/akeneo` (PIM) | "track marketing campaign budget spend and program financial actuals" (example-dam MO) | No budget/financials in a PIM catalog | ✅ **clean negative** — top hits are Google-Shopping insights / Studio analytics; Akeneo has no marketing-spend tracking |

> **📌 Key lesson — how to design a clean true-negative probe.**
> A clean true negative needs a concept with **no adjacent feature** in the target
> product. Two examples of what *not* to do, found while building this scenario:
> - "brand guidelines portal" against `cloud/inriver` → matched inriver's own
>   **Brand Store** (`…Introduction-to-the-new-Brand-Store.md`).
> - "workflow designer and task approvals" against `cloud/akeneo` → matched Akeneo's
>   own **collaboration workflows** (`…what-are-collaboration-workflows.md`).
>
> Neither was the queried DAM/MO concept, but both are *genuine* features of the PIM
> product — so semantic search correctly surfaced the nearest real concept. That is
> **semantic search working**, not a scoping failure. The B3/B4 queries above were
> therefore sharpened to concepts that are truly unique to the *other* domain
> (rendition generation = DAM engine; marketing budget = example-dam MO), which produce
> clean negatives. Rule of thumb: probe with a **product-unique** concept, never a
> generic verb like "workflow" or "portal".
>
> A real regression would be a DAM index returning an **on-topic PIM entity-model**
> doc for B1/B2 — that would mean mis-scoped mounts or corpus contamination.

---

## C. Cross-product overlap — shared concept, per-product answers

A concept the three DAMs **all** share ("metadata fields on a digital asset").
Query each DAM individually, then the whole peer via the group.

| # | Scope | Query | PASS = |
|---|-------|-------|--------|
| C1 | `project=cloud/example-dam` | "add and edit metadata fields on a digital asset" | example-dam field/classification docs |
| C2 | `project=cloud/bynder` | "add and edit metadata fields on a digital asset" | Bynder metaproperty/tagging docs |
| C3 | `project=cloud/digizuite` | "add and edit metadata fields on a digital asset" | Digizuite metadata docs |
| C4 | `group=docs` | "add and edit metadata fields on a digital asset" | Fused results from **multiple** peers; each result carries the correct `source`/`chunk_ref` for its origin |

**Expected:** C1–C3 each return that product's own vocabulary; C4 interleaves hits
from more than one DAM and every result is correctly attributed. (C4 also exercises
RRF fusion across federated peers.)

---

## D. `get_chunk` namespaced round-trip — Stage A acceptance / regression

This is the **gating** test for the fix. inriver on the peer is a multi-repo index,
which is exactly the shape that triggered the original `ambiguous_chunk_id` bug.

**Steps**
1. `search(project="cloud/inriver", query="what is an entity", limit=3)` → note the top result's `chunk_ref`.
2. `get_chunk(chunk_ref="<that value>", context_lines=5)`.

| Binary | Step 1 `chunk_ref` shape | Step 2 result |
|--------|--------------------------|---------------|
| **Old (pre-Stage-A, current live serve)** | legacy `"cloud:<id>"` — alias dropped | ❌ FAIL — `ambiguous_chunk_id` (peer can't disambiguate the multi-repo index), the bug that started this |
| **New (Stage-A binary)** | namespaced `"cloud/inriver:<id>"` | ✅ PASS — returns the chunk body; `project=inriver` is forwarded to the peer so the lookup is unambiguous |

**PASS criteria (new binary):**
- `chunk_ref` is `"cloud/inriver:<id>"` (namespaced).
- `get_chunk` returns the chunk `content` (the entity-definition prose), **not** an error.
- A legacy `"cloud:<id>"` ref still resolves via the group-scope fallback (backward-compat) — optional extra check.

---

## E. Fail-open sanity (web-guard interplay) — optional

Confirms the guard doesn't over-block once mounts exist and steers correctly.

| # | Setup | Action | PASS = |
|---|-------|--------|--------|
| E1 | mounts present (P1) | trigger a `WebSearch` on a product-doc question | web-guard **denies once** with guidance to `search(project="cloud/…")` + `get_chunk(chunk_ref=…)` |
| E2 | same query retried within 5 min | repeat the `WebSearch` | guard **allows** it (retry-escape) |
| E3 | `remote_mounts` empty in `repos.json` | trigger a `WebSearch` | guard **passes through** (fail-open, nothing to steer toward) |

---

## F. Cross-vendor overlap + isolation (5 scenarios)

Where **B** proved isolation (a concept absent from the wrong domain), **F** proves
the complementary half: a concept **shared** across vendors must surface hits from
**multiple vendors at once** via `group="docs"` (federated RRF fusion) — while a
domain-specific concept still stays absent from the other domain (isolation).

Run each **overlap query** with `search(group="docs", …)` and confirm ≥2 vendors
return *on-topic* hits. Run each **isolation probe** with `project="<opposite-domain vendor>"`
and confirm *off-topic* results. All rows below were executed (Run 1).

> **How `group="docs"` fusion reads:** RRF interleaves each peer's rank-1 hit at the
> same top score (~0.0476), so a healthy overlap looks like *one strong hit per
> relevant vendor* stacked at the top. Judge by the `path`, not the score.

### F1 — Category hierarchy *(cross-domain organizational concept)*
- **Overlap** `group=docs`: *"organize products into a category hierarchy or category tree"*
- **On-topic, multi-vendor:** akeneo `…/serenity-what-is-a-category.md`, bynder `…/Glossary/…What-is-a-Taxonomy.md`, digizuite `…/api/tree/nodes/item/…`
- **Adjacent (not wrong):** example-dam `expense_hierarchies` (MO financial), inriver release notes, custom-kb classification pickers
- **Verdict:** ✅ PASS — 3 vendors on-topic across **both** domains (PIM akeneo + DAM bynder/digizuite)

### F2 — Product data completeness *(PIM-owned overlap + DAM isolation)*
- **Overlap** `group=docs`: *"measure product data completeness and enrichment quality"*
- **On-topic PIM:** inriver `…/working-in-enrich/…different-completeness-rules….md`, akeneo `…/understand-data-quality.md`
- **Isolation probe** `project=cloud/bynder`: top hits `Tips-For-Measuring-Success-And-Adoption`, `Stibo-Integration`, `Collections-Dashboard` — **no** product-completeness concept
- **Verdict:** ✅ PASS — two PIMs own it; a pure DAM does not (clean isolation)

### F3 — Asset access permissions *(DAM-owned overlap)*
- **Overlap** `group=docs`: *"restrict who can view or download an asset using permissions and rights"*
- **On-topic:** bynder `…/Permission-Management/…Customize-User-Permissions-to-Download-Assets.md`, digizuite `…/api/assets/security/…`, example-dam `…/rights_reference.html.md`, akeneo (DAM module) `…/set-rights-on-your-asset-families.md`
- **Isolation signal** inriver: `…/entities/…Locking-Entities.md` — its own *entity-locking*, not asset download → stays in its lane
- **Verdict:** ✅ PASS — 3 DAMs + Akeneo's DAM module converge; PIM entity-locking is adjacent, not a false hit

### F4 — Asset version history *(DAM-owned overlap + clean PIM isolation)*
- **Overlap** `group=docs`: *"keep version history of an asset and revert to a previous version"*
- **On-topic:** example-dam `…/digital_assets_creating_versions.html.md`, bynder `…/Upload/…Upload-New-Version-of-an-Asset.md`, digizuite `…/api/assets/create-versions.md`, akeneo `…/how-to-view-and-restore-a-previous-version-of-an-asset.md`
- **Isolation probe** `project=cloud/inriver`: **release notes only** — inriver (PIM) has no asset-versioning/revert
- **Verdict:** ✅ PASS — strongest 4-vendor DAM overlap + clean PIM isolation

### F5 — Publish / syndicate to a channel *(true cross-domain overlap — the highlight)*
- **Overlap** `group=docs`: *"publish or syndicate content out to an external channel or destination"*
- **On-topic PIM:** inriver `…October-2025…Syndication-Workflows….md`, akeneo `…/managing-and-distributing-enhanced-content.md`
- **On-topic DAM:** example-dam `…/integration_workbench_publishers_concept.html.md`, bynder `…/Guide-to-Delivering-Multi-Channel-Content-with-Content-Workflow.md`, digizuite `…/api/admin/mediatranscode.md`
- **Verdict:** ✅ PASS — on-topic hits from **both** domains; the best single demonstration of full-peer federated fusion

### Verdict — F (overlap + isolation)

**5/5 PASS.** Federated RRF fusion surfaces the right *set* of vendors for a shared
concept (F1/F5 span both domains; F3/F4 converge the DAMs), and isolation still holds
where a concept is domain-specific (F2 PIM-only, F4 PIM has no asset versioning). This
is the positive counterpart to B: not just "not found in the wrong place", but
"found across all the right places, each correctly attributed".

---

## Run 1 — executed results (Stage-A binary, serve restarted)

`chunk_ref` came back **namespaced** (`cloud/<alias>:<id>`) and `source` = `cloud/<alias>`
on every remote result → **P3 PASS**, the Stage-A fix is live.

| Case | Scope | Query | Top chunk `path` | `chunk_ref` | Verdict |
|------|-------|-------|------------------|-------------|---------|
| A1 | cloud/inriver | product↔variant↔channel | `…/elastic-data-model…/What-is-an-entity.md` + `…/Intelligent-linking-of-Entities…md` | `cloud/inriver:1004` | ✅ PASS |
| A2 | cloud/akeneo | families/attribute groups | `…/serenity-what-is-a-family.md` + `…/manage-attribute-inheritance.md` | `cloud/akeneo:3770` | ✅ PASS |
| A3 | cloud/example-dam | asset review/approval | `…/workflow_admin/workflow_designer_concepts.html.md` | `cloud/example-dam:9645` | ✅ PASS |
| A4 | cloud/bynder | approval workflow + collections | `…/Asset-Workflow/…Asset-Workflow-Assets.md` + `…Asset-Workflow.md` | `cloud/bynder:1458` | ✅ PASS |
| A5 | cloud/digizuite | renditions + publish | `…/LegacyService/POST/api/renditions/_assetId_.md` | `cloud/digizuite:491` | ✅ PASS |
| B1 | cloud/bynder | PIM entity model (neg) | off-topic (Product-Feedback, AI-Agents) | — | ✅ PASS (clean neg) |
| B2 | cloud/example-dam | PIM entity model (neg) | off-topic (`system_types_reference`, DAM `RecordLink`) | — | ✅ PASS (clean neg) |
| B3 | cloud/inriver | DAM rendition/derivative engine (neg) | off-topic (release notes / product announcements) | — | ✅ PASS (clean neg) |
| B4 | cloud/akeneo | example-dam MO budget/financials (neg) | off-topic (Google-Shopping insights, Studio analytics) | — | ✅ PASS (clean neg) |
| C1 | cloud/example-dam | asset metadata fields | `…/Asset_Studio_Help/MetadataTemplates.htm.md` | `cloud/example-dam:5874` | ✅ PASS |
| C2 | cloud/bynder | asset metadata fields | `…/Upload/…Understanding-And-Using-Metadata.md` | `cloud/bynder:1116` | ✅ PASS |
| C3 | cloud/digizuite | asset metadata fields | `…/GET/api/metafield/asset-info.md` + `…/POST/api/metadata/editor.md` | `cloud/digizuite:1008` | ✅ PASS |
| C4 | group=docs | asset metadata fields | fused: digizuite + inriver + custom-kb + bynder + example-dam + akeneo | mixed, each correctly attributed | ✅ PASS (RRF fusion + attribution) |
| D | cloud/inriver | `get_chunk("cloud/inriver:1004")` | returned full "What is an entity?" body, **no `ambiguous_chunk_id`** | `cloud/inriver:1004` | ✅ **PASS (gating)** |
| E1–E3 | web-guard | — | — | — | ⬜ not run this pass |

### Verdict — Run 1

- **A (recall): 5/5 PASS** — semantic search finds the right doc in the owning product even when the query wording differs from the docs.
- **B (isolation): 4/4 clean negatives** — no cross-domain contamination. (B3/B4 were sharpened to product-unique concepts after the first draft's generic probes matched the PIMs' own adjacent features — see the 📌 note; that was test-design, not a product bug.)
- **C (overlap/fusion): 4/4 PASS** — per-product answers are product-specific, and `group=docs` fuses all six peers with correct `source`/`chunk_ref` attribution.
- **D (Stage-A gating): PASS** — namespaced `chunk_ref` round-trips; the original `ambiguous_chunk_id` bug is fixed on the live binary.

**Overall: PASS.** The remote-mount semantic search behaves as designed; the only
follow-up is refining the true-negative probes (B3/B4) to product-unique concepts.

**Overall PASS criterion (for re-runs) =** all A PASS (recall) **and** B shows no
on-topic cross-domain hit (isolation) **and** D PASS on the Stage-A binary (round-trip).
C and E are supporting evidence.
