use super::*;

fn remote() -> EmbedConfig {
    EmbedConfig {
        embed_provider: Some("openai".into()),
        embed_base_url: Some("http://localhost:4000/v1".into()),
        model: "bedrock/model".into(),
        ..Default::default()
    }
}

#[test]
fn explicit_endpoint_and_safe_url_required() {
    for url in [
        None,
        Some("https://user:secret@localhost/v1"),
        Some("https://localhost/v1?key=secret"),
        Some("file:///v1"),
    ] {
        let config = EmbedConfig {
            embed_base_url: url.map(str::to_owned),
            ..remote()
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(!error.contains("secret"));
    }
    assert_eq!(
        remote().endpoint().unwrap().as_str(),
        "http://localhost:4000/v1/embeddings"
    );
}

#[test]
fn transforms_truncate_on_utf8_boundary_and_isolate_identity() {
    let config = EmbedConfig {
        embed_query_prefix: "Q:".into(),
        embed_document_prefix: "D:".into(),
        embed_max_input_bytes: 5,
        ..remote()
    };
    assert_eq!(config.transform("ééé", true), "Q:é");
    assert_eq!(config.transform("ééé", false), "D:é");
    assert_ne!(config.fingerprint(), remote().fingerprint());
    assert!(!config.fingerprint().contains('/'));
}
