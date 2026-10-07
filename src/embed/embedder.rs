use anyhow::{anyhow, Result};
use fastembed::{EmbeddingModel as FastEmbedModel, TextEmbedding, TextInitOptions};
use ort::ep::CPU;

use crate::file::Language;

/// Available embedding models
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModelType {
    // === MiniLM Family ===
    /// All-MiniLM-L6-v2 - 384 dimensions, fast and efficient
    AllMiniLML6V2,
    /// Quantized All-MiniLM-L6-v2 - 384 dimensions, faster
    #[default]
    AllMiniLML6V2Q,
    /// All-MiniLM-L12-v2 - 384 dimensions, better quality than L6
    AllMiniLML12V2,
    /// Quantized All-MiniLM-L12-v2 - 384 dimensions
    AllMiniLML12V2Q,
    /// Paraphrase-MiniLM-L6-v2 - 384 dimensions
    ParaphraseMLMiniLML12V2,

    // === BGE Family ===
    /// BGE Small EN v1.5 - 384 dimensions, good balance (DEFAULT)
    BGESmallENV15,
    /// Quantized BGE Small EN v1.5 - 384 dimensions, faster
    BGESmallENV15Q,
    /// BGE Base EN v1.5 - 768 dimensions, higher quality
    BGEBaseENV15,
    /// BGE Large EN v1.5 - 1024 dimensions, best BGE quality
    BGELargeENV15,

    // === Nomic Family ===
    /// Nomic Embed Text v1 - 768 dimensions
    NomicEmbedTextV1,
    /// Nomic Embed Text v1.5 - 768 dimensions, improved
    NomicEmbedTextV15,
    /// Quantized Nomic Embed Text v1.5 - 768 dimensions
    NomicEmbedTextV15Q,

    // === Specialized Models ===
    /// Jina Embeddings v2 Base Code - 768 dimensions, optimized for code
    JinaEmbeddingsV2BaseCode,
    /// Multilingual E5 Small - 384 dimensions, multilingual support
    MultilingualE5Small,
    /// MxBai Embed Large v1 - 1024 dimensions, high quality
    MxbaiEmbedLargeV1,
    /// ModernBERT Embed Large - 1024 dimensions, latest architecture
    ModernBertEmbedLarge,
    /// Quantized EmbeddingGemma 300M - 768 dimensions, multilingual retrieval
    EmbeddingGemma300MQ4,
}

impl ModelType {
    pub fn to_fastembed_model(self) -> FastEmbedModel {
        match self {
            // MiniLM Family
            Self::AllMiniLML6V2 => FastEmbedModel::AllMiniLML6V2,
            Self::AllMiniLML6V2Q => FastEmbedModel::AllMiniLML6V2Q,
            Self::AllMiniLML12V2 => FastEmbedModel::AllMiniLML12V2,
            Self::AllMiniLML12V2Q => FastEmbedModel::AllMiniLML12V2Q,
            Self::ParaphraseMLMiniLML12V2 => FastEmbedModel::ParaphraseMLMiniLML12V2,
            // BGE Family
            Self::BGESmallENV15 => FastEmbedModel::BGESmallENV15,
            Self::BGESmallENV15Q => FastEmbedModel::BGESmallENV15Q,
            Self::BGEBaseENV15 => FastEmbedModel::BGEBaseENV15,
            Self::BGELargeENV15 => FastEmbedModel::BGELargeENV15,
            // Nomic Family
            Self::NomicEmbedTextV1 => FastEmbedModel::NomicEmbedTextV1,
            Self::NomicEmbedTextV15 => FastEmbedModel::NomicEmbedTextV15,
            Self::NomicEmbedTextV15Q => FastEmbedModel::NomicEmbedTextV15Q,
            // Specialized
            Self::JinaEmbeddingsV2BaseCode => FastEmbedModel::JinaEmbeddingsV2BaseCode,
            Self::MultilingualE5Small => FastEmbedModel::MultilingualE5Small,
            Self::MxbaiEmbedLargeV1 => FastEmbedModel::MxbaiEmbedLargeV1,
            Self::ModernBertEmbedLarge => FastEmbedModel::ModernBertEmbedLarge,
            Self::EmbeddingGemma300MQ4 => FastEmbedModel::EmbeddingGemma300MQ4,
        }
    }

