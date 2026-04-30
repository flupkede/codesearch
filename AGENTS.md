# `codesearch serve` TUI met ratatui

**Branch:** `feature/serve-tui`
**Status:** In planning
**Scope:** Vervang het flikkerende print+cursor overzicht in `codesearch serve` door een echte TUI met ratatui. Read-only in v1.

---

## 1. Achtergrond

`codesearch serve` heeft al een live overzicht-tabel die per repo toont: alias,
status, laatste tool call, lock status, etc. De huidige implementatie gebruikt
direct `print!` met cursor positioning — wat resulteert in zichtbare flikkering
omdat de terminal de tussenstaat rendert tussen het verplaatsen van de cursor
en het schrijven van de nieuwe waarden.

Logs gaan al naar file (geen stdout meer in serve), dus de stdout is vrij voor
een fullscreen TUI. Dat maakt deze migratie eenvoudig.

---

## 2. Wat er moet gebeuren

Vervang de huidige render-loop door een ratatui-gebaseerde TUI:

1. Alternate screen buffer (zoals `vim`) — bij start fullscreen overnemen,
   bij exit terug naar de oorspronkelijke prompt zonder vervuiling van scrollback
2. Virtual buffer + diff render — geen flikkering meer, alleen gewijzigde cellen
   worden naar de terminal gestuurd
3. Auto-resize wanneer terminal venster groter/kleiner wordt
4. Read-only — geen acties in v1 (alleen `q` voor quit, `↑↓` voor scroll als nodig)

---

## 3. Dependencies

In `Cargo.toml`:

```toml
ratatui = "0.29"
crossterm = "0.28"
```

`crossterm` is de cross-platform terminal backend (Windows + Unix) die ratatui
onder de motorkap gebruikt. Beide werken zonder gedoe op Windows 11.

---

## 4. Architectuur

### Tokio task layout

`codesearch serve` heeft vandaag al meerdere tokio tasks (HTTP server, file
watchers per repo, status collector). Voeg er één toe:

```rust
// In src/serve/mod.rs:
let tui_handle = tokio::spawn(run_tui(state.clone(), shutdown_rx.clone()));
```

De TUI task:
- Heeft een `Arc<RwLock<ServeState>>` referentie naar dezelfde state als de bestaande
  status-tabel render gebruikt — geen aparte data layer nodig
- Roept elke 500ms een `terminal.draw(...)` aan
- Pollt `crossterm` events met 100ms timeout voor `q` keystroke en resize events
- Bij `q` of `Ctrl+C`: stuurt shutdown signaal via de bestaande `shutdown_tx`

### State sharing

Geen wijzigingen aan de bestaande state struct. De TUI leest precies dezelfde
velden als de huidige print-renderer:

- `repos: Vec<RepoStatus>` met alias, path, status, chunks, files, last_tool_call, lock_status
- `serve_url: String`
- `started_at: Instant`

### Shutdown

```rust
// Bij q/Ctrl+C:
ratatui::restore();         // alternate screen buffer afsluiten
shutdown_tx.send(()).await; // bestaande clean shutdown van serve
```

De bestaande shutdown logica blijft ongewijzigd — TUI is alleen een nieuwe
trigger source.

---

## 5. Layout

```
┌─ codesearch serve · http://127.0.0.1:39725 ──────── 12:34:56 ──┐
│                                                                  │
│  Alias                Status     Chunks  Files  Last call  Lock  │
│  ─────────────────────────────────────────────────────────────── │
│  codesearch           ✓ ready     1612     54   2s ago     read  │
│  HUSQ.Aprimo          ⟳ idx...   18243   1411   —          write │
│  BRU.Aprimo           ✓ ready    12109    876   45s ago    read  │
│  DPS                  ✓ ready      820     42   12m ago    avail │
│  ...                                                             │
│                                                                  │
├──────────────────────────────────────────────────────────────────┤
│ [q] quit  [↑↓] scroll                                            │
└──────────────────────────────────────────────────────────────────┘
```

