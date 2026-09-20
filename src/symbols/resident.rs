//! Resident SCIP helper (serve mode) + `WorkspacePool` — todo #115.
//!
//! The `scip-csharp serve` subcommand loads a solution's Roslyn workspace
//! ONCE and then answers find-refs requests as JSON lines over stdin/stdout.
//! The pool below owns helper lifecycle: admission (max N resident, LRU
//! eviction), per-workspace heap caps, and idle teardown. Eviction is safe —
//! resolved references persist in the LMDB ref cache, so only latency is
//! lost, never data.
//!
//! Memory model (the governor): `MAX_RESIDENT × heap cap` bounds total
//! helper memory. A runaway workspace fails fast at its cap and the typed
//! `failed` path turns that into an agent-actionable answer.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::SymbolReference;

/// References plus the completeness warnings from one resident find-refs
/// call. The warnings must travel WITH the references, not alongside them
/// in a log line: a partial answer that gets cached must stay partial
/// forever after unless the warnings are persisted with it.
#[derive(Debug, Default)]
pub(crate) struct ResidentRefs {
    pub references: Vec<SymbolReference>,
    /// Non-empty means `references` may be incomplete (a project failed to
    /// compile, FindReferencesAsync threw). Empty = complete.
    pub warnings: Vec<String>,
}

/// Behaviour seam so the pool can be unit-tested without real processes.
pub(crate) trait ClientLike: Send + Sync {
    fn find_refs(&self, symbol: &str) -> Result<ResidentRefs>;
    /// Kill the helper process. Must be idempotent.
    fn kill(&self);
}

type SpawnFn = Box<dyn Fn(&Path, &Path, u64) -> Result<Arc<dyn ClientLike>> + Send + Sync>;

/// The in-flight reservation taken under the pool lock and released by the
/// caller-owned parts — eviction may remove the map entry while the request
/// runs, so release must not depend on the map.
type Reservation = (Arc<dyn ClientLike>, Arc<AtomicUsize>, Arc<AtomicBool>);

/// A live `scip-csharp serve` child: spawned with a heap cap, handshaken on
/// the ready line, then strictly request-response. Kill-on-drop is the
/// backstop; the pool kills explicitly on eviction.
pub(crate) struct ServeClient {
    /// Mutexes over the two pipe halves; request-response is strictly
    /// sequential so no condition variable is needed.
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    child: Mutex<Child>,
}

impl ServeClient {
    /// Spawn `scip-csharp serve --solution <sln>` and wait for the ready
    /// handshake. The workspace load takes MINUTES on large solutions — this
    /// call blocks until the helper reports ready, which is why callers run
    /// it on the blocking pool and why the find_impact budget exists.
    pub(crate) fn spawn(helper: &Path, solution: &Path, heap_cap_bytes: u64) -> Result<Self> {
        // DOTNET_GCHeapHardLimit is interpreted by the .NET runtime as hex.
        let heap_cap_hex = format!("{heap_cap_bytes:X}");
        let mut child = Command::new(helper)
            .arg("serve")
            .arg("--solution")
            .arg(solution)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Helper stderr is diagnostics (load progress + workspace-load
            // warnings). Pipe it and drain via tracing: an inherited stderr
            // bypasses the file-only serve logger entirely and sprays raw
            // MSBuild output straight onto the TUI, scrambling it.
            .stderr(Stdio::piped())
            .env("DOTNET_GCHeapHardLimit", heap_cap_hex)
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn resident scip-csharp serve at {}",
                    helper.display()
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .context("serve helper has no stdin pipe")?;
        let stdout = child
            .stdout
            .take()
            .context("serve helper has no stdout pipe")?;

        // Drain helper stderr through tracing (warnings classified as warn).
        // Detached: the thread lives until the helper dies (EOF), so kill and
        // Drop teardown need no coordination with it. Without a concurrent
        // drain the pipe buffer would fill during the minutes-long workspace
        // load and block the helper.
        if let Some(stderr) = child.stderr.take() {
            let label = solution
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| solution.display().to_string());
            thread::spawn(move || {
                super::csharp::drain_pipe_to_tracing(stderr, |line| {
                    if !line.is_empty() {
                        super::csharp::emit_helper_stderr_line("scip-csharp serve", &label, line);
                    }
                });
            });
        }

        let client = Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            child: Mutex::new(child),
        };

