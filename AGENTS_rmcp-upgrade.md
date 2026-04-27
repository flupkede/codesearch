# Upgrade: rmcp 0.9.1 -> 1.5.0 (MCP protocol 2025-11-05)

**Status:** Blokkerend voor Claude Code 2.1.x
**Branch:** `feature/rmcp-upgrade`
**Eigenaar:** OpenCode

---

## 1. Probleem

codesearch adverteert MCP `protocolVersion: "2025-03-26"` (rmcp 0.9.1).
Claude Code 2.1.119 stuurt `ProtocolVersion("2025-11-25")` in `initialize` en
verlaat de sessie zonder te onderhandelen als de server een oudere versie adverteert.
`tools/list` wordt nooit gestuurd, tools zijn onbeschikbaar, `/mcp` toont "Failed to reconnect."

Root cause: rmcp 0.9.1 kent protocol `2025-03-26`. Vanaf rmcp 1.x is dit `2025-11-05`.

Crates.io status (27 april 2026):
- **0.9.1** — huidig (protocol `2025-03-26`)
- **1.5.0** — latest stable, gepubliceerd 16 april 2026 (protocol `2025-11-05`)

---

## 2. Scope van de wijzigingen

Dit is een **major version bump** (0.x → 1.x) met meerdere breaking changes.
De upgrade raakt vrijwel alle MCP-gerelateerde code in `src/mcp/mod.rs`,
`src/serve/mod.rs`, en `Cargo.toml`.

### 2.1 Cargo.toml

```toml
# Was:
rmcp = { version = "0.9.1", features = ["server", "client", "transport-io",
  "transport-streamable-http-server",
  "transport-streamable-http-client-reqwest", "macros"] }

# Wordt:
rmcp = { version = "1.5.0", features = ["server", "client", "transport-io",
  "transport-streamable-http-server",
  "transport-streamable-http-client-reqwest", "macros"] }
```

`schemars` gaat ook van 0.8 naar 1.0 (rmcp 1.x trekt schemars 1.x mee):

```toml
# Was:
schemars = "0.8"

# Wordt:
schemars = "1.0"
```

### 2.2 Breaking changes per categorie

#### Protocol version in ServerInfo

In rmcp 1.x heeft `ServerInfo` een expliciet `protocol_version` veld:

```rust
// Was (rmcp 0.9.1): protocol_version afwezig, rmcp zette het impliciet
ServerInfo {
    capabilities: ...,
    server_info: Implementation { ... },
    instructions: Some("...".to_string()),
    ..Default::default()
}

// Wordt (rmcp 1.x):
use rmcp::model::ProtocolVersion;
ServerInfo {
    protocol_version: ProtocolVersion::V_2025_11_05,
    capabilities: ...,
    server_info: Implementation { ... },
    instructions: Some("...".to_string()),
    ..Default::default()
}
```

Zoek alle `ServerInfo {` constructies in `src/mcp/mod.rs` en `src/serve/mod.rs`
en voeg `protocol_version: ProtocolVersion::V_2025_11_05` toe.

#### CallToolRequest / CallToolRequestParam

```rust
// Was:
async fn call_tool(
    &self,
    request: CallToolRequest,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResult, McpError>

// Wordt (naam veranderd):
async fn call_tool(
    &self,
    request: CallToolRequestParam,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResult, ErrorData>
```

`McpError` is hernoemd naar `ErrorData`. Zoek alle `McpError` en vervang door `ErrorData`.
Check ook imports: `use rmcp::ErrorData as McpError;` of pas alle call sites aan.

#### #[tool_router] macro en ToolRouter veld

In rmcp 1.x moet de struct een `ToolRouter<Self>` veld bevatten:

```rust
// Was (rmcp 0.9.1): tool_router als field was optioneel / anders opgezet
#[derive(Clone)]
pub struct CodesearchService {
    // ... velden
}

// Wordt (rmcp 1.x):
use rmcp::handler::server::tool::ToolRouter;
#[derive(Clone)]
pub struct CodesearchService {
    tool_router: ToolRouter<Self>,
    // ... rest van de velden
}

impl CodesearchService {
    pub fn new(...) -> Self {
        Self {
            tool_router: Self::tool_router(), // gegenereerd door #[tool_router]
            // ...
        }
    }
}
```

De `#[tool_router]` macro op de impl-blok genereert de `tool_router()` associated
function. De `#[tool_handler]` macro op de `impl ServerHandler` blok wires de
router aan `list_tools` en `call_tool`.

Patroon in rmcp 1.x:

```rust
#[tool_router]
impl CodesearchService {
    #[tool(description = "...")]
    async fn search(&self, Parameters(req): Parameters<SearchRequest>, ...) 
        -> Result<CallToolResult, ErrorData> { ... }
}

#[tool_handler]
impl ServerHandler for CodesearchService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_11_05,
            ...
        }
    }
}
```

#### schemars 0.8 → 1.0

schemars 1.0 heeft breaking changes:
- `#[schemars(description = "...")]` werkt nog steeds
- `JsonSchema` derive werkt nog steeds
- Maar sommige helper types en re-exports zijn gewijzigd

Codesearch gebruikt schemars voornamelijk via `#[derive(JsonSchema)]` en
`#[schemars(description = "...")]` attributes op request structs. Deze werken
grotendeels ongewijzigd. Laat de compiler de specifieke fouten aanwijzen.

#### Parameters wrapper import