Drie regions:
- Header: serve URL + huidige tijd (rechts)
- Body: tabel met alle repos
- Footer: keybinding hints

### Kleuren

- Status `ready` → groen
- Status `indexing`/`refresh` → geel met spinner unicode (`⟳`/`◐`)
- Status `error` → rood
- Lock `write` → cyaan
- Lock `read` → grijs
- Lock `available` → wit

### Scrollen

Als aantal repos > beschikbare hoogte: vertical scroll met `↑↓`. Indicator
rechts van de tabel toont positie (bv. `[3/12]`).

---

## 6. Concrete bestanden

Nieuw:
- `src/serve/tui.rs` — alle ratatui code: state binding, draw functie,
  event loop

Aangepast:
- `src/serve/mod.rs` — start de TUI task naast de bestaande tasks,
  verwijder de oude print+cursor render functie
- `Cargo.toml` — ratatui + crossterm dependencies

---

## 7. Definition of Done

- [ ] `cargo check --all-targets` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo test --lib` groen
- [ ] `codesearch serve` toont fullscreen TUI bij start
- [ ] Geen flikkering meer bij refresh van de tabel
- [ ] `q` of `Ctrl+C` triggert clean shutdown van serve
- [ ] Bij exit: terminal staat terug zoals voor serve werd gestart (geen vervuiling)
- [ ] Resize van terminal venster werkt zonder layout breaks
- [ ] Logs blijven naar file gaan (geen stdout interferentie)
- [ ] `cargo build --release` clean

---

## 8. Niet in scope

- Acties vanuit de TUI (`r` reindex, `f` pause watcher, `d` details panel) —
  later in v2
- Filter/zoek binnen de tabel
- Color theme configuration
- Web UI (separate feature)
- Alternative `--no-tui` mode (kan later toegevoegd worden als opt-out vlag)

---

## 9. Risico's en aandachtspunten

| Risico | Mitigatie |
|--------|-----------|
| Crossterm op Windows 11 + WezTerm/Windows Terminal | Beide ondersteunen ANSI volledig, ratatui werkt zonder extra config |
| TUI start op een non-TTY (bv. piped stdout) | Detecteer met `atty::is(Stream::Stdout)`. Als geen TTY: fallback naar oude print-mode of gewoon log "TUI disabled, no TTY" en draai zonder render |
| Ctrl+C handling onderbreekt `terminal.draw()` mid-frame | crossterm signal handling + ratatui's eigen restore in `Drop` impl regelen dit |
| Service mode (Windows Task Scheduler / systemd) heeft geen TTY | Zelfde detectie als boven — geen TUI als geen TTY |
| Conflict met bestaande status print code | Alle bestaande `print!`/`eprintln!` voor status display verwijderen — alleen tracing-naar-file blijft |

---

## 10. Implementatievolgorde

1. Add `ratatui` + `crossterm` deps, `cargo check`
2. Maak `src/serve/tui.rs` skeleton met empty draw + quit handling
3. Bind aan bestaande `ServeState`, render lege tabel
4. Implementeer header, footer, kleuren
5. Verwijder oude print+cursor render code uit `src/serve/mod.rs`
6. TTY detection + graceful fallback
7. Test handmatig: start, resize, quit, ctrl+c, geen TTY
8. `cargo clippy` + `cargo build --release`

Geschatte tijd: 1-2 uur voor MVP.

---

## 11. Commit message voorstel

```
feat(serve): TUI with ratatui replaces flickering status table

Replace direct print+cursor positioning in `codesearch serve` with a
ratatui-based TUI using crossterm backend.

- Alternate screen buffer (clean exit, no scrollback pollution)
- Virtual buffer + diff render eliminates flickering
- Auto-resize on terminal window changes
- Read-only in v1: q to quit, arrows for scroll
- TTY detection: falls back gracefully when no terminal (services, pipes)

Adds ratatui 0.29 and crossterm 0.28 dependencies.
```
