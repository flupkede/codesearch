//! Logging module for codesearch
//!
//! Provides centralized logging configuration with:
//! - Daily log file rotation (via tracing-appender)
//! - Periodic cleanup of old log files (by age and count)
//! - Per-database log storage in .codesearch.db/logs/
//! - Configurable via environment variables
//!
//! Daily rotation creates files named `codesearch.log.YYYY-MM-DD`.
//! Cleanup removes files older than `retention_days` and enforces `max_files`.

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::constants::{DEFAULT_LOG_MAX_FILES, DEFAULT_LOG_RETENTION_DAYS, LOG_DIR_NAME, LOG_FILE_NAME};

/// Result of logger initialization, indicating whether file logging is active
#[derive(Debug)]
pub enum LoggerInitResult {
    /// File logging successfully initialized (with optional console output)
    FileLogging,
    /// Subscriber already set, only console logging active (fallback)
    ConsoleOnly,
}

/// Log level configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Parse from string (case-insensitive)
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" => Some(LogLevel::Debug),
            "trace" => Some(LogLevel::Trace),
            _ => None,
        }
    }

    /// Convert to string
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// Log rotation configuration
#[derive(Debug, Clone)]
pub struct LogRotationConfig {
    /// Maximum number of log files to retain
    pub max_files: usize,
    /// Number of days to retain log files
    pub retention_days: i64,
}

impl LogRotationConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        Self {
            max_files: std::env::var("CODESEARCH_LOG_MAX_FILES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOG_MAX_FILES),
            retention_days: std::env::var("CODESEARCH_LOG_RETENTION_DAYS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOG_RETENTION_DAYS as i64),
        }
    }
}

/// Get the log directory path for a given database path
pub fn get_log_dir(db_path: &Path) -> PathBuf {
    db_path.join(LOG_DIR_NAME)
}

/// Ensure the log directory exists
pub fn ensure_log_dir(log_dir: &Path) -> Result<()> {
    if !log_dir.exists() {
        fs::create_dir_all(log_dir)?;
        tracing::debug!("Created log directory: {:?}", log_dir);
    }
    Ok(())
}

/// Try to extract a date from a daily-rotated log filename.
///
/// tracing-appender DAILY rotation produces files named `<prefix>.YYYY-MM-DD`.
/// Returns `None` if the filename doesn't match the expected pattern.
fn parse_log_date(file_name: &str) -> Option<NaiveDate> {
    // Pattern: "codesearch.log.YYYY-MM-DD"
    let suffix = file_name.strip_prefix(&format!("{}.", LOG_FILE_NAME))?;
    NaiveDate::parse_from_str(suffix, "%Y-%m-%d").ok()
}

/// Remove old log files based on retention period and max file count.
///
/// Two independent criteria:
/// 1. Files older than `retention_days` are always removed.
/// 2. If more than `max_files` remain, the oldest are removed.
pub fn cleanup_old_logs(log_dir: &Path, config: &LogRotationConfig) -> Result<()> {
    if !log_dir.exists() {
        return Ok(());
    }

    let today = Utc::now().date_naive();

    // Collect dated log files: (date, path)
    let mut dated_files: Vec<(NaiveDate, PathBuf)> = Vec::new();

    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(date) = parse_log_date(file_name) {
                dated_files.push((date, path));
            }
        }
    }

    // Sort by date, oldest first
    dated_files.sort_by_key(|(date, _)| *date);

    let mut removed_count = 0u32;

    // Pass 1: remove files older than retention_days
    dated_files.retain(|(date, path)| {
        let age_days = (today - *date).num_days();
        if age_days > config.retention_days {
            if let Err(e) = fs::remove_file(path) {
                tracing::warn!("Failed to remove old log file {:?}: {}", path, e);
            } else {
                tracing::debug!("Removed old log file {:?} (age: {} days)", path, age_days);
                removed_count += 1;
            }
            false // remove from list
        } else {
            true // keep in list
        }
    });

    // Pass 2: enforce max_files (remove oldest beyond the limit)
    if dated_files.len() > config.max_files {
        let excess = dated_files.len() - config.max_files;
        for (_, path) in dated_files.iter().take(excess) {
            if let Err(e) = fs::remove_file(path) {
                tracing::warn!("Failed to remove excess log file {:?}: {}", path, e);
            } else {
                tracing::debug!("Removed excess log file {:?}", path);
                removed_count += 1;
            }
        }
    }

    if removed_count > 0 {
        tracing::info!(
            "Log cleanup: removed {} file(s) (retention={}d, max_files={})",
            removed_count,
            config.retention_days,
            config.max_files
        );
    }

    Ok(())
}

