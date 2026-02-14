use anyhow::Result;
use colored::Colorize;
use rayon::prelude::*;
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
pub fn read_metadata(db_path: &Path) -> Option<(String, usize, Option<String>)> {
    let metadata_path = db_path.join("metadata.json");
    if let Ok(content) = std::fs::read_to_string(&metadata_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            let model = json.get("model_short_name")?.as_str()?.to_string();
            let dims = json.get("dimensions")?.as_u64()? as usize;
            let primary_language = json
                .get("primary_language")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return Some((model, dims, primary_language));
        }
    }
    None
}

/// Detect if query contains likely code identifiers
///
/// Returns identifiers that look like:
/// - PascalCase (Class, Struct, Interface)
/// - snake_case (function, method)
/// - camelCase (property, variable)
pub fn detect_identifiers(query: &str) -> Vec<String> {
    let mut identifiers = Vec::new();
    for token in query.split_whitespace() {
        let is_pascal = token
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
            && token.chars().any(|c| c.is_lowercase())
            && !["Find", "Show", "Get", "Where", "How", "What", "All"].contains(&token);
        let is_snake =
            token.contains('_') && token.chars().all(|c| c.is_alphanumeric() || c == '_');
        let is_camel = token
            .chars()
            .next()
            .map(|c| c.is_lowercase())
            .unwrap_or(false)
            && token.chars().any(|c| c.is_uppercase());

        if is_pascal || is_snake || is_camel {
            identifiers.push(token.to_string());
        }
    }
    identifiers
}

/// Detects structural intent in user queries (e.g., "class X", "function foo")
/// Returns the ChunkKind that matches the intent, if any
///
/// This function now only returns a kind when the query contains BOTH:
/// 1. A structural keyword (class, struct, function, method, enum, interface, trait)
/// 2. A PascalCase or snake_case identifier suggesting a specific type/function
///
/// This prevents excessive noise where "enum" would boost ALL enums in results
pub fn detect_structural_intent(query: &str) -> Option<crate::chunker::ChunkKind> {
    use crate::chunker::ChunkKind;

    let query_lower = query.to_lowercase();

    // Check if query contains a PascalCase or snake_case identifier
    // This indicates the user is looking for a specific type/function, not just any of that kind
    let has_identifier = contains_identifier(query);

    eprintln!(
        "🔍 detect_structural_intent: query='{}', has_identifier={}",
        query, has_identifier
    );

    if !has_identifier {
        return None; // No specific identifier - don't apply kind boost
    }

    let kind = if query_lower.contains("class ") {
        Some(ChunkKind::Class)
    } else if query_lower.contains("struct ") {
        Some(ChunkKind::Struct)
    } else if query_lower.contains("function ") || query_lower.contains("fn ") {
        Some(ChunkKind::Function)
    } else if query_lower.contains("method ") {
        Some(ChunkKind::Method)
    } else if query_lower.contains("enum ") {
        Some(ChunkKind::Enum)
    } else if query_lower.contains("interface ") {
        Some(ChunkKind::Interface)
    } else if query_lower.contains("trait ") {
        Some(ChunkKind::Trait)
    } else {
        None
    };

    eprintln!("🔍 detect_structural_intent: kind={:?}", kind);
    kind
}

/// Checks if query contains a PascalCase or snake_case identifier
/// indicating a specific type/function name is being searched for
///
/// Simple heuristic without regex dependency:
/// - PascalCase: contains uppercase letter followed by lowercase/digit
/// - snake_case: contains underscore with lowercase letters around it
/// - camelCase: contains lowercase letter followed by uppercase letter
fn contains_identifier(query: &str) -> bool {
    let chars: Vec<char> = query.chars().collect();

    // Look for PascalCase: uppercase letter followed by lowercase letter or digit
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i].is_uppercase() && (chars[i + 1].is_lowercase() || chars[i + 1].is_ascii_digit())
        {
            return true;
        }
    }

    // Look for snake_case: underscore surrounded by lowercase letters
    for i in 1..chars.len().saturating_sub(1) {
        if chars[i] == '_' && chars[i - 1].is_lowercase() && chars[i + 1].is_lowercase() {
            return true;
        }
    }

    // Look for camelCase: lowercase letter followed by uppercase letter
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i].is_lowercase() && chars[i + 1].is_uppercase() {
            return true;
        }
    }

    false
}

