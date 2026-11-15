use super::*;

/// Set in the child process the test below re-executes itself in.
const CHILD_ENV: &str = "CODESEARCH_TEST_ORT_PRELOAD_CHILD";
const TEST_NAME: &str = "embed::onnx_runtime::tests::missing_onnx_runtime_library_is_a_clear_error";

/// A `load-dynamic` build without a loadable ONNX Runtime must fail every
/// local ONNX consumer with an error that names the library path, the
/// loader's reason and `ORT_DYLIB_PATH`, instead of panicking inside `ort`.
///
/// `ort` keeps the first library it loads in a process-global, so once any
/// earlier test in this process has loaded a real ONNX Runtime the missing
/// path would never be tried. The scenario therefore runs in a fresh process:
/// the test re-executes this test binary for itself alone, with
/// `ORT_DYLIB_PATH` pointing at a file that does not exist.
#[test]
fn missing_onnx_runtime_library_is_a_clear_error() {
    if std::env::var_os(CHILD_ENV).is_some() {
        assert_every_consumer_reports_missing_library();
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("libonnxruntime-missing.so");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env(ORT_DYLIB_PATH_ENV, &missing)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "child test failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "child must actually run the scenario:\n{stdout}"
    );
}

fn assert_every_consumer_reports_missing_library() {
    let missing = std::env::var(ORT_DYLIB_PATH_ENV).unwrap();
    let consumers = [
        ("preload", ensure_onnx_runtime()),
        ("embedder", crate::embed::FastEmbedder::new().map(drop)),
        ("reranker", crate::rerank::NeuralReranker::new().map(drop)),
    ];
    for (consumer, result) in consumers {
        let err = format!("{:#}", result.expect_err(consumer));
        assert!(
            err.contains(&missing),
            "{consumer}: error must name the path it tried: {err}"
        );
        assert!(
            err.contains(ORT_DYLIB_PATH_ENV),
            "{consumer}: error must name the variable to set: {err}"
        );
        // The loader's reason, not just libloading's bare "dlopen failed".
        #[cfg(unix)]
        assert!(
            err.to_lowercase().contains("no such file"),
            "{consumer}: error must carry the loader's reason: {err}"
        );
    }
}
