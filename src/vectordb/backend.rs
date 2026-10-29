use anyhow::{bail, Result};
use std::path::Path;

use super::{storage_backend, ChunkMetadata, SearchResult, StoreStats, VectorStore};
use crate::embed::EmbeddedChunk;

/// Storage for a repository's vectors and complete indexed chunk payloads.
pub enum ChunkStore {
    Local(VectorStore),
    #[cfg(feature = "pgvector")]
    Postgres {
        store: super::PostgresStore,
        repo: String,
    },
}

impl ChunkStore {
    pub async fn open(
        db_path: &Path,
        repo: &str,
        model: &str,
        dimensions: usize,
        readonly: bool,
    ) -> Result<Self> {
        let backend = storage_backend(db_path)?;
        match backend.as_str() {
            "local" => Ok(Self::Local(if readonly {
                VectorStore::open_readonly(db_path, dimensions)?
            } else {
                VectorStore::new(db_path, dimensions)?
            })),
            "postgres" => {
                #[cfg(feature = "pgvector")]
                {
                    let url = std::env::var("CODESEARCH_DATABASE_URL").map_err(|_| {
                        anyhow::anyhow!("Postgres storage requires CODESEARCH_DATABASE_URL")
                    })?;
                    let store = super::PostgresStore::connect(&url, model, dimensions).await?;
                    Ok(Self::Postgres {
                        store,
                        repo: repo.to_owned(),
                    })
                }
                #[cfg(not(feature = "pgvector"))]
                {
                    let _ = (repo, model);
                    bail!("Postgres storage requires a build with --features pgvector");
                }
            }
            _ => bail!("CODESEARCH_STORAGE must be 'local' or 'postgres'"),
        }
    }

