use super::{FastEmbedder, ModelType};
use anyhow::Result;

/// Retrieval input semantics belong to the provider; local prompts remain unchanged.
pub trait Embedder: Send {
    fn embed_documents(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;
    fn embed_queries(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn model_name(&self) -> &str;
    fn local_model(&self) -> Option<ModelType>;
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed_queries(vec![text.to_owned()])?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No embedding generated"))
    }
}
impl Embedder for FastEmbedder {
    fn embed_documents(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        FastEmbedder::embed_documents(self, texts)
    }
    fn embed_queries(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        FastEmbedder::embed_queries(self, texts)
    }
    fn dimensions(&self) -> usize {
        FastEmbedder::dimensions(self)
    }
    fn model_name(&self) -> &str {
        FastEmbedder::model_name(self)
    }
    fn local_model(&self) -> Option<ModelType> {
        Some(self.model_type())
    }
}
