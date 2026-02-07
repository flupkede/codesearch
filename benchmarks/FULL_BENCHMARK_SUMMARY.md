# DemonGrep Embedding Model Benchmark - Full Summary

**Date**: 2025-11-25
**Test Chunks**: ~607 chunks from codesearch codebase
**Test Queries**: 9 semantic search queries

## Results Summary (Sorted by Accuracy)

| Model | Dims | Accuracy | Avg Score | Index Time | Query Time | Notes |
|-------|------|----------|-----------|------------|------------|-------|
| AllMiniLML6V2Q | 384 | **100%** | 0.554 | ~25s | 1.79ms | **BEST ACCURACY** - Quantized, fastest |
| JinaEmbeddingsV2BaseCode | 768 | 89% | 0.714 | 73.6s | 10.5ms | Code-optimized, low false positives (0.34) |
| MultilingualE5Small | 384 | 89% | 0.886 | 28.0s | 3.4ms | High scores but high false positive (0.90) |
| BGESmallENV15 | 384 | 89% | 0.792 | ~30s | ~2ms | **DEFAULT** - Good balance |
| BGESmallENV15Q | 384 | 89% | 0.792 | ~30s | ~2ms | Quantized version |
| AllMiniLML6V2 | 384 | 78% | 0.556 | ~30s | ~3ms | Non-quantized |
| AllMiniLML12V2Q | 384 | 78% | 0.570 | 25.8s | 1.8ms | Quantized L12 |
| ParaphraseMLMiniLML12V2 | 384 | 78% | 0.598 | 30.6s | 2.8ms | Paraphrase-optimized |
| NomicEmbedTextV1 | 768 | 78% | 0.624 | 72.7s | 11.7ms | |
| NomicEmbedTextV15 | 768 | 78% | 0.666 | 68.4s | 11.7ms | |
| NomicEmbedTextV15Q | 768 | 78% | 0.662 | 59.9s | 4.7ms | Quantized |
| MxbaiEmbedLargeV1 | 1024 | 78% | 0.771 | 164.4s | 33.1ms | Large model |
| BGEBaseENV15 | 768 | 67% | 0.792 | 64.8s | 8.3ms | |
| AllMiniLML12V2 | 384 | 56% | 0.567 | ~25s | ~2ms | |
| ModernBertEmbedLarge | 1024 | 56% | 0.699 | 203.1s | 30.6ms | Newest architecture, slow |

**Skipped**: BGELargeENV15 (1024 dims) - Memory intensive

## Key Findings

### Top Performers for Code Search
1. **AllMiniLML6V2Q** (100% accuracy) - Best overall, quantized = fast
2. **JinaEmbeddingsV2BaseCode** (89%) - Code-specialized, excellent false positive resistance
3. **BGESmallENV15** (89%) - Current default, good balance of speed and accuracy

### Speed vs Quality Tradeoffs
- **Fastest**: AllMiniLML6V2Q - 1.79ms query time, 25s indexing
- **Slowest**: ModernBertEmbedLarge - 30.63ms query time, 203s indexing
- **Best balance**: BGESmallENV15 - ~2ms query, ~30s indexing, 89% accuracy

### Observations
- Quantized models (Q suffix) are faster with minimal accuracy loss
- Larger models (768/1024 dims) don't necessarily mean better code search accuracy
- Code-specialized models (Jina) perform well on code search tasks
- MultilingualE5Small has high scores but poor discrimination (0.90 false positive)

## Recommendations

| Use Case | Recommended Model |
|----------|-------------------|
| Best accuracy | AllMiniLML6V2Q |
| Code-specific search | JinaEmbeddingsV2BaseCode |
| Balanced (current default) | BGESmallENV15 |
| Resource constrained | AllMiniLML6V2Q |
| Need high semantic similarity | MultilingualE5Small (watch false positives) |

## Test Queries Used

1. "SemanticChunker struct" → src/chunker/semantic.rs
2. "VectorStore insert chunks" → src/vectordb/store.rs
3. "tree-sitter grammar loading" → src/chunker/parser.rs
4. "extract function signature from AST" → src/chunker/extractor.rs
5. "how do we detect binary files" → src/file/binary.rs
6. "where is the main entry point" → src/main.rs
7. "CLI argument parsing clap" → src/cli/mod.rs
8. "FileWalker walk directory" → file_walker
9. "RustExtractor python typescript" → src/chunker/extractor.rs

False positive test: "kubernetes deployment yaml" (should score < 0.85)
