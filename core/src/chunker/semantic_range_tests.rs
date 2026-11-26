use super::*;

#[test]
fn structured_split_bounds_are_inclusive() {
    let source = format!("fn example() {{\n{}}}", "    do_work();\n".repeat(180));
    let chunks = SemanticChunker::new(80,6000,10).chunk_semantic(Language::Rust, Path::new("example.rs"), &source).unwrap();
    assert_eq!(chunks.iter().map(|chunk| (chunk.start_line,chunk.end_line)).collect::<Vec<_>>(), [(1,80),(71,150),(141,182)]);
}

#[test]
fn fragmented_utf8_lines_keep_physical_positions() {
    for (language, path) in [(Language::Markdown,"example.md"), (Language::Unknown,"example.data")] {
        let source = format!("short\n{}\nlast", "界".repeat(5000));
        let chunks = SemanticChunker::new(80,6000,10).chunk_semantic(language,Path::new(path),&source).unwrap();
        assert!(chunks.len() >= 3);
        let mut ranges = Vec::new();
        for chunk in chunks {
            assert!((1..=3).contains(&chunk.start_line));
            assert!((chunk.start_line..=3).contains(&chunk.end_line));
            assert!(chunk.content.len() <= 6000);
            if chunk.content.contains("short") { assert_eq!(chunk.start_line,1); }
            if chunk.content.contains("last") { assert_eq!(chunk.end_line,3); }
            ranges.push((chunk.start_line,chunk.end_line));
        }
        assert_eq!(ranges, [(1,1),(2,2),(2,2),(2,3)]);
    }
}
