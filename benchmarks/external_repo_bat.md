# External Repo Benchmark: bat (sharkdp/bat)

**Date**: 2025-11-25
**Repository**: https://github.com/sharkdp/bat
**Size**: 396 files, 3518 chunks, 2.6 MB

## Test Queries

| # | Query | Expected File |
|---|-------|---------------|
| 1 | syntax highlighting theme | theme.rs |
| 2 | read file from stdin input | input.rs |
| 3 | pager less integration | pager.rs or less.rs |
| 4 | git diff decorations | diff.rs |
| 5 | parse command line arguments config | config.rs |
| 6 | error handling Result type | error.rs |

## Results

### minilm-l6-q (384 dims, quantized)
- **Index Time**: 69s
- **Accuracy**: 5/6 (83%)

| Query | Result | Correct |
|-------|--------|---------|
| 1 | theme.rs | ✅ |
| 2 | input.rs | ✅ |
| 3 | pager.rs | ✅ |
| 4 | decorations.rs | ✅ |
| 5 | 50-paru.toml | ❌ |
| 6 | error.rs | ✅ |

### jina-code (768 dims, code-optimized)
- **Index Time**: 363s (~5x slower)
- **Accuracy**: 5/6 (83%)

| Query | Result | Correct |
|-------|--------|---------|
| 1 | theme.rs | ✅ |
| 2 | input.rs | ✅ |
| 3 | pager.rs | ✅ |
| 4 | diff.rs | ✅ |
| 5 | config.rs | ✅ |
| 6 | numpy_test_multiarray.py | ❌ |

## Analysis

Both models achieved 83% accuracy but on different queries:
- **minilm-l6-q** correctly found error.rs but missed config.rs
- **jina-code** correctly found config.rs and diff.rs but missed error.rs

### Performance Comparison

| Model | Dims | Index Time | Query Time | Accuracy |
|-------|------|------------|------------|----------|
| minilm-l6-q | 384 | 69s | ~2ms | 83% |
| jina-code | 768 | 363s | ~10ms | 83% |

### Recommendation

For code search tasks:
- **minilm-l6-q** offers best speed/accuracy tradeoff (5x faster indexing)
- **jina-code** may be better for specific code-related queries but much slower

The default BGE-small model (89% on codesearch codebase) is also a good balanced choice.
