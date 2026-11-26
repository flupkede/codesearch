use super::*;
use codesearch_core::chunker::{ChunkKind, SemanticChunker};
use codesearch_core::file::Language;
use std::path::Path;

#[test]
fn document_uses_final_content_and_core_metadata_preparation() {
    let mut chunk = Chunk::new(
        "fn example() {}".into(),
        2,
        2,
        ChunkKind::Function,
        "example.rs".into(),
    );
    chunk.context = vec!["File: example.rs".into()];
    chunk.signature = Some("fn example()".into());
    chunk.docstring = Some("/// quasarneedle".into());
    let document = Document::new(&chunk, ModelType::default());
    assert!(document.text.contains("Context: File: example.rs"));
    assert!(document.text.contains("Signature: fn example()"));
    assert!(document.text.contains("Name: example"));
    assert!(document.text.contains("Documentation: quasarneedle"));
    assert_eq!(document.hash, Chunk::compute_hash(&document.text));
    chunk.content.insert_str(0, "// [Part 1/2] fn renamed()\n");
    let changed = Document::new(&chunk, ModelType::default());
    assert_ne!(changed.hash, document.hash, "ignore stale core chunk.hash");
}

#[test]
fn python_inline_documentation_is_not_duplicated() {
    let source =
        "def example():\n    \"\"\"quasarneedle describes this function.\"\"\"\n    return 42\n";
    let chunks = SemanticChunker::new(80, 6000, 10)
        .chunk_semantic(Language::Python, Path::new("example.py"), source)
        .unwrap();
    let chunk = chunks
        .iter()
        .find(|chunk| chunk.docstring.is_some())
        .expect("production extractor must exercise inline docstring");
    let document = Document::new(chunk, ModelType::default());
    assert_eq!(document.text.matches("quasarneedle").count(), 1);
}