/// Initialize the logger with file rotation and optional console output.
///
/// # Arguments
/// * `db_path` - Path to the database directory (logs stored in `db_path/logs/`)
/// * `log_level` - Log level to use
/// * `quiet` - If true, suppress console output (log only to file)
///
/// # Returns
/// Returns `LoggerInitResult` indicating whether file logging is active:
/// - `FileLogging`: File logging successfully initialized
/// - `ConsoleOnly`: Subscriber already set, fallback to console-only
///
/// Uses `try_init()` so it won't panic if a subscriber is already set
/// (e.g. early console-only subscriber from main.rs).
pub fn init_logger(
    db_path: &Path,
    log_level: LogLevel,
    quiet: bool,
) -> Result<LoggerInitResult> {
    let log_dir = get_log_dir(db_path);
    ensure_log_dir(&log_dir)?;

    let config = LogRotationConfig::from_env();

    // Create file appender with DAILY rotation.
    // Produces files like: logs/codesearch.log.2026-02-09
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &log_dir, LOG_FILE_NAME);

    // Build EnvFilter with per-crate directives.
    // Specific crate directives override the default level.
    let filter_str = format!(
        "{level},tantivy=warn,arroy=warn,ort=warn,h2=warn,hyper=warn,tower=warn",
        level = log_level.as_str()
    );
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&filter_str));

    let subscriber = tracing_subscriber::registry().with(env_filter);

    if quiet {
        // File-only logging (MCP mode: keep stdout clean for JSON-RPC)
        let result = subscriber
            .with(
                fmt::layer()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(false),
            )
            .try_init();

        if let Err(e) = result {
            eprintln!("Logger: subscriber already set ({}), file logging not active", e);
            return Ok(LoggerInitResult::ConsoleOnly);
        }
    } else {
        // Console (stderr) + file logging
        let result = subscriber
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(true)
                    .with_target(true)
                    .with_thread_ids(false),
            )
            .with(
                fmt::layer()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(false),
            )
            .try_init();

        if let Err(e) = result {
            eprintln!("Logger: subscriber already set ({}), file logging not active", e);
            return Ok(LoggerInitResult::ConsoleOnly);
        }
    }

    tracing::info!(
        "Logger initialized: level={}, log_dir={:?}, max_files={}, retention_days={}",
        log_level.as_str(),
        log_dir,
        config.max_files,
        config.retention_days,
    );

    Ok(LoggerInitResult::FileLogging)
}

