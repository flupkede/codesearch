# AGENTS.md — features/serve-single-instance

## Goal

Prevent `codesearch serve` from starting a second instance when one is already
running. Currently the process fails with a cryptic OS-level `AddrInUse` error.
The fix detects a running instance before binding the port and exits cleanly
with a clear message.

Note: Ctrl-C is intentionally NOT a quit key in the TUI (removed in a previous
commit — crossterm raw mode delivers it as a key event, bypassing the OS handler).
Use `q` to quit the TUI.

---

## Implementation

### File: `src/serve/mod.rs`

At the very start of `run_serve()`, before the `TcpListener::bind` call,
add a health-check probe:

```rust
// Single-instance guard: probe the health endpoint before trying to bind.
// If a serve is already running on this port, exit cleanly instead of
// crashing with a cryptic AddrInUse OS error.
let probe_url = format!("http://{}:{}/health", host, port);
if let Ok(resp) = reqwest::Client::new()
    .get(&probe_url)
    .timeout(std::time::Duration::from_millis(500))
    .send()
    .await
{
    if resp.status().is_success() {
        eprintln!(
            "codesearch serve is already running at http://{}:{}\n\
             Use 'q' in the TUI window to stop it, or kill the process manually.",
            host, port
        );
        std::process::exit(1);
    }
}
```

Notes:
- `reqwest` is already a dependency — no new deps needed.
- Timeout of 500ms is enough: if serve is up it answers in <10ms;
  if nothing is listening the OS rejects the connection immediately.
- Use `std::process::exit(1)` not `anyhow::bail!` — this is an intentional
  early-exit, not an unexpected error.
- The message says `q` (not Ctrl-C) because Ctrl-C is not a TUI quit key.

### Where to insert

Find the function `pub async fn run_serve(` in `src/serve/mod.rs`.
The probe goes after the port/host are resolved but before
`tokio::net::TcpListener::bind(addr).await?`.

Approximate location (search for `TcpListener::bind` — first non-test occurrence):

```
src/serve/mod.rs line ~1769: let listener = tokio::net::TcpListener::bind(addr).await?;
```

Insert the probe block 5-10 lines above that.

---

## Quality gates

- [ ] `cargo check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --lib --bins` — all tests pass (the two test occurrences of
      `TcpListener::bind("127.0.0.1:0")` are on random ports and unaffected)
- [ ] Manual: start serve, open second terminal, run `codesearch serve` again →
      prints clear message and exits with code 1
- [ ] Manual: no serve running, `codesearch serve` starts normally

## CHANGELOG

Add under a new version section:

```markdown
### Fixed

- `codesearch serve` now detects a running instance before binding the port
  and exits with a clear message instead of crashing with a cryptic
  `AddrInUse` OS error.
```

## Branch flow

```powershell
git push origin features/serve-single-instance
# PR features/serve-single-instance → develop → merge → release.ps1
```

## Done when

- [ ] Health-check probe added to `run_serve()`
- [ ] Quality gates pass
- [ ] Manual smoke tests pass
- [ ] CHANGELOG updated
- [ ] PR opened against `develop`
