use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_RESULTS: usize = 100;
pub const MAX_CHUNKS: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("revision conflict; read current state before retrying")]
    Conflict,
    #[error("repository not found")]
    NotFound,
    #[error("embedding provider failed")]
    Embedding,
    #[error("storage operation failed")]
    Storage(#[from] sqlx::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInput {
    pub name: String,
    pub description: String,
}
impl RepositoryInput {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        validate_text(&self.description, 2048, true)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, sqlx::FromRow)]
pub struct Repository {
    pub name: String,
    pub description: String,
    pub revision: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileUpdate {
    pub path: String,
    /// Zero means never indexed. Deletions retain a revisioned tombstone.
    pub expected_revision: i64,
    /// Caller-supplied source identifier, not a repository-wide snapshot claim.
    pub source_revision: String,
    /// Null deletes the file; an empty string indexes an empty file.
    pub content: Option<String>,
}
impl FileUpdate {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.path, false)?;
        if !(0..i64::MAX).contains(&self.expected_revision) {
            return Err(Error::Invalid("invalid expected revision"));
        }
        validate_text(&self.source_revision, 256, false)?;
        if let Some(content) = &self.content {
            validate_text(content, MAX_FILE_BYTES, true)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, sqlx::FromRow)]
pub struct FileState {
    pub path: String,
    pub revision: i64,
    pub source_revision: String,
    pub content_hash: Option<String>,
    pub deleted: bool,
    pub chunk_count: i32,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Keyword,
    Vector,
    Hybrid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub repositories: Vec<String>,
    pub query: String,
    pub mode: SearchMode,
    pub limit: usize,
    pub path_prefix: Option<String>,
}
impl SearchRequest {
    pub fn validate(&self) -> Result<()> {
        if self.repositories.is_empty() || self.repositories.len() > 32 {
            return Err(Error::Invalid("provide 1 to 32 explicit repositories"));
        }
        for name in &self.repositories {
            validate_name(name)?;
        }
        validate_text(&self.query, 4096, false)?;
        if !(1..=MAX_RESULTS).contains(&self.limit) {
            return Err(Error::Invalid("limit must be 1 to 100"));
        }
        if let Some(prefix) = &self.path_prefix {
            validate_path(prefix, true)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, sqlx::FromRow)]
pub struct SearchHit {
    pub id: i64,
    pub repository: String,
    pub path: String,
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
    pub file_revision: i64,
    pub source_revision: String,
    pub score: f64,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(Error::Invalid(
            "repository name must be 1 to 128 ASCII letters, digits, hyphens, underscores or dots",
        ));
    }
    Ok(())
}
fn validate_text(value: &str, max: usize, empty: bool) -> Result<()> {
    if value.len() > max || value.contains('\0') || (!empty && value.trim().is_empty()) {
        return Err(Error::Invalid("text is empty, too large or contains NUL"));
    }
    Ok(())
}
/// The API accepts only canonical, relative slash-separated paths. Rejecting
/// aliases rather than rewriting them keeps CAS keys and prefix scopes identical.
pub fn validate_path(path: &str, prefix: bool) -> Result<()> {
    if path.len() > 1024 || path.contains(['\\', ':', '\0']) {
        return Err(Error::Invalid("invalid relative path"));
    }
    let components = if prefix {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };
    if prefix && path.is_empty() {
        return Ok(());
    }
    if components
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(Error::Invalid(
            "path must use canonical relative slash-separated components",
        ));
    }
    Ok(())
}
pub fn validate_vector(vector: &[f32], dimensions: usize) -> Result<()> {
    if vector.len() != dimensions
        || vector.iter().any(|v| !v.is_finite())
        || vector.iter().all(|v| *v == 0.0)
    {
        return Err(Error::Embedding);
    }
    Ok(())
}

pub(crate) fn fuse(
    keyword: Vec<SearchHit>,
    vector: Vec<SearchHit>,
    limit: usize,
) -> Vec<SearchHit> {
    let mut hits: BTreeMap<i64, SearchHit> = BTreeMap::new();
    for list in [keyword, vector] {
        for (rank, mut hit) in list.into_iter().enumerate() {
            let score = 1.0 / (60.0 + rank as f64 + 1.0);
            hits.entry(hit.id)
                .and_modify(|h| h.score += score)
                .or_insert_with(|| {
                    hit.score = score;
                    hit
                });
        }
    }
    let mut hits: Vec<_> = hits.into_values().collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    hits.truncate(limit);
    hits
}
#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
