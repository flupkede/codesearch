use super::*;

pub(super) struct TestDatabase {
    admin: sqlx::PgPool,
    pool: sqlx::PgPool,
    name: String,
    url: String,
}
impl TestDatabase {
    pub(super) async fn new() -> Self {
        let options: sqlx::postgres::PgConnectOptions = std::env::var("CODESEARCH_TEST_DATABASE_URL")
            .expect("set disposable CODESEARCH_TEST_DATABASE_URL")
            .parse()
            .unwrap();
        let admin = sqlx::PgPool::connect_with(options.clone()).await.unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let name = format!("cs_stage2_{}_{}_{}", std::process::id(), nonce, sequence);
        // Only this freshly generated database is ever modified/dropped.
        eprintln!("isolated regression database: {name}");
        admin.execute(format!("CREATE DATABASE {name}").as_str()).await.unwrap();
        let options = options.database(&name);
        let pool = sqlx::PgPool::connect_with(options.clone()).await.unwrap();
        pool.execute("CREATE EXTENSION IF NOT EXISTS vector; CREATE EXTENSION IF NOT EXISTS pg_search CASCADE").await.unwrap();
        Self { admin, pool, name, url: options.to_url_lossy().to_string() }
    }
    pub(super) async fn store(&self, config: &EmbedConfig) -> Store {
        Store::connect(&self.url, Embedding::new(config.clone(), ModelType::default()).unwrap()).await.unwrap()
    }
    pub(super) async fn cleanup(self) {
        self.pool.close().await;
        self.admin.execute(format!("DROP DATABASE {} WITH (FORCE)", self.name).as_str()).await.unwrap();
        self.admin.close().await;
    }
}
pub(super) async fn boundary(state: &Simulated) -> Service {
    serve(axum::Router::new().route("/v1/embeddings", post(embeddings)).with_state(state.clone())).await
}
async fn register(client: &reqwest::Client, service: &Service, name: &str) {
    request(client, service, reqwest::Method::POST, "/repositories", json!({"name":name,"description":"regression"}), 200).await;
}
async fn publish(client: &reqwest::Client, service: &Service, repo: &str, revision: i64, source: &str) -> Value {
    let mut body = update(revision, Some(source));
    body["path"] = json!("src/example.rs");
    request(client, service, reqwest::Method::PUT, &format!("/repositories/{repo}/files"), body, 200).await
}

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search 0.27.1 + vector and CREATEDB"]
async fn long_function_rename_embeds_final_slices_and_reuses_identical_text() {
    let db = TestDatabase::new().await;
    let state = Simulated::default();
    let boundary = boundary(&state).await;
    let config = config(&boundary.url);
    let service = serve(router(db.store(&config).await)).await;
    let client = reqwest::Client::new();
    register(&client, &service, "first").await;
    register(&client, &service, "second").await;
    let body = (0..180).map(|i| format!("    let value_{i} = {i};\n")).collect::<String>();
    let original = format!("fn originallong() {{\n{body}}}");
    let renamed = original.replace("originallong", "renamedlong");
    let first = publish(&client, &service, "first", 0, &original).await;
    let count = first["chunk_count"].as_u64().unwrap() as usize;
    assert!(count >= 3, "must exercise shared middle/tail slices");
    state.inputs.lock().unwrap().clear();
    publish(&client, &service, "first", 1, &renamed).await;
    let inputs = state.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), count, "each changed final signature-bearing slice needs fresh inference; inputs: {inputs:?}");
    assert!(inputs.iter().all(|text| text.contains("renamedlong")));
    for input in &inputs {
        let text = input.strip_prefix("document:").unwrap();
        let cached: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM cs_server_embedding_cache WHERE content_hash=$1)")
            .bind(codesearch_core::chunker::Chunk::compute_hash(text))
            .fetch_one(&db.pool).await.unwrap();
        assert!(cached, "cache identity hashes exactly the representation supplied to provider");
    }
    let hits = search(&client, &service, &["first"], "renamedlong", "vector", None).await;
    assert_eq!(hits.len(), count);
    assert!(hits.iter().all(|hit| hit["score"].as_f64().unwrap() > 0.999), "persisted vectors must match renamed inputs: {hits:?}");
    let before = state.documents.load(Ordering::SeqCst);
    state.fail.store(true, Ordering::SeqCst);
    publish(&client, &service, "second", 0, &renamed).await;
    assert_eq!(state.documents.load(Ordering::SeqCst), before, "identical representations reuse cache across repositories");
    drop(service);
    drop(boundary);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search 0.27.1 + vector and CREATEDB"]
