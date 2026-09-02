//! Shared test-only helpers. Compiled into the crate under `#[cfg(test)]`
//! only, so none of this ships in release builds.

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
