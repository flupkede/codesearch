use super::*;

async fn setup() -> (document_tests::TestDatabase, Service, Service, reqwest::Client) {
    let db = document_tests::TestDatabase::new().await;
    let boundary = document_tests::boundary(&Simulated::default()).await;
    let service = serve(router(db.store(&config(&boundary.url)).await)).await;
    (db, boundary, service, reqwest::Client::new())
}
async fn register(client: &reqwest::Client, service: &Service, name: &str) {
    request(client, service, reqwest::Method::POST, "/repositories", json!({"name":name,"description":"publication regression"}), 200).await;
}
async fn publish(client: &reqwest::Client, service: &Service, repo: &str, path: &str, revision: i64, source: &str, status: u16) -> Value {
    let mut body = update(revision, Some(source));
    body["path"] = json!(path);
    request(client, service, reqwest::Method::PUT, &format!("/repositories/{repo}/files"), body, status).await
}
async fn repositories(client: &reqwest::Client, service: &Service, query: &str, status: u16) -> Value {
    request(client, service, reqwest::Method::GET, &format!("/repositories{query}"), Value::Null, status).await
}

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search/vector and CREATEDB"]
async fn malformed_notebook_preserves_published_file_and_revisions() {
    let (db, boundary, service, client) = setup().await;
    register(&client, &service, "notebook").await;
    let valid = json!({"cells":[{"cell_type":"code","source":["print('orchard notebookneedle')"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}).to_string();
    let published = publish(&client, &service, "notebook", "example.ipynb", 0, &valid, 200).await;
    let before = repositories(&client, &service, "", 200).await;
    for invalid in ["{", "{}", "{\"cells\":null}", "{\"cells\":{}}", "{\"cells\":[{}]}", "{\"cells\":[{\"cell_type\":\"code\",\"source\":[42]}]}"] {
        publish(&client, &service, "notebook", "example.ipynb", 1, invalid, 400).await;
        let current = request(&client, &service, reqwest::Method::GET, "/repositories/notebook/file?path=example.ipynb", Value::Null, 200).await;
        assert_eq!(current, published, "invalid notebook must preserve all file state");
        assert_eq!(repositories(&client, &service, "", 200).await, before);
        for mode in ["keyword", "vector", "hybrid"] {
            let hits = search(&client, &service, &["notebook"], "notebookneedle", mode, None).await;
            assert_eq!(hits.len(), 1, "preserve {mode} hit after {invalid}");
            assert_eq!(hits[0]["file_revision"], 1);
            assert!(hits[0]["content"].as_str().unwrap().contains("notebookneedle"));
        }
    }
    let empty = publish(&client, &service, "notebook", "example.ipynb", 1, "{\"cells\":[]}", 200).await;
    assert_eq!(empty["revision"], 2);
    assert_eq!(empty["chunk_count"], 0);
    assert_eq!(empty["deleted"], false);
    assert_eq!(repositories(&client, &service, "", 200).await[0]["revision"], 2);
    assert!(search(&client, &service, &["notebook"], "notebookneedle", "keyword", None).await.is_empty());
    drop(service); drop(boundary); db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search/vector and CREATEDB"]
async fn split_source_ranges_are_inclusive_physical_lines() {
    let (db, boundary, service, client) = setup().await;
    register(&client, &service, "ranges").await;
    let body: String = (0..180).map(|i| format!("    let value_{i} = {i};\n")).collect();
    let source = format!("fn rangeneedle() {{\n{body}}}");
    publish(&client, &service, "ranges", "example.rs", 0, &source, 200).await;
    let mut hits = search(&client, &service, &["ranges"], "rangeneedle", "keyword", Some("example.rs")).await;
    hits.sort_by_key(|hit| hit["start_line"].as_u64().unwrap());
    let ranges: Vec<_> = hits.iter().map(|hit| (hit["start_line"].as_u64().unwrap(), hit["end_line"].as_u64().unwrap())).collect();
    assert_eq!(ranges, [(1,80),(71,150),(141,182)]);
    let text = "orchard ".repeat(1800);
    publish(&client, &service, "ranges", "one.txt", 0, &text, 200).await;
    let hits = search(&client, &service, &["ranges"], "orchard", "keyword", Some("one.txt")).await;
    assert!(hits.len() >= 3);
    for hit in hits { assert_eq!((hit["start_line"].as_u64(), hit["end_line"].as_u64()), (Some(1),Some(1)), "fragment must stay on physical line1"); }
    drop(service); drop(boundary); db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search/vector and CREATEDB"]
async fn repository_keyset_pagination_discovers_1001_names_once() {
    let (db, boundary, service, client) = setup().await;
    for i in 0..1001 { register(&client, &service, &format!("repo{i:04}")).await; }
    publish(&client, &service, "repo1000", "a.txt", 0, "orchard", 200).await;
    let mut names = Vec::new();
    let mut after = String::new();
    for _ in 0..12 {
        let page = repositories(&client, &service, &format!("?limit=100&after={after}"), 200).await;
        let page = page.as_array().unwrap();
        assert!(page.len() <= 100, "bounded page size");
        if page.is_empty() { break; }
        for repo in page {
            let name = repo["name"].as_str().unwrap();
            assert!(name > after.as_str());
            assert_eq!(repo["revision"], if name == "repo1000" {1} else {0});
            names.push(name.to_string());
            after = name.to_string();
        }
    }
    assert_eq!(names, (0..1001).map(|i| format!("repo{i:04}")).collect::<Vec<_>>());
    for query in ["?limit=0", "?limit=1001", "?limit=-1"] { repositories(&client, &service, query, 400).await; }
    drop(service); drop(boundary); db.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires disposable PG18 with pg_search/vector and CREATEDB"]
async fn global_cache_inserts_sorted_unique_keys_independent_of_chunk_order() {
    let (db, boundary, service, client) = setup().await;
    register(&client, &service, "first").await;
    register(&client, &service, "second").await;
    // A transaction-local trigger deterministically rejects descending or repeated
    // unique-key lock attempts. This pins lock ordering without a timing lottery.
    db.pool.execute("CREATE FUNCTION cs_check_cache_order() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE previous text; BEGIN previous := current_setting('cs_test.previous_hash',true); IF previous IS NOT NULL AND previous <> '' AND NEW.content_hash <= previous THEN RAISE EXCEPTION 'cache keys not strictly ascending'; END IF; PERFORM set_config('cs_test.previous_hash',NEW.content_hash,true); RETURN NEW; END $$; CREATE TRIGGER cs_check_cache_order BEFORE INSERT ON cs_server_embedding_cache FOR EACH ROW EXECUTE FUNCTION cs_check_cache_order()").await.unwrap();
    let a = "fn cache_alpha() {}";
    let b = "fn cache_beta() {}";
    let documents = |source: &str| {
        codesearch_core::chunker::SemanticChunker::new(80,6000,10)
            .chunk_semantic(codesearch_core::file::Language::Rust, std::path::Path::new("same.rs"), source).unwrap()
            .iter().map(|chunk| crate::document::Document::new(chunk, ModelType::default())).collect::<Vec<_>>()
    };
    let pair = documents(&format!("{a}\n{b}"));
    assert_eq!(pair.len(),2);
    let (high, low) = if pair[0].hash > pair[1].hash {(a,b)} else {(b,a)};
    let descending = format!("{high}\n{low}\n{high}");
    let reversed = format!("{low}\n{high}\n{low}");
    let (first, second) = tokio::join!(
        publish(&client, &service, "first", "same.rs", 0, &descending, 200),
        publish(&client, &service, "second", "same.rs", 0, &reversed, 200)
    );
    assert_eq!(first["chunk_count"],3);
    assert_eq!(second["chunk_count"],3);
    for (repo, source) in [("first", descending), ("second", reversed)] {
        let actual: Vec<String> = sqlx::query_scalar("SELECT c.document FROM cs_server_chunks c JOIN cs_server_repositories r ON r.id=c.repo_id WHERE r.name=$1 ORDER BY c.ordinal")
            .bind(repo).fetch_all(&db.pool).await.unwrap();
        assert_eq!(actual, documents(&source).into_iter().map(|doc| doc.text).collect::<Vec<_>>(), "cache sort must not reorder chunk ordinals");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM cs_server_embedding_cache").fetch_one(&db.pool).await.unwrap();
    assert_eq!(count,2);
    let repos = repositories(&client,&service,"",200).await;
    assert!(repos.as_array().unwrap().iter().all(|repo| repo["revision"]==1));
    drop(service); drop(boundary); db.cleanup().await;
}
