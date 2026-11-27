//! `codesearch hooks opencode install` — installs the OpenCode plugin
//! (`integrations/opencode/codesearch.ts`) into the directory OpenCode
//! auto-loads plugins from.
//!
//! The plugin is embedded at compile time (single source of truth remains
//! `integrations/opencode/codesearch.ts`) so the shipped binary is
//! self-contained — same contract as the claude installer. Unlike the claude
//! installer nothing is merged: the plugin is one file, a rerun overwrites it.
//!
//! `<plugins_dir>` is `~/.config/opencode/plugins` (user scope) or
//! `./.opencode/plugins` (`--project`).

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;

/// The embedded plugin source — the repo file itself, so embed and source
/// cannot drift.
static PLUGIN_TS: &str = include_str!("../../integrations/opencode/codesearch.ts");

const PLUGIN_NAME: &str = "codesearch.ts";

fn plugins_dir(project: bool) -> Result<std::path::PathBuf> {
    if project {
        Ok(std::env::current_dir()
            .context("could not determine current directory")?
            .join(".opencode")
            .join("plugins"))
    } else {
        Ok(dirs::home_dir()
            .context("could not determine home directory")?
            .join(".config")
            .join("opencode")
            .join("plugins"))
    }
}

/// Write the plugin into `plugins_dir` (created when missing); returns the
/// written file. Injected directory keeps the tests off the real home.
pub fn run_opencode_install_with(plugins_dir: &Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(plugins_dir)
        .with_context(|| format!("creating {}", plugins_dir.display()))?;
    let file = plugins_dir.join(PLUGIN_NAME);
    std::fs::write(&file, PLUGIN_TS).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

pub fn run_opencode_install(project: bool) -> Result<()> {
    let file = run_opencode_install_with(&plugins_dir(project)?)?;
    eprintln!(
        "{}",
        format!("✓ OpenCode plugin installed: {}", file.display()).green()
    );
    eprintln!("  Restart OpenCode (or start a new session) for the plugin to load.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_the_plugin_and_a_rerun_is_byte_identical() {
        let dir = std::env::temp_dir().join(format!("cs-oc-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = run_opencode_install_with(&dir).expect("first install");
        assert!(file.ends_with(PLUGIN_NAME));
        let first = std::fs::read_to_string(&file).expect("read plugin");
        assert!(first.contains("export default"), "not the plugin source");
        run_opencode_install_with(&dir).expect("rerun");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), first);
        std::fs::remove_dir_all(&dir).ok();
    }
}
