use codesearch::ModelType;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Command;

fn write_index_markers(project: &Path, model: &str, dimensions: usize) {
    let db = project.join(".codesearch.db");
    fs::create_dir_all(db.join("fts")).expect("database directories should be created");
    fs::write(db.join("data.mdb"), []).expect("LMDB marker should be created");
    fs::write(
        db.join("metadata.json"),
        serde_json::to_vec(&json!({
            "model_short_name": model,
            "dimensions": dimensions
        }))
        .expect("metadata should serialize"),
    )
    .expect("metadata should be written");
}

#[test]
fn unknown_model_error_lists_every_supported_model() {
    let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .args(["--model", "not-a-model", "search", "query"])
        .output()
        .expect("codesearch should start");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");

    for model in ModelType::all() {
        assert!(
            stderr.contains(model.short_name()),
            "unknown-model error omitted '{}'",
            model.short_name()
        );
    }
}

#[test]
fn search_rejects_a_model_that_does_not_match_the_index() {
    let project = tempfile::tempdir().expect("temporary project should be created");
    write_index_markers(project.path(), "minilm-l6-q", 384);

    let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .args(["--model", "embeddinggemma-q4", "search", "query", "--path"])
        .arg(project.path())
        .arg("--create-index=false")
        .output()
        .expect("codesearch should start");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("minilm-l6-q"), "{stderr}");
    assert!(stderr.contains("embeddinggemma-q4"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
}

#[test]
fn search_rejects_an_override_when_the_index_model_is_unknown() {
    let project = tempfile::tempdir().expect("temporary project should be created");
    write_index_markers(project.path(), "future-model", 768);

    let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .args([
            "--model",
            "embeddinggemma-q4",
            "search",
            "query",
            "--sync",
            "--path",
        ])
        .arg(project.path())
        .arg("--create-index=false")
        .output()
        .expect("codesearch should start");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("future-model"), "{stderr}");
    assert!(stderr.contains("embeddinggemma-q4"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
}
