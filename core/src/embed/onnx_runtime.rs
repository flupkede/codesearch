//! ONNX Runtime availability check shared by every local ONNX consumer
//! (the fastembed embedder and the neural reranker).

use anyhow::Result;

/// Environment variable `ort` reads the ONNX Runtime library path from when
/// built with the `load-dynamic` feature.
#[cfg(feature = "load-dynamic")]
const ORT_DYLIB_PATH_ENV: &str = "ORT_DYLIB_PATH";

/// Make sure ONNX Runtime can be used. Call before building any fastembed
/// model or `ort` session.
///
/// Default builds link ONNX Runtime at build time, so this is a no-op. With
/// the `load-dynamic` feature, `ort` would load the shared library lazily on
/// first use and panic when it is missing or older than the API version it
/// was built against. This loads it first, from the same location `ort` would
/// use (`ORT_DYLIB_PATH`, else the platform's `onnxruntime` library name on
/// the loader search path), and turns a failure into an error naming the fix.
/// Once loaded, later calls reuse the library.
pub fn ensure_onnx_runtime() -> Result<()> {
    #[cfg(feature = "load-dynamic")]
    load_dynamic()?;
    Ok(())
}

#[cfg(feature = "load-dynamic")]
fn load_dynamic() -> Result<()> {
    let path: std::path::PathBuf = match std::env::var(ORT_DYLIB_PATH_ENV) {
        Ok(path) if !path.is_empty() => path.into(),
        _ => format!(
            "{}onnxruntime{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        )
        .into(),
    };
    // Only the library load matters here: the environment builder is dropped
    // uncommitted, and fastembed's sessions use the default environment.
    drop(ort::init_from(&path).map_err(|e| {
        // libloading reports only "dlopen failed"; the loader's own reason
        // (file not found, missing dependency, wrong architecture) is its
        // source.
        let reason = match &e {
            ort::LoadDynamicError::Dlopen { error, .. } => std::error::Error::source(error)
                .map(|source| format!(" ({source})"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        anyhow::anyhow!(
            "Failed to load ONNX Runtime: {e}{reason}. This codesearch build loads ONNX Runtime \
             at runtime (`load-dynamic` feature): set {ORT_DYLIB_PATH_ENV} to the shared \
             library of ONNX Runtime 1.{}.x or newer \
             (https://github.com/microsoft/onnxruntime/releases)",
            ort::MINOR_VERSION
        )
    })?);
    Ok(())
}

#[cfg(all(test, feature = "load-dynamic"))]
#[path = "onnx_runtime_tests.rs"]
mod tests;
