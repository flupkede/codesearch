/// Comprehensive demo showing the complete codesearch workflow:
///
/// 1. File discovery (Phase 1)
/// 2. Semantic chunking (Phase 2)
/// 3. Embedding generation (Phase 3)
/// 4. Vector storage (Phase 4)
/// 5. Search and retrieval
///
/// Run with: cargo run --example vectordb_demo
use anyhow::Result;
use codesearch::chunker::SemanticChunker;
use codesearch::{ChunkKind, EmbeddingService, FileWalker, Language, VectorStore};
use colored::Colorize;
use std::path::Path;
use std::time::Instant;

fn print_section(title: &str) {
    println!("\n{}", "=".repeat(60));
    println!("{}", title.bright_cyan().bold());
    println!("{}\n", "=".repeat(60));
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("codesearch=info")
        .init();

    print_section("🚀 Codesearch Vector Store Demo");

    // Configuration
    let project_path = Path::new(".");
    let db_path = Path::new(".codesearch_demo.db");
    let query = "function that handles authentication or login";

    println!("📂 Project path: {}", project_path.display());
    println!("💾 Database path: {}", db_path.display());
    println!("🔍 Query: \"{}\"", query);

    // Clean up previous demo database
    if db_path.exists() {
        println!("\n🗑️  Removing previous demo database...");
        std::fs::remove_dir_all(db_path)?;
    }

    // ============================================================================
    // Phase 1: File Discovery
    // ============================================================================
    print_section("Phase 1: File Discovery");

    let start = Instant::now();
    let walker = FileWalker::new(project_path.to_path_buf());
    let (files, _stats) = walker.walk()?;
    let duration = start.elapsed();

    println!("✅ Found {} code files in {:?}", files.len(), duration);
    println!("\nFile breakdown by language:");

    let mut rust_files = 0;
    let mut python_files = 0;
    let mut js_files = 0;
    let mut ts_files = 0;
    let mut other_files = 0;

    for file in &files {
        match file.language {
            Language::Rust => rust_files += 1,
            Language::Python => python_files += 1,
            Language::JavaScript => js_files += 1,
            Language::TypeScript => ts_files += 1,
            Language::Unknown => other_files += 1,
            _ => other_files += 1,
        }
    }

    println!("  - Rust: {}", rust_files);
    println!("  - Python: {}", python_files);
    println!("  - JavaScript: {}", js_files);
    println!("  - TypeScript: {}", ts_files);
    println!("  - Other: {}", other_files);

    // Limit to first 5 files for demo purposes
    let files_to_process = files.into_iter().take(5).collect::<Vec<_>>();
    println!(
        "\n📝 Processing first {} files for demo...",
        files_to_process.len()
    );

    // ============================================================================
    // Phase 2: Semantic Chunking
    // ============================================================================
    print_section("Phase 2: Semantic Chunking");

    let start = Instant::now();
    let mut chunker = SemanticChunker::new(100, 2000, 10);
    let mut all_chunks = Vec::new();

    for file in &files_to_process {
        println!("📄 Chunking: {}", file.path.display());

        let source_code = std::fs::read_to_string(&file.path)?;
        let chunks = chunker.chunk_semantic(file.language, &file.path, &source_code)?;

        println!(
            "   ➜ {} chunks ({} functions, {} classes, {} mods)",
            chunks.len(),
            chunks
                .iter()
                .filter(|c| matches!(c.kind, ChunkKind::Function))
                .count(),
            chunks
                .iter()
                .filter(|c| matches!(c.kind, ChunkKind::Class))
                .count(),
            chunks
                .iter()
                .filter(|c| matches!(c.kind, ChunkKind::Mod))
                .count()
        );

        all_chunks.extend(chunks);
    }

    let duration = start.elapsed();
    println!("\n✅ Created {} chunks in {:?}", all_chunks.len(), duration);

    // Show example chunk
    if let Some(chunk) = all_chunks.first() {
        println!("\n📦 Example chunk:");
        println!("   Path: {}", chunk.path);
        println!("   Kind: {:?}", chunk.kind);
        println!("   Lines: {}-{}", chunk.start_line, chunk.end_line);
        if let Some(sig) = &chunk.signature {
            println!("   Signature: {}", sig);
        }
        println!("   Content preview: {}", {
            let preview = chunk.content.lines().take(3).collect::<Vec<_>>().join("\n");
            if preview.len() > 100 {
                format!("{}...", &preview[..100])
            } else {
                preview
            }
        });
    }

    // ============================================================================
    // Phase 3: Embedding Generation
    // ============================================================================
    print_section("Phase 3: Embedding Generation");

    let start = Instant::now();
    println!("🔄 Initializing embedding model...");

    let mut embedding_service = EmbeddingService::new()?;

    println!(
        "✅ Model loaded: {} dimensions",
        embedding_service.dimensions()
    );

    println!(
        "\n🔄 Generating embeddings for {} chunks...",
        all_chunks.len()
    );
    let embedded_chunks = embedding_service.embed_chunks(all_chunks)?;
    let duration = start.elapsed();

    println!(
        "✅ Generated {} embeddings in {:?}",
        embedded_chunks.len(),
        duration
    );
    println!(
        "   Average: {:?} per chunk",
        duration / embedded_chunks.len() as u32
    );

    // Show cache stats
    let cache_stats = embedding_service.cache_stats();
    println!("\n📊 Cache statistics:");
    println!("   Hit rate: {:.1}%", cache_stats.hit_rate() * 100.0);
    println!("   Entries: {}", cache_stats.size);

    // ============================================================================
    // Phase 4: Vector Storage
    // ============================================================================
    print_section("Phase 4: Vector Storage (NEW!)");

    println!("🔄 Creating vector database...");
    let mut store = VectorStore::new(db_path, embedding_service.dimensions())?;

    println!("✅ Database opened: {}", db_path.display());

    // Insert chunks
    let start = Instant::now();
    println!("\n🔄 Inserting {} chunks...", embedded_chunks.len());
    let count = store.insert_chunks(embedded_chunks)?;
    let duration = start.elapsed();

    println!("✅ Inserted {} chunks in {:?}", count, duration);

    // Build index
    let start = Instant::now();
    println!("\n🔄 Building vector index...");
    store.build_index()?;
    let duration = start.elapsed();

    println!("✅ Index built in {:?}", duration);

    // Show stats
    let stats = store.stats()?;
    println!("\n📊 Database statistics:");
    println!("   Total chunks: {}", stats.total_chunks);
    println!("   Total files: {}", stats.total_files);
    println!("   Indexed: {}", stats.indexed);
    println!("   Dimensions: {}", stats.dimensions);

    // Check database file size
    let db_size = std::fs::metadata(db_path)?.len();
    println!("   File size: {:.2} KB", db_size as f64 / 1024.0);

    // ============================================================================
    // Phase 5: Search and Retrieval
    // ============================================================================
    print_section("Phase 5: Search and Retrieval");

    println!("🔍 Query: \"{}\"", query);

    let start = Instant::now();
    println!("\n🔄 Generating query embedding...");
    let query_embedding = embedding_service.embed_query(query)?;
    let embed_duration = start.elapsed();

    let start = Instant::now();
    println!("🔄 Searching vector database...");
    let results = store.search(&query_embedding, 5)?;
    let search_duration = start.elapsed();

    println!("\n✅ Found {} results", results.len());
    println!("   Embedding: {:?}", embed_duration);
    println!("   Search: {:?}", search_duration);
    println!("   Total: {:?}", embed_duration + search_duration);

    // Display results
    println!("\n🎯 Top Results:\n");

    for (i, result) in results.iter().enumerate() {
        println!("{}", "─".repeat(60));
        println!(
            "{}. {} (score: {:.3})",
            i + 1,
            result.path.bright_green(),
            result.score
        );
        println!("   Kind: {}", result.kind.bright_yellow());
        println!("   Lines: {}-{}", result.start_line, result.end_line);

        if let Some(sig) = &result.signature {
            println!("   Signature: {}", sig.bright_cyan());
        }

        if let Some(ctx) = &result.context {
            println!("   Context: {}", ctx.dimmed());
        }

        if let Some(doc) = &result.docstring {
            let doc_preview = if doc.len() > 100 {
                format!("{}...", &doc[..100])
            } else {
                doc.clone()
            };
            println!("   Docstring: {}", doc_preview.dimmed());
        }

        // Show content preview
        let content_preview = result
            .content
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        println!("\n   Preview:");
        for line in content_preview.lines() {
            println!("     {}", line.dimmed());
        }
        println!();
    }

    // ============================================================================
    // Summary
    // ============================================================================
    print_section("🎉 Demo Complete!");

    println!("✅ Successfully demonstrated all phases:");
    println!("   1. File discovery: {} files", files_to_process.len());
    println!("   2. Semantic chunking: {} chunks", count);
    println!("   3. Embedding generation: {} embeddings", count);
    println!(
        "   4. Vector storage: single {} directory",
        db_path.display()
    );
    println!(
        "   5. Search: {} results in {:?}",
        results.len(),
        search_duration
    );

    println!("\n💾 Database persisted at: {}", db_path.display());
    println!("   You can now use this database for subsequent searches!");

    println!("\n🧹 To clean up:");
    println!("   rm -rf {} (Unix/Mac)", db_path.display());
    println!("   rmdir /s /q {} (Windows)", db_path.display());

    Ok(())
}
