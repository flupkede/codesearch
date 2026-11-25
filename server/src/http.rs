use crate::{
    Error, FileState, FileUpdate, Repository, RepositoryInput, SearchHit, SearchRequest, Store,
    MAX_FILE_BYTES,
};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

pub fn router(store: Store) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async {
                Json(serde_json::json!({"status":"up", "authentication":"none", "stage":2}))
            }),
        )
        .route("/repositories", get(repositories).post(register))
        .route("/repositories/{name}/files", get(files).put(update_file))
        .route("/repositories/{name}/file", get(file))
        .route("/search", post(search))
        .layer(DefaultBodyLimit::max(MAX_FILE_BYTES * 2))
        .with_state(store)
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Conflict => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Embedding => StatusCode::BAD_GATEWAY,
            Self::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        // Database messages can echo bound source and infrastructure details.
        // Preserve the error category, never leak the underlying diagnostic.
        (status, Json(serde_json::json!({"error":self.to_string()}))).into_response()
    }
}
async fn register(
    State(store): State<Store>,
    Json(input): Json<RepositoryInput>,
) -> Result<Json<Repository>, Error> {
    store.register(input).await.map(Json)
}
async fn repositories(State(store): State<Store>) -> Result<Json<Vec<Repository>>, Error> {
    store.repositories().await.map(Json)
}
#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    after: String,
    #[serde(default = "page_limit")]
    limit: i64,
}
fn page_limit() -> i64 {
    100
}
async fn files(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<FileState>>, Error> {
    store.files(&name, &page.after, page.limit).await.map(Json)
}
#[derive(Deserialize)]
struct FileQuery {
    path: String,
}
async fn file(
    State(store): State<Store>,
    Path(name): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<Option<FileState>>, Error> {
    store.file(&name, &query.path).await.map(Json)
}
async fn update_file(
    State(store): State<Store>,
    Path(name): Path<String>,
    Json(input): Json<FileUpdate>,
) -> Result<Json<FileState>, Error> {
    store.update_file(&name, input).await.map(Json)
}
async fn search(
    State(store): State<Store>,
    Json(input): Json<SearchRequest>,
) -> Result<Json<Vec<SearchHit>>, Error> {
    store.search(input).await.map(Json)
}
