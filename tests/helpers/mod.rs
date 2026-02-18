//! Test helpers for codesearch integration tests.
//!
//! Provides utilities for creating temporary git repositories
//! with branches, commits, and file changes for testing.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// A test git repository with helpers for creating branches and commits.
pub struct TestRepo {
    /// Temporary directory containing the git repository
    pub dir: TempDir,
    /// Path to the repository root
    pub path: PathBuf,
}

impl TestRepo {
    /// Create a new git repository and initialize it.
    ///
    /// Sets up git config (user.name, user.email) for commits,
    /// creates initial files, and makes the first commit.
    pub fn new() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().to_path_buf();

        // Initialize git repo
        Self::run_git(&path, &["init"])?;

        // Configure git user for commits
        Self::run_git(&path, &["config", "user.name", "Test User"])?;
        Self::run_git(&path, &["config", "user.email", "test@example.com"])?;

        // Create initial files and commit
        let src_dir = path.join("src");
        fs::create_dir_all(&src_dir)?;

        // Create main.rs
        fs::write(
            src_dir.join("main.rs"),
            "fn main() {\n    println!(\"Hello, world!\");\n}\n\nfn greet(name: &str) -> String {\n    format!(\"Hello, {}!\", name)\n}\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )?;

        // Create lib.rs
        fs::write(
            src_dir.join("lib.rs"),
            "pub struct Calculator {\n    pub value: i32,\n}\n\nimpl Calculator {\n    pub fn new() -> Self {\n        Self { value: 0 }\n    }\n\n    pub fn add(&mut self, n: i32) {\n        self.value += n;\n    }\n}\n\npub struct Config {\n    pub debug: bool,\n}\n",
        )?;

        // Create Cargo.toml
        fs::write(
            path.join("Cargo.toml"),
            "[package]\nname = \"test-project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
        )?;

        // Initial commit
        Self::run_git(&path, &["add", "."])?;
        Self::run_git(&path, &["commit", "-m", "Initial commit"])?;

        Ok(Self { dir, path })
    }

    /// Create a new branch with the given changes.
    ///
    /// Creates a new branch from the current HEAD, applies the specified
    /// file changes, and commits them.
    ///
    /// # Arguments
    /// * `name` - Branch name to create
    /// * `changes` - List of (path, content) tuples to write as files
    pub fn create_branch(&self, name: &str, changes: &[(&str, &str)]) -> anyhow::Result<()> {
        // Create and checkout new branch
        Self::run_git(&self.path, &["checkout", "-b", name])?;

        // Apply changes
        for (path, content) in changes {
            let file_path = self.path.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&file_path, content)?;
        }

        // Stage and commit changes
        Self::run_git(&self.path, &["add", "."])?;
        Self::run_git(
            &self.path,
            &["commit", "-m", &format!("Changes in {}", name)],
        )?;

        Ok(())
    }

    /// Checkout the specified branch or reference.
    ///
    /// # Arguments
    /// * `name` - Branch name, commit hash, or reference to checkout
    pub fn checkout(&self, name: &str) -> anyhow::Result<()> {
        Self::run_git(&self.path, &["checkout", name])?;
        Ok(())
    }

    /// Get the current content of .git/HEAD file.
    ///
    /// This is useful for testing branch change detection.
    pub fn head_content(&self) -> String {
        fs::read_to_string(self.path.join(".git").join("HEAD")).unwrap_or_default()
    }

    /// Get the path to the codesearch database.
    ///
    /// Returns the path where `.codesearch.db` would be created.
    pub fn db_path(&self) -> PathBuf {
        self.path.join(".codesearch.db")
    }

    /// Get the path to a file in the repository.
    pub fn file_path(&self, relative_path: &str) -> PathBuf {
        self.path.join(relative_path)
    }

    /// Read the content of a file in the repository.
    pub fn read_file(&self, relative_path: &str) -> anyhow::Result<String> {
        Ok(fs::read_to_string(self.file_path(relative_path))?)
    }

    /// Write content to a file in the repository.
    pub fn write_file(&self, relative_path: &str, content: &str) -> anyhow::Result<()> {
        let file_path = self.file_path(relative_path);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file_path, content)?;
        Ok(())
    }

    /// Run a git command in the repository.
    fn run_git(cwd: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
        let output = Command::new("git").args(args).current_dir(cwd).output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_repo_creation() {
        let repo = TestRepo::new().unwrap();

        // Check that .git directory exists
        assert!(repo.path.join(".git").exists());

        // Check that initial files were created
        assert!(repo.path.join("src/main.rs").exists());
        assert!(repo.path.join("src/lib.rs").exists());
        assert!(repo.path.join("Cargo.toml").exists());

        // Check HEAD content (should be on main branch)
        let head = repo.head_content();
        assert!(head.contains("refs/heads/main"));
    }

    #[test]
    fn test_test_repo_create_branch() {
        let repo = TestRepo::new().unwrap();

        // Create a feature branch
        repo.create_branch(
            "feature",
            &[
                ("src/feature.rs", "pub fn feature() {}"),
                ("src/main.rs", "fn main() { feature(); }"),
            ],
        )
        .unwrap();

        // Checkout back to main
        repo.checkout("main").unwrap();

        // Feature file should not exist on main
        assert!(!repo.path.join("src/feature.rs").exists());

        // Checkout feature branch
        repo.checkout("feature").unwrap();

        // Feature file should exist on feature branch
        assert!(repo.path.join("src/feature.rs").exists());
    }

    #[test]
    fn test_test_repo_head_content() {
        let repo = TestRepo::new().unwrap();

        let head = repo.head_content();
        assert!(head.contains("refs/heads/main") || head.contains("refs/heads/master"));

        // Create a new branch
        repo.create_branch("test", &[]).unwrap();

        // Head should now point to test
        let head = repo.head_content();
        assert!(head.contains("refs/heads/test"));
    }
}