async fn extracted_documentation_is_searchable_embedded_and_persisted() {
    let db = TestDatabase::new().await;
    let state = Simulated::default();
    let boundary = boundary(&state).await;
    let config = config(&boundary.url);
    let service = serve(router(db.store(&config).await)).await;
    let client = reqwest::Client::new();
    register(&client, &service, "docs").await;
    publish(&client, &service, "docs", 0, "/// quasarneedle describes the hidden behavior\nfn example() {}\n").await;
    let hits = search(&client, &service, &["docs"], "quasarneedle", "keyword", None).await;
    assert_eq!(hits.len(), 1, "term appears only in extracted preceding documentation");
    assert_eq!(hits[0]["content"], "fn example() {}");
    assert_eq!(hits[0]["start_line"], 2, "source bounds must still describe the AST code, not synthetic documentation");
    assert_eq!(hits[0]["end_line"], 2);
    let inputs = state.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].matches("quasarneedle").count(), 1);
    let document: String = sqlx::query_scalar("SELECT document FROM cs_server_chunks")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(inputs[0].strip_prefix("document:").unwrap(), document, "BM25 persistence and provider input share one representation");
    drop(service);
    let service = serve(router(db.store(&config).await)).await;
    state.fail.store(true, Ordering::SeqCst);
    let persisted = search(&client, &service, &["docs"], "quasarneedle", "keyword", None).await;
    assert_eq!(persisted.len(), 1, "documentation survives a new Store/provider/router");
    publish(&client, &service, "docs", 1, "/// quasarneedle describes the hidden behavior\nfn example() {}\n").await;
    state.fail.store(false, Ordering::SeqCst);
    let vectors = search(&client, &service, &["docs"], "quasarneedle", "vector", None).await;
    assert!(vectors[0]["score"].as_f64().unwrap() > 0.999, "documentation reached the provider and persisted vector");
    // Documentation-only edits must invalidate the representation, without changing source code.
    let before = state.documents.load(Ordering::SeqCst);
    publish(&client, &service, "docs", 2, "/// changedneedle describes new behavior\nfn example() {}\n").await;
    assert_eq!(state.documents.load(Ordering::SeqCst), before + 1);
    assert!(search(&client, &service, &["docs"], "quasarneedle", "keyword", None).await.is_empty());
    assert_eq!(search(&client, &service, &["docs"], "changedneedle", "keyword", None).await.len(), 1);
    drop(service);
    drop(boundary);
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search 0.27.1 + vector and CREATEDB"]
async fn prior_document_representation_is_refused_without_mutation() {
    let db = TestDatabase::new().await;
    let state = Simulated::default();
    let boundary = boundary(&state).await;
    let mut config = config(&boundary.url);
    config.embed_dimensions = Some(3);
    let legacy = config.fingerprint();
    db.pool.execute("CREATE TABLE cs_server_index (singleton boolean PRIMARY KEY CHECK(singleton), schema_version integer NOT NULL, fingerprint text NOT NULL, dimensions integer NOT NULL); CREATE TABLE retained_marker (value text); INSERT INTO retained_marker VALUES ('do not destroy')").await.unwrap();
    sqlx::query("INSERT INTO cs_server_index VALUES (true,1,$1,3)").bind(&legacy).execute(&db.pool).await.unwrap();
    let error = Store::connect(&db.url, Embedding::new(config, ModelType::default()).unwrap()).await.err().expect("old representation must require fresh database/reindex");
    assert!(error.to_string().contains("mismatch"));
    let recorded: String = sqlx::query_scalar("SELECT fingerprint FROM cs_server_index").fetch_one(&db.pool).await.unwrap();
    assert_eq!(recorded, legacy);
    let marker: String = sqlx::query_scalar("SELECT value FROM retained_marker").fetch_one(&db.pool).await.unwrap();
    assert_eq!(marker, "do not destroy");
    let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname='public' AND tablename LIKE 'cs_server_%'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(tables, 1, "failed startup does not migrate/create index tables");
    drop(boundary);
    db.cleanup().await;
}
