//! Shared test-only helpers. Compiled into the crate under `#[cfg(test)]`
//! only, so none of this ships in release builds.

/// Hermetic test defaults, applied once per test binary via the `ctor` in
/// this module (and the copy in `tests/common/mod.rs` for integration tests):
///
/// - Scratch goes to `<manifest>/../test-scratch/` — visible, never in git,
///   outside the Defender-churned `%TEMP%`, and OUTSIDE the checkout: it must
///   neither sit inside a git worktree ("outside any repo" fixtures and
///   `find_best_database` ancestor scans break) nor under a path whose
///   components the walker/FSW exclude by NAME (`target` is in
///   `ALWAYS_EXCLUDED` — scratch under `target/` is invisible to both).
/// - `CODESEARCH_REPOS_CONFIG` points at an isolated repos.json, so any test
///   that forgets its own override writes there instead of into the
///   developer's live registry.
/// - `CODESEARCH_SERVE_PORT` = 1, so CLI delegation that skips its env
///   override fails fast against a dead port instead of registering tempdir
///   repos on the developer's running serve.
/// - git is hermetic: `GIT_CONFIG_GLOBAL` points at a minimal config and
///   `GIT_CONFIG_NOSYSTEM=1`, so the machine-global hook dispatcher (which
///   chains codesearch's post-checkout registration against the live serve
///   on every branch checkout) never fires inside tests. Every `git
///   worktree add` in a test used to POST its tempdir to the running hub —
///   the `wt*` registration leak. The minimal config keeps identity and
///   `init.defaultBranch` so commits keep working.
///
/// The real global file stays observable: `global_config_canary` reads
/// `~/.codesearch/repos.json` directly and keeps catching genuine leaks.
///
/// The env names are literals because this file is dual-compiled — lib
/// tests AND `#[path]` includes from integration binaries — so `crate::`
/// paths cannot resolve here. Keep in sync with `constants::REPOS_CONFIG_ENV`
/// and `constants::SERVE_PORT_ENV` (a constants unit test pins the pair).
pub fn redirect_test_scratch() {
    // Beside `<manifest>/../target` (the `target-dir` pinned in
    // `.cargo/config.toml`) — outside the checkout, name not excluded.
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test-scratch");
    let _ = std::fs::create_dir_all(&base);
    std::env::set_var("TMP", &base);
    std::env::set_var("TEMP", &base);
    std::env::set_var("CODESEARCH_REPOS_CONFIG", base.join("repos.json"));
    std::env::set_var("CODESEARCH_SERVE_PORT", "1");
    let gitconfig = base.join("gitconfig");
    let _ = std::fs::write(
        &gitconfig,
        "[init]\n\tdefaultBranch = main\n[user]\n\tname = Test\n\temail = test@example.com\n",
    );
    std::env::set_var("GIT_CONFIG_GLOBAL", &gitconfig);
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
}

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn hermetic_test_env() {
    redirect_test_scratch();
}

/// RAII guard that sets the given env vars and restores their previous
/// values (including "was unset") when dropped.
///
/// Tests that mutate process-global env vars must BOTH use this guard AND
/// be annotated `#[serial]` (serial_test crate): the guard makes the
/// mutation panic-safe (a failing assertion can no longer leak a stale
/// value into every later test), and `#[serial]` serializes them against
/// the other tests in the same cargo-test process that read the same vars
/// -- the doctor tests read CODESEARCH_REPOS_CONFIG on code paths that
/// race the remove_order_tests writes, which is the flake class this
/// pair was introduced to close. See AGENTS.md "Notes for agents".
pub struct EnvRestore {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvRestore {
    /// Snapshot the current values of `vars`, then set them. Restores on drop.
    pub fn set(vars: &[(&'static str, &str)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        Self { saved }
    }

    /// Snapshot the current values of `vars` (including "was unset"), then
    /// REMOVE them all. Restores on drop. Complements [`EnvRestore::set`]
    /// for the "variable must be absent" cases (e.g. asserting the default
    /// fallback of an env-overridable knob).
    pub fn remove(vars: &[&'static str]) -> Self {
        let saved = vars.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in vars {
            std::env::remove_var(k);
        }
        Self { saved }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (k, prev) in &self.saved {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
