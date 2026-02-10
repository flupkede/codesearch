use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cache::FileMetaStore;
use crate::chunker::SemanticChunker;
use crate::embed::{EmbeddingService, ModelType};
use crate::file::FileWalker;
use crate::fts::FtsStore;
use crate::rerank::{rrf_fusion, vector_only, FusedResult, NeuralReranker, DEFAULT_RRF_K};
use crate::vectordb::VectorStore;

/// Configuration options for search operations
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Maximum number of results to return
    pub max_results: usize,
    /// Maximum number of results per file
    pub per_file: Option<usize>,
    /// Number of content lines to show
    pub content_lines: usize,
    /// Whether to show scores
    pub show_scores: bool,
    /// Compact output mode
    pub compact: bool,
    /// Sync database before search
    pub sync: bool,
    /// JSON output mode
    pub json: bool,
    /// Optional path filter
    pub filter_path: Option<String>,
    /// Optional model override
    pub model_override: Option<String>,
    /// Vector-only mode (skip FTS)
    pub vector_only: bool,
    /// RRF fusion constant
    pub rrf_k: Option<usize>,
    /// Enable neural reranking
    pub rerank: bool,
    /// Number of results to rerank
    pub rerank_top: Option<usize>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_results: 10,
            per_file: None,
            content_lines: 3,
            show_scores: false,
            compact: false,
            sync: false,
            json: false,
            filter_path: None,
            model_override: None,
            vector_only: false,
            rrf_k: None,
            rerank: false,
            rerank_top: None,
        }
    }
}

/// JSON output format for search results
#[derive(Serialize)]
struct JsonOutput {
    query: String,
    results: Vec<JsonResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timing: Option<JsonTiming>,
}

#[derive(Serialize)]
struct JsonResult {
    path: String,
    start_line: usize,
    end_line: usize,
    kind: String,
    content: String,
    score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_prev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_next: Option<String>,
}

#[derive(Serialize)]
struct JsonTiming {
    total_ms: u64,
    embed_ms: u64,
    search_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rerank_ms: Option<u64>,
}

/// Get the database path and project path for a given project directory
/// Uses automatic database discovery to find indexes in parent/global directories
fn get_db_path(path: Option<PathBuf>) -> Result<(PathBuf, PathBuf)> {
    use crate::db_discovery::resolve_database_with_message;
    resolve_database_with_message(path.as_deref(), "searching")
}

/// Read model metadata from database
fn read_metadata(db_path: &Path) -> Option<(String, usize)> {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let model = json.get("model_short_name")?.as_str()?.to_string();
            let dims = json.get("dimensions")?.as_u64()? as usize;
            return Some((model, dims));
        }
    }
    None
}

/// Expand query with variants for better matching
///
/// For example:
/// - "handle_file_modified" → ["handle_file_modified", "fn handle_file_modified", "async fn handle_file_modified", ...]
/// - "UserService" → ["UserService", "struct UserService", "impl UserService", ...]
/// - "authentication" → ["authentication", "auth"]
fn expand_query(query: &str) -> Vec<String> {
    let mut variants = Vec::new();

    // Always include original query
    variants.push(query.to_string());

    // Check if query looks like a function name (snake_case with underscores, no spaces)
    let looks_like_function = query.contains('_') && !query.contains(' ');

    // Check if query looks like a type/struct name (PascalCase, starts with uppercase)
    let looks_like_type = query
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
        && !query.contains(' ');

    if looks_like_function {
        // Function name variants - add various function prefixes
        variants.push(format!("fn {}", query));
        variants.push(format!("async fn {}", query));
        variants.push(format!("pub fn {}", query));
        variants.push(format!("pub async fn {}", query));

        // Also add method-style variants
        variants.push(format!("{} function", query));
        variants.push(format!("{} method", query));
        variants.push(format!("{} implementation", query));

        // Add "Function: name" context format (as used in chunk context)
        variants.push(format!("Function: {}", query));
        variants.push(format!("Method: {}", query));
    }

    if looks_like_type {
        // Type/struct name variants
        variants.push(format!("struct {}", query));
        variants.push(format!("enum {}", query));
        variants.push(format!("trait {}", query));
        variants.push(format!("impl {}", query));
        variants.push(format!("type {}", query));
        variants.push(format!("class {}", query));
        variants.push(format!("interface {}", query));

        // Add context format
        variants.push(format!("Struct: {}", query));
        variants.push(format!("Impl: {}", query));
    }

    // If query is a single word without underscores and lowercase, it might be a concept
    let is_single_concept = !query.contains('_')
        && !query.contains(' ')
        && query
            .chars()
            .next()
            .map(|c| c.is_lowercase())
            .unwrap_or(false);

    if is_single_concept {
        // Add function-style variant
        variants.push(format!("fn {}", query));
        variants.push(format!("{} function", query));
    }

    // Common abbreviations
    let abbreviations = vec![
        ("auth", "authentication"),
        ("config", "configuration"),
        ("info", "information"),
        ("msg", "message"),
        ("err", "error"),
        ("ctx", "context"),
        ("req", "request"),
        ("resp", "response"),
        ("impl", "implementation"),
        ("mod", "module"),
        ("db", "database"),
        ("conn", "connection"),
        ("stmt", "statement"),
        ("param", "parameter"),
        ("args", "arguments"),
    ];

    for (abbr, full) in abbreviations {
        if query.contains(abbr) {
            let expanded = query.replace(abbr, full);
            if expanded != query {
                variants.push(expanded);
            }
        }
    }

    variants
}