/// Start periodic log cleanup task.
///
/// Runs every `CODESEARCH_LOG_CLEANUP_INTERVAL_HOURS` hours (default: 24)
/// and removes old log files based on retention_days and max_files.
pub fn start_cleanup_task(
    log_dir: PathBuf,
    config: LogRotationConfig,
    cancel_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let cleanup_interval_hours: u64 = std::env::var("CODESEARCH_LOG_CLEANUP_INTERVAL_HOURS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        let interval = std::time::Duration::from_secs(cleanup_interval_hours * 3600);

        tracing::info!(
            "Log cleanup task started: interval={}h, retention_days={}, max_files={}",
            cleanup_interval_hours,
            config.retention_days,
            config.max_files,
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = cleanup_old_logs(&log_dir, &config) {
                        tracing::error!("Failed to cleanup old logs: {}", e);
                    }
                }
                _ = cancel_token.cancelled() => {
                    tracing::info!("Log cleanup task stopped");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_log_level_from_str() {
        assert_eq!(LogLevel::from_str("error"), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_str("ERROR"), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_str("warn"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::from_str("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::from_str("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::from_str("debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::from_str("trace"), Some(LogLevel::Trace));
        assert_eq!(LogLevel::from_str("invalid"), None);
    }

    #[test]
    fn test_log_level_as_str() {
        assert_eq!(LogLevel::Error.as_str(), "error");
        assert_eq!(LogLevel::Warn.as_str(), "warn");
        assert_eq!(LogLevel::Info.as_str(), "info");
        assert_eq!(LogLevel::Debug.as_str(), "debug");
        assert_eq!(LogLevel::Trace.as_str(), "trace");
    }

    #[test]
    fn test_log_rotation_config_from_env() {
        let config = LogRotationConfig::from_env();
        assert!(config.max_files > 0);
        assert!(config.retention_days > 0);
    }

    #[test]
    fn test_get_log_dir() {
        let db_path = PathBuf::from("/test/db");
        let log_dir = get_log_dir(&db_path);
        assert_eq!(log_dir, PathBuf::from("/test/db/logs"));
    }

    #[test]
    fn test_parse_log_date() {
        assert_eq!(
            parse_log_date("codesearch.log.2026-02-09"),
            Some(NaiveDate::from_ymd_opt(2026, 2, 9).unwrap())
        );
        assert_eq!(parse_log_date("codesearch.log"), None);
        assert_eq!(parse_log_date("codesearch.log.1"), None);
        assert_eq!(parse_log_date("other.log.2026-02-09"), None);
    }

    #[test]
    fn test_cleanup_old_logs_by_retention() {
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path();

        // Create a "recent" log file (today)
        let today = Utc::now().date_naive();
        let recent_name = format!("{}.{}", LOG_FILE_NAME, today.format("%Y-%m-%d"));
        let recent_path = log_dir.join(&recent_name);
        let mut f = File::create(&recent_path).unwrap();
        write!(f, "recent log").unwrap();

        // Create an "old" log file (10 days ago)
        let old_date = today - chrono::Duration::days(10);
        let old_name = format!("{}.{}", LOG_FILE_NAME, old_date.format("%Y-%m-%d"));
        let old_path = log_dir.join(&old_name);
        let mut f = File::create(&old_path).unwrap();
        write!(f, "old log").unwrap();

        let config = LogRotationConfig {
            max_files: 100, // high limit so only retention matters
            retention_days: 5,
        };

        cleanup_old_logs(log_dir, &config).unwrap();

        // Recent file should still exist
        assert!(recent_path.exists(), "Recent log file should be retained");
        // Old file should be removed
        assert!(!old_path.exists(), "Old log file should be removed");
    }

    #[test]
    fn test_cleanup_old_logs_by_max_files() {
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path();

        let today = Utc::now().date_naive();

        // Create 5 log files (today, yesterday, ...)
        let mut paths = Vec::new();
        for i in 0..5 {
            let date = today - chrono::Duration::days(i);
            let name = format!("{}.{}", LOG_FILE_NAME, date.format("%Y-%m-%d"));
            let path = log_dir.join(&name);
            let mut f = File::create(&path).unwrap();
            write!(f, "log day {}", i).unwrap();
            paths.push(path);
        }

        let config = LogRotationConfig {
            max_files: 3,
            retention_days: 30, // high limit so only max_files matters
        };

        cleanup_old_logs(log_dir, &config).unwrap();

        // 3 most recent should remain
        assert!(paths[0].exists(), "Today's log should remain");
        assert!(paths[1].exists(), "Yesterday's log should remain");
        assert!(paths[2].exists(), "2 days ago log should remain");
        // 2 oldest should be removed
        assert!(!paths[3].exists(), "3 days ago log should be removed");
        assert!(!paths[4].exists(), "4 days ago log should be removed");
    }

    #[test]
    fn test_cleanup_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let config = LogRotationConfig {
            max_files: 5,
            retention_days: 5,
        };
        // Should not error on empty directory
        assert!(cleanup_old_logs(temp_dir.path(), &config).is_ok());
    }

    #[test]
    fn test_cleanup_nonexistent_dir() {
        let config = LogRotationConfig {
            max_files: 5,
            retention_days: 5,
        };
        // Should not error on non-existent directory
        assert!(cleanup_old_logs(Path::new("/nonexistent/path"), &config).is_ok());
    }
}
