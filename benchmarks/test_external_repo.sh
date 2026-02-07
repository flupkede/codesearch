#!/bin/bash
# Benchmark demongrep on external repo (bat) with top 5 models

REPO_PATH="/tmp/bat"
DEMONGREP="./target/release/demongrep"

# Top 5 models from our benchmark
MODELS=("minilm-l6-q" "jina-code" "e5-multilingual" "bge-small" "bge-small-q")

# Test queries for bat codebase (cat clone with syntax highlighting)
# Format: "query|expected_file_pattern"
QUERIES=(
    "syntax highlighting theme|theme"
    "read file from stdin|input"
    "pager less integration|less"
    "git diff decorations|diff"
    "parse command line arguments|config"
    "error handling Result|error"
    "Controller print output|controller"
    "asset loading syntaxes|assets"
)

echo "=========================================="
echo "DEMONGREP EXTERNAL REPO BENCHMARK"
echo "Repository: bat (sharkdp/bat)"
echo "=========================================="
echo ""

# Results file
RESULTS_FILE="benchmarks/external_repo_results.md"
echo "# External Repo Benchmark: bat" > $RESULTS_FILE
echo "" >> $RESULTS_FILE
echo "**Date**: $(date '+%Y-%m-%d %H:%M')" >> $RESULTS_FILE
echo "**Repository**: sharkdp/bat" >> $RESULTS_FILE
echo "" >> $RESULTS_FILE
echo "## Results Summary" >> $RESULTS_FILE
echo "" >> $RESULTS_FILE
echo "| Model | Accuracy | Index Time | Avg Query Time |" >> $RESULTS_FILE
echo "|-------|----------|------------|----------------|" >> $RESULTS_FILE

for MODEL in "${MODELS[@]}"; do
    echo ""
    echo "=========================================="
    echo "Testing model: $MODEL"
    echo "=========================================="

    # Clear any existing index
    rm -rf "$REPO_PATH/.demongrep.db"

    # Index with this model
    echo "Indexing..."
    INDEX_START=$(date +%s.%N)
    $DEMONGREP --model $MODEL index --path $REPO_PATH 2>&1 | grep -E "(chunks|Embedding|Total:)"
    INDEX_END=$(date +%s.%N)
    INDEX_TIME=$(echo "$INDEX_END - $INDEX_START" | bc)

    # Run test queries
    CORRECT=0
    TOTAL=${#QUERIES[@]}
    QUERY_TIMES=()

    echo ""
    echo "Running ${TOTAL} test queries..."

    for QUERY_PAIR in "${QUERIES[@]}"; do
        QUERY=$(echo $QUERY_PAIR | cut -d'|' -f1)
        EXPECTED=$(echo $QUERY_PAIR | cut -d'|' -f2)

        QUERY_START=$(date +%s.%N)
        RESULT=$($DEMONGREP search "$QUERY" --path $REPO_PATH --compact 2>&1 | grep -v "INFO" | head -1)
        QUERY_END=$(date +%s.%N)
        QUERY_TIME=$(echo "$QUERY_END - $QUERY_START" | bc)
        QUERY_TIMES+=($QUERY_TIME)

        if echo "$RESULT" | grep -qi "$EXPECTED"; then
            echo "  ✅ \"$QUERY\" -> $RESULT"
            ((CORRECT++))
        else
            echo "  ❌ \"$QUERY\" -> $RESULT (expected: *$EXPECTED*)"
        fi
    done

    # Calculate average query time
    TOTAL_QUERY_TIME=0
    for T in "${QUERY_TIMES[@]}"; do
        TOTAL_QUERY_TIME=$(echo "$TOTAL_QUERY_TIME + $T" | bc)
    done
    AVG_QUERY_TIME=$(echo "scale=3; $TOTAL_QUERY_TIME / $TOTAL" | bc)

    ACCURACY=$(echo "scale=0; $CORRECT * 100 / $TOTAL" | bc)

    echo ""
    echo "Results for $MODEL:"
    echo "  Accuracy: $ACCURACY% ($CORRECT/$TOTAL)"
    echo "  Index time: ${INDEX_TIME}s"
    echo "  Avg query time: ${AVG_QUERY_TIME}s"

    # Add to results file
    echo "| $MODEL | $ACCURACY% | ${INDEX_TIME}s | ${AVG_QUERY_TIME}s |" >> $RESULTS_FILE
done

echo ""
echo "=========================================="
echo "Benchmark complete! Results saved to $RESULTS_FILE"
echo "=========================================="
