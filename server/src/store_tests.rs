use super::*;

#[test]
fn tls_verifies_by_default_and_only_explicit_parameter_overrides() {
    for url in [
        "postgres://user:password@localhost/db",
        "postgres://user:sslmode=disable@localhost/db",
    ] {
        assert!(matches!(
            connection_options(url).unwrap().get_ssl_mode(),
            PgSslMode::VerifyFull
        ));
    }
    assert!(matches!(
        connection_options("postgres://localhost/db?sslmode=disable")
            .unwrap()
            .get_ssl_mode(),
        PgSslMode::Disable
    ));
    let error = connection_options("not a url secret-password")
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret-password"));
}

#[test]
fn notebook_validation_distinguishes_empty_from_extraction_failure() {
    for valid in [
        r#"{"cells":[]}"#,
        r#"{"cells":[{"cell_type":"code","source":"print(1)"}]}"#,
        r##"{"cells":[{"cell_type":"markdown","source":["# Title","text"]}]}"##,
        r#"{"cells":[{"cell_type":"raw","source":[]}]}"#,
    ] {
        assert!(validate_notebook(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "{",
        "null",
        "[]",
        "{}",
        r#"{"cells":null}"#,
        r#"{"cells":{}}"#,
        r#"{"cells":[null]}"#,
        r#"{"cells":[{}]}"#,
        r#"{"cells":[{"cell_type":1,"source":[] }]}"#,
        r#"{"cells":[{"cell_type":"code","source":null}]}"#,
        r#"{"cells":[{"cell_type":"typo","source":[]}]}"#,
        r#"{"cells":[{"cell_type":"code","source":["valid",2]}]}"#,
    ] {
        assert!(validate_notebook(invalid).is_err(), "{invalid}");
    }
}
