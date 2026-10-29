use anyhow::{bail, Context, Result};
use pgvector::Vector;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use std::str::FromStr;
use std::time::Duration;

use super::{ChunkMetadata, SearchResult, StoreStats};
use crate::embed::EmbeddedChunk;

/// Shared database connection pool; all repositories use one embedding space.
#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
    dimensions: usize,
    model: String,
}

impl PostgresStore {
    pub fn validate_embedding_space(&self, model: &str, dimensions: usize) -> Result<()> {
        if self.model != model || self.dimensions != dimensions {
            bail!("Postgres model/dimension mismatch: changing models requires resetting and reindexing every repository in this database");
        }
        Ok(())
    }

    pub async fn connect(url: &str, model: &str, dimensions: usize) -> Result<Self> {
        if !(1..=2000).contains(&dimensions) {
            bail!("Postgres cosine HNSW requires an embedding dimension between 1 and 2000; got {dimensions}");
        }
        // Never attach the URL or parser error: either may contain credentials.
        let options = PgConnectOptions::from_str(url)
            .map_err(|_| anyhow::anyhow!("Invalid CODESEARCH_DATABASE_URL"))?;
        let options = if url.contains("sslmode=") {
            options
        } else {
            options.ssl_mode(PgSslMode::VerifyFull)
        };
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .after_connect(|connection, _| Box::pin(async move {
                sqlx::query("SET statement_timeout = '30s'")
                    .execute(connection)
                    .await?;
                Ok(())
            }))
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .context("Cannot connect to Postgres storage (check host, TLS certificates and authentication)")?;
        let store = Self {
            pool,
            dimensions,
            model: model.to_owned(),
        };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&self.pool)
            .await
            .context("Cannot enable pgvector; ask the database administrator to run CREATE EXTENSION vector and grant schema/table creation privileges")?;
        let version: String =
            sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
                .fetch_one(&self.pool)
                .await?;
        let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        if (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) < (0, 8) {
            bail!(
                "Postgres storage requires pgvector 0.8 or later; installed version is {version}"
            );
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS codesearch_index (singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton), schema_version integer NOT NULL, model text NOT NULL, dimensions integer NOT NULL)")
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO codesearch_index (singleton, schema_version, model, dimensions) VALUES (true, 1, $1, $2) ON CONFLICT DO NOTHING")
            .bind(&self.model).bind(self.dimensions as i32).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT schema_version, model, dimensions FROM codesearch_index WHERE singleton FOR UPDATE")
            .fetch_one(&mut *tx).await?;
        let stored_model: String = row.try_get("model")?;
        let stored_dimensions: i32 = row.try_get("dimensions")?;
        if stored_model != self.model || stored_dimensions != self.dimensions as i32 {
            bail!("Postgres model/dimension mismatch: database uses '{stored_model}' ({stored_dimensions} dimensions), requested '{}' ({} dimensions). Changing models requires resetting and reindexing every repository in this database", self.model, self.dimensions);
        }
        if row.try_get::<i32, _>("schema_version")? != 1 {
            bail!("Unsupported Postgres storage schema version");
        }
        sqlx::query("CREATE TABLE IF NOT EXISTS codesearch_repos (repo text PRIMARY KEY, next_chunk_id bigint NOT NULL DEFAULT 0 CHECK (next_chunk_id >= 0))")
            .execute(&mut *tx).await?;
        // Dimensions are validated integers, never user-provided SQL identifiers.
        sqlx::query(&format!("CREATE TABLE IF NOT EXISTS codesearch_chunks (repo text NOT NULL REFERENCES codesearch_repos(repo) ON DELETE CASCADE, chunk_id bigint NOT NULL CHECK (chunk_id BETWEEN 0 AND 4294967295), path text NOT NULL, language text NOT NULL, payload jsonb NOT NULL, embedding vector({}) NOT NULL, PRIMARY KEY (repo, chunk_id))", self.dimensions))
            .execute(&mut *tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS codesearch_chunks_embedding ON codesearch_chunks USING hnsw (embedding vector_cosine_ops)")
            .execute(&mut *tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS codesearch_chunks_repo_path ON codesearch_chunks (repo, path text_pattern_ops)")
            .execute(&mut *tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS codesearch_chunks_path ON codesearch_chunks (path text_pattern_ops)")
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn replace_file(
        &self,
        repo: &str,
        path: &str,
        chunks: &[EmbeddedChunk],
    ) -> Result<Vec<u32>> {
        for chunk in chunks {
            if chunk.chunk.path != path {
                bail!("File replacement contains a chunk from a different path");
            }
            self.validate_embedding(&chunk.embedding)?;
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO codesearch_repos (repo) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        let first: i64 = sqlx::query_scalar("UPDATE codesearch_repos SET next_chunk_id = next_chunk_id + $2 WHERE repo = $1 RETURNING next_chunk_id - $2")
            .bind(repo).bind(chunks.len() as i64).fetch_one(&mut *tx).await?;
        if first + chunks.len() as i64 > u32::MAX as i64 + 1 {
            bail!("Repository chunk ID space exhausted");
        }
        sqlx::query("DELETE FROM codesearch_chunks WHERE repo = $1 AND path = $2")
            .bind(repo)
            .bind(path)
            .execute(&mut *tx)
            .await?;
        let ids: Vec<u32> = (0..chunks.len())
            .map(|i| (first + i as i64) as u32)
            .collect();
        for (batch, batch_ids) in chunks.chunks(1000).zip(ids.chunks(1000)) {
            let mut query = QueryBuilder::<Postgres>::new("INSERT INTO codesearch_chunks (repo, chunk_id, path, language, payload, embedding) ");
            query.push_values(batch.iter().zip(batch_ids), |mut row, (chunk, id)| {
                row.push_bind(repo)
                    .push_bind(i64::from(*id))
                    .push_bind(path)
                    .push_bind(format!(
                        "{:?}",
                        crate::file::Language::from_path(std::path::Path::new(path))
                    ))
                    .push_bind(sqlx::types::Json(ChunkMetadata::from_embedded_chunk(chunk)))
                    .push_bind(Vector::from(chunk.embedding.clone()));
            });
            query.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    fn validate_embedding(&self, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dimensions {
            bail!(
                "Query embedding dimension mismatch: expected {}, got {}",
                self.dimensions,
                embedding.len()
            );
        }
        if embedding.iter().any(|v| !v.is_finite()) || embedding.iter().all(|v| *v == 0.0) {
            bail!("Cosine embeddings must contain finite values and have nonzero norm");
        }
        Ok(())
    }

    pub async fn search(
        &self,
        repos: &[String],
        embedding: &[f32],
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        self.validate_embedding(embedding)?;
        if repos.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let pattern = prefix.map(|p| {
            format!(
                "{}%",
                p.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        });
        let vector = Vector::from(embedding.to_vec());
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL hnsw.iterative_scan = strict_order")
            .execute(&mut *tx)
            .await?;
        let sql = "SELECT repo, chunk_id, payload, embedding <=> $2 AS distance FROM codesearch_chunks WHERE repo = ANY($1) AND ($3::text IS NULL OR path LIKE $3) ORDER BY embedding <=> $2 LIMIT $4";
        let mut rows = sqlx::query(sql)
            .bind(repos)
            .bind(&vector)
            .bind(&pattern)
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await?;
        if rows.len() < limit {
            // Iterative HNSW stops at its scan budget; exact retrieval fills selective filters.
            sqlx::query("SET LOCAL enable_indexscan = off")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL enable_bitmapscan = off")
                .execute(&mut *tx)
                .await?;
            rows = sqlx::query(sql)
                .bind(repos)
                .bind(&vector)
                .bind(&pattern)
                .bind(limit as i64)
                .fetch_all(&mut *tx)
                .await?;
        }
        let results = rows
            .iter()
            .map(|row| {
                let payload: sqlx::types::Json<ChunkMetadata> = row.try_get("payload")?;
                let id: i64 = row.try_get("chunk_id")?;
                let distance: f64 = row.try_get("distance")?;
                Ok(payload
                    .0
                    .into_result(id as u32, distance as f32, Some(row.try_get("repo")?)))
            })
            .collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(results)
    }

    pub async fn get_chunk(&self, repo: &str, id: u32) -> Result<Option<ChunkMetadata>> {
        let payload: Option<sqlx::types::Json<ChunkMetadata>> = sqlx::query_scalar(
            "SELECT payload FROM codesearch_chunks WHERE repo = $1 AND chunk_id = $2",
        )
        .bind(repo)
        .bind(i64::from(id))
        .fetch_optional(&self.pool)
        .await?;
        Ok(payload.map(|p| p.0))
    }

    pub async fn get_embedding(&self, repo: &str, id: u32) -> Result<Option<Vec<f32>>> {
        let vector: Option<Vector> = sqlx::query_scalar(
            "SELECT embedding FROM codesearch_chunks WHERE repo = $1 AND chunk_id = $2",
        )
        .bind(repo)
        .bind(i64::from(id))
        .fetch_optional(&self.pool)
        .await?;
        Ok(vector.map(|v| v.to_vec()))
    }

    pub async fn file_chunks(
        &self,
        repo: &str,
        path: Option<&str>,
    ) -> Result<Vec<(u32, ChunkMetadata)>> {
        let rows = sqlx::query("SELECT chunk_id, payload FROM codesearch_chunks WHERE repo = $1 AND ($2::text IS NULL OR path = $2) ORDER BY chunk_id")
            .bind(repo).bind(path).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                let id: i64 = row.try_get("chunk_id")?;
                let payload: sqlx::types::Json<ChunkMetadata> = row.try_get("payload")?;
                Ok((id as u32, payload.0))
            })
            .collect()
    }

    pub async fn delete_file(&self, repo: &str, path: &str) -> Result<()> {
        sqlx::query("DELETE FROM codesearch_chunks WHERE repo = $1 AND path = $2")
            .bind(repo)
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_chunks(&self, repo: &str, ids: &[u32]) -> Result<usize> {
        let ids: Vec<i64> = ids.iter().copied().map(i64::from).collect();
        let result =
            sqlx::query("DELETE FROM codesearch_chunks WHERE repo = $1 AND chunk_id = ANY($2)")
                .bind(repo)
                .bind(ids)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() as usize)
    }

    pub async fn delete_repo(&self, repo: &str) -> Result<()> {
        sqlx::query("DELETE FROM codesearch_repos WHERE repo = $1")
            .bind(repo)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn stats(&self, repo: &str) -> Result<StoreStats> {
        let row = sqlx::query("SELECT count(*) AS chunks, count(DISTINCT path) AS files, coalesce(max(chunk_id), 0) AS max_id FROM codesearch_chunks WHERE repo = $1")
            .bind(repo).fetch_one(&self.pool).await?;
        Ok(StoreStats {
            total_chunks: row.try_get::<i64, _>("chunks")? as usize,
            total_files: row.try_get::<i64, _>("files")? as usize,
            max_chunk_id: row.try_get::<i64, _>("max_id")? as u32,
            dimensions: self.dimensions,
            indexed: true,
        })
    }
}