/// Boosts results that match a specific ChunkKind by a factor
pub fn boost_kind(
    results: &mut Vec<crate::vectordb::SearchResult>,
    target_kind: crate::chunker::ChunkKind,
) {
    let boost_factor = 0.15; // 15% boost for matching kind
                             // Convert ChunkKind to string for comparison
    let target_kind_str = format!("{:?}", target_kind);
    for result in results.iter_mut() {
        if result.kind == target_kind_str {
            result.score *= 1.0 + boost_factor;
        }
    }
    // Re-sort after boosting
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
}

/// Expand query with variants for better matching
///
/// OPTIMIZATION: Generate fewer, more targeted variants based on query complexity.
/// This reduces embedding time and search overhead.
///
/// For example:
/// - "handle_file_modified" → ["handle_file_modified", "fn handle_file_modified", "async fn handle_file_modified", ...]
/// - "UserService" → ["UserService", "struct UserService", "impl UserService", ...]
/// - "authentication" → ["authentication", "auth"]
fn expand_query(query: &str) -> Vec<String> {
    let mut variants = Vec::new();

    // OPTIMIZATION: Track variant count for logging
    let original_query = query.to_string();

    // Always include original query
    variants.push(query.to_string());

    // OPTIMIZATION: Early exit for very short queries or very long complex queries
    // Short queries: fewer variants needed
    // Long queries: already descriptive, fewer variants needed
    if query.len() < 4 || query.len() > 50 {
        return variants;
    }

    // Check if query looks like a function name (snake_case with underscores, no spaces)
    let looks_like_function = query.contains('_') && !query.contains(' ');

    // Check if query looks like a type/struct name (PascalCase, starts with uppercase)
    let looks_like_type = query
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
        && !query.contains(' ');

    // OPTIMIZATION: Limit number of variants per category
    const MAX_FUNCTION_VARIANTS: usize = 5;
    const MAX_TYPE_VARIANTS: usize = 5;
    const MAX_CONCEPT_VARIANTS: usize = 2;
    const MAX_ABBREV_VARIANTS: usize = 2;

    if looks_like_function {
        // OPTIMIZATION: Only add most relevant function variants
        // Function name variants - prioritize common prefixes
        variants.push(format!("fn {}", query));
        variants.push(format!("async fn {}", query));
        variants.push(format!("pub fn {}", query));

        // Only add method-style variants if we haven't hit the limit
        if variants.len() - 1 < MAX_FUNCTION_VARIANTS {
            variants.push(format!("{} method", query));
        }
        if variants.len() - 1 < MAX_FUNCTION_VARIANTS {
            variants.push(format!("Function: {}", query));
        }
    }

    if looks_like_type {
        // OPTIMIZATION: Only add most relevant type variants
        // Type/struct name variants - prioritize common keywords
        variants.push(format!("struct {}", query));
        variants.push(format!("impl {}", query));
        variants.push(format!("enum {}", query));

        // Only add more variants if we haven't hit the limit
        if variants.len() - 1 < MAX_TYPE_VARIANTS {
            variants.push(format!("class {}", query));
        }
        if variants.len() - 1 < MAX_TYPE_VARIANTS {
            variants.push(format!("Struct: {}", query));
        }
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
        // OPTIMIZATION: Add only most relevant concept variants
        variants.push(format!("fn {}", query));
        if variants.len() - 1 < MAX_CONCEPT_VARIANTS {
            variants.push(format!("{} function", query));
        }
    }

    // OPTIMIZATION: Only expand a few common abbreviations
    let abbreviations: &[(&str, &str)] = &[
        ("auth", "authentication"),
        ("config", "configuration"),
        ("db", "database"),
        ("conn", "connection"),
        ("err", "error"),
        ("msg", "message"),
    ];

    let mut abbrev_count = 0;
    for (abbr, full) in abbreviations {
        if abbrev_count >= MAX_ABBREV_VARIANTS {
            break;
        }
        if query.contains(abbr) {
            let expanded = query.replace(abbr, full);
            if expanded != query {
                variants.push(expanded);
                abbrev_count += 1;
            }
        }
    }

    // OPTIMIZATION: Cap total variants to avoid excessive processing
    // Keep original + at most 8 additional variants
    const MAX_TOTAL_VARIANTS: usize = 9;
    if variants.len() > MAX_TOTAL_VARIANTS {
        variants.truncate(MAX_TOTAL_VARIANTS);
    }

    // OPTIMIZATION: Log variant count for monitoring (when verbose)
    // This helps track the effectiveness of query variant reduction
    if std::env::var("CODESEARCH_VERBOSE").is_ok() && variants.len() > 1 {
        eprintln!(
            "[optimization] Query expansion: {} -> {} variants (original + {} expansions)",
            original_query,
            variants.len(),
            variants.len() - 1
        );
    }

    variants
}

