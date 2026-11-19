//! Hermetic test defaults for the integration test binaries.
//!
//! Mirrors `crate::testing::redirect_test_scratch` — that module is
//! `#[cfg(test)]`-gated to the lib, so integration binaries cannot call it
//! (`remote_embeddings.rs` includes the module directly and is covered by
//! the same ctor from there). Scratch lands beside `target/` (outside the
//! checkout and outside every excluded-by-name dir), registry writes on an
//! isolated repos.json, delegation defaults to a dead port, and git runs
//! without the machine-global hook chain. See the lib-side doc for the full
//! rationale (the `wt*` registry-leak class).

#[ctor::ctor(unsafe)]
fn hermetic_test_env() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-scratch");
    let _ = std::fs::create_dir_all(&base);
    std::env::set_var("TMP", &base);
    std::env::set_var("TEMP", &base);
    std::env::set_var(
        codesearch::constants::REPOS_CONFIG_ENV,
        base.join("repos.json"),
    );
    std::env::set_var(codesearch::constants::SERVE_PORT_ENV, "1");
    let gitconfig = base.join("gitconfig");
    let _ = std::fs::write(
        &gitconfig,
        "[init]\n\tdefaultBranch = main\n[user]\n\tname = Test\n\temail = test@example.com\n",
    );
    std::env::set_var("GIT_CONFIG_GLOBAL", &gitconfig);
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
}
