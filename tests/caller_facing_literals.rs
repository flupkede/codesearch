//! Guard against caller-facing string literals with a collapsed line continuation.
//!
//! A Rust string literal wrapped across source lines needs a trailing `\`, or
//! the leading indentation of the next line becomes part of the message:
//!
//! ```ignore
//! // WRONG — renders as "in scope           failed"
//! "... {count} store(s) in scope
//!      failed, so ..."
//! ```
//!
//! Three separate commits on this branch shipped that defect, each time through
//! a review that was explicitly hunting it, because the mangled text still
//! satisfies every `contains(...)` assertion a test would make. Review is the
//! wrong instrument. This makes it a build failure.
//!
//! Scope: `src/` sources, string literals only, comments excluded.

use std::path::{Path, PathBuf};

/// Minimum interior space run treated as a swallowed continuation.
///
/// Chosen from evidence, not taste. Deliberate CLI column alignment in this
/// codebase uses 3-10 spaces (`"Model load:    {:?}"`, `"codesearch index add
/// <path>   # ..."`). A swallowed continuation instead reproduces the source
/// indentation of the wrapped line, which is 20+ spaces at the nesting depth
/// where these messages live. Twelve sits in the empty gap between the two, so
/// the guard fires on real defects without punishing formatted output.
const MIN_COLLAPSED_RUN: usize = 12;

/// Return the offending literal when it holds a long interior run of spaces.
/// Leading and trailing runs are fine — only an interior run between two
/// non-space characters indicates a swallowed continuation.
fn collapsed_run(literal: &str) -> Option<String> {
    let chars: Vec<char> = literal.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            let start = i;
            while i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
            if i - start >= MIN_COLLAPSED_RUN && start > 0 && i < chars.len() {
                return Some(literal.trim().to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Extract the string literals on one source line, honouring `\` escapes.
fn literals_on_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    let mut buf = String::new();

    for ch in line.chars() {
        if escaped {
            escaped = false;
            if in_str {
                buf.push(ch);
            }
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => {
                if in_str {
                    out.push(std::mem::take(&mut buf));
                }
                in_str = !in_str;
            }
            _ if in_str => buf.push(ch),
            _ => {}
        }
    }
    out
}

fn rust_sources(dir: &Path, acc: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, acc);
        } else if path.extension().is_some_and(|e| e == "rs") {
            acc.push(path);
        }
    }
}

#[test]
fn no_source_literal_has_a_collapsed_line_continuation() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src_root, &mut files);

    assert!(
        !files.is_empty(),
        "found no sources under {} — a scan that cannot fail proves nothing",
        src_root.display()
    );

    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Comments legitimately contain aligned prose, and this file's own
            // doc comment shows the wrong pattern on purpose.
            if trimmed.starts_with("//") {
                continue;
            }
            for literal in literals_on_line(line) {
                if let Some(bad) = collapsed_run(&literal) {
                    violations.push(format!(
                        "  {}:{} -> {}",
                        file.strip_prefix(&src_root).unwrap_or(file).display(),
                        i + 1,
                        bad
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "string literal(s) with a collapsed line continuation; the wrapped line \
         needs a trailing backslash:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_detector_can_actually_fail() {
    // A clean result from the scan above is only meaningful if the detector is
    // capable of returning a dirty one. Positive control.
    // Both real defects from this branch, verbatim.
    assert!(collapsed_run("{count} store(s) in scope                      failed").is_some());
    assert!(collapsed_run("Verify the                      chunk_id and index state.").is_some());

    // And must not fire on deliberate CLI column alignment, which is why the
    // threshold is 12 and not 3.
    assert!(collapsed_run("no similar chunks found").is_none());
    assert!(collapsed_run("  leading and trailing runs are fine   ").is_none());
    assert!(collapsed_run("a  b").is_none(), "two spaces is not a wrap");
    assert!(collapsed_run("Model load:    {:?}").is_none());
    assert!(collapsed_run("codesearch index add <path>   # a comment").is_none());
    assert!(collapsed_run("codesearch index add          # register cwd").is_none());

    assert_eq!(
        literals_on_line(r#"let a = "x  y"; // "not a literal""#).len(),
        2,
        "the extractor must see both literals on a line"
    );
}
