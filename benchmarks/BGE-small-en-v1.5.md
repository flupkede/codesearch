# Benchmark: BAAI/bge-small-en-v1.5

**Date**: 2024-11-24
**Model**: `BAAI/bge-small-en-v1.5`
**Variant**: `BGESmallENV15`
**Dimensions**: 384
**Quantized**: No

## Indexing Performance

| Metric | Value |
|--------|-------|
| Files indexed | 46 |
| Chunks created | 592 |
| Database size | 4.06 MB |
| Avg per chunk | 7.03 KB |

## Search Performance

| Metric | Value |
|--------|-------|
| Database load | ~70µs |
| Model load | ~150ms |
| Query embed | ~4ms |
| Search | ~600µs |
| **Total latency** | **~155ms** |

## Accuracy Tests

| Query | Expected File | Top Result | Score | Correct |
|-------|---------------|------------|-------|---------|
| "SemanticChunker struct" | semantic.rs | `src/chunker/semantic.rs` SemanticChunker | **0.929** | ✅ |
| "VectorStore insert chunks" | store.rs | `src/vectordb/store.rs` insert_chunks() | **0.912** | ✅ |
| "tree-sitter grammar loading" | parser.rs | `src/chunker/parser.rs` | **0.903** | ✅ |
| "extract function signature from AST" | extractor.rs | `src/chunker/extractor.rs` extract_signature() | **0.894** | ✅ |
| "how do we detect binary files" | binary.rs | `src/file/binary.rs` | **0.909** | ✅ |
| "where is the main entry point" | main.rs | `src/main.rs` main() | ✅ | ✅ |
| "CLI argument parsing clap" | cli/mod.rs | `src/cli/mod.rs` Cli struct | ✅ | ✅ |
| "FileWalker walk directory" | file walker | `examples/file_walker_demo.rs` | ✅ | ✅ |
| "RustExtractor python typescript" | extractor.rs | `src/chunker/extractor.rs` | **0.894** | ✅ |

### Edge Case (Non-existent content)

| Query | Result | Score | Note |
|-------|--------|-------|------|
| "kubernetes deployment yaml" | PROJECT_STATUS.md | **0.825** | False positive, lower score |

## Summary

| Metric | Value |
|--------|-------|
| **Accuracy** | 9/9 (100%) |
| **Avg score (correct)** | 0.90 |
| **False positive score** | 0.825 |
| **Suggested threshold** | 0.85 |

## Notes

- Excellent accuracy on code-related queries
- Natural language questions work well
- False positives have noticeably lower scores (~0.82 vs ~0.90)
- Fast search latency after initial model load
