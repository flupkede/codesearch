//! Guard against caller-facing string literals that were wrapped across source
//! lines without a `\` continuation.
//!
//! A Rust string literal spanning two source lines needs a trailing backslash,
//! or the next line's indentation becomes part of the message. Three separate
//! commits on this branch shipped that defect, each through a review explicitly
//! hunting it, because the mangled text still satisfies every `contains(...)`
//! assertion a test would make. Review is the wrong instrument; this makes it a
//! build failure.
//!
//! Two independent rules, because the defect has two manifestations:
//!
//! * **Rule A — embedded newline.** A non-raw literal containing a real newline
//!   is a wrap with no continuation. Exact, threshold-free, and independent of
//!   how deeply the code is nested.
//! * **Rule B — collapsed run.** When the continuation *was* present but the
//!   edit swallowed it, the newline is gone and only a long run of spaces
//!   remains. Needs a threshold; see `MIN_COLLAPSED_RUN`.
//!
//! Rule A alone would have missed both defects that actually occurred here;
//! Rule B alone misses the canonical wrap at shallow nesting. Both are needed.
//!
//! Scope: `src/` sources. Comments, raw strings and byte strings are excluded —
//! see `scan_source` for why each is handled the way it is.

use std::path::{Path, PathBuf};

/// Minimum interior space run treated as a swallowed continuation.
///
/// Derived from the source, not chosen by taste. Measured over `src/`, runs of
/// deliberate CLI column alignment (`"Model load:    {:?}"`, `"codesearch index
/// add          # register current directory"`) top out at exactly 10, and
/// nothing legitimate sits at 11 or above. A swallowed continuation instead
/// reproduces the wrapped line's indentation, 20+ at the depth these messages
/// live at. Twelve sits inside the empty gap.
///
/// The limitation is real and is why Rule A exists: a continuation indented
/// ≤10 spaces is arithmetically indistinguishable from column alignment, and
/// lowering the threshold to catch it would collide with the legitimate
/// population. Rule A catches that case without depending on indentation.
const MIN_COLLAPSED_RUN: usize = 12;

#[derive(Debug, PartialEq)]
enum Violation {
    /// Rule A: literal spans lines with no `\` continuation.
    EmbeddedNewline,
    /// Rule B: continuation was swallowed, leaving a run of spaces.
    CollapsedRun(String),
}

/// Return the offending text when `literal` holds a long interior run of
/// spaces. Leading and trailing runs are fine — only a run between two
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

/// Lex a whole source file and report violating literals as `(line, violation)`.
///
/// Whole-file rather than line-by-line: the canonical defect is a literal split
/// across two lines, which a per-line scanner cannot see at all — it observes an
/// unterminated quote on one line and an unopened one on the next, and emits
/// nothing. That blind spot let a real mangling through a previous version of
/// this guard.
fn scan_source(src: &str) -> Vec<(usize, Violation)> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1usize;

    while i < chars.len() {
        let c = chars[i];

        // Line comment — skip to end of line. Comments legitimately contain
        // aligned prose, and this file's own docs show the wrong pattern.
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comment — nesting is legal in Rust.
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < chars.len() && depth > 0 {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        // Char literal — `'"'` would otherwise desynchronise quote pairing and
        // silently hide every literal after it on that line.
        if c == '\'' && is_char_literal(&chars, i) {
            i += 1;
            if chars.get(i) == Some(&'\\') {
                i += 1;
            }
            i += 1; // the character itself
            if chars.get(i) == Some(&'\'') {
                i += 1;
            }
            continue;
        }
        // Raw string — no escapes, so a newline inside is intentional (SQL,
        // embedded templates). Skipped entirely rather than reported.
        if let Some(next) = raw_string_end(&chars, i) {
            for c in chars.iter().take(next).skip(i) {
                if *c == '\n' {
                    line += 1;
                }
            }
            i = next;
            continue;
        }
        // Normal or byte string literal.
        if c == '"' {
            let start_line = line;
            let mut buf = String::new();
            let mut has_newline = false;
            i += 1;
            while i < chars.len() {
                match chars[i] {
                    '\\' => {
                        // A continuation (`\` then newline) is the CORRECT form:
                        // consume it without recording a newline. Must tolerate
                        // CRLF — this repo checks out with `\r\n`, and treating
                        // the `\r` as content made every correct continuation in
                        // the tree look like a violation.
                        let mut k = i + 1;
                        if chars.get(k) == Some(&'\r') {
                            k += 1;
                        }
                        if chars.get(k) == Some(&'\n') {
                            line += 1;
                            i = k + 1;
                            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                                i += 1;
                            }
                            continue;
                        }
                        buf.push(chars[i]);
                        if let Some(n) = chars.get(i + 1) {
                            buf.push(*n);
                        }
                        i += 2;
                    }
                    '"' => {
                        i += 1;
                        break;
                    }
                    '\n' => {
                        has_newline = true;
                        line += 1;
                        buf.push('\n');
                        i += 1;
                    }
                    ch => {
                        buf.push(ch);
                        i += 1;
                    }
                }
            }
            if has_newline {
                out.push((start_line, Violation::EmbeddedNewline));
            } else if let Some(bad) = collapsed_run(&buf) {
                out.push((start_line, Violation::CollapsedRun(bad)));
            }
            continue;
        }

        if c == '\n' {
            line += 1;
        }
        i += 1;
    }
    out
}

