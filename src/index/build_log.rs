//! Clean build log for `index` / `index add` builds.
//!
//! When stdout is not a TTY (CI, redirected output, agents), the console
//! progress bar mangles output with `\r` redraws. In that mode a plain,
//! line-oriented `build.log` is written next to the DB instead; a
//! `--log-file <path>` override forces it on regardless of TTY state.

use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// True when stdout is attached to an interactive terminal.
pub fn stdout_is_interactive() -> bool {
    std::io::stdout().is_terminal()
}

/// Append-only structured build log (no progress-bar redraw spam).
pub struct BuildLog {
    file: Option<File>,
    path: PathBuf,
}

impl BuildLog {
    /// Open the build log for a project.
    ///
    /// - `override_path` (from `--log-file`): always log to this file.
    /// - Otherwise: log to `<project_root>/.codesearch.db/build.log` only when
    ///   stdout is not a TTY; on an interactive console no log is written and
    ///   console output behaves exactly as before.
    pub fn open(project_root: &Path, override_path: Option<&Path>) -> Result<Self> {
        let path = match override_path {
            Some(p) => p.to_path_buf(),
            None => {
                if stdout_is_interactive() {
                    return Ok(Self {
                        file: None,
                        path: PathBuf::new(),
                    });
                }
                project_root
                    .join(crate::constants::DB_DIR_NAME)
                    .join("build.log")
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }

    /// The log file path when logging is active, for reporting to the user.
    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|_| self.path.as_path())
    }

    /// Write one structured line. Best-effort: logging must never fail a build.
    /// ANSI color codes are stripped so the file stays plain text — callers
    /// pass pre-styled (`colored`) strings meant for the console.
    pub fn line(&self, msg: impl AsRef<str>) {
        if let Some(file) = &self.file {
            let mut file = file;
            let _ = writeln!(file, "{}", strip_ansi(msg.as_ref()));
            let _ = file.flush();
        }
    }
}

/// Remove ANSI escape sequences (CSI: ESC `[` params final-byte).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            // Consume up to and including the final byte (@-~)
            for f in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&f) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
