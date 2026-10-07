use anyhow::{anyhow, Result};

use crate::embed::ModelType;

/// Resolve the CLI `--model` argument to a [`ModelType`].
///
/// Thin setup-specific wrapper over [`super::parse_model_arg`]: setup wants
/// the default model when the flag is absent, where other commands keep
/// `None` for per-query resolution.
fn resolve_model(model: Option<&str>) -> Result<ModelType> {
    Ok(super::parse_model_arg(model)?.unwrap_or_default())
}

pub async fn run(model: Option<String>) -> Result<()> {
    let model_type = resolve_model(model.as_deref())?;

    let cache_dir = crate::constants::get_global_models_cache_dir()?;

    println!(
        "📦 Downloading embedding model: {} ({})",
        model_type.name(),
        model_type.short_name()
    );
    println!("   Cache dir: {}", cache_dir.display());

    // Unlike the runtime paths, setup shows the download progress bar: it
    // pulls hundreds of MB on purpose and a silent run looks like a hang.
    let mut embedder = crate::embed::FastEmbedder::with_cache_dir_progress(
        model_type,
        Some(&cache_dir),
        true,
    )
    .map_err(|e| {
        anyhow!(
            "Failed to initialize embedding model: {e}\n\
             Hint: if huggingface.co is unreachable, set HF_ENDPOINT to a HuggingFace mirror and retry."
        )
    })?;

    // Probe inference so a broken download fails here, not mid-indexing.
    let dimensions = embedder.embed_query("setup probe")?.len();
    if dimensions != model_type.dimensions() {
        return Err(anyhow!(
            "Model returned {dimensions} dims, expected {}",
            model_type.dimensions()
        ));
    }

    // Record the choice so serve picks it up automatically on future
    // launches — without this the operator had to re-type `serve --model X`
    // every time (setup used to download and then silently forget).
    crate::embed::save_default_model(model_type)?;

    println!(
        "✅ Setup complete! Model '{}' ({dimensions} dims) ready at {}",
        model_type.short_name(),
        cache_dir.display()
    );
    println!(
        "🧠 Default model for new indexes persisted: {}. `codesearch serve` now uses it \
         automatically (override per-run with --model).",
        model_type.short_name()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_is_rejected_with_valid_list() {
        let err = resolve_model(Some("not-a-model")).expect_err("unknown model must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not-a-model"),
            "error must echo the bad name: {msg}"
        );
        assert!(
            msg.contains(ModelType::valid_short_names().as_str()),
            "error must list valid models: {msg}"
        );
    }

    #[test]
    fn none_resolves_to_default_model() {
        assert_eq!(resolve_model(None).unwrap(), ModelType::default());
    }

    #[test]
    fn known_short_names_resolve() {
        assert_eq!(
            resolve_model(Some("embeddinggemma-q4")).unwrap(),
            ModelType::EmbeddingGemma300MQ4
        );
        assert_eq!(
            resolve_model(Some("minilm-l6-q")).unwrap(),
            ModelType::default()
        );
    }
}