/// True when the quote at `i` opens a char literal rather than a lifetime.
fn is_char_literal(chars: &[char], i: usize) -> bool {
    // `'a` (lifetime) has no closing quote within 3 chars; `'x'` and `'\n'` do.
    matches!(chars.get(i + 2), Some(&'\'')) || matches!(chars.get(i + 3), Some(&'\''))
}

/// If a raw string starts at `i`, return the index just past its end.
fn raw_string_end(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = {
        let start = j;
        while chars.get(j) == Some(&'#') {
            j += 1;
        }
        j - start
    };
    if chars.get(j) != Some(&'"') {
        return None;
    }
    j += 1;
    let closing: String = std::iter::once('"')
        .chain(std::iter::repeat_n('#', hashes))
        .collect();
    let closing: Vec<char> = closing.chars().collect();
    while j < chars.len() {
        if chars[j] == '"' && chars[j..].starts_with(&closing[..]) {
            return Some(j + closing.len());
        }
        j += 1;
    }
    Some(chars.len())
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
fn no_source_literal_was_wrapped_without_a_continuation() {
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
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let rel = file.strip_prefix(&src_root).unwrap_or(file).display();
        for (line, v) in scan_source(&text) {
            violations.push(match v {
                Violation::EmbeddedNewline => {
                    format!("  {rel}:{line} -> literal spans lines with no `\\` continuation")
                }
                Violation::CollapsedRun(text) => {
                    format!("  {rel}:{line} -> collapsed continuation: {text}")
                }
            });
        }
    }

    assert!(
        violations.is_empty(),
        "caller-facing literal(s) wrapped without a trailing backslash:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_detector_can_actually_fail() {
    // A clean scan is meaningless unless something proves the scan can come
    // back dirty. Both real defects from this branch, verbatim.
    assert!(collapsed_run("{count} store(s) in scope                      failed").is_some());
    assert!(collapsed_run("Verify the                      chunk_id and index state.").is_some());

    // Rule B must not fire on deliberate CLI column alignment — which is why
    // the threshold is 12 and not 3.
    assert!(collapsed_run("no similar chunks found").is_none());
    assert!(collapsed_run("  leading and trailing runs are fine   ").is_none());
    assert!(collapsed_run("a  b").is_none(), "two spaces is not a wrap");
    assert!(collapsed_run("Model load:    {:?}").is_none());
    assert!(collapsed_run("codesearch index add          # register cwd").is_none());
}

#[test]
fn rule_a_catches_the_wrap_that_rule_b_cannot() {
    // The canonical defect: literal split across lines, no continuation. A
    // per-line scanner sees an unterminated quote then an unopened one and
    // emits nothing — this is the blind spot that let a real mangling through.
    let src = "fn f() { let m = \"No indexed chunks found for path. Verify the\n         file is within the project root.\"; }";
    assert_eq!(
        scan_source(src),
        vec![(1, Violation::EmbeddedNewline)],
        "a literal wrapped across lines must be caught regardless of indent depth"
    );

    // And it must catch it at SHALLOW indentation, where Rule B's threshold
    // cannot distinguish it from column alignment.
    let shallow = "fn f() { let m = \"first half\n   second half\"; }";
    assert_eq!(scan_source(shallow), vec![(1, Violation::EmbeddedNewline)]);

    // CRLF: tolerating `\r` in the continuation must not also make an
    // UNcontinued CRLF wrap invisible. This repo checks out with `\r\n`, so a
    // blind spot here would be a blind spot everywhere.
    let crlf = "fn f() { let m = \"first half\r\n   second half\"; }";
    assert_eq!(
        scan_source(crlf),
        vec![(1, Violation::EmbeddedNewline)],
        "a CRLF wrap with no continuation is still a violation"
    );
    let crlf_ok = "fn f() { let m = \"first half \\\r\n         second half\"; }";
    assert_eq!(
        scan_source(crlf_ok),
        vec![],
        "a CRLF wrap WITH a continuation is correct and must stay silent"
    );
}

#[test]
fn the_lexer_is_not_desynchronised_by_awkward_source() {
    // A `'"'` char literal must not swallow the rest of the line: a previous
    // version paired that quote with the next one and silently skipped a real
    // mangling.
    let src = "fn f() { let q = '\"'; let m = \"a                      b\"; }";
    assert_eq!(
        scan_source(src),
        vec![(
            1,
            Violation::CollapsedRun("a                      b".to_string())
        )],
        "char literal containing a quote must not hide the literal after it"
    );

    // A trailing comment with aligned prose is not a violation. The old scanner
    // only skipped comments at line start, so this failed the build.
    let ok = "let x = 1; // aligned                      like this\n";
    assert_eq!(scan_source(ok), vec![]);

    // Raw strings legitimately contain newlines.
    let raw = "let sql = r#\"SELECT a\nFROM b\"#;\n";
    assert_eq!(scan_source(raw), vec![]);

    // A correct continuation is the whole point — it must stay silent.
    let good = "let m = \"first half \\\n         second half\";\n";
    assert_eq!(scan_source(good), vec![]);

    // Lifetimes must not be mistaken for char literals.
    let lifetime = "fn f<'a>(x: &'a str) -> &'a str { x }\n";
    assert_eq!(scan_source(lifetime), vec![]);
}