    pub fn dimensions(&self) -> usize {
        match self {
            // 384 dimensions
            Self::AllMiniLML6V2
            | Self::AllMiniLML6V2Q
            | Self::AllMiniLML12V2
            | Self::AllMiniLML12V2Q
            | Self::ParaphraseMLMiniLML12V2
            | Self::BGESmallENV15
            | Self::BGESmallENV15Q
            | Self::MultilingualE5Small => 384,
            // 768 dimensions
            Self::BGEBaseENV15
            | Self::NomicEmbedTextV1
            | Self::NomicEmbedTextV15
            | Self::NomicEmbedTextV15Q
            | Self::JinaEmbeddingsV2BaseCode
            | Self::EmbeddingGemma300MQ4 => 768,
            // 1024 dimensions
            Self::BGELargeENV15 | Self::MxbaiEmbedLargeV1 | Self::ModernBertEmbedLarge => 1024,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::AllMiniLML6V2 => "sentence-transformers/all-MiniLM-L6-v2",
            Self::AllMiniLML6V2Q => "sentence-transformers/all-MiniLM-L6-v2 (quantized)",
            Self::AllMiniLML12V2 => "sentence-transformers/all-MiniLM-L12-v2",
            Self::AllMiniLML12V2Q => "sentence-transformers/all-MiniLM-L12-v2 (quantized)",
            Self::ParaphraseMLMiniLML12V2 => "sentence-transformers/paraphrase-MiniLM-L6-v2",
            Self::BGESmallENV15 => "BAAI/bge-small-en-v1.5",
            Self::BGESmallENV15Q => "BAAI/bge-small-en-v1.5 (quantized)",
            Self::BGEBaseENV15 => "BAAI/bge-base-en-v1.5",
            Self::BGELargeENV15 => "BAAI/bge-large-en-v1.5",
            Self::NomicEmbedTextV1 => "nomic-ai/nomic-embed-text-v1",
            Self::NomicEmbedTextV15 => "nomic-ai/nomic-embed-text-v1.5",
            Self::NomicEmbedTextV15Q => "nomic-ai/nomic-embed-text-v1.5 (quantized)",
            Self::JinaEmbeddingsV2BaseCode => "jinaai/jina-embeddings-v2-base-code",
            Self::MultilingualE5Small => "intfloat/multilingual-e5-small",
            Self::MxbaiEmbedLargeV1 => "mixedbread-ai/mxbai-embed-large-v1",
            Self::ModernBertEmbedLarge => "lightonai/modernbert-embed-large",
            Self::EmbeddingGemma300MQ4 => "onnx-community/embeddinggemma-300m-ONNX (Q4)",
        }
    }

    /// Check if model is quantized (faster but slightly less accurate)
    #[allow(dead_code)] // Reserved for model selection UI
    pub fn is_quantized(&self) -> bool {
        matches!(
            self,
            Self::AllMiniLML6V2Q
                | Self::AllMiniLML12V2Q
                | Self::BGESmallENV15Q
                | Self::NomicEmbedTextV15Q
                | Self::EmbeddingGemma300MQ4
        )
    }

