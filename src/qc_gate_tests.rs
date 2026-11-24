//! Drift guard for the local QC gate. `scripts/qc.ps1` and `scripts/qc.sh`
//! claim to mirror `.github/workflows/ci.yml`; a gate that silently narrows
//! what it builds is worse than no gate, because it is still trusted — it
//! once dropped `--workspace`, so a dependency crate's tests were never
//! compiled locally and CI went red on every push, unseen.

fn repo_file(rel: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every single-line `run: cargo …` command of a workflow, in file order.
/// Multi-line `run: |` blocks are skipped: they are jobs with their own
/// toolchain setup (the C# helper integration job), outside the gate's scope.
fn ci_cargo_commands(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- run: "))
        .filter(|cmd| cmd.starts_with("cargo "))
        .map(str::to_string)
        .collect()
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a gate script line runs `cmd` as-is. A trailing redirect/pipe is
/// ignored; anything else appended to or dropped from the command — a
/// `--no-run`, a missing `--workspace` — is not coverage.
fn gate_runs(script: &str, cmd: &str) -> bool {
    let cmd = normalize(cmd);
    script.lines().any(|line| {
        let head = line.split("2>&1").next().unwrap_or_default();
        let head = head.trim().trim_end_matches(['"', '\\', ' ', '&']);
        normalize(head).ends_with(&cmd)
    })
}

#[test]
fn qc_gate_scripts_run_every_ci_cargo_command() {
    let ci = ci_cargo_commands(&repo_file(".github/workflows/ci.yml"));
    assert!(
        ci.iter().any(|c| c.contains("--workspace")),
        "fixture: the workflow is expected to run workspace-wide"
    );
    for script in ["scripts/qc.ps1", "scripts/qc.sh"] {
        let text = repo_file(script);
        for cmd in &ci {
            assert!(
                gate_runs(&text, cmd),
                "{script} does not run CI's `{cmd}` — the gate must mirror ci.yml"
            );
        }
        assert!(
            gate_runs(&text, "cargo test --workspace --test '*' --no-run"),
            "{script}: -Fast must still COMPILE the integration tests"
        );
    }
}

#[test]
fn pre_push_hook_runs_the_gate() {
    let hook = repo_file(".githooks/pre-push");
    assert!(
        hook.contains("scripts/qc.ps1"),
        "the pre-push hook must invoke the QC gate scripts"
    );
}

#[test]
fn gate_runs_matcher_matrix() {
    let cmd = "cargo test --workspace --lib -- --nocapture";
    // Both script shapes: a plain line with a pipe tail, and sh's step wrapper.
    assert!(gate_runs(&format!("    {cmd} 2>&1 | Write-Host"), cmd));
    assert!(gate_runs(&format!("step \"lib\" \"{cmd} 2>&1\""), cmd));
    // The regression that reached develop: the root-only form.
    assert!(!gate_runs(
        "    cargo test --lib -- --nocapture 2>&1 | Write-Host",
        cmd
    ));
    // Compile-only is not running.
    assert!(!gate_runs(
        "    cargo test --workspace --lib --no-run 2>&1",
        cmd
    ));
    assert!(!gate_runs("    cargo check --workspace 2>&1", cmd));

    let workflow = "- run: cargo fmt --all -- --check\n  \
                    - run: |\n    cargo test --features x\n  \
                    - run: cargo clippy --workspace --all-targets -- -D warnings\n  \
                    - run: echo hi\n";
    assert_eq!(
        ci_cargo_commands(workflow),
        vec![
            "cargo fmt --all -- --check".to_string(),
            "cargo clippy --workspace --all-targets -- -D warnings".to_string(),
        ],
        "single-line cargo commands in order; block steps and non-cargo steps skipped"
    );
}