```rust
// Was:
use rmcp::handler::server::tool::Parameters;

// In rmcp 1.x (check exacte pad):
use rmcp::handler::server::wrapper::Parameters;
// of:
use rmcp::handler::server::tool::Parameters;
// Laat compiler bepalen wat correct is
```

#### StdioProxyHandler (toegevoegd in feature/fix-mcp-client, nu op master)

De `StdioProxyHandler` struct in `run_mcp_client` implementeert `ServerHandler`
handmatig (geen macros). De signatures moeten worden aangepast aan rmcp 1.x:

```rust
// Aanpassen:
impl rmcp::ServerHandler for StdioProxyHandler {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo {
            protocol_version: rmcp::model::ProtocolVersion::V_2025_11_05,
            ...
        }
    }

    async fn list_tools(
        &self,
        request: Option<rmcp::model::PaginatedRequestParam>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> { ... }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParam,  // let op: Param niet Request
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> { ... }
}
```

---

## 3. Aanpak

### Stap 1: Cargo.toml updaten

Bump `rmcp` naar `"1.5.0"` en `schemars` naar `"1.0"` in `Cargo.toml`.
Run `cargo build 2>&1 | head -100` om de eerste golf compile errors te zien.

### Stap 2: Iteratief compile-driven fixen

rmcp 1.x heeft veel compile errors bij upgrade vanwege de breaking changes.
Aanpak:

1. `cargo build` — bekijk eerste batch errors
2. Fix de meest fundamentele (imports, hernoemingen)
3. Herhaal tot clean

Verwachte volgorde van fixes:
1. `McpError` → `ErrorData` (global find-replace)
2. `CallToolRequest` → `CallToolRequestParam` (let op: alleen in ServerHandler impl, niet bij calls)
3. `protocol_version: ProtocolVersion::V_2025_11_05` toevoegen aan alle `ServerInfo` constructies
4. `ToolRouter<Self>` veld toevoegen aan `CodesearchService` en `new()` aanpassen
5. `schemars` gerelateerde fouten per geval oplossen
6. `Parameters` import pad corrigeren indien nodig

### Stap 3: StdioProxyHandler aanpassen

Zie sectie 2.2 hierboven. Pas signatures aan en voeg `protocol_version` toe aan `build_proxy_server_info()`.

### Stap 4: Verifieer protocol version in initialize response

Na een succesvolle build, test met:

```bash
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | codesearch mcp --mode local
```

Verwacht: response met `"protocolVersion":"2025-11-05"` (niet `"2025-03-26"`).
Claude Code stuurt `2025-11-25`, wij antwoorden `2025-11-05` — dit is de versie
die rmcp 1.5.0 ondersteunt en Claude Code accepteert voor backwards compat.

---

## 4. Definition of Done

- [ ] `cargo build --release` compileert zonder errors of warnings
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test --lib` groen (alle bestaande tests)
- [ ] `initialize` response bevat `"protocolVersion":"2025-11-05"` (niet `"2025-03-26"`)
- [ ] Claude Code 2.1.x: `tools/list` wordt gestuurd na `initialize`
- [ ] Claude Code `/mcp` toont codesearch als "Connected" in-session
- [ ] OpenCode: tools nog steeds beschikbaar (regressietest)
- [ ] `codesearch mcp --mode client` werkt nog (StdioProxyHandler)

---

## 5. Niet in scope

- Upgraden naar rmcp 2.x of hoger (als die uitkomt voor merge)
- OAuth / auth features van rmcp 1.x
- Nieuwe rmcp 1.x features (elicitation, sampling, etc.)
- Wijzigingen aan zoeklogica of indexering

---

## 6. Risico's

| Risico | Kans | Mitigatie |
|--------|------|-----------|
| schemars 1.0 breekt JsonSchema derives | Gemiddeld | Compiler wijst fouten aan; meeste derives werken ongewijzigd |
| tool_router macro gedrag veranderd | Gemiddeld | Gebruik rmcp voorbeelden als referentie; compile-driven |
| HTTP transport API gewijzigd (serve mode) | Laag | Serve gebruikt StreamableHttpServer; check rmcp 1.x transport docs |
| reqwest versie conflict (rmcp 1.x trekt reqwest 0.13) | Laag | Check Cargo.lock na bump; los versie conflicts op |

---

## 7. Commit message voorstel

```
fix(mcp): upgrade rmcp 0.9.1 -> 1.5.0 for protocol 2025-11-05 support

Claude Code 2.1.x announces protocolVersion "2025-11-25" and abandons
the session if the server responds with the old "2025-03-26" version
(rmcp 0.9.1). tools/list was never sent, making codesearch unavailable
in Claude Code sessions.

rmcp 1.5.0 advertises protocol version 2025-11-05 which Claude Code
accepts. Breaking changes addressed:
- McpError -> ErrorData
- CallToolRequest -> CallToolRequestParam
- ServerInfo: add explicit protocol_version field
- ToolRouter<Self> field required in handler struct
- schemars 0.8 -> 1.0
- StdioProxyHandler signatures updated

Closes: AGENTS_rmcp-upgrade.md
```

---

## 8. Referenties

- rmcp 1.5.0 crates.io: https://crates.io/crates/rmcp/1.5.0
- rmcp GitHub: https://github.com/modelcontextprotocol/rust-sdk
- Breaking changes observeerbaar via: `cargo build` compile errors na bump