    /// Get a short identifier for the model (for filenames, etc.)
    pub fn short_name(&self) -> &'static str {
        match self {
            Self::AllMiniLML6V2 => "minilm-l6",
            Self::AllMiniLML6V2Q => "minilm-l6-q",
            Self::AllMiniLML12V2 => "minilm-l12",
            Self::AllMiniLML12V2Q => "minilm-l12-q",
            Self::ParaphraseMLMiniLML12V2 => "paraphrase-minilm",
            Self::BGESmallENV15 => "bge-small",
            Self::BGESmallENV15Q => "bge-small-q",
            Self::BGEBaseENV15 => "bge-base",
            Self::BGELargeENV15 => "bge-large",
            Self::NomicEmbedTextV1 => "nomic-v1",
            Self::NomicEmbedTextV15 => "nomic-v1.5",
            Self::NomicEmbedTextV15Q => "nomic-v1.5-q",
            Self::JinaEmbeddingsV2BaseCode => "jina-code",
            Self::MultilingualE5Small => "e5-multilingual",
            Self::MxbaiEmbedLargeV1 => "mxbai-large",
            Self::ModernBertEmbedLarge => "modernbert-large",
            Self::EmbeddingGemma300MQ4 => "embeddinggemma-q4",
        }
    }

    /// Stamp this model's identity into a metadata JSON object.
    ///
    /// Single source of truth for the three metadata keys (`model_short_name`,
    /// `model_name`, `dimensions`). Every index-creation path — CLI
    /// (`index_with_options`), serve/git-hook force-reindex, incremental
    /// refresh, and `--model` override — writes the model through here so the
    /// keys and value derivation cannot drift, and so `read_model_metadata`
    /// never has to fall back to the `unknown` sentinel (which disables the
    /// empty-index self-heal). Overwrites any existing values for these keys.
    pub fn write_metadata_fields(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        obj.insert(
            "model_short_name".to_string(),
            serde_json::Value::String(self.short_name().to_string()),
        );
        obj.insert(
            "model_name".to_string(),
            serde_json::Value::String(self.name().to_string()),
        );
        obj.insert(
            "dimensions".to_string(),
            serde_json::Value::Number(self.dimensions().into()),
        );
    }

    /// List all available models
    pub fn all() -> &'static [ModelType] {
        &[
            Self::AllMiniLML6V2,
            Self::AllMiniLML6V2Q,
            Self::AllMiniLML12V2,
            Self::AllMiniLML12V2Q,
            Self::ParaphraseMLMiniLML12V2,
            Self::BGESmallENV15,
            Self::BGESmallENV15Q,
            Self::BGEBaseENV15,
            Self::BGELargeENV15,
            Self::NomicEmbedTextV1,
            Self::NomicEmbedTextV15,
            Self::NomicEmbedTextV15Q,
            Self::JinaEmbeddingsV2BaseCode,
            Self::MultilingualE5Small,
            Self::MxbaiEmbedLargeV1,
            Self::ModernBertEmbedLarge,
            Self::EmbeddingGemma300MQ4,
        ]
    }

    /// Comma-separated list of all valid model short names.
    ///
    /// Single source of truth for the "valid models" messages shown by the CLI
    /// and the serve API, so they cannot drift from the set `parse()` accepts.
    pub fn valid_short_names() -> String {
        Self::all()
            .iter()
            .map(|m| m.short_name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Parse model from string (for CLI)
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "minilm-l6" | "allminiml6v2" => Some(Self::AllMiniLML6V2),
            "minilm-l6-q" | "allminiml6v2q" => Some(Self::AllMiniLML6V2Q),
            "minilm-l12" | "allminiml12v2" => Some(Self::AllMiniLML12V2),
            "minilm-l12-q" | "allminiml12v2q" => Some(Self::AllMiniLML12V2Q),
            "paraphrase-minilm" => Some(Self::ParaphraseMLMiniLML12V2),
            "bge-small" | "bgesmallenv15" => Some(Self::BGESmallENV15),
            "bge-small-q" | "bgesmallenv15q" => Some(Self::BGESmallENV15Q),
            "bge-base" | "bgebaseenv15" => Some(Self::BGEBaseENV15),
            "bge-large" | "bgelargeenv15" => Some(Self::BGELargeENV15),
            "nomic-v1" | "nomicembedtextv1" => Some(Self::NomicEmbedTextV1),
            "nomic-v1.5" | "nomicembedtextv15" => Some(Self::NomicEmbedTextV15),
            "nomic-v1.5-q" | "nomicembedtextv15q" => Some(Self::NomicEmbedTextV15Q),
            "jina-code" | "jinaembeddingsv2basecode" => Some(Self::JinaEmbeddingsV2BaseCode),
            "e5-multilingual" | "multilinguale5small" => Some(Self::MultilingualE5Small),
            "mxbai-large" | "mxbaiembedlargev1" => Some(Self::MxbaiEmbedLargeV1),
            "modernbert-large" | "modernbertembedlarge" => Some(Self::ModernBertEmbedLarge),
            "embeddinggemma-q4" | "embeddinggemma300mq4" => Some(Self::EmbeddingGemma300MQ4),
            _ => None,
        }
    }

    /// Resolve the embedding model recorded in an index's `metadata.json`.
    ///
    /// The reader counterpart to [`Self::write_metadata_fields`]. Returns `None`
    /// when the file is missing/unreadable, carries no `model_short_name`, or
    /// names a model this build does not know — callers fall back to
    /// [`ModelType::default`]. Every query path MUST resolve the model per
    /// target index through here (instead of assuming the default): embedding a
    /// query with any model other than the one the index was built with either
    /// fails with a dimension mismatch or silently compares incomparable vector
    /// spaces. Multi-repo serve mode is where this matters most, because one
    /// hub can hold indexes built with different models.
    pub fn from_index_metadata(db_path: &std::path::Path) -> Option<Self> {
        let content = std::fs::read_to_string(db_path.join("metadata.json")).ok()?;
        let json: serde_json::Value = serde_json::from_str(&content).ok()?;
        let name = json.get("model_short_name").and_then(|v| v.as_str())?;
        Self::parse(name)
    }

    pub fn prepare_query(&self, text: &str) -> String {
        match self {
            Self::EmbeddingGemma300MQ4 => format!("task: search result | query: {text}"),
            _ => text.to_string(),
        }
    }

    pub fn prepare_document(&self, text: &str) -> String {
        match self {
            Self::EmbeddingGemma300MQ4 => format!("title: none | text: {text}"),
            _ => text.to_string(),
        }
    }

    /// Label prefix for a chunk's main content when building the embedding input.
    ///
    /// EmbeddingGemma benefits from distinguishing prose from source code, so
    /// Markdown and plain-text files are labeled `Text`. Every other model keeps
    /// the historical `Code` label unconditionally, leaving their embeddings —
    /// and therefore existing indexes — byte-for-byte unchanged.
    pub fn content_label(&self, path: &str) -> &'static str {
        match self {
            Self::EmbeddingGemma300MQ4
                if Language::from_path(std::path::Path::new(path)) == Language::Markdown =>
            {
                "Text"
            }
            _ => "Code",
        }
    }

    /// Whether this model produces larger embeddings than the default model.
    pub fn is_heavier_than_default(&self) -> bool {
        self.dimensions() > Self::default().dimensions()
    }
}

