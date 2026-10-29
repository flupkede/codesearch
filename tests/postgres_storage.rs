#![cfg(feature = "pgvector")]

use codesearch::vectordb::PostgresStore;
use codesearch::{Chunk, ChunkKind, EmbeddedChunk};
use std::process::Command;

struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Database(String);

impl Drop for Database {
    fn drop(&mut self) {
        let output = Command::new("docker").args(["rm", "-f", &self.0]).output();
        if !matches!(output, Ok(ref result) if result.status.success()) {
            eprintln!(
                "Failed to remove disposable pgvector test container {}",
                self.0
            );
        }
    }
}

impl Database {
    async fn start() -> (Self, String) {
        let name = format!("codesearch-pgvector-{}", uuid::Uuid::new_v4());
        let output = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--rm",
                "--name",
                &name,
                "-e",
                "POSTGRES_HOST_AUTH_METHOD=trust",
                "-p",
                "127.0.0.1::5432",
                "pgvector/pgvector:0.8.0-pg17",
            ])
            .output()
            .expect("Docker is required for pgvector integration tests");
        assert!(
            output.status.success(),
            "container startup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let database = Self(name);
        let output = Command::new("docker")
            .args(["port", &database.0, "5432/tcp"])
            .output()
            .expect("inspect test container port");
        assert!(
            output.status.success(),
            "container port lookup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let address = String::from_utf8(output.stdout).expect("container address");
        let url = format!(
            "postgres://postgres@{}/postgres?sslmode=disable",
            address.trim()
        );
        for _ in 0..120 {
            let output = Command::new("docker")
                .args([
                    "exec",
                    &database.0,
                    "pg_isready",
                    "-h",
                    "127.0.0.1",
                    "-U",
                    "postgres",
                ])
                .output()
                .expect("probe test database");
            if output.status.success() {
                return (database, url);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        panic!("pgvector test database did not become ready");
    }
}

fn chunk(path: &str, content: &str, embedding: [f32; 3]) -> EmbeddedChunk {
    EmbeddedChunk::new(
        Chunk::new(
            content.to_owned(),
            1,
            2,
            ChunkKind::Function,
            path.to_owned(),
        ),
        embedding.to_vec(),
    )
}

#[tokio::test]
async fn postgres_file_lifecycle_and_filtered_search() {
    let (_database, url) = Database::start().await;
    let store = PostgresStore::connect(&url, "test-model", 3).await.unwrap();
    // Opening repeatedly exercises idempotent schema initialization.
    let reopened = PostgresStore::connect(&url, "test-model", 3).await.unwrap();
    let local_state = tempfile::tempdir().unwrap();
    std::fs::write(local_state.path().join("metadata.json"), r#"{"storage_backend":"postgres","storage_repo":"alpha","model_short_name":"test-model","dimensions":3}"#).unwrap();
    assert!(!local_state.path().join("data.mdb").exists());
    let ids = store
        .replace_file(
            "alpha",
            "src/main.rs",
            &[chunk("src/main.rs", "first", [1.0, 0.0, 0.0])],
        )
        .await
        .unwrap();
    store
        .replace_file(
            "beta",
            "lib.rs",
            &[chunk("lib.rs", "other repo", [0.0, 1.0, 0.0])],
        )
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_chunk("alpha", ids[0])
            .await
            .unwrap()
            .unwrap()
            .content,
        "first"
    );
    assert_eq!(
        store.get_embedding("alpha", ids[0]).await.unwrap().unwrap(),
        vec![1.0, 0.0, 0.0]
    );
    let hits = store
        .search(&["alpha".into(), "beta".into()], &[1.0, 0.0, 0.0], None, 2)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].origin.as_deref(), Some("alpha"));
    let hits = store
        .search(&["alpha".into()], &[1.0, 0.0, 0.0], Some("src/"), 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(store
        .search(&["alpha".into()], &[1.0, 0.0, 0.0], Some("lib"), 5)
        .await
        .unwrap()
        .is_empty());
    let replacement = store
        .replace_file(
            "alpha",
            "src/main.rs",
            &[chunk("src/main.rs", "replacement", [0.0, 0.0, 1.0])],
        )
        .await
        .unwrap();
    assert_ne!(ids, replacement);
    assert!(store.get_chunk("alpha", ids[0]).await.unwrap().is_none());
    assert_eq!(store.stats("alpha").await.unwrap().total_chunks, 1);
    let mut backend = codesearch::vectordb::ChunkStore::Postgres {
        store: store.clone(),
        repo: "alpha".into(),
    };
    assert_eq!(
        backend
            .get_chunk_as_result(replacement[0])
            .await
            .unwrap()
            .unwrap()
            .content,
        "replacement"
    );
    assert_eq!(
        backend.chunks_for_file("src/main.rs").await.unwrap().len(),
        1
    );
    assert_eq!(
        backend.get_chunks_by_file().await.unwrap()["src/main.rs"],
        replacement
    );
    assert_eq!(backend.index_health().await.unwrap(), (1, true));
    backend.build_index().await.unwrap();
    // Invalid replacement must preserve the complete previous file.
    let invalid = EmbeddedChunk::new(
        Chunk::new(
            "invalid".into(),
            1,
            1,
            ChunkKind::Function,
            "src/main.rs".into(),
        ),
        vec![1.0, 2.0],
    );
    assert!(store
        .replace_file("alpha", "src/main.rs", &[invalid])
        .await
        .is_err());
    assert_eq!(
        store
            .get_chunk("alpha", replacement[0])
            .await
            .unwrap()
            .unwrap()
            .content,
        "replacement"
    );
    assert!(PostgresStore::connect(&url, "test-model", 4)
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("mismatch"));
    assert!(PostgresStore::connect(&url, "different-model", 3)
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("mismatch"));
    // Missing local publication state must not erase a committed remote file,
    // even when embedding cannot proceed.
    let recovery = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(recovery.path().join("src")).unwrap();
    std::fs::write(recovery.path().join("src/main.rs"), "fn replacement() {}\n").unwrap();
    let recovery_db = recovery.path().join(".codesearch.db");
    let shared = codesearch::index::SharedStores::new(&recovery_db, 3).unwrap();
    *shared.vector_store.write().await = codesearch::vectordb::ChunkStore::Postgres {
        store: store.clone(),
        repo: "alpha".into(),
    };
    std::fs::write(
        recovery_db.join("metadata.json"),
        r#"{"model_short_name":"test-model","dimensions":3}"#,
    )
    .unwrap();
    let result = codesearch::index::IndexManager::perform_incremental_refresh_with_stores(
        recovery.path(),
        &recovery_db,
        &shared,
        &tokio_util::sync::CancellationToken::new(),
        None,
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "unavailable embedding model must fail refresh"
    );
    assert_eq!(
        store
            .get_chunk("alpha", replacement[0])
            .await
            .unwrap()
            .unwrap()
            .content,
        "replacement"
    );

    // Sparse path scopes must fill the requested limit despite closer vectors elsewhere.
    let distractors: Vec<_> = (0..300)
        .map(|_| chunk("bulk.rs", "distractor", [1.0, 0.0, 0.0]))
        .collect();
    store
        .replace_file("noise", "bulk.rs", &distractors)
        .await
        .unwrap();
    let scoped: Vec<_> = (0..12)
        .map(|_| chunk("selected.rs", "scoped", [0.1, 0.9, 0.0]))
        .collect();
    store
        .replace_file("scope", "selected.rs", &scoped)
        .await
        .unwrap();
    let hits = store
        .search(
            &["noise".into(), "scope".into()],
            &[1.0, 0.0, 0.0],
            Some("selected"),
            10,
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 10);
    assert!(hits
        .iter()
        .all(|hit| hit.origin.as_deref() == Some("scope")));
    store.delete_file("alpha", "src/main.rs").await.unwrap();
    assert_eq!(store.stats("alpha").await.unwrap().total_chunks, 0);
    store.delete_repo("beta").await.unwrap();
    assert_eq!(store.stats("beta").await.unwrap().total_chunks, 0);

    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("CREATE DATABASE cli_fixture")
        .execute(&pool)
        .await
        .unwrap();
    let cli_url = url.replace("/postgres?", "/cli_fixture?");
    let cli_store = PostgresStore::connect(&cli_url, "minilm-l6-q", 384)
        .await
        .unwrap();
    let project = tempfile::tempdir().unwrap();
    let db = project.path().join(".codesearch.db");
    std::fs::create_dir_all(&db).unwrap();
    std::fs::write(db.join("metadata.json"), r#"{"schema_version":1,"storage_backend":"postgres","storage_repo":"cli","model_short_name":"minilm-l6-q","dimensions":384,"partial":false}"#).unwrap();
    let ids = cli_store
        .replace_file(
            "cli",
            "src/rule.xml",
            &[EmbeddedChunk::new(
                Chunk::new(
                    "<ref:classification search=\"gadget\" />".into(),
                    1,
                    1,
                    ChunkKind::Block,
                    "src/rule.xml".into(),
                ),
                vec![0.5; 384],
            )],
        )
        .await
        .unwrap();
    let mut fts = codesearch::fts::FtsStore::new_with_writer(&db).unwrap();
    fts.add_chunk(
        ids[0],
        "<ref:classification search=\"gadget\" />",
        "src/rule.xml",
        None,
        "Block",
    )
    .unwrap();
    fts.commit().unwrap();
    drop(fts);
    for args in [
        vec!["gadget", "--literal"],
        vec![r"ref:classification\s+search=", "--regex"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
            .arg("search")
            .args(args)
            .arg("--json")
            .arg("--path")
            .arg(project.path())
            .env("CODESEARCH_DATABASE_URL", &cli_url)
            .env("CODESEARCH_STORAGE", "postgres")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "CLI search failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "unexpected CLI diagnostics: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["results"][0]["path"], "src/rule.xml");
    }
    assert!(!db.join("data.mdb").exists());

    use rmcp::ServiceExt;
    let shared =
        codesearch::index::SharedStores::new(&recovery.path().join("protocol-state"), 384).unwrap();
    *shared.vector_store.write().await = codesearch::vectordb::ChunkStore::Postgres {
        store: cli_store.clone(),
        repo: "cli".into(),
    };
    *shared.fts_store.write().await = codesearch::fts::FtsStore::new_readonly(&db).unwrap();
    let service = codesearch::mcp::CodesearchService::new_with_stores(
        Some(project.path().to_path_buf()),
        Some(std::sync::Arc::new(shared)),
    )
    .unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        service
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(client_transport).await.unwrap();
    for (name, arguments) in [
        (
            "search",
            serde_json::json!({"query":"gadget","mode":"literal"}),
        ),
        ("get_chunk", serde_json::json!({"chunk_id":ids[0]})),
        (
            "explore",
            serde_json::json!({"target":"src/rule.xml","kind":"outline"}),
        ),
    ] {
        let response = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new(name)
                    .with_arguments(arguments.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true), "{response:?}");
        let text = response
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|content| content.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if name == "explore" {
            let items: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(items[0]["chunk_id"], ids[0]);
            assert_eq!(items[0]["kind"], "Block");
        } else {
            assert!(text.contains("src/rule.xml"), "{name}: {text}");
        }
    }
    client.cancel().await.unwrap();
    server.await.unwrap();

    let force_stores =
        codesearch::index::SharedStores::new(&recovery.path().join("force-state"), 384).unwrap();
    *force_stores.vector_store.write().await = codesearch::vectordb::ChunkStore::Postgres {
        store: cli_store.clone(),
        repo: "cli".into(),
    };
    let result = codesearch::index::IndexManager::force_reindex_with_stores(
        project.path(),
        &db,
        &force_stores,
        Some(codesearch::embed::ModelType::EmbeddingGemma300MQ4),
        &tokio_util::sync::CancellationToken::new(),
        None,
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("mismatch"));
    assert!(cli_store.get_chunk("cli", ids[0]).await.unwrap().is_some());
    codesearch::index::IndexManager::perform_incremental_refresh_with_stores(
        project.path(),
        &db,
        &force_stores,
        &tokio_util::sync::CancellationToken::new(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(cli_store.get_chunk("cli", ids[0]).await.unwrap().is_none());

    let mut config = codesearch::db_discovery::repos::ReposConfig::default();
    for (alias, namespace) in [("alpha", "group-a"), ("beta", "group-b")] {
        let root = recovery.path().join(alias);
        let db = root.join(".codesearch.db");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(
            db.join("metadata.json"),
            serde_json::json!({
                "schema_version":1,"storage_backend":"postgres","storage_repo":namespace,
                "model_short_name":"minilm-l6-q","dimensions":384,"partial":false,
            })
            .to_string(),
        )
        .unwrap();
        cli_store
            .replace_file(
                namespace,
                "lib.rs",
                &[EmbeddedChunk::new(
                    Chunk::new(
                        format!("gadget {alias}"),
                        1,
                        1,
                        ChunkKind::Function,
                        "lib.rs".into(),
                    ),
                    vec![0.5; 384],
                )],
            )
            .await
            .unwrap();
        let fts = codesearch::fts::FtsStore::new(&db).unwrap();
        drop(fts);
        config.repos.insert(alias.into(), root);
        config.repo_read_only.insert(alias.into(), true);
    }
    let local_root = recovery.path().join("local");
    let local_db = local_root.join(".codesearch.db");
    let mut local = codesearch::vectordb::VectorStore::new(&local_db, 384).unwrap();
    local
        .insert_chunks_with_ids(vec![EmbeddedChunk::new(
            Chunk::new(
                "local gadget".into(),
                1,
                1,
                ChunkKind::Function,
                "local.rs".into(),
            ),
            vec![0.5; 384],
        )])
        .unwrap();
    local.build_index().unwrap();
    drop(local);
    std::fs::write(
        local_db.join("metadata.json"),
        r#"{"schema_version":1,"model_short_name":"minilm-l6-q","dimensions":384,"partial":false}"#,
    )
    .unwrap();
    drop(codesearch::fts::FtsStore::new(&local_db).unwrap());
    config.repos.insert("local".into(), local_root);
    config.repo_read_only.insert("local".into(), true);
    // Only alpha holds source id 1; beta's id 0 must remain a neighbour.
    cli_store
        .replace_file(
            "group-a",
            "source.rs",
            &[EmbeddedChunk::new(
                Chunk::new(
                    "source".into(),
                    1,
                    1,
                    ChunkKind::Function,
                    "source.rs".into(),
                ),
                vec![0.5; 384],
            )],
        )
        .await
        .unwrap();
    config.groups.insert(
        "all".into(),
        vec!["alpha".into(), "beta".into(), "local".into()],
    );
    let config_path = recovery.path().join("repos.json");
    config.save_to(&config_path).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let server_log = tempfile::tempfile().unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_codesearch"))
            .args([
                "serve",
                "--no-tui",
                "--create-index=false",
                "--port",
                &port.to_string(),
            ])
            .env("CODESEARCH_DATABASE_URL", &cli_url)
            .env("CODESEARCH_REPOS_CONFIG", &config_path)
            .env_remove("CODESEARCH_STORAGE")
            .current_dir(recovery.path())
            .stdout(server_log.try_clone().unwrap())
            .stderr(server_log.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..100 {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "test server exited during startup"
        );
        if http.get(format!("{base}/health")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(ready, "test server readiness timeout");
    let response: serde_json::Value = http
        .post(format!("{base}/explore"))
        .json(&serde_json::json!({"target":"1","kind":"similar","group":"all","limit":5}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = response.to_string();
    assert!(text.contains("alpha/lib.rs"), "{text}");
    assert!(text.contains("beta/lib.rs"), "{text}");
    assert!(text.contains("local/local.rs"), "{text}");
    assert!(
        !text.contains("alpha/source.rs"),
        "source must be excluded: {text}"
    );
    let info: serde_json::Value = http
        .get(format!("{base}/repos/alpha/info"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["backend"], "postgres");
    assert_eq!(info["chunks"], 2);
    assert!(info["storage_error"].is_null());
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    let empty_project = recovery.path().join("empty");
    let empty_db = empty_project.join(".codesearch.db");
    std::fs::create_dir_all(&empty_db).unwrap();
    std::fs::write(empty_db.join("metadata.json"), r#"{"schema_version":1,"storage_backend":"postgres","storage_repo":"empty","model_short_name":"minilm-l6-q","dimensions":384,"partial":false}"#).unwrap();
    drop(codesearch::fts::FtsStore::new(&empty_db).unwrap());
    cli_store
        .replace_file(
            "empty",
            "removed.rs",
            &[EmbeddedChunk::new(
                Chunk::new(
                    "deleted".into(),
                    1,
                    1,
                    ChunkKind::Function,
                    "removed.rs".into(),
                ),
                vec![0.5; 384],
            )],
        )
        .await
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .arg("index")
        .arg(&empty_project)
        .env("CODESEARCH_DATABASE_URL", &cli_url)
        .env("CODESEARCH_STORAGE", "postgres")
        .env("CODESEARCH_REPOS_CONFIG", &config_path)
        .env("CODESEARCH_SERVE_PORT", port.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "empty repo cleanup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(cli_store.stats("empty").await.unwrap().total_chunks, 0);
    let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .args(["index", "rm"])
        .arg(&empty_project)
        .arg("--keep-config")
        .env("CODESEARCH_DATABASE_URL", &cli_url)
        .env("CODESEARCH_STORAGE", "postgres")
        .env("CODESEARCH_REPOS_CONFIG", &config_path)
        .env("CODESEARCH_SERVE_PORT", port.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index rm failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!empty_db.exists());
    use std::io::{Read, Seek};
    let mut log = server_log;
    log.rewind().unwrap();
    let mut output = String::new();
    log.read_to_string(&mut output).unwrap();
    for line in output.lines() {
        if line.contains('⚠') {
            assert!(
                line.contains("Default embedding model 'minilm-l6-q' is not in the models cache"),
                "unexpected warning: {line}"
            );
        }
        assert!(
            !line.contains("ERROR") && !line.contains("panicked"),
            "unexpected server error: {line}"
        );
    }
}
