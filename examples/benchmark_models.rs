//! Benchmark suite for comparing embedding models
//!
//! Run with: cargo run --release --example benchmark_models
//!
//! This will test different embedding models and generate benchmark results.

use anyhow::Result;
use codesearch::chunker::{Chunk, SemanticChunker};
use codesearch::embed::{FastEmbedder, ModelType};
use codesearch::file::FileWalker;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Test queries with expected top results
const TEST_QUERIES: &[(&str, &str)] = &[
    ("SemanticChunker struct", "src/chunker/semantic.rs"),
    ("VectorStore insert chunks", "src/vectordb/store.rs"),
    ("tree-sitter grammar loading", "src/chunker/parser.rs"),
    (
        "extract function signature from AST",
        "src/chunker/extractor.rs",
    ),
    ("how do we detect binary files", "src/file/binary.rs"),
    ("where is the main entry point", "src/main.rs"),
    ("CLI argument parsing clap", "src/cli/mod.rs"),
    ("FileWalker walk directory", "file_walker"),
    (
        "RustExtractor python typescript",
        "src/chunker/extractor.rs",
    ),
];

/// False positive test (should have low score)
const FALSE_POSITIVE_QUERY: &str = "kubernetes deployment yaml";

#[derive(Debug)]
struct BenchmarkResult {
    model: ModelType,
    model_load_time: Duration,
    index_time: Duration,
    chunks_created: usize,
    avg_query_time: Duration,
    accuracy: f32,
    avg_score: f32,
    false_positive_score: f32,
}

fn main() -> Result<()> {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║           DEMONGREP EMBEDDING MODEL BENCHMARK                ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // Models to benchmark - Batch 4: ModernBERT (final model)
    // All others tested successfully
    let models_to_test = vec![ModelType::ModernBertEmbedLarge];

    println!("📋 Models to benchmark: {}", models_to_test.len());
    for model in &models_to_test {
        let model_name: &str = model.name();
        println!("   - {} ({} dims)", model_name, model.dimensions());
    }
    println!();

    // Collect files and chunks once
    println!("📂 Collecting files from current directory...");
    let walker = FileWalker::new(PathBuf::from("."));
    let (files, _stats) = walker.walk()?;
    let files_count: usize = files.len();
    println!("   Found {} indexable files", files_count);

    // Create chunks
    println!("🔪 Creating semantic chunks...");
    let mut chunker = SemanticChunker::new(100, 4000, 5);
    let mut all_chunks = Vec::new();
    for file in &files {
        if let Ok(content) = fs::read_to_string(&file.path) {
            if let Ok(chunks) = chunker.chunk_semantic(file.language, &file.path, &content) {
                all_chunks.extend(chunks);
            }
        }
    }
    println!("   Created {} chunks", all_chunks.len());
    println!();

    // Benchmark each model
    let mut results = Vec::new();

    for model_type in models_to_test {
        println!("═══════════════════════════════════════════════════════════════");
        let model_name: &str = model_type.name();
        println!("🧪 Testing: {}", model_name);
        println!("   Dimensions: {}", model_type.dimensions());
        println!("   Quantized: {}", model_type.is_quantized());
        println!("───────────────────────────────────────────────────────────────");

        match benchmark_model(model_type, &all_chunks) {
            Ok(result) => {
                print_result(&result);
                results.push(result);
            }
            Err(e) => {
                println!("   ❌ Error: {}", e);
            }
        }
        println!();
    }

    // Print summary
    print_summary(&results);

    // Save results to markdown
    save_results_to_markdown(&results)?;

    Ok(())
}

