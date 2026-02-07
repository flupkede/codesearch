use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Get the version from Cargo.toml
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap();

    // Get git commit count
    let commit_count = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "0".to_string());

    // Get current git commit hash (short)
    let commit_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Get current branch name
    let branch_name = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Construct full version string
    let version_full = format!("{}+{}", cargo_version, commit_count);

    // Set environment variables for the main binary
    println!("cargo:rustc-env=DEMONGREP_VERSION_FULL={}", version_full);
    println!("cargo:rustc-env=DEMONGREP_COMMIT_HASH={}", commit_hash);
    println!("cargo:rustc-env=DEMONGREP_COMMIT_COUNT={}", commit_count);
    println!("cargo:rustc-env=DEMONGREP_BRANCH={}", branch_name);

    // Also set for display in --version output
    println!("cargo:rustc-env=CARGO_PKG_VERSION_FULL={}", version_full);
}
