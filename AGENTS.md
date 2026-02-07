# OpenCode AGENTS.md

**Build Commands:**
- `cargo build` - Build debug version (FAST, use for development)
- `cargo build --release` - Build optimized release (SLOW, only when explicitly requested)
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
- Debug builds go to `target/debug/`
- Release builds go to `target/release/`
- Use debug builds during development
- Only build release when explicitly requested by user

---

## [0.2.1] - 2025-01-28

### Bug Fixes 🐛

#### File Walker Infinite Loop Fix
- Fixed infinite loop in file walker when scanning excluded directories
- Added `filter_entry()` callback to `WalkBuilder` to skip excluded directories **before** descending
- Excluded directories (node_modules, .git, target, etc.) are now completely skipped, not visited per-file
- Removed redundant `should_skip()` and `is_in_excluded_dir()` functions

#### FTS Store Windows File Locking Fix
- Fixed "Access is denied" errors during incremental indexing on Windows
- Changed `FtsStore::new()` to `FtsStore::new_with_writer()` for incremental indexing
- FTS store now opens in R/W mode instead of read-only mode during indexing
- Added retry logic with `open_or_create_index_with_retry()` and `create_writer_with_retry()`

#### MCP/Server Quiet Mode
- Added `index_quiet()` function for server/MCP mode (no CLI output)
- `IndexManager::perform_incremental_refresh()` now uses `index_quiet()` instead of `index()`
- Prevents verbose CLI output spam during MCP/serve operations
- Uses `tracing` for logging instead of `println!` in quiet mode

### Technical Changes

#### FTS Store Access Patterns
- **Index/Serve/MCP (write):** `FtsStore::new_with_writer()` - R/W mode
- **Search (read):** `FtsStore::open_readonly()` - Read-only mode
- Proper separation of read/write access prevents file locking conflicts

#### Index Function Refactoring
- `index()` - CLI function with verbose output (unchanged API)
- `index_quiet()` - Server/MCP function with no output (new)
- `index_with_options()` - Internal function with `quiet` parameter
- Uses `log_print!` macro for conditional output

### Files Changed
- `src/file/mod.rs` - Filter excluded directories in walker
- `src/fts/tantivy_store.rs` - Retry logic and R/W mode fixes
- `src/index/mod.rs` - Quiet mode support, `index_quiet()` function
- `src/index/manager.rs` - Use `index_quiet()` for incremental refresh

---

## [0.2.0] - 2025-01-23

### Nieuwe Features 🚀

#### Git-based Versioning
- Automatische versienummering op basis van git commit count
- Versieformaat: `0.2.0+<commit-count>` (bijv. `0.2.0+127`)
- `build.rs` script genereert build metadata tijdens compilatie
- Toont versie in `--version`, `--help` en startup logs
- Elke commit update automatisch het build nummer

#### Target Directory Outside Repository
- Build artifacts worden opgeslagen buiten de source tree
- Gebruikt `.cargo/config.toml` met `target-dir = "../target"`
- Houdt repository schoon (geen grote `target/` directory)
- Snellere git operaties

#### Index Commando Restructuring
- `codesearch index [PATH]` - Indexeer directory (auto-detecteert lokaal of globaal)
- `codesearch index add` - Maakt nieuwe lokale index aan
- `codesearch index add -g` - Maakt nieuwe globale index aan
- `codesearch index rm` - Verwijder index (auto-detecteert welke)
- `codesearch index list` - Toon index status (lokale of globale)
- Geen subcommando's meer, alles via flags
- Auto-detectie van lokale vs globale index
- Kan nooit beide lokale en globale index hebben voorzelfde project
- `add -g` geeft error als lokale index bestaat
- `rm` verwijdert lokale met warning als beide bestaan (mag niet!)

#### Incremental Indexing
- `codesearch index` doet nu automatisch incremental updates als database bestaat
- Indexeert alleen gewijzigde, toegevoegde en verwijderde bestanden
- Gebruikt FileMetaStore om bestandsmetadata te tracken (hash, mtime, size)
- Stopt vroeg als database al up-to-date is
- Volledige re-index met `--force` flag (ook beschikbaar als `--full`, `-f`)

