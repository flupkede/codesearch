use crate::{validate_vector, Error, Result};
use codesearch_core::embed::{create_embedder, EmbedConfig, Embedder, ModelType};
use std::sync::{Arc, Mutex};

/// No local index/cache: only provider model artifacts may live on disk.
#[derive(Clone)]
pub struct Embedding {
    provider: Arc<Mutex<Box<dyn Embedder>>>,
    pub(crate) fingerprint: String,
    pub(crate) dimensions: usize,
    pub(crate) document_model: ModelType,
}
impl Embedding {
    pub fn new(mut config: EmbedConfig, model: ModelType) -> anyhow::Result<Self> {
        let provider = create_embedder(config.clone(), model)?;
        let dimensions = provider.dimensions();
        let document_model = provider.local_model().unwrap_or_default();
        anyhow::ensure!(
            (1..=2000).contains(&dimensions),
            "embedding dimensions must be 1 to 2000"
        );
        let fingerprint = if config.remote() {
            // Match core identity semantics, but normalize probed dimensions.
            config.embed_dimensions = Some(dimensions);
            config.fingerprint()
        } else {
            format!("local:{}:{dimensions}", model.short_name())
        };
        Ok(Self {
            provider: Arc::new(Mutex::new(provider)),
            fingerprint: format!("{fingerprint}:server-documents-v2"),
            dimensions,
            document_model,
        })
    }
    pub(crate) async fn embed(&self, texts: Vec<String>, query: bool) -> Result<Vec<Vec<f32>>> {
        let provider = self.provider.clone();
        let dimensions = self.dimensions;
        tokio::task::spawn_blocking(move || {
            let count = texts.len();
            let mut provider = provider.lock().map_err(|_| Error::Embedding)?;
            // Provider errors may contain untrusted response text; never forward it.
            let vectors = if query {
                provider.embed_queries(texts)
            } else {
                provider.embed_documents(texts)
            }
            .map_err(|_| Error::Embedding)?;
            if vectors.len() != count {
                return Err(Error::Embedding);
            }
            for vector in &vectors {
                validate_vector(vector, dimensions)?;
            }
            Ok(vectors)
        })
        .await
        .map_err(|_| Error::Embedding)?
    }
}
