use crate::{
    Error, FileState, FileUpdate, Repository, RepositoryInput, SearchHit, SearchRequest, Store,
    MAX_FILE_BYTES,
};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, uri::Authority, StatusCode},
    middleware::{self, Next},
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
        .layer(middleware::from_fn(loopback_boundary))
        .with_state(store)
}
// Parse an HTTP authority without URL normalization, DNS resolution, userinfo,
// empty ports or ambiguous unbracketed IPv6. Never trust forwarded headers.
fn loopback_authority(value: &str) -> bool {
    let Ok(authority) = value.parse::<Authority>() else {
        return false;
    };
    let host = authority.host();
    let Some(suffix) = value.strip_prefix(host) else {
        return false;
    };
    if !suffix.is_empty() {
        let Some(port) = suffix.strip_prefix(':') else {
            return false;
        };
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|port| port > 0)
        {
            return false;
        }
    }
    let ip = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || ip
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
async fn loopback_boundary(request: Request, next: Next) -> Response {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next();
    let valid_host = match host {
        Some(host) => host.to_str().is_ok_and(loopback_authority) && hosts.next().is_none(),
        None => request.uri().authority().is_some(), // HTTP/2 :authority
    };
    let valid_authority = request
        .uri()
        .authority()
        .is_none_or(|value| loopback_authority(value.as_str()));
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let valid_origin = match origins.next() {
        None => true, // Native clients need not supply Origin.
        Some(origin) => {
            origin.to_str().is_ok_and(|origin| {
                origin
                    .strip_prefix("http://")
                    .or_else(|| origin.strip_prefix("https://"))
                    .is_some_and(loopback_authority)
            }) && origins.next().is_none()
        }
    };
    if !valid_host || !valid_authority || !valid_origin {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"loopback Host and Origin required"})),
        )
            .into_response();
    }
    next.run(request).await
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
