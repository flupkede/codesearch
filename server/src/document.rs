use codesearch_core::{
    chunker::Chunk,
    embed::{BatchEmbedder, ModelType},
};

/// One representation for provider input, BM25 and cache identity. Source content
/// and editor line bounds remain separate; metadata is not a source excerpt.
pub(crate) struct Document {
    pub text: String,
    pub hash: String,
}
impl Document {
    pub fn new(chunk: &Chunk, model: ModelType) -> Self {
        let mut chunk = chunk.clone();
        // Some grammars (e.g. Python) include the docstring inside the AST node.
        // Preserve it there instead of adding a second Documentation section.
        if chunk
            .docstring
            .as_ref()
            .is_some_and(|doc| chunk.content.contains(doc))
        {
            chunk.docstring = None;
        }
        let text = BatchEmbedder::prepare_text(&chunk, model);
        let hash = Chunk::compute_hash(&text);
        Self { text, hash }
    }
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