/// Search the codebase
pub async fn search(query: &str, path: Option<PathBuf>, options: SearchOptions) -> Result<()> {
    let (db_path, _project_path) = get_db_path(path)?;

    if !db_path.exists() {
        println!("{}", "❌ No database found!".red());
        println!("   Run {} first", "codesearch index".bright_cyan());
        println!();
        println!(
            "{}",
            "💡 Tip: codesearch can find databases in parent directories. Use 'codesearch list' to see all indexed projects.".dimmed()
        );
        return Ok(());
    }

    // Read model metadata from database FIRST (needed for sync)
    let (model_type, dimensions) = if let Some(ref model_name) = options.model_override {
        // User specified a model - use it (warning: may not match indexed data!)
        let mt = ModelType::parse(model_name).unwrap_or_default();
        (mt, mt.dimensions())
    } else if let Some((model_name, dims)) = read_metadata(&db_path) {
        // Use model from metadata
        if let Some(mt) = ModelType::parse(&model_name) {
            (mt, dims)
        } else {
            // Model name not recognized, fall back to default
            eprintln!(
                "{}",
                "⚠️  Unknown model in metadata, using default".yellow()
            );
            (ModelType::default(), 384)
        }
    } else {
        // No metadata, fall back to default
        (ModelType::default(), 384)
    };

    // Perform incremental sync if requested (after we know the model)
    if options.sync {
        println!("{}", "🔄 Syncing database...".yellow());
        sync_database(&db_path, model_type)?;
    }

    // Load database
    let start = Instant::now();
    let store = VectorStore::new(&db_path, dimensions)?;
    let load_duration = start.elapsed();

    // Initialize embedding service with the correct model
    let start = Instant::now();
    let cache_dir = crate::constants::get_global_models_cache_dir()?;
    let mut embedding_service = EmbeddingService::with_cache_dir(model_type, Some(&cache_dir))?;
    let model_load_duration = start.elapsed();

    // Expand query with variants for better matching
    let query_variants = expand_query(query);

    // Embed all query variants
    let start = Instant::now();
    let mut all_query_embeddings = Vec::new();
    for variant in &query_variants {
        if let Ok(emb) = embedding_service.embed_query(variant) {
            all_query_embeddings.push(emb);
        }
    }

    let embed_duration = start.elapsed();

    // Search - hybrid by default, vector-only if requested
    let start = Instant::now();

    // Fetch more results for RRF fusion (200 per source, per osgrep pattern)
    let retrieval_limit = if options.vector_only {
        options.max_results
    } else {
        200
    };

    // Search with all query variants and combine results
    let mut all_vector_results = Vec::new();
    for query_emb in &all_query_embeddings {
        let results = store.search(query_emb, retrieval_limit)?;
        all_vector_results.extend(results);
    }

    // Deduplicate by chunk ID (keep highest score)
    let mut vector_results_map: std::collections::HashMap<u32, crate::vectordb::SearchResult> =
        std::collections::HashMap::new();
    for result in all_vector_results {
        vector_results_map
            .entry(result.id)
            .and_modify(|e| {
                if result.score > e.score {
                    e.score = result.score;
                    e.distance = result.distance;
                }
            })
            .or_insert(result);
    }

    let mut vector_results: Vec<crate::vectordb::SearchResult> =
        vector_results_map.into_values().collect();
    vector_results.truncate(retrieval_limit);

    let fused_results: Vec<FusedResult> = if options.vector_only {
        // Vector-only mode
        vector_only(&vector_results)
    } else {
        // Hybrid search with RRF fusion
        match FtsStore::new(&db_path) {
            Ok(fts_store) => {
                let fts_results = fts_store.search(query, retrieval_limit)?;
                let k = options.rrf_k.unwrap_or(DEFAULT_RRF_K as usize) as f32;
                rrf_fusion(&vector_results, &fts_results, k)
            }
            Err(_) => {
                // FTS not available, fall back to vector-only
                eprintln!(
                    "{}",
                    "⚠️  FTS index not found, using vector-only search".yellow()
                );
                vector_only(&vector_results)
            }
        }
    };

    // Map fused results back to full SearchResult
    let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
    let chunk_id_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
        vector_results.iter().map(|r| (r.id, r)).collect();

    // Take top rerank_top results for reranking (or max_results if not reranking)
    let take_count = if options.rerank {
        options
            .rerank_top
            .unwrap_or(options.max_results)
            .min(fused_results.len())
    } else {
        options.max_results
    };

    for fused in fused_results.iter().take(take_count) {
        if let Some(result) = chunk_id_to_result.get(&fused.chunk_id) {
            // Update score to RRF score
            let mut r = (*result).clone();
            r.score = fused.rrf_score;
            results.push(r);
        } else {
            // Result only from FTS, need to fetch from store
            if let Ok(Some(mut result)) = store.get_chunk_as_result(fused.chunk_id) {
                result.score = fused.rrf_score;
                results.push(result);
            }
        }
    }

    let search_duration = start.elapsed();

    // Neural reranking (if enabled)
    let mut rerank_duration = Duration::ZERO;
    if options.rerank && !results.is_empty() {
        let start = Instant::now();

        // Initialize neural reranker (Jina Reranker v1 Turbo)
        match NeuralReranker::new() {
            Ok(mut reranker) => {
                // Prepare documents for reranking
                let documents: Vec<String> = results.iter().map(|r| r.content.clone()).collect();
                let rrf_scores: Vec<f32> = results.iter().map(|r| r.score).collect();

                // Rerank and blend scores
                match reranker.rerank_and_blend(query, &documents, &rrf_scores) {
                    Ok(reranked) => {
                        // Reorder results based on reranked indices
                        let mut reordered: Vec<crate::vectordb::SearchResult> =
                            Vec::with_capacity(results.len());
                        for (idx, score) in reranked {
                            let mut result = results[idx].clone();
                            result.score = score;
                            reordered.push(result);
                        }
                        results = reordered;
                        println!("{}", "✅ Neural reranking applied".green());
                    }
                    Err(e) => {
                        eprintln!("{}", format!("⚠️  Reranking failed: {}", e).yellow());
                    }
                }
            }
            Err(e) => {
                eprintln!("{}", format!("⚠️  Could not load reranker: {}", e).yellow());
            }
        }

        rerank_duration = start.elapsed();
    }

    // Filter by path if specified
    if let Some(ref filter) = options.filter_path {
        let filter_normalized = filter.trim_start_matches("./");
        results.retain(|r| {
            let path_normalized = r.path.trim_start_matches("./");
            path_normalized.starts_with(filter_normalized)
        });
    }

    // Truncate to max_results after reranking and filtering
    results.truncate(options.max_results);

    // Output results
    if options.json {
        let json_results: Vec<JsonResult> = results
            .iter()
            .map(|r| JsonResult {
                path: r.path.clone(),
                start_line: r.start_line,
                end_line: r.end_line,
                kind: r.kind.clone(),
                content: r.content.clone(),
                score: r.score,
                signature: r.signature.clone(),
                context_prev: r.context_prev.clone(),
                context_next: r.context_next.clone(),
            })
            .collect();

        let timing = if options.show_scores {
            Some(JsonTiming {
                total_ms: (load_duration
                    + model_load_duration
                    + embed_duration
                    + search_duration
                    + rerank_duration)
                    .as_millis() as u64,
                embed_ms: embed_duration.as_millis() as u64,
                search_ms: search_duration.as_millis() as u64,
                rerank_ms: if options.rerank {
                    Some(rerank_duration.as_millis() as u64)
                } else {
                    None
                },
            })
        } else {
            None
        };

        let output = JsonOutput {
            query: query.to_string(),
            results: json_results,
            timing,
        };

        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    if options.compact {
        // Show only file paths (like grep -l)
        let mut seen_files = std::collections::HashSet::new();
        for result in &results {
            if !seen_files.contains(&result.path) {
                println!("{}", result.path);
                seen_files.insert(result.path.clone());
            }
        }
        return Ok(());
    }

    // Standard output
    println!("{}", "🔍 Search Results".bright_cyan().bold());
    println!("{}", "=".repeat(60));
    println!("Query: \"{}\"", query.bright_yellow());
    println!("Found {} results", results.len());
    println!();

    if options.show_scores {
        println!("Timing:");
        println!("   Database load: {:?}", load_duration);
        println!("   Model load:    {:?}", model_load_duration);
        println!("   Query embed:   {:?}", embed_duration);
        println!("   Search:        {:?}", search_duration);
        if options.rerank {
            println!("   Reranking:     {:?}", rerank_duration);
        }
        println!(
            "   Total:         {:?}",
            load_duration
                + model_load_duration
                + embed_duration
                + search_duration
                + rerank_duration
        );
        println!();
    }

    // Check if no results
    if results.is_empty() {
        println!("{}", "No matches found.".dimmed());
        println!("Try:");
        println!("  - Using different keywords");
        println!("  - Making your query more general");
        println!(
            "  - Running {} if the codebase changed",
            "codesearch index --force".bright_cyan()
        );
        return Ok(());
    }

    // Group results by file if per_file > 0
    if let Some(per_file) = options.per_file {
        if per_file > 0 && per_file < options.max_results {
            let mut by_file: std::collections::HashMap<String, Vec<_>> =
                std::collections::HashMap::new();

            for result in results {
                by_file.entry(result.path.clone()).or_default().push(result);
            }

            let mut files: Vec<_> = by_file.into_iter().collect();
            files.sort_by(|a, b| {
                b.1.iter()
                    .map(|r| r.score)
                    .fold(0.0f32, f32::max)
                    .partial_cmp(&a.1.iter().map(|r| r.score).fold(0.0f32, f32::max))
                    .unwrap()
            });

            for (_file_path, mut file_results) in files {
                file_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
                file_results.truncate(per_file);

                for (idx, result) in file_results.iter().enumerate() {
                    print_result(
                        result,
                        idx == 0,
                        options.content_lines > 0,
                        options.show_scores,
                    )?;
                }
            }
        } else {
            // Show all results
            for result in &results {
                print_result(result, true, options.content_lines > 0, options.show_scores)?;
            }
        }
    } else {
        // Show all results
        for result in &results {
            print_result(result, true, options.content_lines > 0, options.show_scores)?;
        }
    }

    Ok(())
}

/// Sync database by re-indexing changed files
fn sync_database(db_path: &Path, model_type: ModelType) -> Result<()> {
    let project_path = db_path.parent().unwrap_or(std::path::Path::new("."));

    // Load file metadata store
    let mut file_meta =
        FileMetaStore::load_or_create(db_path, model_type.short_name(), model_type.dimensions())?;

    // Walk the file system
    let walker = FileWalker::new(project_path.to_path_buf());
    let (files, _stats) = walker.walk()?;

    // Initialize services
    let cache_dir = crate::constants::get_global_models_cache_dir()?;
    let mut embedding_service = EmbeddingService::with_cache_dir(model_type, Some(&cache_dir))?;
    let mut chunker = SemanticChunker::new(100, 2000, 10);
    let mut store = VectorStore::new(db_path, model_type.dimensions())?;

    let mut changes = 0;

    // Check for changed files
    for file in &files {
        let (needs_reindex, old_chunk_ids) = file_meta.check_file(&file.path)?;

        if !needs_reindex {
            continue;
        }

        changes += 1;
        println!("  📝 {}", file.path.display());

        // Delete old chunks
        if !old_chunk_ids.is_empty() {
            store.delete_chunks(&old_chunk_ids)?;
        }

        // Read and chunk file
        let source_code = match std::fs::read_to_string(&file.path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        let chunks = chunker.chunk_semantic(file.language, &file.path, &source_code)?;

        if chunks.is_empty() {
            file_meta.update_file(&file.path, vec![])?;
            continue;
        }

        // Embed and insert
        let embedded_chunks = embedding_service.embed_chunks(chunks)?;
        let chunk_ids = store.insert_chunks_with_ids(embedded_chunks)?;
        file_meta.update_file(&file.path, chunk_ids)?;
    }

    // Check for deleted files
    let deleted_files = file_meta.find_deleted_files();
    for (path, chunk_ids) in &deleted_files {
        changes += 1;
        println!("  🗑️  {} (deleted)", path);
        if !chunk_ids.is_empty() {
            store.delete_chunks(chunk_ids)?;
        }
        file_meta.remove_file(std::path::Path::new(path));
    }

    // Rebuild index if changes were made
    if changes > 0 {
        println!("  🔨 Rebuilding index...");
        store.build_index()?;
        file_meta.save(db_path)?;
        println!("  ✅ {} file(s) synced", changes);
    } else {
        println!("  ✅ Already up to date");
    }

    Ok(())
}

fn print_result(
    result: &crate::vectordb::SearchResult,
    show_file: bool,
    show_content: bool,
    show_scores: bool,
) -> Result<()> {
    if show_file {
        println!("{}", "─".repeat(60));
        let file_display = format!("📄 {}", result.path);
        println!("{}", file_display.bright_green());
    }

    // Show location and kind
    let location = format!(
        "   Lines {}-{} • {}",
        result.start_line, result.end_line, result.kind
    );
    println!("{}", location.dimmed());

    // Show signature if available
    if let Some(sig) = &result.signature {
        println!("   {}", sig.bright_cyan());
    }

    // Show score if requested
    if show_scores {
        let score_color = if result.score > 0.8 {
            "green"
        } else if result.score > 0.6 {
            "yellow"
        } else {
            "red"
        };

        let score_text = format!("   Score: {:.3}", result.score);
        println!(
            "{}",
            match score_color {
                "green" => score_text.green(),
                "yellow" => score_text.yellow(),
                _ => score_text.red(),
            }
        );
    }

    // Show context if available
    if let Some(ctx) = &result.context {
        println!("   Context: {}", ctx.dimmed());
    }

    // Show content if requested
    if show_content {
        // Show context before (if available)
        if let Some(ctx_prev) = &result.context_prev {
            println!("\n   {}:", "Context (before)".dimmed());
            for line in ctx_prev.lines() {
                println!("   │ {}", line.bright_black());
            }
        }

        println!("\n   {}:", "Content".bright_yellow());
        for line in result.content.lines().take(10) {
            println!("   │ {}", line.dimmed());
        }
        if result.content.lines().count() > 10 {
            println!("   │ {}", "...".dimmed());
        }

        // Show context after (if available)
        if let Some(ctx_next) = &result.context_next {
            println!("\n   {}:", "Context (after)".dimmed());
            for line in ctx_next.lines() {
                println!("   │ {}", line.bright_black());
            }
        }
    } else {
        // Show a snippet
        let snippet: String = result.content.lines().take(3).collect::<Vec<_>>().join(" ");

        let snippet = if snippet.len() > 100 {
            format!("{}...", &snippet[..100])
        } else {
            snippet
        };

        println!("   {}", snippet.dimmed());
    }

    println!();

    Ok(())
}
