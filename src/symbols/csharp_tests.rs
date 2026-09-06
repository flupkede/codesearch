//! Helper stderr routing tests (`csharp.rs`). Sibling `_tests.rs` file
//! per repo convention.

use super::csharp::{drain_pipe_to_tracing, is_helper_warning_line};
use std::io::Cursor;
use std::sync::Mutex;

fn drained(input: &[u8]) -> Vec<String> {
    let out: Mutex<Vec<String>> = Mutex::new(Vec::new());
    drain_pipe_to_tracing(Cursor::new(input.to_vec()), |line| {
        out.lock().expect("test mutex").push(line.to_string());
    });
    out.into_inner().expect("test mutex")
}

#[test]
fn drain_survives_bad_bytes_and_strips_eol() {
    let mut raw = b"first line\n".to_vec();
    raw.extend_from_slice(b"bad \xFF\xFE bytes\n"); // invalid UTF-8 mid-stream
    raw.extend_from_slice(b"crlf line\r\n");
    raw.extend_from_slice(b"no trailing newline");

    let lines = drained(&raw);

    // The defect this pins: lines().map_while(Result::ok) ended the drain
    // permanently at the non-UTF-8 line, silently dropping lines 3-4 and
    // eventually stalling the pipe. All four lines must come through.
    assert_eq!(lines.len(), 4, "drain stopped early: {lines:?}");
    assert_eq!(lines[0], "first line");
    assert!(
        lines[1].starts_with("bad ") && lines[1].ends_with(" bytes"),
        "expected lossy-decoded line, got: {:?}",
        lines[1]
    );
    assert_eq!(lines[2], "crlf line");
    assert_eq!(lines[3], "no trailing newline");
}

#[test]
fn drain_stops_at_eof_and_handles_empty_pipe() {
    assert!(drained(b"").is_empty());
    assert_eq!(drained(b"only\n"), vec!["only".to_string()]);
}

/// Read impl whose first read fails with `Interrupted` (EINTR), then
/// behaves like a normal pipe.
struct InterruptedOnce {
    armed: bool,
    inner: Cursor<Vec<u8>>,
}

impl std::io::Read for InterruptedOnce {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.armed {
            self.armed = false;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "eintr",
            ));
        }
        self.inner.read(buf)
    }
}

#[test]
fn drain_retries_after_transient_interrupted_read() {
    // Contract: a transient Interrupted read must not end the drain. std's
    // own read_until already retries Interrupted internally (rustc 1.98,
    // library/std/src/io/mod.rs), so the local arm in drain_pipe_to_tracing
    // is belt-and-braces and unreachable via its internal BufReader — this
    // test pins the observable contract, not that arm.
    let mut out: Vec<String> = Vec::new();
    drain_pipe_to_tracing(
        InterruptedOnce {
            armed: true,
            inner: Cursor::new(b"before\nafter\n".to_vec()),
        },
        |line| out.push(line.to_string()),
    );
    assert_eq!(out, vec!["before".to_string(), "after".to_string()]);
}

#[test]
fn helper_warning_classification_table() {
    let cases: [(&str, bool); 10] = [
        // Helper's own workspace-failure warnings (WorkspaceFailed handler).
        (
            "[WARN] Workspace error: [Failure] Msbuild failed when processing the file 'C:\\r\\p.csproj' with message: Dependency specified was X but ended up with X 1.2.3",
            true,
        ),
        (
            "[WARN] Solution load partially failed (InvalidOperationException: boom); continuing with 13 loaded project(s).",
            true,
        ),
        (
            "serve: [WARN] no projects loaded — cannot serve.",
            true,
        ),
        // Bare MSBuildWorkspace diagnostic without the helper prefix.
        (
            "[Failure] Msbuild failed when processing the file 'C:\\r\\p.csproj'",
            true,
        ),
        ("[WARN] Project file not found: C:\\r\\missing.csproj", true),
        // Progress / info lines must NOT escalate to warn.
        (
            "[INFO] Skipping unsupported project type: C:\\r\\x.shproj",
            false,
        ),
        ("Loading solution: C:\\r\\x.sln", false),
        ("Loaded 13 project(s) from filtered solution", false),
        (
            "MSBuild: registering '.NET SDK' v10.0.303 at C:\\Program Files\\dotnet\\sdk\\10.0.303",
            false,
        ),
        ("Index written to: C:\\tmp\\out.json", false),
    ];

    for (line, expected) in cases {
        assert_eq!(
            is_helper_warning_line(line),
            expected,
            "misclassified line: {line}"
        );
    }
}
