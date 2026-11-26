use crate::types::fuse;
use crate::{
    document::Document, Embedding, Error, FileState, FileUpdate, Repository, RepositoryInput, Result, SearchHit,
    SearchMode, SearchRequest, MAX_CHUNKS,
};
use anyhow::{bail, Context};
use codesearch_core::chunker::{Chunk, SemanticChunker};
use codesearch_core::file::Language;
use pgvector::Vector;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{ConnectOptions, PgPool, Row};
use std::{collections::BTreeSet, path::Path, str::FromStr, time::Duration};

fn connection_options(url: &str) -> anyhow::Result<PgConnectOptions> {
    let parsed = url::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid database URL"))?;
    let explicit_tls = parsed.query_pairs().any(|(key, _)| key == "sslmode");
    let options =
        PgConnectOptions::from_str(url).map_err(|_| anyhow::anyhow!("invalid database URL"))?;
    Ok(if explicit_tls {
        options
    } else {
        options.ssl_mode(PgSslMode::VerifyFull)
    }
    .disable_statement_logging())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    embedding: Embedding,
}
impl Store {
    pub async fn connect(url: &str, embedding: Embedding) -> anyhow::Result<Self> {
        // Do not attach the URL, parser error or database connection error: any
        // can contain credentials. SQL statement logging is disabled as well.
        let options = connection_options(url)?;
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout = '30s'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET idle_in_transaction_session_timeout = '30s'")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .map_err(|_| {
                anyhow::anyhow!("cannot connect to PostgreSQL; check host, TLS and authentication")
            })?;
        let store = Self { pool, embedding };
        store.migrate().await.map_err(|error| {
            // SQL errors may echo source, roles or connection details.
            if error.downcast_ref::<sqlx::Error>().is_some() {
                anyhow::anyhow!("PostgreSQL schema initialization failed; verify extensions, schema privileges and database availability")
            } else { error }
        })?;
        Ok(store)
    }
    async fn migrate(&self) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        // Serialize first-start DDL across server instances, not just within one process.
        sqlx::query("SELECT pg_advisory_xact_lock(384002)")
            .execute(&mut *tx)
            .await?;
        let search: Option<String> =
            sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'pg_search'")
                .fetch_optional(&mut *tx)
                .await?;
        let vector: Option<String> =
            sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
                .fetch_optional(&mut *tx)
                .await?;
        if search.as_deref() != Some("0.27.1") {
            bail!("codesearch-server requires pg_search 0.27.1 (with shared_preload_libraries configured); ask the DBA to provision extensions");
        }
        let vector = vector.context(
            "codesearch-server requires pgvector >=0.8; ask the DBA to CREATE EXTENSION vector",
        )?;
        let parts: Vec<_> = vector
            .split('.')
            .map(str::parse::<u32>)
            .collect::<std::result::Result<_, _>>()?;
        if parts.len() < 2 || (parts[0], parts[1]) < (0, 8) {
            bail!("codesearch-server requires pgvector >=0.8");
        }
        sqlx::query("CREATE TABLE IF NOT EXISTS cs_server_index (singleton boolean PRIMARY KEY CHECK(singleton), schema_version integer NOT NULL, fingerprint text NOT NULL, dimensions integer NOT NULL)").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO cs_server_index VALUES (true, 1, $1, $2) ON CONFLICT DO NOTHING")
            .bind(&self.embedding.fingerprint)
            .bind(self.embedding.dimensions as i32)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT schema_version, fingerprint, dimensions FROM cs_server_index WHERE singleton FOR UPDATE").fetch_one(&mut *tx).await?;
        if row.try_get::<i32, _>("schema_version")? != 1 {
            bail!("unsupported server database schema version");
        }
        if row.try_get::<String, _>("fingerprint")? != self.embedding.fingerprint
            || row.try_get::<i32, _>("dimensions")? != self.embedding.dimensions as i32
        {
            bail!("database embedding fingerprint/dimension mismatch; use a separate database and reindex for another embedding strategy");
        }
        sqlx::query("CREATE TABLE IF NOT EXISTS cs_server_repositories (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text UNIQUE NOT NULL, description text NOT NULL, revision bigint NOT NULL DEFAULT 0 CHECK(revision >= 0))").execute(&mut *tx).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS cs_server_files (repo_id bigint NOT NULL REFERENCES cs_server_repositories(id), path text NOT NULL, revision bigint NOT NULL CHECK(revision > 0), source_revision text NOT NULL, content_hash text, deleted boolean NOT NULL, chunk_count integer NOT NULL CHECK(chunk_count >= 0), PRIMARY KEY(repo_id,path))").execute(&mut *tx).await?;
        // Only a range-checked integer is interpolated; all runtime data is bound.
        sqlx::query(&format!("CREATE TABLE IF NOT EXISTS cs_server_chunks (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, repo_id bigint NOT NULL, path text NOT NULL, ordinal integer NOT NULL, content text NOT NULL, document text NOT NULL, start_line integer NOT NULL, end_line integer NOT NULL, file_revision bigint NOT NULL, source_revision text NOT NULL, embedding vector({}) NOT NULL, FOREIGN KEY(repo_id,path) REFERENCES cs_server_files(repo_id,path), UNIQUE(repo_id,path,ordinal))", self.embedding.dimensions)).execute(&mut *tx).await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS cs_server_chunks_search ON cs_server_chunks USING paradedb (id, repo_id, (document::pdb.source_code))").execute(&mut *tx).await?;
        sqlx::query(&format!("CREATE TABLE IF NOT EXISTS cs_server_embedding_cache (content_hash text PRIMARY KEY, embedding vector({}) NOT NULL)", self.embedding.dimensions)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn register(&self, input: RepositoryInput) -> Result<Repository> {
        input.validate()?;
        let row = sqlx::query_as::<_, Repository>("INSERT INTO cs_server_repositories(name,description) VALUES ($1,$2) ON CONFLICT(name) DO UPDATE SET name=EXCLUDED.name WHERE cs_server_repositories.description=EXCLUDED.description RETURNING name,description,revision")
            .bind(&input.name).bind(&input.description).fetch_optional(&self.pool).await?;
        row.ok_or(Error::Conflict)
    }
    pub async fn repositories(&self) -> Result<Vec<Repository>> {
        Ok(sqlx::query_as(
            "SELECT name,description,revision FROM cs_server_repositories ORDER BY name LIMIT 1000",
        )
        .fetch_all(&self.pool)
        .await?)
    }
    pub async fn files(&self, name: &str, after: &str, limit: i64) -> Result<Vec<FileState>> {
        crate::validate_name(name)?;
        if !(1..=1000).contains(&limit) {
            return Err(Error::Invalid("file page limit must be 1 to 1000"));
        }
        let repo = self.repo_id(name).await?;
        Ok(sqlx::query_as("SELECT path,revision,source_revision,content_hash,deleted,chunk_count FROM cs_server_files WHERE repo_id=$1 AND path>$2 ORDER BY path LIMIT $3").bind(repo).bind(after).bind(limit).fetch_all(&self.pool).await?)
    }
    pub async fn file(&self, name: &str, path: &str) -> Result<Option<FileState>> {
        crate::validate_name(name)?;
        crate::validate_path(path, false)?;
        let repo = self.repo_id(name).await?;
        Ok(sqlx::query_as("SELECT path,revision,source_revision,content_hash,deleted,chunk_count FROM cs_server_files WHERE repo_id=$1 AND path=$2").bind(repo).bind(path).fetch_optional(&self.pool).await?)
    }
    async fn repo_id(&self, name: &str) -> Result<i64> {
        sqlx::query_scalar("SELECT id FROM cs_server_repositories WHERE name=$1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)
    }
    pub async fn update_file(&self, name: &str, update: FileUpdate) -> Result<FileState> {
        crate::validate_name(name)?;
        update.validate()?;
        let repo = self.repo_id(name).await?;
        // Reject already-stale requests before inference; the transaction must
        // still recheck after inference to close the concurrent-writer race.
        let current = self
            .file(name, &update.path)
            .await?
            .map_or(0, |f| f.revision);
        if current != update.expected_revision {
            return Err(Error::Conflict);
        }
        let path = update.path.clone();
        let content = update.content.clone();
        let chunks = tokio::task::spawn_blocking(move || -> Result<Vec<Chunk>> {
            let Some(content) = content else {
                return Ok(Vec::new());
            };
            let mut chunker = SemanticChunker::new(80, 6000, 10);
            let chunks = chunker
                .chunk_semantic(
                    Language::from_path(Path::new(&path)),
                    Path::new(&path),
                    &content,
                )
                .map_err(|_| Error::Invalid("cannot chunk source file"))?;
            if chunks.len() > MAX_CHUNKS {
                return Err(Error::Invalid("too many chunks in file"));
            }
            Ok(chunks)
        })
        .await
        .map_err(|_| Error::Invalid("chunking failed"))??;
        let documents: Vec<_> = chunks
            .iter()
            .map(|chunk| Document::new(chunk, self.embedding.document_model))
            .collect();
        let mut vectors = Vec::with_capacity(chunks.len());
        let mut misses = Vec::new();
        let mut miss_indices = Vec::new();
        for (index, document) in documents.iter().enumerate() {
            let cached: Option<Vector> = sqlx::query_scalar(
                "SELECT embedding FROM cs_server_embedding_cache WHERE content_hash=$1",
            )
            .bind(&document.hash)
            .fetch_optional(&self.pool)
            .await?;
            match cached {
                Some(vector) => {
                    let vector = vector.to_vec();
                    crate::validate_vector(&vector, self.embedding.dimensions)?;
                    vectors.push(vector);
                }
                None => {
                    vectors.push(Vec::new());
                    misses.push(document.text.clone());
                    miss_indices.push(index);
                }
            }
        }
        if !misses.is_empty() {
            let embedded = self.embedding.embed(misses, false).await?;
            for (index, vector) in miss_indices.into_iter().zip(embedded) {
                vectors[index] = vector;
            }
        }
        let mut tx = self.pool.begin().await?;
        // Stable repository row serializes file creation, deletion and CAS. The
        // retained file tombstone prevents delete/recreate ABA revision reuse.
        sqlx::query("SELECT id FROM cs_server_repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
        let current: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM cs_server_files WHERE repo_id=$1 AND path=$2")
                .bind(repo)
                .bind(&update.path)
                .fetch_optional(&mut *tx)
                .await?;
        if current.unwrap_or(0) != update.expected_revision {
            return Err(Error::Conflict);
        }
        let revision = update.expected_revision + 1;
        let hash = update.content.as_deref().map(Chunk::compute_hash);
        let state = FileState {
            path: update.path.clone(),
            revision,
            source_revision: update.source_revision.clone(),
            content_hash: hash,
            deleted: update.content.is_none(),
            chunk_count: chunks.len() as i32,
        };
        sqlx::query("INSERT INTO cs_server_files(repo_id,path,revision,source_revision,content_hash,deleted,chunk_count) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(repo_id,path) DO UPDATE SET revision=EXCLUDED.revision,source_revision=EXCLUDED.source_revision,content_hash=EXCLUDED.content_hash,deleted=EXCLUDED.deleted,chunk_count=EXCLUDED.chunk_count")
            .bind(repo).bind(&state.path).bind(revision).bind(&state.source_revision).bind(&state.content_hash).bind(state.deleted).bind(state.chunk_count).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM cs_server_chunks WHERE repo_id=$1 AND path=$2")
            .bind(repo)
            .bind(&state.path)
            .execute(&mut *tx)
            .await?;
        for (ordinal, ((chunk, document), vector)) in chunks.iter().zip(documents).zip(vectors).enumerate() {
            let vector = Vector::from(vector);
            sqlx::query("INSERT INTO cs_server_chunks(repo_id,path,ordinal,content,document,start_line,end_line,file_revision,source_revision,embedding) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                .bind(repo).bind(&state.path).bind(ordinal as i32).bind(&chunk.content).bind(&document.text).bind(chunk.start_line as i32).bind(chunk.end_line as i32).bind(revision).bind(&state.source_revision).bind(&vector).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO cs_server_embedding_cache(content_hash,embedding) VALUES ($1,$2) ON CONFLICT DO NOTHING").bind(&document.hash).bind(&vector).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE cs_server_repositories SET revision=revision+1 WHERE id=$1")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(state)
    }
    pub async fn search(&self, request: SearchRequest) -> Result<Vec<SearchHit>> {
        request.validate()?;
        let vector = if request.mode != SearchMode::Keyword {
            Some(Vector::from(
                self.embedding
                    .embed(vec![request.query.clone()], true)
                    .await?
                    .remove(0),
            ))
        } else {
            None
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let names: Vec<_> = request
            .repositories
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM cs_server_repositories WHERE name=ANY($1)")
                .bind(&names)
                .fetch_all(&mut *tx)
                .await?;
        if ids.len() != names.len() {
            return Err(Error::NotFound);
        }
        let candidates = if request.mode == SearchMode::Hybrid {
            (request.limit * 4).min(400)
        } else {
            request.limit
        } as i64;
        let keyword = if request.mode != SearchMode::Vector {
            // pg_search scoring requires a ParadeDB scan; PostgreSQL's generic
            // prepared plan can omit it after repeated executions. Keep bound
            // queries but plan this transaction using their actual parameters.
            sqlx::query("SET LOCAL plan_cache_mode=force_custom_plan")
                .execute(&mut *tx)
                .await?;
            sqlx::query_as::<_, SearchHit>("SELECT c.id,r.name AS repository,c.path,c.content,c.start_line,c.end_line,c.file_revision,c.source_revision,pdb.score(c.id)::double precision AS score FROM cs_server_chunks c JOIN cs_server_repositories r ON r.id=c.repo_id WHERE c.document ||| $1::text AND c.repo_id=ANY($2::bigint[]) AND ($3::text IS NULL OR left(c.path,length($3))=$3) ORDER BY pdb.score(c.id) DESC,c.id LIMIT $4")
                .bind(&request.query).bind(&ids).bind(&request.path_prefix).bind(candidates).fetch_all(&mut *tx).await?
        } else {
            Vec::new()
        };
        // Exact cosine ranking intentionally favors correct bounded scope over
        // ANN underfill; no filtered approximate-index completeness claim.
        let vector = if let Some(vector) = vector {
            sqlx::query_as::<_, SearchHit>("SELECT c.id,r.name AS repository,c.path,c.content,c.start_line,c.end_line,c.file_revision,c.source_revision,1-(c.embedding <=> $1) AS score FROM cs_server_chunks c JOIN cs_server_repositories r ON r.id=c.repo_id WHERE c.repo_id=ANY($2::bigint[]) AND ($3::text IS NULL OR left(c.path,length($3))=$3) ORDER BY c.embedding <=> $1,c.id LIMIT $4")
                .bind(vector).bind(&ids).bind(&request.path_prefix).bind(candidates).fetch_all(&mut *tx).await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        Ok(match request.mode {
            SearchMode::Keyword => keyword,
            SearchMode::Vector => vector,
            SearchMode::Hybrid => fuse(keyword, vector, request.limit),
        })
    }
}
