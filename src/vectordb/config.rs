use anyhow::{bail, Result};
use std::path::Path;

pub async fn delete_postgres_repo(db_path: &Path, project_path: &Path) -> Result<()> {
    if storage_backend(db_path)? != "postgres" {
        return Ok(());
    }
    let (model, dimensions) = crate::index::IndexManager::resolve_embed_model(db_path)?;
    let mut store = super::ChunkStore::open_for_path(
        db_path,
        project_path,
        model.short_name(),
        dimensions,
        true,
    )
    .await?;
    store.clear().await
}

/// Resolve runtime selection without mistaking an existing index for another backend.
pub fn storage_backend(db_path: &Path) -> Result<String> {
    let recorded = match std::fs::read_to_string(db_path.join("metadata.json")) {
        Ok(content) => serde_json::from_str::<serde_json::Value>(&content)?
            .get("storage_backend")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(e) => return Err(e.into()),
    };
    let recorded = recorded.or_else(|| {
        db_path
            .join("data.mdb")
            .exists()
            .then(|| "local".to_owned())
    });
    let selected = std::env::var("CODESEARCH_STORAGE").ok();
    if let (Some(selected), Some(recorded)) = (&selected, &recorded) {
        if selected != recorded {
            bail!("Storage backend mismatch: index uses '{recorded}', CODESEARCH_STORAGE selects '{selected}'. Rebuild explicitly before switching storage backends");
        }
    }
    let backend = selected.or(recorded).unwrap_or_else(|| "local".into());
    if backend != "local" && backend != "postgres" {
        bail!("CODESEARCH_STORAGE must be 'local' or 'postgres'");
    }
    Ok(backend)
}
