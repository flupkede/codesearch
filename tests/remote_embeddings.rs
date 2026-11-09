#[path = "../src/testing.rs"]
mod testing;
use codesearch::embed::{configure, EmbedConfig, EmbeddingModel, EmbeddingServicePool};
use codesearch::index::{IndexManager, SharedStores};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

fn cli(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codesearch"));
    for key in [
        "PROVIDER",
        "BASE_URL",
        "DIMENSIONS",
        "BATCH_SIZE",
        "TIMEOUT_SECS",
        "CONCURRENCY",
        "QUERY_PREFIX",
        "DOCUMENT_PREFIX",
        "MAX_INPUT_BYTES",
        "API_KEY",
    ] {
        cmd.env_remove(format!("CODESEARCH_EMBED_{key}"));
    }
    cmd.env("CODESEARCH_HOME", home)
        .env("CODESEARCH_EMBED_PROVIDER", "fastembed");
    cmd
}

/// Separate test process isolates the single process strategy from library tests.
#[tokio::test]
#[serial_test::serial]
async fn remote_refresh_failure_preserves_completed_and_previous_files_then_retries() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let db = repo.join(".codesearch.db");
    let _env = testing::EnvRestore::set(&[
        ("CODESEARCH_HOME", home.to_str().unwrap()),
        ("CODESEARCH_INCREMENTAL_BATCH_SIZE", "1"),
    ]);
    let _credentials = testing::EnvRestore::remove(&["CODESEARCH_EMBED_API_KEY"]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let fail_after = Arc::new(AtomicUsize::new(usize::MAX));
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let fail_after = fail_after.clone();
        let calls = calls.clone();
        let stop = stop.clone();
        let requests = requests.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buffer = [0; 4096];
                let body = loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break None;
                    }
                    data.extend_from_slice(&buffer[..read]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())
                                    .flatten()
                            })
                            .unwrap();
                        if data.len() >= end + 4 + length {
                            break Some(
                                serde_json::from_slice::<Value>(&data[end + 4..end + 4 + length])
                                    .unwrap(),
                            );
                        }
                    }
                };
                let Some(body) = body else { continue };
                let count = body["input"].as_array().unwrap().len();
                requests.lock().unwrap().push(body);
                let call = calls.fetch_add(1, Ordering::Relaxed);
                let (status, body) = if call >= fail_after.load(Ordering::Relaxed) {
                    (400, "secret server body".to_owned())
                } else {
                    (200, json!({"data": (0..count).rev().map(|index| json!({"index": index, "embedding": [3.0,4.0]})).collect::<Vec<_>>()}).to_string())
                };
                let _ = write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        })
    };
    let config = EmbedConfig {
        embed_provider: Some("openai".into()),
        embed_base_url: Some(url.clone()),
        model: "proxy/model".into(),
        embed_dimensions: Some(2),
        embed_batch_size: 1,
        embed_timeout_secs: 3,
        ..Default::default()
    };
    configure(config, None, &[]).unwrap();
    EmbeddingModel::ActiveRemote.try_dimensions().unwrap();
    let stores = SharedStores::new(&db, 2).unwrap();
    let mut metadata = serde_json::Map::new();
    EmbeddingModel::ActiveRemote.write_metadata_fields(&mut metadata);
    std::fs::write(
        db.join("metadata.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    for name in ["a", "b"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!("fn old_{name}() {{ println!(\"old {name}\"); }}\n"),
        )
        .unwrap();
    }
    let pool = Arc::new(EmbeddingServicePool::new(None));
    let token = CancellationToken::new();
    IndexManager::perform_incremental_refresh_with_stores(
        &repo,
        &db,
        &stores,
        &token,
        Some(&pool),
        None,
    )
    .await
    .unwrap();
    let initial = stores.vector_store.read().await.iter_all_chunks().unwrap();
    assert_eq!(initial.len(), 2);
    for name in ["a", "b"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!(
                "fn replacement_{name}() {{ println!(\"new {name} with longer contents\"); }}\n"
            ),
        )
        .unwrap();
    }
    fail_after.store(calls.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    let error = IndexManager::perform_incremental_refresh_with_stores(
        &repo,
        &db,
        &stores,
        &token,
        Some(&pool),
        None,
    )
    .await
    .unwrap_err();
    assert!(!format!("{error:#}").contains("secret server body"));
    let partial = stores.vector_store.read().await.iter_all_chunks().unwrap();
    assert_eq!(
        partial.len(),
        2,
        "one completed replacement and one previous file must survive"
    );
    assert_eq!(
        partial
            .iter()
            .filter(|(_, c)| c.content.contains("replacement_"))
            .count(),
        1
    );
    assert_eq!(
        partial
            .iter()
            .filter(|(_, c)| c.content.contains("old_"))
            .count(),
        1
    );
    assert!(
        stores
            .vector_store
            .read()
            .await
            .search(&[0.6, 0.8], 10)
            .is_ok(),
        "completed batches remain searchable"
    );
    fail_after.store(usize::MAX, Ordering::Relaxed);
    IndexManager::perform_incremental_refresh_with_stores(
        &repo,
        &db,
        &stores,
        &token,
        Some(&pool),
        None,
    )
    .await
    .unwrap();
    let final_chunks = stores.vector_store.read().await.iter_all_chunks().unwrap();
    assert_eq!(final_chunks.len(), 2);
    assert!(final_chunks
        .iter()
        .all(|(_, c)| c.content.contains("replacement_")));
    // Actual query path uses the same remote strategy/model and normalized vectors.
    let output = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .env("CODESEARCH_HOME", &home)
        .env("CODESEARCH_EMBED_PROVIDER", "openai")
        .env("CODESEARCH_EMBED_BASE_URL", &url)
        .env("CODESEARCH_EMBED_DIMENSIONS", "2")
        .env("CODESEARCH_EMBED_API_KEY", "private-test-key")
        .args(["--model", "proxy/model", "search", "replacement", "--path"])
        .arg(&repo)
        .arg("--create-index=false")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("replacement_"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let notice = format!("Remote embeddings active: endpoint={url}/embeddings model=proxy/model");
    assert_eq!(stderr.matches(&notice).count(), 1, "{stderr}");
    assert!(stderr.contains("Source chunks and queries will be sent to this endpoint."));
    assert!(!stderr.contains("private-test-key"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&notice));
    let calls_before_mcp = calls.load(Ordering::Relaxed);
    let mut mcp = cli(&home)
        .env("CODESEARCH_EMBED_PROVIDER", "openai")
        .env("CODESEARCH_EMBED_BASE_URL", &url)
        .env("CODESEARCH_EMBED_DIMENSIONS", "2")
        .env("CODESEARCH_EMBED_API_KEY", "private-test-key")
        .args([
            "--model",
            "proxy/model",
            "mcp",
            "--mode",
            "local",
            "--readonly",
        ])
        .arg(&repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = mcp.stdout.take().unwrap();
    let stderr = mcp.stderr.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        BufReader::new(stderr).read_to_string(&mut text).unwrap();
        text
    });
    let requests = [
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":"remote-privacy-test", "version":"1.0"}
        }}),
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
            "name":"search", "arguments":{"query":"replacement", "mode":"semantic"}
        }}),
    ];
    let mut responses = Vec::new();
    for request in requests {
        writeln!(mcp.stdin.as_mut().unwrap(), "{request}").unwrap();
        mcp.stdin.as_mut().unwrap().flush().unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        loop {
            let line = match receive.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(line) => line.unwrap(),
                Err(error) => {
                    mcp.kill().unwrap();
                    mcp.wait().unwrap();
                    panic!(
                        "MCP response timeout/disconnect: {error}; {}",
                        errors.join().unwrap()
                    );
                }
            };
            let response: Value = serde_json::from_str(&line).expect("JSON-RPC stdout");
            assert_eq!(response["jsonrpc"], "2.0");
            let matched = response.get("id") == Some(id);
            responses.push(response);
            if matched {
                break;
            }
        }
    }
    drop(mcp.stdin.take());
    assert!(mcp.wait().unwrap().success());
    reader.join().unwrap();
    for line in receive.try_iter() {
        let response: Value = serde_json::from_str(&line.unwrap()).expect("JSON-RPC stdout");
        assert_eq!(response["jsonrpc"], "2.0");
    }
    let stderr = errors.join().unwrap();
    assert_eq!(stderr.matches(&notice).count(), 1, "{stderr}");
    assert!(stderr.contains("Source chunks and queries will be sent to this endpoint."));
    assert!(!stderr.contains("private-test-key"));
    let result = &responses.iter().find(|r| r["id"] == 2).unwrap()["result"];
    assert_ne!(result["isError"], true, "{result}");
    assert!(result.to_string().contains("replacement_"), "{result}");
    assert!(calls.load(Ordering::Relaxed) > calls_before_mcp);
    let doctor = cli(&home)
        .env("CODESEARCH_EMBED_PROVIDER", "openai")
        .env("CODESEARCH_EMBED_BASE_URL", &url)
        .env("CODESEARCH_EMBED_DIMENSIONS", "2")
        .current_dir(&repo)
        .args(["--model", "proxy/model", "doctor", "--json"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&doctor.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&doctor.stderr)));
    let strategy = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "Embedding strategy")
        .unwrap();
    assert_eq!(strategy["status"], "pass", "{strategy}");
    assert!(strategy["message"]
        .as_str()
        .unwrap()
        .contains("proxy/model"));
    assert_eq!(
        strategy["details"],
        format!("Remote endpoint: {url}/embeddings")
    );
    let before = std::fs::read(db.join("metadata.json")).unwrap();
    let rejected = cli(&home)
        .args(["search", "query", "--path"])
        .arg(&repo)
        .arg("--create-index=false")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("--force"));
    assert!(!stderr.contains("Remote embeddings active:"));
    let rejected = cli(&home)
        .env("CODESEARCH_EMBED_PROVIDER", "openai")
        .env(
            "CODESEARCH_EMBED_BASE_URL",
            "https://user:url-password@example.invalid/v1?api_key=query-secret",
        )
        .current_dir(&repo)
        .args(["--model", "proxy/model", "doctor", "--json"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("without credentials, query or fragment"));
    for secret in ["url-password", "query-secret", "Remote embeddings active:"] {
        assert!(!stderr.contains(secret), "{stderr}");
        assert!(!String::from_utf8_lossy(&rejected.stdout).contains(secret));
    }
    assert_eq!(std::fs::read(db.join("metadata.json")).unwrap(), before);
    let rejected = cli(&home).args(["index"]).arg(&repo).output().unwrap();
    assert!(!rejected.status.success());
    assert_eq!(std::fs::read(db.join("metadata.json")).unwrap(), before);
    let persisted = String::from_utf8(before).unwrap();
    assert!(!persisted.contains(&url) && !persisted.contains("API_KEY"));
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
}
