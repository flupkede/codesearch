//! Guard against flattening a store `Err` into a missing hit on MCP
//! resolution paths ("search errors must not become empty results").
//!
//! `VectorStore::get_chunk` returns `Result<Option<Chunk>>`: `Ok(None)` is a
//! true miss ("this store does not hold that chunk"), `Err` is a broken
//! store. Collapsing the two makes a dead store render as an ordinary
//! empty or short result — the most misleading signal this system can emit.
//! This exact defect was fixed in sibling handlers and left in others four
//! times across review rounds (see AGENTS.md); review is the wrong
//! instrument, so this makes it a build failure.
//!
//! Two textual manifestations, both banned on a DIRECT `get_chunk` call:
//!
//! * **Rule A — `.ok()` on the call.** `store.get_chunk(id).ok()??` inside a
//!   `filter_map` silently drops the hit. Binding first
//!   (`let looked_up = store.get_chunk(id);` → note the `Err` → use
//!   `looked_up.ok()`) is compliant — only the direct call is flagged.
//! * **Rule B — `if let Ok(Some(..)) = store.get_chunk(..)`.** The `Err` arm
//!   vanishes into the else. Binding first and matching on the bound value
//!   (after noting the `Err`) is compliant.
//!
//! Limitation (deliberate): line-based, like `caller_facing_literals.rs`.
//! A call wrapped across lines so the `.ok()` lands on the next line escapes
//! Rule A; the compliant population is written on one line and rustfmt keeps
//! it that way at these widths.

use std::fs;
use std::path::{Path, PathBuf};

/// MCP handler sources: everything under `src/mcp/` except the sibling test
/// files, which construct fixtures and may name the APIs directly.
fn mcp_sources(repo_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let dir = repo_root.join("src").join("mcp");
    let Ok(entries) = fs::read_dir(&dir) else {
        panic!("cannot read {}", dir.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && !name.ends_with("_tests.rs")
            && name != "tests.rs"
        {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn repo_root() -> PathBuf {
    // Integration tests run with CWD = repo root.
    std::env::current_dir().expect("cwd")
}

fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty() && !t.starts_with("//") && !t.starts_with("///")
}

#[test]
fn no_direct_get_chunk_flattened_on_mcp_paths() {
    let mut violations: Vec<String> = Vec::new();
    for file in mcp_sources(&repo_root()) {
        let src = fs::read_to_string(&file).expect("readable");
        for (idx, line) in src.lines().enumerate() {
            if !is_code(line) {
                continue;
            }
            if line.contains(".get_chunk(") && line.contains(".ok()") {
                violations.push(format!(
                    "{}:{}: direct get_chunk flattened with .ok(): {}",
                    file.display(),
                    idx + 1,
                    line.trim()
                ));
            }
            if line.contains("if let Ok(Some(") && line.contains("= store.get_chunk(") {
                violations.push(format!(
                    "{}:{}: get_chunk Err flattened in if-let scrutinee: {}",
                    file.display(),
                    idx + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "store Err collapsed into a missing hit (bind the result, note the \
         Err via note_store_failure or propagate it with `?`):\n{}",
        violations.join("\n")
    );
}
