use crate::output;
use anyhow::{anyhow, Result};
use fastembed::{EmbeddingModel as FastEmbedModel, InitOptions, TextEmbedding};
use ort::execution_providers::CPUExecutionProvider;

/// Available embedding models
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

impl ModelType {
    pub fn to_fastembed_model(&self) -> FastEmbedModel {
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
            | Self::JinaEmbeddingsV2BaseCode => 768,
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
        }
    }

    /// List all available models
    #[allow(dead_code)] // Reserved for model listing command
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
        ]
    }

    /// Parse model from string (for CLI)
    pub fn from_str(s: &str) -> Option<Self> {
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
            _ => None,
        }
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
        output::print_info(format_args!(
            "📦 Loading embedding model: {}",
            model_type.name()
        ));
        output::print_info(format_args!("   Dimensions: {}", model_type.dimensions()));

        // Set cache directory via environment variable if provided
        // Note: fastembed library uses FASTEMBED_CACHE_DIR (not FASTEMBED_CACHE_PATH)
        if let Some(cache_dir) = cache_dir {
            std::env::set_var(
                "FASTEMBED_CACHE_DIR",
                cache_dir.to_string_lossy().to_string(),
            );
        }

        // Use CPU execution provider with arena allocator for better memory performance
        let cpu_ep = CPUExecutionProvider::default()
            .with_arena_allocator(true)
            .build();

        let model = TextEmbedding::try_new(
            InitOptions::new(model_type.to_fastembed_model())
                .with_show_download_progress(true)
                .with_execution_providers(vec![cpu_ep]),
        )
        .map_err(|e| anyhow!("Failed to initialize embedding model: {}", e))?;

        output::print_info(format_args!("✅ Model loaded successfully!"));

        Ok(Self { model, model_type })
    }
    /// Embed a batch of texts (processes in mini-batches to avoid OOM)
    /// Uses adaptive batch size based on model dimensions
    /// Can be overridden with CODESEARCH_BATCH_SIZE environment variable
    pub fn embed_batch(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        // Check for env var override (tune with CODESEARCH_BATCH_SIZE=N)
        let batch_size = if let Ok(env_size) = std::env::var("CODESEARCH_BATCH_SIZE") {
            env_size.parse().unwrap_or(256)
        } else {
            // Adaptive batch size: smaller batches for larger models to avoid OOM
            // Benchmarked on 12-core/24-thread CPU - batch size has minimal impact
            // when CPU is saturated, but larger batches slightly more efficient
            match self.model_type.dimensions() {
                d if d <= 384 => 256, // Small models: larger batches OK
                d if d <= 768 => 128, // Medium models
                _ => 64,              // Large models: smaller to avoid OOM
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_all_models() {
        let all = ModelType::all();
        assert_eq!(all.len(), 16);
    }

    #[test]
    fn test_from_str() {
        assert_eq!(
            ModelType::from_str("bge-small"),
            Some(ModelType::BGESmallENV15)
        );
        assert_eq!(
            ModelType::from_str("jina-code"),
            Some(ModelType::JinaEmbeddingsV2BaseCode)
        );
        assert_eq!(
            ModelType::from_str("minilm-l6-q"),
            Some(ModelType::AllMiniLML6V2Q)
        );
        assert_eq!(ModelType::from_str("unknown"), None);
    }

    #[test]
    fn test_is_quantized() {
        assert!(ModelType::AllMiniLML6V2Q.is_quantized());
        assert!(ModelType::BGESmallENV15Q.is_quantized());
        assert!(!ModelType::BGESmallENV15.is_quantized());
        assert!(!ModelType::JinaEmbeddingsV2BaseCode.is_quantized());
    }

    #[test]
    #[ignore] // Requires downloading model
    fn test_embedder_creation() {
        let embedder = FastEmbedder::new();
        assert!(embedder.is_ok());

        let embedder = embedder.unwrap();
        assert_eq!(embedder.dimensions(), 384);
    }

    #[test]
    #[ignore] // Requires model
    fn test_embed_single_text() {
        let mut embedder = FastEmbedder::new().unwrap();
        let embedding = embedder.embed_one("Hello, world!").unwrap();

        assert_eq!(embedding.len(), 384);
        // Check embedding is normalized (roughly unit length)
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.1);
    }

    #[test]
    #[ignore] // Requires model
    fn test_embed_batch() {
        let mut embedder = FastEmbedder::new().unwrap();
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
        let mut embedder = FastEmbedder::new().unwrap();

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
