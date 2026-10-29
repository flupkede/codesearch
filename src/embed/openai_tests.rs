use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Mock {
    fn new(responses: Vec<(u16, String, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = requests.clone();
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let Some(request) = read_request(&mut stream) else {
                            continue;
                        };
                        captured.lock().unwrap().push(request);
                        let (status, headers, body) =
                            responses
                                .next()
                                .unwrap_or((500, String::new(), String::new()));
                        let response = format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len());
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("mock accept: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn config(&self) -> EmbedConfig {
        EmbedConfig {
            embed_provider: Some("openai".into()),
            embed_base_url: Some(self.url.clone()),
            model: "proxy/bedrock-model".into(),
            embed_dimensions: Some(2),
            embed_timeout_secs: 3,
            ..Default::default()
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn read_request(stream: &mut TcpStream) -> Option<(String, serde_json::Value)> {
    stream.set_nonblocking(false).ok()?;
    let mut data = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8(data[..end].to_vec()).ok()?;
            let length: usize = headers.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })?;
            if data.len() >= end + 4 + length {
                return Some((
                    headers,
                    serde_json::from_slice(&data[end + 4..end + 4 + length]).ok()?,
                ));
            }
        }
    }
}
fn success(count: usize) -> (u16, String, String) {
    (200, String::new(), serde_json::json!({"data": (0..count).rev().map(|index| serde_json::json!({"index": index, "embedding": [3.0, 4.0]})).collect::<Vec<_>>()}).to_string())
}

#[test]
fn mock_reads_fragmented_request_on_an_accepted_nonblocking_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_nonblocking(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        client
            .write_all(b"POST /v1/embeddings HTTP/1.1\r\nContent-Length: 2\r\n\r\n")
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        client.write_all(b"{}").unwrap();
    });
    let request = read_request(&mut stream);
    sender.join().unwrap();
    assert_eq!(
        request.expect("complete fragmented request").1,
        serde_json::json!({})
    );
}

#[test]
fn response_order_normalization_and_validation() {
    let response = Response {
        data: vec![
            Datum {
                index: 1,
                embedding: vec![0.0, 2.0],
            },
            Datum {
                index: 0,
                embedding: vec![3.0, 4.0],
            },
        ],
    };
    assert_eq!(
        validate_response(response, 2, 2).unwrap(),
        vec![vec![0.6, 0.8], vec![0.0, 1.0]]
    );
    for data in [
        vec![],
        vec![Datum {
            index: 1,
            embedding: vec![1.0, 2.0],
        }],
        vec![Datum {
            index: 0,
            embedding: vec![f32::NAN, 2.0],
        }],
        vec![Datum {
            index: 0,
            embedding: vec![0.0, 0.0],
        }],
        vec![Datum {
            index: 0,
            embedding: vec![1.0],
        }],
    ] {
        assert!(validate_response(Response { data }, 1, 2).is_err());
    }
}

#[test]
fn retry_after_supports_seconds_and_http_date() {
    assert_eq!(retry_after_delay("3"), Some(Duration::from_secs(3)));
    assert!(
        retry_after_delay(&httpdate::fmt_http_date(
            SystemTime::now() + Duration::from_secs(10)
        ))
        .unwrap()
            > Duration::from_secs(8)
    );
    assert!(retry_after_delay("invalid").is_none());
}

#[test]
#[serial_test::serial]
fn http_batches_auth_prefixes_truncation_and_normalization() {
    let _env = crate::testing::EnvRestore::set(&[("CODESEARCH_EMBED_API_KEY", "test-secret")]);
    let server = Mock::new(vec![success(2), success(1), success(1)]);
    let mut config = server.config();
    config.embed_batch_size = 2;
    config.embed_concurrency = 1;
    config.embed_query_prefix = "Q:".into();
    config.embed_document_prefix = "D:".into();
    config.embed_max_input_bytes = 5;
    let mut embedder = OpenAiEmbedder::new(config).unwrap();
    assert_eq!(
        embedder.embed_documents(vec!["ééé".into(); 3]).unwrap(),
        vec![vec![0.6, 0.8]; 3]
    );
    assert_eq!(embedder.embed_query("ééé").unwrap(), vec![0.6, 0.8]);
    let captured = server.requests.lock().unwrap();
    assert_eq!(captured.len(), 3);
    assert!(captured[0].0.starts_with("POST /v1/embeddings "));
    assert!(captured[0]
        .0
        .to_lowercase()
        .contains("authorization: bearer test-secret"));
    assert_eq!(captured[0].1["model"], "proxy/bedrock-model");
    assert_eq!(captured[0].1["dimensions"], 2);
    assert_eq!(captured[0].1["input"], serde_json::json!(["D:é", "D:é"]));
    assert_eq!(captured[2].1["input"], serde_json::json!(["Q:é"]));
}

#[test]
#[serial_test::serial]
fn http_dimension_probe_once_and_optional_auth() {
    let _env = crate::testing::EnvRestore::remove(&["CODESEARCH_EMBED_API_KEY"]);
    let server = Mock::new(vec![success(1), success(1), success(1)]);
    let mut config = server.config();
    config.embed_dimensions = None;
    let mut embedder = OpenAiEmbedder::new(config).unwrap();
    assert_eq!(embedder.dimensions(), 2);
    for _ in 0..2 {
        embedder.embed_query("query").unwrap();
    }
    let captured = server.requests.lock().unwrap();
    assert_eq!(captured.len(), 3, "one probe, two queries");
    assert!(captured.iter().all(|(headers, body)| !headers
        .to_lowercase()
        .contains("authorization:")
        && body.get("dimensions").is_none()));
}

#[test]
#[serial_test::serial]
fn http_retries_only_transient_status_and_redacts_errors() {
    let _env = crate::testing::EnvRestore::set(&[("CODESEARCH_EMBED_API_KEY", "test-secret")]);
    let server = Mock::new(vec![
        (429, "Retry-After: 0\r\n".into(), "test-secret".into()),
        (503, String::new(), "source text".into()),
        success(1),
        (401, String::new(), "test-secret source text".into()),
    ]);
    let mut embedder = OpenAiEmbedder::new(server.config()).unwrap();
    embedder.embed_query("query").unwrap();
    let error = format!("{:#}", embedder.embed_query("query").unwrap_err());
    assert!(error.contains("401"));
    assert!(!error.contains("test-secret") && !error.contains("source text"));
    assert_eq!(server.requests.lock().unwrap().len(), 4);
}

#[test]
fn http_rejects_dimension_count_duplicate_and_invalid_json() {
    for body in [
        r#"{"data":[{"index":0,"embedding":[1]}]}"#,
        r#"{"data":[]}"#,
        r#"{"data":[{"index":1,"embedding":[1,2]}]}"#,
        "secret response not json",
    ] {
        let server = Mock::new(vec![(200, String::new(), body.into())]);
        let mut embedder = OpenAiEmbedder::new(server.config()).unwrap();
        let error = format!("{:#}", embedder.embed_query("query").unwrap_err());
        assert!(!error.contains("secret response"));
    }
}

#[test]
fn http_deadline_bounds_retry_after() {
    let server = Mock::new(vec![(429, "Retry-After: 60\r\n".into(), String::new())]);
    let mut config = server.config();
    config.embed_timeout_secs = 1;
    let mut embedder = OpenAiEmbedder::new(config).unwrap();
    let start = std::time::Instant::now();
    let error = embedder.embed_query("query").unwrap_err().to_string();
    assert!(error.contains("deadline"), "unexpected error: {error:#}");
    assert!(start.elapsed() < Duration::from_secs(3));
}