#### Database Discovery
- Index commando zoekt nu in parent/global directories naar bestaande databases
- Gebruikt `find_best_database()` voor automatische database locatie
- Toont informatief bericht bij gebruik van database uit parent directory
- Consistent gedrag met search commando

#### CLI Verbeteringen
- `--full` en `-f` aliases toegevoegd voor `--force` flag in index commando
- `--remove` alias toegevoegd voor `--rm` flag
- Betere gebruikersfeedback tijdens incremental indexing
- Help tekst altijd up-to-date met commando's en argumenten

#### Smart Grep Wrapper (voor AI Agents)
- Wrapper aangemaakt op `~/.local/bin/grep` voor AI agents
- Gebruikt automatisch codesearch voor geïndexeerde source code projecten
- Valt terug op reguliere grep voor non-code bestanden
- Geoptimaliseerd voor ASP.NET Core:
  - `.cs`, `.cshtml`, `.razor`, `.csproj`, `.sln`, `.sql`
  - Ook: `.ts`, `.tsx`, `.js`, `.jsx`, `.vue`, `.svelte`
  - Andere talen: `.rs`, `.go`, `.py`, `.java`, `.c`, `.cpp`, etc.
- Minimale performance overhead

### Technische Wijzigingen

#### Gewijzigde Bestanden
- `build.rs`: Nieuw - Automatische versie generatie
- `src/index/mod.rs`: Index commando herstructurering, `add_to_index()`, `remove_from_index()`, `list_index_status()`, `get_db_stats()`
- `src/cli/mod.rs`: Index commando met flags (geen subcommando's), `--list` ondersteuning als path argument
- `src/db_discovery/mod.rs`: Fix voor `REPOS_CONFIG_FILE` path, verbeterde error handling
- `src/main.rs`: `db_discovery` module declaratie, versie weergave
- `src/lib.rs`: `db_discovery` module export
- `src/search/mod.rs`: Database discovery integratie
- `src/mcp/mod.rs`: Database discovery integratie
- `.cargo/config.toml`: Nieuw - Target directory configuratie
- `.gitignore`: `.cargo/` toegevoegd

#### Nieuwe Bestanden
- `src/db_discovery/mod.rs`: Database discovery module
- `scripts/bump-version.ps1`: Hernoemd van `copy-to-common.ps1`

### Gebruik

```bash
# Incremental index (standaard als DB bestaat)
codesearch index

# Volledige re-index
codesearch index --force
codesearch index --full
codesearch index -f

# Index vanuit subfolder (vindt parent database)
cd src/components
codesearch index

# Index beheer
codesearch index                          # Indexeer (auto-detecteert lokaal/globaal)
codesearch index -f                       # Forceer volledige re-index
codesearch index add                      # Maak lokale index
codesearch index add -g                   # Maak globale index
codesearch index rm                       # Verwijder index (auto-detect)
codesearch index list                     # Toon index status
```

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

## [0.1.0] - Initiële Versie

### Basis Functionaliteit
- Semantisch zoeken in code met embeddings
- Full-text search met Tantivy
- File watching met auto-reindex
- MCP server integratie
- Ondersteuning voor meerdere programmeertalen
- Vector database met Arroy + Heed (MDB)

---

## Versie Geschiedenis

| Versie | Datum | Beschrijving |
|--------|-------|--------------|
| 0.2.0 | 2025-01-23 | Git-based versioning, global index registry, target directory outside repo |
| 0.1.0 | - | Initiële versie |

---

## Volgende Stappen

### Gepland voor 0.3.0
- [ ] Performance verbeteringen voor grote codebases
- [ ] Meer talen ondersteunen
- [ ] Betere error handling
- [ ] Unit tests uitbreiden

### Toekomstige Features
- [ ] Distributed indexing
- [ ] Real-time collaboration
- [ ] Web UI
- [ ] Plugin systeem