fn benchmark_model(model_type: ModelType, chunks: &[Chunk]) -> Result<BenchmarkResult> {
    // 1. Load model
    let start = Instant::now();
    let mut embedder = FastEmbedder::with_model(model_type)?;
    let model_load_time = start.elapsed();
    println!("   ⏱️  Model load: {:?}", model_load_time);

    // 2. Create embeddings for all chunks
    let start = Instant::now();
    let texts: Vec<String> = chunks
        .iter()
        .map(|c| {
            let context_str = c.context.join(" > ");
            format!(
                "{}\n{}\n{}",
                context_str,
                c.signature.as_deref().unwrap_or(""),
                c.content
            )
        })
        .collect();

    let embeddings = embedder.embed_batch(texts)?;
    let index_time = start.elapsed();
    println!("   ⏱️  Indexing {} chunks: {:?}", chunks.len(), index_time);

    // 3. Run accuracy tests
    let mut correct = 0;
    let mut total_score = 0.0f32;
    let mut query_times = Vec::new();

    for (query, expected_file) in TEST_QUERIES {
        let start = Instant::now();
        let query_embedding = embedder.embed_one(query)?;
        query_times.push(start.elapsed());

        // Find best match
        let mut best_score = 0.0f32;
        let mut best_idx = 0;

        for (i, emb) in embeddings.iter().enumerate() {
            let score = cosine_similarity(&query_embedding, emb);
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        let best_chunk = &chunks[best_idx];
        let is_correct = best_chunk.path.contains(expected_file);

        if is_correct {
            correct += 1;
        }
        total_score += best_score;

        println!(
            "   {} \"{}\" -> {} (score: {:.3})",
            if is_correct { "✅" } else { "❌" },
            &query[..query.len().min(30)],
            best_chunk
                .path
                .split('/')
                .last()
                .unwrap_or(&best_chunk.path),
            best_score
        );
    }

    // 4. Test false positive
    let query_embedding = embedder.embed_one(FALSE_POSITIVE_QUERY)?;
    let mut false_positive_score = 0.0f32;
    for emb in &embeddings {
        let score = cosine_similarity(&query_embedding, emb);
        if score > false_positive_score {
            false_positive_score = score;
        }
    }
    println!(
        "   ⚠️  False positive score: {:.3} (should be < 0.85)",
        false_positive_score
    );

    let accuracy = correct as f32 / TEST_QUERIES.len() as f32;
    let avg_score = total_score / TEST_QUERIES.len() as f32;
    let avg_query_time = query_times.iter().sum::<Duration>() / query_times.len().max(1) as u32;

    Ok(BenchmarkResult {
        model: model_type,
        model_load_time,
        index_time,
        chunks_created: chunks.len(),
        avg_query_time,
        accuracy,
        avg_score,
        false_positive_score,
    })
}

fn print_result(result: &BenchmarkResult) {
    println!("───────────────────────────────────────────────────────────────");
    println!(
        "   📊 Accuracy: {:.0}% ({}/{})",
        result.accuracy * 100.0,
        (result.accuracy * TEST_QUERIES.len() as f32) as usize,
        TEST_QUERIES.len()
    );
    println!("   📊 Avg score: {:.3}", result.avg_score);
    println!("   📊 Avg query time: {:?}", result.avg_query_time);
}

fn print_summary(results: &[BenchmarkResult]) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                        SUMMARY                               ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // Sort by accuracy, then by avg_score
    let mut sorted: Vec<_> = results.iter().collect();
    sorted.sort_by(|a, b| {
        b.accuracy
            .partial_cmp(&a.accuracy)
            .unwrap()
            .then(b.avg_score.partial_cmp(&a.avg_score).unwrap())
    });

    println!(
        "{:<25} {:>6} {:>8} {:>10} {:>12} {:>8}",
        "Model", "Dims", "Acc", "Avg Score", "Index Time", "Query"
    );
    println!("{}", "─".repeat(75));

    for r in sorted {
        println!(
            "{:<25} {:>6} {:>7.0}% {:>10.3} {:>12.2?} {:>8.2?}",
            r.model.short_name(),
            r.model.dimensions(),
            r.accuracy * 100.0,
            r.avg_score,
            r.index_time,
            r.avg_query_time
        );
    }
}

fn save_results_to_markdown(results: &[BenchmarkResult]) -> Result<()> {
    let mut content = String::new();

    content.push_str("# Embedding Model Benchmark Results\n\n");
    content.push_str(&format!(
        "**Date**: {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    ));
    content.push_str(&format!(
        "**Chunks**: {}\n\n",
        results.first().map(|r| r.chunks_created).unwrap_or(0)
    ));

    content.push_str("## Summary\n\n");
    content.push_str("| Model | Dims | Accuracy | Avg Score | Index Time | Query Time |\n");
    content.push_str("|-------|------|----------|-----------|------------|------------|\n");

    for r in results {
        content.push_str(&format!(
            "| {} | {} | {:.0}% | {:.3} | {:.2?} | {:.2?} |\n",
            r.model.short_name(),
            r.model.dimensions(),
            r.accuracy * 100.0,
            r.avg_score,
            r.index_time,
            r.avg_query_time
        ));
    }

    content.push_str("\n## Individual Results\n\n");

    for r in results {
        content.push_str(&format!("### {}\n\n", r.model.name()));
        content.push_str(&format!("- **Dimensions**: {}\n", r.model.dimensions()));
        content.push_str(&format!("- **Quantized**: {}\n", r.model.is_quantized()));
        content.push_str(&format!("- **Model Load**: {:?}\n", r.model_load_time));
        content.push_str(&format!("- **Index Time**: {:?}\n", r.index_time));
        content.push_str(&format!("- **Accuracy**: {:.0}%\n", r.accuracy * 100.0));
        content.push_str(&format!("- **Avg Score**: {:.3}\n", r.avg_score));
        content.push_str(&format!(
            "- **False Positive Score**: {:.3}\n\n",
            r.false_positive_score
        ));
    }

    // Create benchmarks directory if needed
    fs::create_dir_all("benchmarks")?;

    let filename = format!(
        "benchmarks/benchmark-{}.md",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    fs::write(&filename, content)?;
    println!("\n📄 Results saved to: {}", filename);

    Ok(())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (mag_a * mag_b)
}
