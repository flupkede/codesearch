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
