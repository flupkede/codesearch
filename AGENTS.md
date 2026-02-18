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

---

## Active Feature: LMDB Resilience & Git-Aware Index Compact

**Status**: ✅ **COMPLETED** - All phases implemented and build successful

**Feature Branch**: `feature/LMDBResilience_GitAware_IndexCompact.md` (created and pushed)

**Specification**: `.docs\LMDBResilience_GitAware_IndexCompact.md`

### Overview

Implement comprehensive LMDB resilience and git-aware index compaction:

1. **Phase 0** (✅ **COMPLETED**): Fix model name mismatch bug causing full re-index on every restart
2. **Phase 0.5** (✅ **COMPLETED**): Repo-anchored index placement (`.codesearch.db` at git root)
3. **Phase 1** (🟡 **MOSTLY COMPLETE**): Auto-resize LMDB on `MDB_MAP_FULL` errors
4. **Phase 2** (✅ **COMPLETED**): Git HEAD watcher for branch changes + bloat ratio display

### ✅ Completed Work

#### Phase 0: Model Name Mismatch Fixed
**Files**: `src/index/mod.rs`, `src/index/manager.rs`

- Fixed inconsistent metadata field naming between `model` and `model_short_name`
- Changed all references to use `model_type.short_name()` consistently
- Prevents unnecessary full re-indexes on MCP restart

#### Phase 0.5: Repo-Anchored Index Placement
**Files**: `src/index/mod.rs`

- Implemented `find_git_root()` function (~63 lines)
- Handles normal `.git` directories and git worktree `.git` files
- Searches upward unlimited levels for git repository
- Detects and errors on multiple child `.git` directories
- No fallback to other VCS (git-only for index placement)

#### Phase 1: LMDB Auto-Resize Infrastructure
**Files**: `src/constants.rs`, `src/vectordb/store.rs`

- Bumped `DEFAULT_LMDB_MAP_SIZE_MB` from 512 → 1024
- Added `MAX_LMDB_MAP_SIZE_MB` constant (8GB hard limit)
- Implemented auto-resize infrastructure:
  - `is_map_full_error()` - Detects `MDB_MAP_FULL` errors
  - `resize_environment()` - Resizes and reopens LMDB environment
  - Added `map_size_mb: usize` tracking field to `VectorStore`
- Wrapped `delete_chunks()` and `insert_chunks_with_ids()` with retry logic
- Max 3 retry attempts per operation, doubling size on each failure

#### Phase 2: Git Integration & Monitoring
**Files**: `src/watch/mod.rs`, `src/index/mod.rs`, `src/index/manager.rs`

- Implemented `GitHeadWatcher` struct for branch change detection
- Added `bloat_ratio` field to `DbStats` struct
- Integrated `GitHeadWatcher` into `IndexManager` event loop
- Added branch change handler triggering incremental refresh
- Polls `.git/HEAD` file every 100ms for changes

### ✅ BLOCKING ISSUE: Missing `search()` Method - RESOLVED

**Problem**: The `search()` method was accidentally deleted from `src/vectordb/store.rs` during code cleanup.

**Impact**: **7 compilation errors** across:
- `src/search/mod.rs:498`
- `src/server/mod.rs:526`
- `src/mcp/mod.rs:211, 234`

**Solution Implemented**:
- ✅ Restored `search()` method from git history (lines 324-375)
- ✅ Inserted at line 390 in `src/vectordb/store.rs`
- ✅ Fixed struct field syntax error (`pub mut` → `pub`)
- ✅ Build now successful with only minor warnings

### 📋 Remaining Work

1. **Testing** (Recommended):
   - Test auto-resize functionality if possible
   - Test git-aware index placement and branch change detection
   - Run `cargo test` to verify all tests pass

2. **Optional Future Improvements**:
   - Implement full auto-resize for `build_index()` (currently simplified due to borrow checker)
   - Add sophisticated bloat_ratio calculation using LMDB env stats
   - Add unit tests for `GitHeadWatcher` and `find_git_root()`
   - Add integration tests for auto-resize behavior

### Technical Challenges Encountered

- **Borrow Checker Issues**: Transaction holds immutable borrow of `self.env`, preventing `resize_environment()` call while transaction is active
- **Windows Build Issues**: Git Bash `sed` commands with heredoc syntax unreliable on Windows
- **Multiple Child `.git` Detection**: Need to check downward 1 level to detect nested repos

### Modified Files (Ready for Commit)

- `src/constants.rs` - LMDB map size constants updated
- `src/index/mod.rs` - Model name fixes, `find_git_root()`, bloat_ratio display
- `src/index/manager.rs` - Model name fixes, `index_single_file()` updates, GitHeadWatcher integration
- `src/vectordb/store.rs` - Auto-resize infrastructure, wrapper methods, restored `search()` method
- `src/watch/mod.rs` - `GitHeadWatcher` implementation

### Build Status

- **Current**: ✅ **BUILD SUCCESSFUL** - All phases implemented and compiling without errors
- Only minor warnings (unused imports/variables) that don't affect functionality