    pub async fn open_for_path(
        db_path: &Path,
        project_path: &Path,
        model: &str,
        dimensions: usize,
        readonly: bool,
    ) -> Result<Self> {
        if storage_backend(db_path)? == "local" {
            return Self::open(db_path, "", model, dimensions, readonly).await;
        }
        let config = crate::db_discovery::repos::ReposConfig::load()?;
        let normalized = crate::cache::normalize_path(project_path);
        let manifest: serde_json::Value =
            match std::fs::read_to_string(db_path.join("metadata.json")) {
                Ok(content) => serde_json::from_str(&content)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
                Err(e) => return Err(e.into()),
            };
        let repo = manifest
            .get("storage_repo")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                config
                    .repos
                    .iter()
                    .find_map(|(alias, entry)| {
                        (crate::cache::normalize_path(entry) == normalized).then(|| alias.clone())
                    })
                    .unwrap_or_else(|| normalized.clone())
            });
        let store = Self::open(db_path, &repo, model, dimensions, readonly).await?;
        if !readonly {
            std::fs::create_dir_all(db_path)?;
            super::merge_metadata_atomic(db_path, |metadata| {
                metadata.insert("storage_backend".into(), "postgres".into());
                metadata.insert("storage_repo".into(), repo.into());
            })?;
        }
        Ok(store)
    }

    pub fn local(&self) -> Option<&VectorStore> {
        match self {
            Self::Local(store) => Some(store),
            #[cfg(feature = "pgvector")]
            Self::Postgres { .. } => None,
        }
    }

    pub fn local_mut(&mut self) -> Option<&mut VectorStore> {
        match self {
            Self::Local(store) => Some(store),
            #[cfg(feature = "pgvector")]
            Self::Postgres { .. } => None,
        }
    }

    pub fn backend(&self) -> &'static str {
        match self {
            Self::Local(_) => "local",
            #[cfg(feature = "pgvector")]
            Self::Postgres { .. } => "postgres",
        }
    }

    pub async fn search(
        &self,
        embedding: &[f32],
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        match self {
            Self::Local(store) => {
                let results = store.search(embedding, limit)?;
                // Local filtering stays at the caller's existing normalized-path boundary.
                let _ = prefix;
                Ok(results)
            }
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => {
                store
                    .search(std::slice::from_ref(repo), embedding, prefix, limit)
                    .await
            }
        }
    }

    pub async fn get_chunk(&self, id: u32) -> Result<Option<ChunkMetadata>> {
        match self {
            Self::Local(store) => store.get_chunk(id),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.get_chunk(repo, id).await,
        }
    }

    pub async fn get_embedding(&self, id: u32) -> Result<Option<Vec<f32>>> {
        match self {
            Self::Local(store) => store.get_embedding(id),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.get_embedding(repo, id).await,
        }
    }

    pub async fn get_chunk_as_result(&self, id: u32) -> Result<Option<SearchResult>> {
        if let Some(store) = self.local() {
            return store.get_chunk_as_result(id);
        }
        Ok(self
            .get_chunk(id)
            .await?
            .map(|chunk| chunk.into_result(id, 1.0, None)))
    }

    pub async fn replace_file(
        &mut self,
        path: &str,
        chunks: Vec<EmbeddedChunk>,
    ) -> Result<Vec<u32>> {
        match self {
            Self::Local(store) => {
                let ids = store.get_chunks_by_file()?.remove(path).unwrap_or_default();
                store.delete_chunks(&ids)?;
                store.insert_chunks_with_ids(chunks)
            }
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.replace_file(repo, path, &chunks).await,
        }
    }

    pub async fn delete_file(&mut self, path: &str) -> Result<()> {
        match self {
            Self::Local(store) => {
                let ids = store.get_chunks_by_file()?.remove(path).unwrap_or_default();
                store.delete_chunks(&ids)?;
                Ok(())
            }
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.delete_file(repo, path).await,
        }
    }

    pub fn validate_embedding_space(&self, model: &str, dimensions: usize) -> Result<()> {
        match self {
            Self::Local(_) => {
                let _ = (model, dimensions);
                Ok(())
            }
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, .. } => store.validate_embedding_space(model, dimensions),
        }
    }

    pub async fn clear(&mut self) -> Result<()> {
        match self {
            Self::Local(store) => store.clear(),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.delete_repo(repo).await,
        }
    }

    pub async fn stats(&self) -> Result<StoreStats> {
        match self {
            Self::Local(store) => store.stats(),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.stats(repo).await,
        }
    }

    pub async fn build_index(&mut self) -> Result<()> {
        match self {
            Self::Local(store) => store.build_index(),
            #[cfg(feature = "pgvector")]
            Self::Postgres { .. } => Ok(()),
        }
    }

    pub async fn chunks_for_file(&self, path: &str) -> Result<Vec<super::store::ChunkMeta>> {
        if let Some(store) = self.local() {
            return store.chunks_for_file(path);
        }
        let normalized = crate::cache::normalize_path_str(path);
        let mut by_key = std::collections::HashMap::new();
        for (id, chunk) in self.iter_all_chunks().await? {
            if crate::cache::normalize_path_str(&chunk.path) != normalized {
                continue;
            }
            let key = (
                chunk.kind.clone(),
                chunk.signature.clone(),
                if chunk.signature.is_some() {
                    (0, 0)
                } else {
                    (chunk.start_line, chunk.end_line)
                },
            );
            by_key.insert(
                key,
                super::store::ChunkMeta {
                    id,
                    kind: chunk.kind,
                    signature: chunk.signature,
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                },
            );
        }
        let mut chunks: Vec<_> = by_key.into_values().collect();
        chunks.sort_by_key(|chunk| chunk.start_line);
        Ok(chunks)
    }

    pub async fn index_health(&self) -> Result<(usize, bool)> {
        match self {
            Self::Local(store) => store.index_health(),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => Ok((store.stats(repo).await?.total_chunks, true)),
        }
    }

    pub async fn delete_chunks(&mut self, ids: &[u32]) -> Result<usize> {
        match self {
            Self::Local(store) => store.delete_chunks(ids),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.delete_chunks(repo, ids).await,
        }
    }

    pub async fn insert_chunks_with_ids(&mut self, chunks: Vec<EmbeddedChunk>) -> Result<Vec<u32>> {
        if let Some(store) = self.local_mut() {
            return store.insert_chunks_with_ids(chunks);
        }
        let mut grouped = std::collections::BTreeMap::<String, Vec<(usize, EmbeddedChunk)>>::new();
        let count = chunks.len();
        for (index, chunk) in chunks.into_iter().enumerate() {
            grouped
                .entry(chunk.chunk.path.clone())
                .or_default()
                .push((index, chunk));
        }
        let mut ids = vec![0; count];
        for (path, entries) in grouped {
            let (positions, chunks): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
            let assigned = self.replace_file(&path, chunks).await?;
            for (position, id) in positions.into_iter().zip(assigned) {
                ids[position] = id;
            }
        }
        Ok(ids)
    }

    pub async fn get_chunks_by_file(&self) -> Result<std::collections::HashMap<String, Vec<u32>>> {
        let mut files = std::collections::HashMap::<String, Vec<u32>>::new();
        for (id, chunk) in self.iter_all_chunks().await? {
            files.entry(chunk.path).or_default().push(id);
        }
        Ok(files)
    }

    pub async fn iter_all_chunks(&self) -> Result<Vec<(u32, ChunkMetadata)>> {
        match self {
            Self::Local(store) => store.iter_all_chunks(),
            #[cfg(feature = "pgvector")]
            Self::Postgres { store, repo } => store.file_chunks(repo, None).await,
        }
    }
}
