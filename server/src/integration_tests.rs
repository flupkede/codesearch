//! Real pg_search/pgvector + HTTP server qualification. Embeddings below are
//! deliberately simulated at the OpenAI HTTP boundary, not inference quality.
use super::*;
use axum::{extract::State, http::StatusCode, routing::post, Json};
use codesearch_core::embed::{EmbedConfig, ModelType};
use serde_json::{json, Value};
use sqlx::{Connection, Executor};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone, Default)]
struct Simulated {
    fail: Arc<AtomicBool>,
    documents: Arc<AtomicUsize>,
    queries: Arc<AtomicUsize>,
}
async fn embeddings(
    State(state): State<Simulated>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if state.fail.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"simulated failure"})),
        );
    }
    let data: Vec<_> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let text = text.as_str().unwrap();
            if text.starts_with("query:") {
                state.queries.fetch_add(1, Ordering::SeqCst);
            } else {
                assert!(text.starts_with("document:"));
                state.documents.fetch_add(1, Ordering::SeqCst);
            }
            let vector = if text.contains("orchard") {
                vec![1.0, 0.1, 0.1]
            } else {
                vec![0.1, 1.0, 0.1]
            };
            json!({"index":index,"embedding":vector})
        })
        .collect();
    (StatusCode::OK, Json(json!({"data":data})))
}
struct Service {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: axum::Router) -> Service {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    Service {
        url,
        task: tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    }
}
fn config(url: &str) -> EmbedConfig {
    EmbedConfig {
        embed_provider: Some("openai".into()),
        embed_base_url: Some(format!("{url}/v1")),
        model: "simulated-not-inference".into(),
        embed_dimensions: Some(3),
        embed_query_prefix: "query:".into(),
        embed_document_prefix: "document:".into(),
        ..Default::default()
    }
}
async fn request(
    client: &reqwest::Client,
    service: &Service,
    method: reqwest::Method,
    path: &str,
    body: Value,
    status: u16,
) -> Value {
    let response = client
        .request(method, format!("{}{path}", service.url))
        .json(&body)
        .send()
        .await
        .unwrap();
    let actual = response.status().as_u16();
    let body = response.text().await.unwrap();
    assert_eq!(actual, status, "{path}: {body}");
    serde_json::from_str(&body).unwrap()
}
fn update(revision: i64, content: Option<&str>) -> Value {
    json!({"path":"src/a.txt","expected_revision":revision,"source_revision":format!("commit-{revision}"),"content":content})
}
async fn search(
    client: &reqwest::Client,
    service: &Service,
    repositories: &[&str],
    query: &str,
    mode: &str,
    prefix: Option<&str>,
) -> Vec<Value> {
    request(client, service, reqwest::Method::POST, "/search", json!({"repositories":repositories,"query":query,"mode":mode,"limit":10,"path_prefix":prefix}), 200).await.as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires CODESEARCH_TEST_DATABASE_URL to a disposable pg_search 0.27.1 + pgvector instance with CREATEDB"]
async fn server_http_postgres_transaction_cache_and_recovery() {
    let admin_url = std::env::var("CODESEARCH_TEST_DATABASE_URL")
        .expect("set disposable CODESEARCH_TEST_DATABASE_URL");
    let admin_options: sqlx::postgres::PgConnectOptions = admin_url.parse().unwrap();
    let admin = sqlx::PgPool::connect_with(admin_options.clone())
        .await
        .unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let database = format!("cs_stage2_{}_{}", std::process::id(), nonce);
    // Generated ASCII identifier only; never interpolate caller input.
    admin
        .execute(format!("CREATE DATABASE {database}").as_str())
        .await
        .unwrap();
    let options = admin_options.database(&database);
    let pool = sqlx::PgPool::connect_with(options.clone()).await.unwrap();
    pool.execute("CREATE EXTENSION vector; CREATE EXTENSION pg_search CASCADE")
        .await
        .unwrap();
    // Serialize/encode credentials through URL, never print them.
    let database_url = options.to_url_lossy().to_string();
    let simulated = Simulated::default();
    let boundary = serve(
        axum::Router::new()
            .route("/v1/embeddings", post(embeddings))
            .with_state(simulated.clone()),
    )
    .await;
    let config = config(&boundary.url);
    let embedding = Embedding::new(config.clone(), ModelType::default()).unwrap();
    let store = Store::connect(&database_url, embedding).await.unwrap();
    let service = serve(router(store.clone())).await;
    let client = reqwest::Client::new();
    for name in ["first", "second"] {
        request(
            &client,
            &service,
            reqwest::Method::POST,
            "/repositories",
            json!({"name":name,"description":"isolated test corpus"}),
            200,
        )
        .await;
    }
    let put = reqwest::Method::PUT;
    let first = request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(0, Some("orchard ancientterm")),
        200,
    )
    .await;
    assert_eq!(first["revision"], 1);
    assert_eq!(
        first["content_hash"].as_str().unwrap(),
        codesearch_core::chunker::Chunk::compute_hash("orchard ancientterm")
    );
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/second/files",
        update(0, Some("orchard ancientterm")),
        200,
    )
    .await;
    assert_eq!(
        simulated.documents.load(Ordering::SeqCst),
        1,
        "second repository uses PostgreSQL cache"
    );
    for mode in ["keyword", "vector", "hybrid"] {
        let hits = search(&client, &service, &["first"], "orchard", mode, Some("src/")).await;
        assert!(!hits.is_empty(), "{mode}");
        assert!(hits.iter().all(|h| h["repository"] == "first"));
        assert!(search(
            &client,
            &service,
            &["first"],
            "orchard",
            mode,
            Some("elsewhere/")
        )
        .await
        .is_empty());
        assert_eq!(
            search(
                &client,
                &service,
                &["first", "second"],
                "orchard",
                mode,
                None
            )
            .await
            .len(),
            2
        );
    }
    assert!(
        simulated.queries.load(Ordering::SeqCst) >= 1,
        "query prefix semantics reached real provider boundary"
    );
    request(
        &client,
        &service,
        reqwest::Method::POST,
        "/search",
        json!({"repositories":["missing"],"query":"orchard","mode":"keyword","limit":10}),
        404,
    )
    .await;
    request(
        &client,
        &service,
        reqwest::Method::POST,
        "/search",
        json!({"repositories":[],"query":"orchard","mode":"keyword","limit":10}),
        400,
    )
    .await;
    let mut invalid = update(1, Some("bad"));
    invalid["path"] = json!("../a.txt");
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        invalid,
        400,
    )
    .await;
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(0, None),
        409,
    )
    .await;
    simulated.fail.store(true, Ordering::SeqCst);
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(1, Some("new uncached failed source")),
        502,
    )
    .await;
    assert_eq!(
        search(
            &client,
            &service,
            &["first"],
            "ancientterm",
            "keyword",
            None
        )
        .await
        .len(),
        1
    );
    request(
        &client,
        &service,
        reqwest::Method::POST,
        "/search",
        json!({"repositories":["first"],"query":"orchard","mode":"hybrid","limit":10}),
        502,
    )
    .await;
    simulated.fail.store(false, Ordering::SeqCst);
    // Inject failure after DELETE inside the actual application transaction.
    pool.execute("CREATE FUNCTION cs_test_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.content LIKE '%faulttoken%' THEN RAISE EXCEPTION 'injected insert failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER cs_test_fault BEFORE INSERT ON cs_server_chunks FOR EACH ROW EXECUTE FUNCTION cs_test_fault()").await.unwrap();
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(1, Some("faulttoken")),
        503,
    )
    .await;
    assert_eq!(
        search(
            &client,
            &service,
            &["first"],
            "ancientterm",
            "keyword",
            None
        )
        .await
        .len(),
        1
    );
    let state = request(
        &client,
        &service,
        reqwest::Method::GET,
        "/repositories/first/file?path=src%2Fa.txt",
        Value::Null,
        200,
    )
    .await;
    assert_eq!(state["revision"], 1);
    assert_eq!(
        pool.fetch_one(
            "SELECT count(*) FROM cs_server_embedding_cache WHERE content_hash='not-a-hash'"
        )
        .await
        .unwrap()
        .len(),
        1
    );
    // Hold the insertion after DELETE; a separate reader must see complete old
    // rows, and killing the writer connection simulates interrupted publication.
    pool.execute("CREATE FUNCTION cs_test_block() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.content LIKE '%blockedtoken%' THEN PERFORM pg_advisory_xact_lock(384003); END IF; RETURN NEW; END $$; CREATE TRIGGER cs_test_block BEFORE INSERT ON cs_server_chunks FOR EACH ROW EXECUTE FUNCTION cs_test_block()").await.unwrap();
    let mut lock = sqlx::PgConnection::connect_with(&options).await.unwrap();
    lock.execute("SELECT pg_advisory_lock(384003)")
        .await
        .unwrap();
    let writer_client = client.clone();
    let writer_url = format!("{}/repositories/first/files", service.url);
    let writer = tokio::spawn(async move {
        writer_client
            .put(writer_url)
            .json(&update(1, Some("blockedtoken")))
            .send()
            .await
            .unwrap()
            .status()
    });
    let mut blocked = false;
    for _ in 0..100 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory'").fetch_one(&pool).await.unwrap();
        if count > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        blocked,
        "application writer reached post-delete blocking insert"
    );
    for mode in ["keyword", "vector", "hybrid"] {
        let hits = search(&client, &service, &["first"], "orchard", mode, None).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["file_revision"], 1);
        assert!(hits[0]["content"].as_str().unwrap().contains("ancientterm"));
    }
    pool.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory'").await.unwrap();
    assert_eq!(writer.await.unwrap().as_u16(), 503);
    lock.execute("SELECT pg_advisory_unlock(384003)")
        .await
        .unwrap();
    lock.close().await.unwrap();
    assert_eq!(
        search(
            &client,
            &service,
            &["first"],
            "ancientterm",
            "keyword",
            None
        )
        .await
        .len(),
        1
    );
    // Competing same-revision writers: exactly one commits, one conflicts.
    let a = client
        .put(format!("{}/repositories/first/files", service.url))
        .json(&update(1, Some("replacementtoken alpha")))
        .send();
    let b = client
        .put(format!("{}/repositories/first/files", service.url))
        .json(&update(1, Some("replacementtoken beta")))
        .send();
    let (a, b) = tokio::join!(a, b);
    let mut statuses = vec![a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, vec![200, 409]);
    assert!(search(
        &client,
        &service,
        &["first"],
        "ancientterm",
        "keyword",
        None
    )
    .await
    .is_empty());
    assert_eq!(
        search(
            &client,
            &service,
            &["first"],
            "replacementtoken",
            "keyword",
            None
        )
        .await
        .len(),
        1
    );
    // New service and provider over the same database: no local persisted state.
    drop(service);
    drop(store);
    let restarted = Store::connect(
        &database_url,
        Embedding::new(config.clone(), ModelType::default()).unwrap(),
    )
    .await
    .unwrap();
    let service = serve(router(restarted.clone())).await;
    let before = simulated.documents.load(Ordering::SeqCst);
    simulated.fail.store(true, Ordering::SeqCst);
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(2, Some("orchard ancientterm")),
        200,
    )
    .await;
    assert_eq!(
        simulated.documents.load(Ordering::SeqCst),
        before,
        "cache survived restart"
    );
    simulated.fail.store(false, Ordering::SeqCst);
    for changed in [
        EmbedConfig {
            model: "other-model".into(),
            ..config.clone()
        },
        EmbedConfig {
            embed_dimensions: Some(4),
            ..config.clone()
        },
        EmbedConfig {
            embed_query_prefix: "other:".into(),
            ..config.clone()
        },
    ] {
        let error = Store::connect(
            &database_url,
            Embedding::new(changed, ModelType::default()).unwrap(),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("mismatch"));
    }
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(3, None),
        200,
    )
    .await;
    for mode in ["keyword", "vector", "hybrid"] {
        assert!(search(&client, &service, &["first"], "orchard", mode, None)
            .await
            .is_empty());
    }
    let deleted = request(
        &client,
        &service,
        reqwest::Method::GET,
        "/repositories/first/file?path=src%2Fa.txt",
        Value::Null,
        200,
    )
    .await;
    assert_eq!(deleted["revision"], 4);
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["content_hash"], Value::Null);
    request(
        &client,
        &service,
        put.clone(),
        "/repositories/first/files",
        update(0, Some("stale recreate")),
        409,
    )
    .await;
    request(
        &client,
        &service,
        put,
        "/repositories/first/files",
        update(4, Some("")),
        200,
    )
    .await;
    let empty = request(
        &client,
        &service,
        reqwest::Method::GET,
        "/repositories/first/files?limit=1",
        Value::Null,
        200,
    )
    .await;
    assert_eq!(empty[0]["deleted"], false);
    assert_eq!(empty[0]["chunk_count"], 0);
    assert_eq!(
        search(&client, &service, &["second"], "orchard", "hybrid", None)
            .await
            .len(),
        1
    );
    // A broken index/store must return an HTTP error, never an empty success.
    pool.execute("ALTER TABLE cs_server_chunks RENAME TO cs_test_unavailable")
        .await
        .unwrap();
    request(
        &client,
        &service,
        reqwest::Method::POST,
        "/search",
        json!({"repositories":["first"],"query":"orchard","mode":"keyword","limit":10}),
        503,
    )
    .await;
    drop(service);
    drop(restarted);
    drop(boundary);
    pool.close().await;
    admin
        .execute(format!("DROP DATABASE {database} WITH (FORCE)").as_str())
        .await
        .unwrap();
    admin.close().await;
}
