use super::*;

#[tokio::test]
#[ignore = "requires disposable PG18 with pg_search 0.27.1 + vector and CREATEDB"]
async fn loopback_authority_and_origin_are_checked_before_all_routes() {
    let db = document_tests::TestDatabase::new().await;
    let state = Simulated::default();
    let boundary = document_tests::boundary(&state).await;
    let service = serve(router(db.store(&config(&boundary.url)).await)).await;
    let client = reqwest::Client::new();
    for host in ["127.0.0.1", "127.0.0.1:39726", "localhost", "LOCALHOST:80", "[::1]", "[::1]:65535"] {
        let response = client.get(format!("{}/health", service.url)).header("host", host).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "native host {host}");
    }
    for origin in ["http://127.0.0.1", "http://localhost:39726", "https://localhost:443", "http://[::1]:80"] {
        let response = client.get(format!("{}/health", service.url)).header("origin", origin).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "local origin {origin}");
    }
    for host in ["untrusted.example:39726", "localhost.untrusted.example", "untrusted-localhost", "127.0.0.1.untrusted.example", "localhost@untrusted.example", "untrusted.example@localhost", "localhost:", "localhost:bogus", "localhost:65536", "localhost:0", "::1", "[::1", "localhost/path", "localhost,127.0.0.1", ""] {
        let response = client.get(format!("{}/health", service.url)).header("host", host).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "reject host {host:?}");
    }
    for origin in ["http://untrusted.example", "http://localhost.untrusted.example", "null", "file://localhost", "http://localhost/extra", "http://localhost?query", "http://localhost#fragment", "http://attacker@localhost", "http://localhost:", "http://localhost:65536", "http://localhost http://untrusted.example", ""] {
        let response = client.get(format!("{}/health", service.url)).header("origin", origin).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "reject origin {origin:?}");
    }
    // Boundary rejection precedes handlers, JSON decoding and fallback routing.
    for path in ["/health", "/repositories", "/repositories/missing/files", "/repositories/missing/file", "/search", "/unknown"] {
        let response = client.post(format!("{}{path}", service.url)).header("host", "untrusted.example").body("not json").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "route {path}");
        let response = client.put(format!("{}{path}", service.url)).header("origin", "http://untrusted.example").body("not json").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "origin route {path}");
    }
    // Duplicate header values must not let a trusted first value mask a hostile second.
    for (name, good, bad) in [("host", "localhost", "untrusted.example"), ("origin", "http://localhost", "http://untrusted.example")] {
        let response = client.get(format!("{}/health", service.url)).header(name, good).header(name, bad).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "duplicate {name}");
    }
    drop(service);
    drop(boundary);
    db.cleanup().await;
}