        client
            .wait_ready()
            .context("serve helper failed to become ready")?;
        Ok(client)
    }

    /// Blocks until the ready handshake line arrives. No timeout: a large
    /// solution loads for minutes BY DESIGN, and the find_impact budget (not
    /// a watchdog here) is what turns that wait into a structured busy
    /// answer for the caller while this lookup continues detached.
    fn wait_ready(&self) -> Result<()> {
        let line = self.read_response_line()?;
        let response: ServeResponse = serde_json::from_str(&line)
            .with_context(|| format!("serve handshake is not valid JSON: {line}"))?;
        if !response.ok || response.ready != Some(true) {
            anyhow::bail!(
                "serve handshake failed: {}",
                response.error.unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Reads one protocol line from stdout. An empty read (EOF) means the
    /// helper died — surface as an error so the caller's fallback kicks in.
    fn read_response_line(&self) -> Result<String> {
        let mut line = String::new();
        let n = {
            let mut reader = self.stdout.lock().expect("serve stdout mutex poisoned");
            reader.read_line(&mut line)?
        };
        if n == 0 {
            anyhow::bail!("serve helper closed stdout (process died?)");
        }
        Ok(line)
    }
}

// ── Protocol wire types (snake_case — matches the helper's serializer) ──

#[derive(serde::Deserialize)]
struct ServeResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ready: Option<bool>,
    #[serde(default)]
    result: Option<ServeResult>,
}

#[derive(serde::Deserialize)]
struct ServeResult {
    #[serde(default)]
    references: Vec<ServeRef>,
    /// Absent in helper output from before warnings existed — default to
    /// empty so old binaries keep parsing as "complete".
    #[serde(default)]
    warnings: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ServeRef {
    file: String,
    start_line: u32,
    end_line: u32,
    #[serde(default = "default_ref_kind")]
    kind: String,
}

fn default_ref_kind() -> String {
    "reference".to_string()
}

impl ClientLike for ServeClient {
    fn find_refs(&self, symbol: &str) -> Result<ResidentRefs> {
        // Sequential protocol: write request, then read exactly one response.
        {
            let mut stdin = self.stdin.lock().expect("serve stdin mutex poisoned");
            let request = serde_json::json!({ "op": "find-refs", "symbol": symbol });
            writeln!(stdin, "{request}").context("failed to write find-refs request")?;
            stdin.flush().context("failed to flush find-refs request")?;
        }

        let line = self.read_response_line()?;
        let response: ServeResponse = serde_json::from_str(&line)
            .with_context(|| format!("serve response is not valid JSON: {line}"))?;

        if !response.ok {
            anyhow::bail!(
                "serve find-refs failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        let result = response
            .result
            .context("serve find-refs response has no result")?;
        Ok(ResidentRefs {
            references: result
                .references
                .into_iter()
                .map(|r| SymbolReference {
                    file: PathBuf::from(r.file),
                    start_line: r.start_line,
                    end_line: r.end_line,
                    kind: r.kind,
                })
                .collect(),
            warnings: result.warnings,
        })
    }

    fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ServeClient {
    fn drop(&mut self) {
        // Backstop: if the pool ever loses the last reference without an
        // explicit kill, the child must not outlive the host.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ── WorkspacePool — admission control IS the memory governor ────────────

/// Per-repo state inside the pool. The Arcs are cloned into every in-flight
/// request so eviction can observe "still in use" and defer the kill — the
/// counter-then-teardown discipline: in_flight is incremented under the same
/// pool lock that decides eviction, so an eviction decision can never miss an
/// in-flight request that grabbed its handle first.
struct PoolEntry {
    client: Arc<dyn ClientLike>,
    in_flight: Arc<AtomicUsize>,
    doomed: Arc<AtomicBool>,
    last_used: Instant,
}

pub(crate) struct WorkspacePool {
    /// Keyed by solution path (the workspace identity).
    entries: Mutex<HashMap<PathBuf, PoolEntry>>,
    /// Per-key spawn mutexes: two concurrent lookups on the SAME repo must
    /// not both pay the minutes-long workspace load. Different repos spawn
    /// in parallel. Entries are never removed (bounded by repo count).
    spawn_locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Bumped by `evict` for a solution, even with no resident entry to
    /// remove — the only signal a spawn-in-progress can check itself
    /// against, since `evict` must never take the per-key spawn lock (that
    /// would block a rebuild for the full minutes-long load). Never removed
    /// (bounded by repo count, same as `spawn_locks`).
    generations: Mutex<HashMap<PathBuf, u64>>,
    max_resident: usize,
    idle: Duration,
    heap_cap: u64,
    spawn_fn: SpawnFn,
}

impl WorkspacePool {
    #[allow(dead_code)]
    pub(crate) fn new(
        max_resident: usize,
        idle: Duration,
        heap_cap: u64,
        spawn_fn: SpawnFn,
    ) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            spawn_locks: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            max_resident,
            idle,
            heap_cap,
            spawn_fn,
        }
    }

    fn current_generation(&self, solution: &Path) -> u64 {
        *self
            .generations
            .lock()
            .expect("generations poisoned")
            .get(solution)
            .unwrap_or(&0)
    }

    /// Resolve references for `symbol` via the resident workspace for
    /// `solution`, spawning the helper if the repo has no resident workspace
    /// yet. Errors here are expected and handled by the caller's one-shot
    /// fallback (spawn failure, heap-cap death, eviction race, protocol).
    /// Completeness warnings travel with the references — the caller
    /// persists them alongside the cached refs.
    pub(crate) fn find_refs(
        &self,
        helper: &Path,
        solution: &Path,
        symbol: &str,
    ) -> Result<ResidentRefs> {
        // Single-flight per repo: the second concurrent lookup on the same
        // repo waits for the first one's spawn instead of duplicating the
        // minutes-long workspace load.
        let key_lock = {
            let mut locks = self.spawn_locks.lock().expect("spawn locks poisoned");
            locks
                .entry(solution.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _key_guard = key_lock.lock().expect("per-key spawn mutex poisoned");

        if let Some((client, in_flight, doomed)) = self.acquire_existing(solution) {
            let outcome = client.find_refs(symbol);
            self.release_parts(&in_flight, &doomed, &client);
            return outcome;
        }

        // Snapshot BEFORE the spawn: if `evict` bumps this generation while
        // we're mid-spawn (a rebuild landing during the minutes-long load),
        // the workspace we're about to install is already stale — caught
        // below without `evict` ever needing to block on `key_lock`.
        let gen_before_spawn = self.current_generation(solution);

        // Spawn OUTSIDE the pool lock — it takes minutes and must not block
        // lookups on other repos (admission still happens under the lock).
        self.enforce_admission();
        let client = (self.spawn_fn)(helper, solution, self.heap_cap)?;
        let in_flight = Arc::new(AtomicUsize::new(1));
        let doomed = Arc::new(AtomicBool::new(false));

        {
            let mut entries = self.entries.lock().expect("workspace pool poisoned");
            // Race: another thread inserted this repo while we spawned. Kill
            // OUR fresh workspace and use the existing resident one instead —
            // two workspaces for one solution is exactly what the governor
            // exists to prevent. The reservation is taken under the same
            // lock that observes the existing entry.
            if let Some(existing) = entries.get_mut(solution) {
                existing.last_used = Instant::now();
                existing.in_flight.fetch_add(1, Ordering::SeqCst);
                let (c, inf, d) = (
                    existing.client.clone(),
                    existing.in_flight.clone(),
                    existing.doomed.clone(),
                );
                drop(entries);
                drop(client); // Drop impl kills our redundant child
                let outcome = c.find_refs(symbol);
                self.release_parts(&inf, &d, &c);
                return outcome;
            }
            // A rebuild evicted this solution while we were spawning: our
            // workspace already predates it. Answer this one caller from it
            // (it already paid the load cost) but do not install it as
            // resident — installing it would resurrect exactly the stale
            // answer `evict` was called to prevent.
            if self.current_generation(solution) != gen_before_spawn {
                drop(entries);
                let outcome = client.find_refs(symbol);
                client.kill();
                return outcome;
            }
            entries.insert(
                solution.to_path_buf(),
                PoolEntry {
                    client: client.clone(),
                    in_flight: Arc::clone(&in_flight),
                    doomed: Arc::clone(&doomed),
                    last_used: Instant::now(),
                },
            );
        }

        let outcome = client.find_refs(symbol);
        self.release_parts(&in_flight, &doomed, &client);
        outcome
    }

    /// Get the resident client for a repo, if any (TTL-checked). Takes the
    /// in-flight reservation under the pool lock and returns the reservation
    /// parts to the caller — release must NOT depend on the map, because the
    /// entry may be evicted (and its map slot removed) while the request is
    /// in flight.
    fn acquire_existing(&self, solution: &Path) -> Option<Reservation> {
        let mut to_kill: Vec<Arc<dyn ClientLike>> = Vec::new();
        let resident = {
            let mut entries = self.entries.lock().expect("workspace pool poisoned");

            // Lazy TTL reap while we hold the lock (counter-then-teardown:
            // in-flight entries are never reaped here — they can't be, the
            // filter requires in_flight == 0).
            let expired: Vec<PathBuf> = entries
                .iter()
                .filter(|(_, e)| {
                    e.last_used.elapsed() > self.idle && e.in_flight.load(Ordering::SeqCst) == 0
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired {
                if let Some(e) = entries.remove(&k) {
                    to_kill.push(e.client);
                }
            }

            let resident = match entries.get_mut(solution) {
                Some(entry) => {
                    entry.last_used = Instant::now();
                    entry.in_flight.fetch_add(1, Ordering::SeqCst);
                    Some((
                        entry.client.clone(),
                        entry.in_flight.clone(),
                        entry.doomed.clone(),
                    ))
                }
                None => None,
            };
            resident
        };
        for c in to_kill {
            c.kill(); // OS call — outside the pool lock
        }
        resident
    }

    /// Drop the resident workspace for `solution`, if any, so the next
    /// `find_refs` respawns against current on-disk source instead of
    /// answering from a compilation loaded before the latest change. A
    /// symbol rebuild refreshes the definitions layer (LMDB) but never
    /// touches this pool on its own — without this call a resident
    /// workspace can silently outlive the commit it was loaded for, up to
    /// the full idle TTL, with no warning surfaced to the caller.
    ///
    /// Also bumps this solution's generation counter unconditionally, even
    /// when nothing is resident to remove: a spawn already in flight for
    /// this solution has no entry yet, but `find_refs` checks the counter
    /// right before installing its result, so that spawn's answer is used
    /// once and discarded instead of being resurrected as resident.
    pub(crate) fn evict(&self, solution: &Path) {
        let victim = {
            let mut entries = self.entries.lock().expect("workspace pool poisoned");
            let result = match entries.remove(solution) {
                Some(e) if e.in_flight.load(Ordering::SeqCst) == 0 => Some(e.client),
                Some(e) => {
                    // In flight: same deferred-kill discipline as LRU eviction.
                    e.doomed.store(true, Ordering::SeqCst);
                    None
                }
                None => None,
            };
            *self
                .generations
                .lock()
                .expect("generations poisoned")
                .entry(solution.to_path_buf())
                .or_insert(0) += 1;
            result
        };
        if let Some(v) = victim {
            v.kill();
        }
    }

    /// Evict as many residents as needed to admit a new workspace — LRU
    /// first. The admission decision AND the in_flight check happen under
    /// the same lock (counter-then-teardown); kills happen outside it.
    fn enforce_admission(&self) {
        let mut victims: Vec<Arc<dyn ClientLike>> = Vec::new();
        {
            let mut entries = self.entries.lock().expect("workspace pool poisoned");
            while entries.len() >= self.max_resident {
                let Some(lru_key) = entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some(e) = entries.remove(&lru_key) {
                    if e.in_flight.load(Ordering::SeqCst) == 0 {
                        victims.push(e.client);
                    } else {
                        // In flight: defer the kill to its release — the
                        // request fails (the pipe dies with the process) and
                        // the caller's one-shot fallback keeps it correct.
                        e.doomed.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
        for v in victims {
            v.kill();
        }
    }

    /// Release the in-flight reservation. Works from the caller-owned Arcs,
    /// NOT the map: the entry may have been evicted (map slot removed) while
    /// this request was in flight, and the deferred doomed-kill must still
    /// fire exactly once — on the decrement that reaches zero.
    fn release_parts(
        &self,
        in_flight: &Arc<AtomicUsize>,
        doomed: &Arc<AtomicBool>,
        client: &Arc<dyn ClientLike>,
    ) {
        // Reached zero on OUR decrement while doomed → the deferred kill is
        // ours to perform (eviction deferred it under the pool lock).
        if in_flight.fetch_sub(1, Ordering::SeqCst) == 1 && doomed.load(Ordering::SeqCst) {
            client.kill();
        }
    }
}

fn env_resolved<T: std::str::FromStr>(env: &str, default: T) -> T {
    match std::env::var(env) {
        Ok(raw) => raw.trim().parse().unwrap_or(default),
        Err(_) => default,
    }
}

/// The process-global pool. Config comes from env at first use (same pattern
/// as the find_impact budget resolver).
pub(crate) static WORKSPACE_POOL: std::sync::LazyLock<WorkspacePool> =
    std::sync::LazyLock::new(|| {
        let spawn: SpawnFn = Box::new(|helper: &Path, solution: &Path, heap_cap: u64| {
            Ok(Arc::new(ServeClient::spawn(helper, solution, heap_cap)?))
        });
        WorkspacePool::new(
            env_resolved(
                crate::constants::SCIP_MAX_RESIDENT_WORKSPACES_ENV,
                crate::constants::DEFAULT_SCIP_MAX_RESIDENT_WORKSPACES,
            ),
            Duration::from_secs(env_resolved(
                crate::constants::SCIP_WORKSPACE_IDLE_SECS_ENV,
                crate::constants::DEFAULT_SCIP_WORKSPACE_IDLE_SECS,
            )),
            env_resolved(
                crate::constants::SCIP_WORKSPACE_HEAP_CAP_ENV,
                crate::constants::DEFAULT_SCIP_WORKSPACE_HEAP_CAP,
            ),
            spawn,
        )
    });
