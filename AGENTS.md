# OpenCode AGENTS.md

** ONLY USE MCP TOOLS !!! **

### Gebruik bash indien alleen specifiek index operatie (niet met MCP actief !!)

```bash

# NEVER EXECUTE a REINDEX Complete
NOT! codesearch index

# NEVER EXECUTE a Complete REINDEX
NOT! codesearch index -f

# If required you can list the index
codesearch index list 
```

**Build Commands (CRITICAL - READ CAREFULLY):**

⚠️ **MANDATORY BUILD RULES - NEVER VIOLATE** ⚠️

### Target Directory (STRICT ENFORCEMENT)
- **Target directory MUST be**: `C:\WorkArea\AI\codesearch\target`
- **NEVER build to**: `C:\WorkArea\AI\codesearch\codesearch.git\target` or any other location
- **Reason**: `.cargo/config.toml` sets `target-dir = "../target"` to keep source tree clean

### Build Type (STRICT ENFORCEMENT)
- **ALWAYS build**: DEBUG builds only
- **NEVER build**: RELEASE builds (`--release` flag)
- **Release builds are FORBIDDEN** - they cause version mismatch issues and waste time

### Correct Commands ✅
```bash
cd codesearch.git && cargo build              # CORRECT - debug build to ../target
cd codesearch.git && cargo test               # CORRECT - debug tests
cd codesearch.git && cargo run -- mcp         # CORRECT - debug run from ../target
```

### Commands NEVER to Use ❌
```bash
cd codesearch.git && cargo build --release    # WRONG - FORBIDDEN
cd codesearch.git && cargo run --release     # WRONG - FORBIDDEN
cargo build --release                         # WRONG - FORBIDDEN
cd codesearch.git && cargo build              # WRONG if target dir is codesearch.git/target
```

### Verify Correct Location
```bash
# Correct location for binary
ls -la /c/WorkArea/AI/codesearch/target/debug/codesearch.exe

# WRONG location - DO NOT USE
ls -la /c/WorkArea/AI/codesearch/codesearch.git/target/
```

### Standard Commands (for reference)
- `cargo build` - Build debug version (FAST, use for development)
- `cargo test` - Run all tests
- `cargo test <test_name>` - Run single test (e.g., `cargo test test_group_chunks_by_path`)
- `cargo test --lib` - Run only library tests
- `cargo clippy` - Lint with Clippy
- `cargo fmt` - Format code
- `cargo doc --no-deps` - Generate documentation

**Code Style Guidelines:**

**Imports:**
- Use `use crate::` for internal module imports (not `use codesearch::`)
- Group imports: std lib → external crates → internal modules
- Prefer `use anyhow::{Result, anyhow}` for error handling
- Use `use colored::Colorize` for terminal colors
- Use `use tracing::{debug, info, warn}` for logging

**Error Handling:**
- Return `anyhow::Result<T>` from fallible functions
- Use `anyhow::anyhow!("message")` for errors
- **CRITICAL:** Never use `.unwrap()` or `.expect()` in library code
- For Mutex: `.lock().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?`
- Use `?` operator for error propagation
- Provide context with `.context()` or `.map_err()` when useful

**Types & Naming:**
- Use `PathBuf` for owned paths, `&Path` for borrowed
- Use `String` for owned strings, `&str` for borrowed
- Prefer `&str` over `String` in function arguments
- Use `HashMap<K, Vec<V>>` for grouping patterns
- Pre-allocate HashMap capacity: `HashMap::with_capacity(size)`
- Use `Arc<Mutex<T>>` for shared mutable state
- Use `Arc` for shared read-only data

**Async:**
- Use `tokio::spawn` for background tasks
- Use `tokio::sync::RwLock` for async shared state
- Use `#[tokio::main]` for async main functions
- Use `.await` for async calls

**Testing:**
- Use `#[cfg(test)]` for test modules
- Use `#[test]` for unit tests
- Place tests in same file as code (in test module)
- Use `use super::*;` in test modules

**Documentation:**
- Use `///` for item documentation (public APIs)
- Use `//!` for module documentation
- Document public structs, functions, and important types

**Performance:**
- Avoid unnecessary `.to_string()` calls
- Use `.to_string_lossy().to_string()` only when needed
- Pre-allocate collections when size is known
- Use `&str` instead of `String` where possible
- Use streaming for large data processing (don't collect all into memory)
- Cache with memory limits using weigher-based eviction
- Keep LMDB map_size reasonable (2GB is sufficient for most use cases)

**Memory Optimization (from `reduce_memory_consumption` branch):**
- Streaming indexing: Process files one at a time, not all chunks at once
- Embedding cache: Enforce 500MB limit using weigher (not just entry count)
- LMDB configuration: Set map_size to 2GB (not 10GB) to reduce reported memory
- Avoid large Vec/HashMap accumulations during processing
- Use immediate writes to vector store/FTS instead of batching all data
- Expected peak memory: ~500-700MB for large codebases (vs 2GB before optimization)

**Signal Handling:**
- Implement graceful CTRL-C handling using tokio::select!
- Use tokio::signal for SIGINT (Unix) and CTRL-C (Windows)
- Exit with code 130 (standard for SIGINT) on interrupt
- Ensure database handles are closed before exit

**CLI (clap):**
- Use `#[derive(Parser, Subcommand)]` for CLI
- Use `#[command(subcommand)]` for subcommands
- Use `#[arg(short, long)]` for options

**Server (axum):**
- Use `State<T>` for dependency injection
- Use `Json<T>` for JSON responses
- Use `StatusCode` for HTTP status codes
- Use `Router::new()` with `.route()` for routing

**Serialization (serde):**
- Use `#[derive(Serialize, Deserialize)]` for data types
- Use `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields

**Module Structure:**
- Each module has `mod.rs` with public exports
- Re-export common types in `lib.rs`
- Use `pub use` for convenience re-exports

**Build Artifacts:**
- Debug builds go to `../target/debug/` (C:\WorkArea\AI\codesearch\target\debug\)
- Release builds FORBIDDEN - never use
- ALWAYS use debug builds for all work
- Target directory is configured in `.cargo/config.toml` as `../target`
- This keeps source tree clean and centralized

### Voordelen

- ✅ Versiebeheer: Automatische versienummers per commit
- ✅ Schone repository: Build artifacts buiten source tree
- ✅ Sneller indexeren: Alleen gewijzigde bestanden verwerken
- ✅ Handig: Werkt vanuit elke subfolder
- ✅ Flexibel: Lokale of globale indexes naar keuze
- ✅ Slim: Automatische detectie van index type
- ✅ Veilig: Gaat correct om met verwijderde bestanden
- ✅ AI-vriendelijk: Smart grep wrapper voor OpenAgents/OpenCoder
- ✅ Documentatie: Help tekst altijd up-to-date
- ✅ Eenvoudig: Geen subcommando's, alles via flags