/// Fast embedding model using fastembed library
pub struct FastEmbedder {
    model: TextEmbedding,
    model_type: ModelType,
}

impl FastEmbedder {
    /// Create a new embedder with default model
    pub fn new() -> Result<Self> {
        Self::with_model(ModelType::default())
    }

    /// Create a new embedder with specified model
    pub fn with_model(model_type: ModelType) -> Result<Self> {
        Self::with_cache_dir(model_type, None)
    }

    /// Create a new embedder with specified model and cache directory
    pub fn with_cache_dir(
        model_type: ModelType,
        cache_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::init(model_type, cache_dir, false)
    }

    /// Create a new embedder with the fastembed download progress bar visible.
    ///
    /// Only `codesearch setup` wants this: it downloads hundreds of MB on
    /// purpose, and a silent run looks like a hang. Runtime paths keep the
    /// bar off so it does not corrupt TUI/serve output.
    pub fn with_cache_dir_progress(
        model_type: ModelType,
        cache_dir: Option<&std::path::Path>,
        show_download_progress: bool,
    ) -> Result<Self> {
        Self::init(model_type, cache_dir, show_download_progress)
    }

    fn init(
        model_type: ModelType,
        cache_dir: Option<&std::path::Path>,
        show_download_progress: bool,
    ) -> Result<Self> {
        // Set cache directory via environment variable if provided
        // Note: fastembed library uses FASTEMBED_CACHE_DIR (not FASTEMBED_CACHE_PATH)
        if let Some(cache_dir) = cache_dir {
            std::env::set_var(
                "FASTEMBED_CACHE_DIR",
                cache_dir.to_string_lossy().to_string(),
            );
        }

        // Use CPU execution provider WITH arena allocator for speed.
        // Arena allocator provides fast memory reuse during inference.
        let cpu_ep = CPU::default().with_arena_allocator(true).build();

        let options = TextInitOptions::new(model_type.to_fastembed_model())
            .with_show_download_progress(show_download_progress)
            .with_execution_providers(vec![cpu_ep]);

        // Cap ONNX intra-op threads when CODESEARCH_EMBED_THREADS is set.
        // Unset leaves fastembed/ONNX Runtime at its default (all cores): one
        // inference alone can saturate a large machine, which starves every
        // other task on a shared box. See `crate::limits`.
        let options = match crate::limits::embed_threads() {
            Some(threads) => options.with_intra_threads(threads),
            None => options,
        };

        let model = TextEmbedding::try_new(options)
            .map_err(|e| anyhow!("Failed to initialize embedding model: {}", e))?;

        Ok(Self { model, model_type })
    }
    /// Mini-batch size for a single ONNX `embed` call: `CODESEARCH_BATCH_SIZE`
    /// when set, otherwise adaptive by model dimensions.
    ///
    /// Shared by [`Self::embed_batch`] and the serve pool's interleaved batch
    /// loop (`EmbeddingServicePool::embed_chunks_yielding`), so both agree on
    /// how long one ONNX call (and therefore one lock acquisition) lasts.
    pub fn effective_batch_size(model_type: ModelType) -> usize {
        // Check for env var override (tune with CODESEARCH_BATCH_SIZE=N)
        if let Ok(env_size) = std::env::var("CODESEARCH_BATCH_SIZE") {
            env_size.parse().unwrap_or(256)
        } else {
            // Adaptive batch size: without arena allocator, ONNX frees buffers after each batch
            // so larger batches are faster without accumulating memory.
            match model_type.dimensions() {
                d if d <= 384 => 256, // Small models (MiniLM etc.)
                d if d <= 768 => 128, // Medium models (BGE-base, Jina etc.)
                _ => 64,              // Large models (BGE-large, MxBai etc.)
            }
        }
    }

