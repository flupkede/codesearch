//! Wire-shape guard for the SEP-2322 `resultType` discriminator.
//!
//! Pins the behavior of the `[codesearch bridge]` block in
//! `vendor/rmcp/src/service/server.rs` (`negotiate_protocol_version`) at the
//! wire level, so the next rmcp bump that loses the bridge fails loudly here
//! instead of silently reintroducing the Claude Code >= 2.1.296 breakage
//! ("Invalid result for tools/list: missing required resultType").
//!
//! Runs the real `codesearch mcp --mode local` binary over stdio, exactly the
//! way an MCP client drives it, and asserts both wire shapes:
//!
//! - initialize offering `2026-07-28`: session negotiated at `2026-07-28` and
//!   every `tools/list` result carries `"resultType":"complete"`;
//! - initialize offering `2025-11-25`: stock legacy negotiation, `tools/list`
//!   keeps the historical shape (no `resultType` field at all).

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

/// Drive the real MCP server over stdio: initialize with `offer`, then
/// tools/list. Returns every JSON-RPC response line, parsed.
fn probe(offer: &str) -> Vec<Value> {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .args(["mcp", "--mode", "local"])
        .current_dir(tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codesearch mcp");

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": offer,
            "capabilities": {},
            "clientInfo": {"name": "bridge-test", "version": "0.0.1"}
        }
    });
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let list_tools = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in [initialize, initialized, list_tools] {
            writeln!(stdin, "{line}").expect("write request");
        }
        stdin.flush().expect("flush stdin");
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("run codesearch mcp");
    assert!(
        output.status.success(),
        "codesearch mcp exited with {:?}",
        output.status
    );

    String::from_utf8(output.stdout)
        .expect("utf-8 stdout")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json-rpc response line"))
        .collect()
}

fn find_response(responses: &[Value], id: i64) -> &Value {
    responses
        .iter()
        .find(|r| r.get("id") == Some(&json!(id)))
        .unwrap_or_else(|| panic!("no response with id {id} in {responses:?}"))
}

#[test]
fn session_on_2026_07_28_keeps_result_type() {
    let responses = probe("2026-07-28");

    let init = find_response(&responses, 1);
    assert_eq!(
        init["result"]["protocolVersion"], "2026-07-28",
        "bridge must honor a 2026-07-28 offer instead of downgrading"
    );

    let tools = find_response(&responses, 2);
    assert_eq!(
        tools["result"]["resultType"], "complete",
        "tools/list must carry the SEP-2322 discriminator on a 2026-07-28 session"
    );
}

#[test]
fn legacy_session_keeps_historical_wire_shape() {
    let responses = probe("2025-11-25");

    let init = find_response(&responses, 1);
    assert_eq!(
        init["result"]["protocolVersion"], "2025-11-25",
        "stock negotiation must stay untouched for legacy clients"
    );

    let tools = find_response(&responses, 2);
    assert!(
        tools["result"].get("resultType").is_none(),
        "legacy sessions must keep the historical wire shape without resultType"
    );
}
