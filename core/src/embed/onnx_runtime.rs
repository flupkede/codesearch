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
    #[cfg(all(
        target_arch = "x86_64",
        feature = "download-binaries",
        not(feature = "load-dynamic")
    ))]
    prebuilt_avx2_gate()?;
    #[cfg(feature = "load-dynamic")]
    load_dynamic()?;
    Ok(())
}

/// x86_64, default build only: the prebuilt ONNX Runtime binaries `ort`
/// downloads at build time require AVX2, and on an older CPU they die with
/// SIGILL the moment they embed. Stop before that with the fix instead —
/// the `load-dynamic` build loads an official release that picks its CPU
/// code paths at runtime and does run there.
#[cfg(all(
    target_arch = "x86_64",
    feature = "download-binaries",
    not(feature = "load-dynamic")
))]
fn prebuilt_avx2_gate() -> Result<()> {
    if is_x86_feature_detected!("avx2") {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "This CPU does not support AVX2, which the prebuilt ONNX Runtime binaries linked by this \
         codesearch build require. Build with `--no-default-features --features load-dynamic` and \
         set ORT_DYLIB_PATH to the shared library of ONNX Runtime 1.31.x or newer \
         (https://github.com/microsoft/onnxruntime/releases) instead."
    ))
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

#[cfg(all(
    test,
    target_arch = "x86_64",
    feature = "download-binaries",
    not(feature = "load-dynamic")
))]
mod avx2_gate_tests {
    use super::prebuilt_avx2_gate;

    #[test]
    fn gate_passes_on_avx2_and_names_the_fix_without_it() {
        let r = prebuilt_avx2_gate();
        if is_x86_feature_detected!("avx2") {
            assert!(r.is_ok());
        } else {
            let msg = r.unwrap_err().to_string();
            assert!(
                msg.contains("AVX2")
                    && msg.contains("load-dynamic")
                    && msg.contains("ORT_DYLIB_PATH"),
                "gate error must name the CPU limit and the fix: {msg}"
            );
        }
    }
}