    /// Embed a batch of texts (processes in mini-batches to avoid OOM)
    /// Uses adaptive batch size based on model dimensions
    /// Can be overridden with CODESEARCH_BATCH_SIZE environment variable
    pub fn embed_batch(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let batch_size = Self::effective_batch_size(self.model_type);
        self.embed_batch_chunked(texts, batch_size)
    }

    /// Embed a batch of texts with configurable mini-batch size
    pub fn embed_batch_chunked(
        &mut self,
        texts: Vec<String>,
        batch_size: usize,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        // Process in mini-batches to avoid OOM with large models
        for chunk in texts.chunks(batch_size) {
            // Check for CTRL-C between mini-batches so we don't block for minutes
            if crate::constants::is_shutdown_requested() {
                return Err(anyhow!("Embedding interrupted by shutdown request"));
            }

            let text_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();

            let embeddings = self
                .model
                .embed(text_refs, None)
                .map_err(|e| anyhow!("Failed to generate embeddings: {}", e))?;

            all_embeddings.extend(embeddings);
        }

        Ok(all_embeddings)
    }

    /// Embed a single text
    pub fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let embeddings = self.embed_batch(vec![text.to_string()])?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No embedding generated"))
    }

    pub fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        let text = self.model_type.prepare_query(text);
        self.embed_one(&text)
    }

    pub fn embed_queries(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let texts = texts
            .into_iter()
            .map(|text| self.model_type.prepare_query(&text))
            .collect();
        self.embed_batch(texts)
    }

    pub fn embed_documents(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let texts = texts
            .into_iter()
            .map(|text| self.model_type.prepare_document(&text))
            .collect();
        self.embed_batch(texts)
    }

    /// Get the dimensionality of embeddings
    pub fn dimensions(&self) -> usize {
        self.model_type.dimensions()
    }

    /// Get the model name
    #[allow(dead_code)] // Reserved for diagnostics
    pub fn model_name(&self) -> &str {
        self.model_type.name()
    }

    /// Get the model type
    #[allow(dead_code)] // Reserved for diagnostics
    pub fn model_type(&self) -> ModelType {
        self.model_type
    }
}

impl Default for FastEmbedder {
    fn default() -> Self {
        Self::new().expect("Failed to create default embedder")
    }
}

// ─── Persisted default model (setup → serve) ─────────────────────────────────
//
// `codesearch setup --model X` downloads X but used to record the choice
// nowhere, so serve started without its own `--model` flag silently fell
// back to the built-in default for every NEW index — the pilot operator had
// to re-type `--model embeddinggemma-q4` on every serve launch. The helpers
// below persist the setup choice under `<codesearch_home>` and let serve
// adopt it automatically.

/// Path of the persisted default-model choice.
fn default_model_file() -> Result<std::path::PathBuf> {
    Ok(crate::constants::codesearch_home()?.join(crate::constants::DEFAULT_MODEL_FILE))
}

/// Persist `model` as the default for new indexes (called by `codesearch setup`).
///
/// tmp+fsync+rename so a crash mid-write can never leave a torn choice file
/// (same discipline as the other codesearch-owned JSON files).
pub fn save_default_model(model: ModelType) -> Result<()> {
    use std::io::Write;

    let path = default_model_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".new");
    let tmp_path = std::path::PathBuf::from(tmp);
    let data = format!("{{\"model\":\"{}\"}}", model.short_name());
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Read the default model persisted by `codesearch setup`.
///
/// `None` when no choice was recorded or the file names an unknown model —
/// serve then keeps its built-in fallback default for new indexes instead of
/// failing to start over a config file it can revive by re-running setup.
pub fn load_default_model() -> Option<ModelType> {
    let path = default_model_file().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    let short_name = parsed.get("model")?.as_str()?;
    match ModelType::parse(short_name) {
        Some(model) => Some(model),
        None => {
            tracing::warn!(
                "Persisted default model '{}' is not known to this build; ignoring it",
                short_name
            );
            None
        }
    }
}