/// Detect query type and adapt RRF-k accordingly
/// Returns (vector_k, fts_k) based on query characteristics
pub fn adapt_rrf_k(query: &str) -> (f64, f64) {
    let has_identifiers = !detect_identifiers(query).is_empty();
    let has_structural_intent = detect_structural_intent(query).is_some();

    match (has_identifiers, has_structural_intent) {
        // Identifier queries: Prioritize vector search (semantic similarity)
        (true, _) => (12.0, 28.0), // Lower vector k, higher FTS k

        // Structural queries: Balance both
        (_, true) => (15.0, 25.0),

        // Semantic queries: Balanced
        _ => (20.0, 20.0),
    }
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
    let (model_type, dimensions, primary_language) =
        if let Some(ref model_name) = options.model_override {
            // User specified a model - use it (warning: may not match indexed data!)
            let mt = ModelType::parse(model_name).unwrap_or_default();
            (mt, mt.dimensions(), None)
        } else if let Some((model_name, dims, lang)) = read_metadata(&db_path) {
            // Use model from metadata
            if let Some(mt) = ModelType::parse(&model_name) {
                (mt, dims, lang)
            } else {
                // Model name not recognized, fall back to default
                eprintln!(
                    "{}",
                    "⚠️  Unknown model in metadata, using default".yellow()
                );
                (ModelType::default(), 384, None)
            }
        } else {
            // No metadata, fall back to default
            (ModelType::default(), 384, None)
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

    // Embed all query variants in a single batch (OPTIMIZATION: batched ONNX calls)
    let start = Instant::now();
    let all_query_embeddings = embedding_service.embed_queries_batch(&query_variants)?;

    let embed_duration = start.elapsed();

    // Search - hybrid by default, vector-only if requested
    let start = Instant::now();

    // Adaptive retrieval limit based on query type and max_results
    // For semantic queries, we need more candidates for good RRF fusion
    // For exact identifier queries, fewer candidates may suffice
    let has_identifiers = !detect_identifiers(query).is_empty();
    let retrieval_limit = if options.vector_only {
        options.max_results
    } else if has_identifiers {
        // Identifier queries: fetch fewer results as exact matches are prioritized
        std::cmp::max(options.max_results * 3, 100)
    } else {
        // Semantic queries: need more candidates for good fusion
        std::cmp::max(options.max_results * 5, 200)
    };

    // Search with all query variants in parallel and combine results
    // OPTIMIZATION: Use efficient deduplication with top-N tracking
    use std::collections::BinaryHeap;

    let vector_search_results: Vec<Vec<crate::vectordb::SearchResult>> = all_query_embeddings
        .par_iter()
        .map(|query_emb| store.search(query_emb, retrieval_limit))
        .collect::<Result<Vec<_>>>()?;

    // OPTIMIZATION: Deduplicate with top-N tracking using BinaryHeap
    // This avoids collecting all results and then truncating
    struct HeapEntry {
        id: u32,
        score: f32,
        distance: f32,
    }

    impl PartialEq for HeapEntry {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }

    impl Eq for HeapEntry {}

    impl PartialOrd for HeapEntry {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for HeapEntry {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            // Max-heap based on score
            self.score
                .partial_cmp(&other.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    }

    // Track top results per chunk ID AND keep one full result per ID
    let mut top_by_id: std::collections::HashMap<u32, HeapEntry> = std::collections::HashMap::new();
    let mut full_results_by_id: std::collections::HashMap<u32, crate::vectordb::SearchResult> =
        std::collections::HashMap::new();

    for results in vector_search_results {
        for result in results {
            top_by_id
                .entry(result.id)
                .and_modify(|e| {
                    if result.score > e.score {
                        e.score = result.score;
                        e.distance = result.distance;
                        // Update the stored full result
                        full_results_by_id.insert(result.id, result.clone());
                    }
                })
                .or_insert_with(|| {
                    let entry = HeapEntry {
                        id: result.id,
                        score: result.score,
                        distance: result.distance,
                    };
                    full_results_by_id.insert(result.id, result.clone());
                    entry
                });
        }
    }

    // Convert to heap and extract top N
    let mut heap: BinaryHeap<HeapEntry> = top_by_id.into_values().collect();
    let mut vector_results: Vec<crate::vectordb::SearchResult> =
        Vec::with_capacity(retrieval_limit);

    while let Some(entry) = heap.pop() {
        if vector_results.len() >= retrieval_limit {
            break;
        }
        if let Some(mut result) = full_results_by_id.get(&entry.id).cloned() {
            result.score = entry.score;
            result.distance = entry.distance;
            vector_results.push(result);
        }
    }

    // Sort by score descending
    vector_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

    // OPTIMIZATION: Early termination for high-confidence exact matches
    // If top results have very high confidence (very low distance), skip FTS search
    // This saves ~30-50ms per search for queries with clear matches
    const HIGH_CONFIDENCE_THRESHOLD: f32 = 0.15; // Distance < 0.15 = very high confidence
    const EARLY_TERMINATION_TOP_N: usize = 5; // Check top 5 results

    let should_use_vector_only = !options.vector_only && {
        // Check if top N results all have high confidence
        let top_results: Vec<_> = vector_results
            .iter()
            .take(EARLY_TERMINATION_TOP_N.min(vector_results.len()))
            .collect();

        let all_high_confidence = top_results
            .iter()
            .all(|r| r.distance < HIGH_CONFIDENCE_THRESHOLD);

        // Also ensure we have at least one result
        !top_results.is_empty() && all_high_confidence
    };

    // Use vector-only mode if early termination conditions are met
    let vector_only_mode = options.vector_only || should_use_vector_only;

    // OPTIMIZATION: Log early termination for monitoring
    if should_use_vector_only && !options.vector_only {
        eprintln!(
            "{}",
            "⚡ Early termination: High-confidence results found, skipping FTS search".green()
        );
    }

    let fused_results: Vec<FusedResult> = if vector_only_mode {
        // Vector-only mode
        vector_only(&vector_results)
    } else {
        // Hybrid search with RRF fusion
        match FtsStore::new(&db_path) {
            Ok(fts_store) => {
                // Detect identifiers for exact match boosting
                let identifiers = detect_identifiers(query);
                // Detect structural intent for kind field boosting
                let structural_intent = detect_structural_intent(query);

                if identifiers.is_empty() {
                    // No identifiers - standard hybrid search
                    let fts_results =
                        fts_store.search(query, retrieval_limit, structural_intent)?;
                    let k = options.rrf_k.unwrap_or(DEFAULT_RRF_K as usize) as f32;
                    rrf_fusion(&vector_results, &fts_results, k)
                } else {
                    // Has identifiers - use exact match boosting
                    let fts_results =
                        fts_store.search(query, retrieval_limit, structural_intent)?;

                    // Search for each identifier and combine exact results
                    let mut all_exact_results = Vec::new();
                    let mut seen_exact_ids = std::collections::HashSet::new();

                    for identifier in &identifiers {
                        if let Ok(exact_matches) =
                            fts_store.search_exact(identifier, retrieval_limit, structural_intent)
                        {
                            for exact_match in exact_matches {
                                // Deduplicate exact results by chunk ID
                                if seen_exact_ids.insert(exact_match.chunk_id) {
                                    all_exact_results.push(exact_match);
                                }
                            }
                        }
                    }

                    // Use adaptive RRF-k based on query type
                    let (vector_k, fts_k) = adapt_rrf_k(query);
                    let k = options.rrf_k.unwrap_or(DEFAULT_RRF_K as usize) as f32;
                    // Use the smaller of user-specified k and adaptive k (more conservative)
                    let vector_k_adaptive = vector_k.min(k as f64) as f32;
                    let fts_k_adaptive = fts_k.min(k as f64) as f32;

                    use crate::rerank::{rrf_fusion_with_exact, EXACT_MATCH_RRF_K};
                    rrf_fusion_with_exact(
                        &vector_results,
                        &fts_results,
                        &all_exact_results,
                        vector_k_adaptive,
                        fts_k_adaptive,
                        EXACT_MATCH_RRF_K,
                    )
                }
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

    // OPTIMIZATION: Apply path filter BEFORE expensive operations (reranking, boosting)
    // This avoids processing results that will be filtered out anyway
    let should_filter_by_path = options.filter_path.is_some();
    let filter_path_normalized = options
        .filter_path
        .as_ref()
        .map(|f| f.trim_start_matches("./").to_string());

    // Take top rerank_top results for reranking (or max_results if not reranking)
    // OPTIMIZATION: Take extra results when path filtering is active to ensure we have enough after filtering
    let take_multiplier = if should_filter_by_path { 3 } else { 1 };
    let take_count = if options.rerank {
        options
            .rerank_top
            .unwrap_or(options.max_results)
            .min(fused_results.len())
    } else {
        options.max_results * take_multiplier
    };

    for fused in fused_results.iter().take(take_count) {
        if let Some(result) = chunk_id_to_result.get(&fused.chunk_id) {
            // OPTIMIZATION: Skip early if path filter doesn't match
            if should_filter_by_path {
                if let Some(ref filter) = filter_path_normalized {
                    let path_normalized = result.path.trim_start_matches("./");
                    if !path_normalized.starts_with(filter) {
                        continue;
                    }
                }
            }

            // Update score to RRF score
            let mut r = (*result).clone();
            r.score = fused.rrf_score;
            results.push(r);
        } else {
            // Result only from FTS, need to fetch from store
            if let Ok(Some(mut result)) = store.get_chunk_as_result(fused.chunk_id) {
                // OPTIMIZATION: Skip early if path filter doesn't match
                if should_filter_by_path {
                    if let Some(ref filter) = filter_path_normalized {
                        let path_normalized = result.path.trim_start_matches("./");
                        if !path_normalized.starts_with(filter) {
                            continue;
                        }
                    }
                }

                result.score = fused.rrf_score;
                results.push(result);
            }
        }
    }

    // Log path filtering optimization (verbose mode)
    if should_filter_by_path {
        let candidates_processed = take_count;
        let results_after_filtering = results.len();
        let filtered_out = candidates_processed.saturating_sub(results_after_filtering);
        eprintln!(
            "{}",
            format!(
                "🔍 Path filter '{}': {} candidates → {} results ({} filtered out)",
                filter_path_normalized.as_ref().unwrap_or(&"".to_string()),
                candidates_processed,
                results_after_filtering,
                filtered_out
            )
            .blue()
        );
    }

    // Language awareness: Boost results from primary language
    // Extract language from file path (since SearchResult doesn't have language field)
    if let Some(ref lang) = primary_language {
        use crate::file::Language;
        let lang_boost = 0.2; // Boost results from primary language by 20%
        for result in results.iter_mut() {
            // Detect language from file path
            let file_lang = format!(
                "{:?}",
                Language::from_path(std::path::Path::new(&result.path))
            );
            if file_lang == *lang {
                result.score *= 1.0 + lang_boost;
            }
        }
        // Re-sort after boosting
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    }

    // ChunkKind-Aware Ranking: Boost results matching structural intent
    if let Some(intent) = detect_structural_intent(query) {
        boost_kind(&mut results, intent);
    }

    // Negative Result Check: Report when no exact matches found for identifier queries
    let identifiers = detect_identifiers(query);
    if !identifiers.is_empty() && results.is_empty() {
        eprintln!(
            "{}",
            format!(
                "❓ No exact matches found for identifiers: {}",
                identifiers.join(", ")
            )
            .yellow()
        );
        eprintln!("{}", "  Try using broader search terms or running `codesearch index --sync` if the codebase changed.".dimmed());
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
