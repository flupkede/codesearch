# Codesearch Improvement Plan — Lessons from Benchmark

**Date:** 2026-02-12  
**Based on:** 20-query benchmark (BOIN.Aprimo C# + Codesearch Rust)  
**Overall score:** Codesearch 0.61 vs Grep 0.52 — but with critical gaps

---

## Executive Summary

Codesearch wins 5/7 categories but has two glaring weaknesses: **exact name matching** (Cat A: 0.29 vs 0.99) and **structural patterns** (Cat B: 0.66 vs 1.00). Both are solvable without fundamental architecture changes. The root causes are:

1. **FTS (Tantivy) is underutilized** — it indexes content but doesn't boost exact identifier matches
2. **No language-aware filtering** — JavaScript noise pollutes C# results
3. **RRF fusion treats all signals equally** — no special weight for exact matches
4. **No project-level language metadata** — the index knows `files_by_language` at walk time but doesn't persist or use it at search time

Below are 7 concrete improvements, ordered by impact, with code-level guidance for your codebase.

---

See full plan in the rendered markdown file.