/// Whether `model`'s ONNX files are already present in the global models cache.
///
/// Resolves fastembed's own model table (`model_code`, `model_file`,
/// `additional_files`) and probes the hf-hub cache layout fastembed downloads
/// into (`models--<org>--<repo>/snapshots/<rev>/…`) instead of duplicating repo
/// ids here. Purely a filesystem probe — never touches the network, so serve
/// can warn about a missing download before warmup starts one mid-flight.
pub fn is_model_in_cache(model: ModelType) -> bool {
    let cache_dir = match crate::constants::get_global_models_cache_dir() {
        Ok(dir) => dir,
        Err(_) => return false,
    };
    let fe_model = model.to_fastembed_model();
    let info = match <FastEmbedModel as fastembed::ModelTrait>::get_model_info(&fe_model) {
        Some(info) => info,
        None => return false,
    };
    let slug = format!("models--{}", info.model_code.replace('/', "--"));
    let snapshots_dir = cache_dir.join(slug).join("snapshots");
    let Ok(revisions) = std::fs::read_dir(&snapshots_dir) else {
        return false;
    };
    for revision in revisions.flatten() {
        let snapshot = revision.path();
        if !snapshot.join(&info.model_file).exists() {
            continue;
        }
        if info.additional_files.iter().all(|f| snapshot.join(f).exists()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn default_model_choice_round_trips_and_degrades_on_unknown_name() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::testing::EnvRestore::set(&[(
            crate::constants::CODESEARCH_HOME_ENV,
            tmp.path().to_str().unwrap(),
        )]);

        assert_eq!(load_default_model(), None, "no choice recorded yet");

        save_default_model(ModelType::EmbeddingGemma300MQ4).unwrap();
        assert_eq!(
            load_default_model(),
            Some(ModelType::EmbeddingGemma300MQ4),
            "the setup choice must survive across processes"
        );

        // A file naming an unknown model (e.g. written by a newer/older build)
        // must degrade to None instead of poisoning serve startup.
        std::fs::write(
            tmp.path().join(crate::constants::DEFAULT_MODEL_FILE),
            "{\"model\":\"not-a-model\"}",
        )
        .unwrap();
        assert_eq!(load_default_model(), None);
    }

    #[test]
    #[serial_test::serial]
    fn is_model_in_cache_matches_hf_hub_download_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::testing::EnvRestore::set(&[(
            crate::constants::CODESEARCH_HOME_ENV,
            tmp.path().to_str().unwrap(),
        )]);

        // Empty models cache → nothing cached (cache dir gets created, but no
        // model directories inside).
        assert!(!is_model_in_cache(ModelType::EmbeddingGemma300MQ4));

        // Reproduce the exact hf-hub layout fastembed downloads into (verified
        // against a live cache with a manually-installed snapshot):
        // models--onnx-community--embeddinggemma-300m-ONNX/snapshots/<rev>/onnx/…
        let model_dir = tmp
            .path()
            .join("models")
            .join("models--onnx-community--embeddinggemma-300m-ONNX");
        let snapshot = model_dir.join("snapshots").join("manual");
        std::fs::create_dir_all(snapshot.join("onnx")).unwrap();
        std::fs::write(snapshot.join("onnx/model_q4.onnx"), "x").unwrap();
        std::fs::write(snapshot.join("onnx/model_q4.onnx_data"), "x").unwrap();
        assert!(is_model_in_cache(ModelType::EmbeddingGemma300MQ4));

        // A half-downloaded model (additional file missing) must not report
        // cached — the pool would hit the network on first use otherwise.
        std::fs::remove_file(snapshot.join("onnx/model_q4.onnx_data")).unwrap();
        assert!(!is_model_in_cache(ModelType::EmbeddingGemma300MQ4));
    }

    #[test]
    fn test_model_type_dimensions() {
        // 384 dimension models
        assert_eq!(ModelType::BGESmallENV15.dimensions(), 384);
        assert_eq!(ModelType::BGESmallENV15Q.dimensions(), 384);
        assert_eq!(ModelType::AllMiniLML6V2.dimensions(), 384);
        assert_eq!(ModelType::AllMiniLML6V2Q.dimensions(), 384);
        assert_eq!(ModelType::AllMiniLML12V2.dimensions(), 384);
        assert_eq!(ModelType::MultilingualE5Small.dimensions(), 384);
        // 768 dimension models
        assert_eq!(ModelType::BGEBaseENV15.dimensions(), 768);
        assert_eq!(ModelType::NomicEmbedTextV1.dimensions(), 768);
        assert_eq!(ModelType::NomicEmbedTextV15.dimensions(), 768);
        assert_eq!(ModelType::JinaEmbeddingsV2BaseCode.dimensions(), 768);
        // 1024 dimension models
        assert_eq!(ModelType::BGELargeENV15.dimensions(), 1024);
        assert_eq!(ModelType::MxbaiEmbedLargeV1.dimensions(), 1024);
        assert_eq!(ModelType::ModernBertEmbedLarge.dimensions(), 1024);
        assert_eq!(ModelType::EmbeddingGemma300MQ4.dimensions(), 768);
    }

    #[test]
    fn test_model_type_names() {
        assert_eq!(ModelType::BGESmallENV15.name(), "BAAI/bge-small-en-v1.5");
        assert_eq!(
            ModelType::AllMiniLML6V2.name(),
            "sentence-transformers/all-MiniLM-L6-v2"
        );
        assert_eq!(
            ModelType::JinaEmbeddingsV2BaseCode.name(),
            "jinaai/jina-embeddings-v2-base-code"
        );
    }

    #[test]
    fn test_default_model() {
        let model = ModelType::default();
        assert_eq!(model, ModelType::AllMiniLML6V2Q);
        assert_eq!(model.dimensions(), 384);
    }

    #[test]
    fn test_short_name_round_trips_through_parse() {
        // Every model advertised by all() must parse back from its short_name.
        // This guards the C1/M6 fix: the embedding path resolves the model from
        // the short name stored in metadata.json, and the CLI/serve error lists
        // are derived from valid_short_names() — a model whose short_name does
        // not parse would silently break indexing or produce a wrong error list.
        for m in ModelType::all() {
            assert_eq!(
                ModelType::parse(m.short_name()),
                Some(*m),
                "short_name '{}' did not round-trip through parse()",
                m.short_name()
            );
        }
    }

    #[test]
    fn test_valid_short_names_lists_every_model() {
        let listed = ModelType::valid_short_names();
        for m in ModelType::all() {
            assert!(
                listed.split(", ").any(|s| s == m.short_name()),
                "valid_short_names() is missing '{}'",
                m.short_name()
            );
        }
        // Regression for the original bug: bge-large was accepted by parse()
        // but omitted from the hand-maintained error lists.
        assert!(listed.contains("bge-large"));
    }

    #[test]
    fn test_parse() {
        assert_eq!(
            ModelType::parse("minilm-l6"),
            Some(ModelType::AllMiniLML6V2)
        );
        assert_eq!(
            ModelType::parse("minilm-l6-q"),
            Some(ModelType::AllMiniLML6V2Q)
        );
        assert_eq!(
            ModelType::parse("minilm-l12"),
            Some(ModelType::AllMiniLML12V2)
        );
        assert_eq!(
            ModelType::parse("minilm-l12-q"),
            Some(ModelType::AllMiniLML12V2Q)
        );
        assert_eq!(
            ModelType::parse("paraphrase-minilm"),
            Some(ModelType::ParaphraseMLMiniLML12V2)
        );
        assert_eq!(
            ModelType::parse("bge-small"),
            Some(ModelType::BGESmallENV15)
        );
        assert_eq!(
            ModelType::parse("bge-small-q"),
            Some(ModelType::BGESmallENV15Q)
        );
        assert_eq!(ModelType::parse("bge-base"), Some(ModelType::BGEBaseENV15));
        assert_eq!(
            ModelType::parse("nomic-v1"),
            Some(ModelType::NomicEmbedTextV1)
        );
        assert_eq!(
            ModelType::parse("nomic-v1.5"),
            Some(ModelType::NomicEmbedTextV15)
        );
        assert_eq!(
            ModelType::parse("nomic-v1.5-q"),
            Some(ModelType::NomicEmbedTextV15Q)
        );
        assert_eq!(
            ModelType::parse("jina-code"),
            Some(ModelType::JinaEmbeddingsV2BaseCode)
        );
        assert_eq!(
            ModelType::parse("embeddinggemma-q4"),
            Some(ModelType::EmbeddingGemma300MQ4)
        );
        assert_eq!(ModelType::parse("invalid"), None);
    }

    #[test]
    fn test_is_quantized() {
        assert!(ModelType::AllMiniLML6V2Q.is_quantized());
        assert!(ModelType::BGESmallENV15Q.is_quantized());
        assert!(!ModelType::BGESmallENV15.is_quantized());
        assert!(!ModelType::JinaEmbeddingsV2BaseCode.is_quantized());
        assert!(ModelType::EmbeddingGemma300MQ4.is_quantized());
    }

    #[test]
    fn test_embeddinggemma_retrieval_prompts() {
        let model = ModelType::EmbeddingGemma300MQ4;
        assert_eq!(
            model.prepare_query("Was wurde entschieden?"),
            "task: search result | query: Was wurde entschieden?"
        );
        assert_eq!(
            model.prepare_document("Eine dauerhafte Notiz"),
            "title: none | text: Eine dauerhafte Notiz"
        );
    }

    #[test]
    fn test_retrieval_prompts_leave_other_models_unchanged() {
        let model = ModelType::AllMiniLML6V2Q;
        assert_eq!(model.prepare_query("search text"), "search text");
        assert_eq!(model.prepare_document("document text"), "document text");
    }

    #[test]
    fn test_content_label_only_distinguishes_prose_for_embeddinggemma() {
        let gemma = ModelType::EmbeddingGemma300MQ4;
        assert_eq!(gemma.content_label("notes.md"), "Text");
        assert_eq!(gemma.content_label("NOTES.MD"), "Text");
        assert_eq!(gemma.content_label("notes.markdown"), "Text");
        assert_eq!(gemma.content_label("notes.txt"), "Text");
        assert_eq!(gemma.content_label("src/lib.rs"), "Code");

        let default = ModelType::default();
        assert_eq!(default.content_label("notes.md"), "Code");
        assert_eq!(default.content_label("notes.txt"), "Code");
        assert_eq!(default.content_label("src/lib.rs"), "Code");
    }

    #[test]
    fn test_heavier_than_default_tracks_vector_dimensions() {
        assert!(!ModelType::default().is_heavier_than_default());
        assert!(!ModelType::MultilingualE5Small.is_heavier_than_default());
        assert!(ModelType::EmbeddingGemma300MQ4.is_heavier_than_default());
        assert!(ModelType::ModernBertEmbedLarge.is_heavier_than_default());
    }

    #[test]
    #[ignore] // Requires downloading model
    fn test_embedder_creation() {
        let embedder = FastEmbedder::new();
        assert!(embedder.is_ok());

        let embedder = embedder.unwrap();
        assert_eq!(embedder.dimensions(), 384);
    }

    /// Get the global models cache dir for tests
    fn test_cache_dir() -> std::path::PathBuf {
        crate::constants::get_global_models_cache_dir().unwrap()
    }

    #[test]
    #[ignore] // Requires model
    fn test_embed_single_text() {
        let mut embedder =
            FastEmbedder::with_cache_dir(ModelType::default(), Some(&test_cache_dir())).unwrap();
        let embedding = embedder.embed_one("Hello, world!").unwrap();

        assert_eq!(embedding.len(), 384);
        // Check embedding is normalized (roughly unit length)
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.1);
    }

    #[test]
    #[ignore] // Requires model
    fn test_embed_batch() {
        let mut embedder =
            FastEmbedder::with_cache_dir(ModelType::default(), Some(&test_cache_dir())).unwrap();
        let texts = vec![
            "Hello, world!".to_string(),
            "Rust is awesome".to_string(),
            "Code search with AI".to_string(),
        ];

        let embeddings = embedder.embed_batch(texts).unwrap();

        assert_eq!(embeddings.len(), 3);
        for embedding in embeddings {
            assert_eq!(embedding.len(), 384);
        }
    }

    #[test]
    #[ignore] // Requires model
    fn test_semantic_similarity() {
        let mut embedder =
            FastEmbedder::with_cache_dir(ModelType::default(), Some(&test_cache_dir())).unwrap();

        let text1 = "The quick brown fox jumps over the lazy dog";
        let text2 = "A fast auburn fox leaps over a sleepy canine";
        let text3 = "Python is a programming language";

        let emb1 = embedder.embed_one(text1).unwrap();
        let emb2 = embedder.embed_one(text2).unwrap();
        let emb3 = embedder.embed_one(text3).unwrap();

        // Cosine similarity
        let sim_1_2 = cosine_similarity(&emb1, &emb2);
        let sim_1_3 = cosine_similarity(&emb1, &emb3);

        // Similar texts should have higher similarity
        assert!(sim_1_2 > sim_1_3);
        assert!(sim_1_2 > 0.7); // Should be quite similar
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (mag_a * mag_b)
    }
}
