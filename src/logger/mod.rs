//!
//! Provides centralized logging configuration with:
//! - Log file rotation based on size (via background task)
//! - Periodic cleanup of old logs
//! - Per-database log storage in .codesearch.db/logs/
//! - Configurable via environment variables
//!

use anyhow::Result;
use chrono::{Duration, Utc};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::constants::{
    DEFAULT_LOG_MAX_FILES, DEFAULT_LOG_MAX_SIZE_MB, DEFAULT_LOG_RETENTION_DAYS,
    LOG_DIR_NAME, LOG_FILE_NAME,
};

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

    /// Convert to tracing Level
    pub fn as_tracing_level(&self) -> Level {
        match self {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Trace => Level::TRACE,
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
    /// Maximum size of each log file in MB
    pub max_size_mb: usize,
    /// Maximum number of log files to retain
    pub max_files: usize,
    /// Number of days to retain log files
    pub retention_days: i64,
}

impl LogRotationConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        Self {
            max_size_mb: std::env::var("CODESEARCH_LOG_MAX_SIZE_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LOG_MAX_SIZE_MB),
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

/// Get the log file path
pub fn get_log_file(db_path: &Path) -> PathBuf {
    get_log_dir(db_path).join(LOG_FILE_NAME)
}

/// Ensure the log directory exists
pub fn ensure_log_dir(log_dir: &Path) -> Result<()> {
    if !log_dir.exists() {
        fs::create_dir_all(log_dir)?;
        tracing::debug!("Created log directory: {:?}", log_dir);
    }
    Ok(())
}

/// Check if current log file exceeds max size and rotate if needed
pub fn rotate_if_needed(log_dir: &Path, config: &LogRotationConfig) -> Result<()> {
    let current_path = log_dir.join(LOG_FILE_NAME);

    // Check current file size
    if let Ok(metadata) = fs::metadata(&current_path) {
        let file_size_mb = metadata.len() / (1024 * 1024) as u64;
        if file_size_mb >= config.max_size_mb as u64 {
            tracing::info!(
                "Log file size limit reached ({} MB >= {} MB), rotating",
                file_size_mb,
                config.max_size_mb
            );

            // Rotate existing numbered files
            for i in (1..config.max_files).rev() {
                let from = log_dir.join(format!("{}.{}", LOG_FILE_NAME, i));
                let to = log_dir.join(format!("{}.{}", LOG_FILE_NAME, i + 1));
                if from.exists() {
                    fs::rename(&from, &to)?;
                }
            }

            // Rename current file to .1
            if current_path.exists() {
                let rotated_path = log_dir.join(format!("{}.1", LOG_FILE_NAME));
                fs::rename(&current_path, &rotated_path)?;
                tracing::debug!("Rotated log file to: {:?}", rotated_path);
            }
        }
    }

    Ok(())
}

/// Remove old log files based on retention period
pub fn cleanup_old_logs(log_dir: &Path, config: &LogRotationConfig) -> Result<()> {
    let retention_duration = Duration::days(config.retention_days);
    let cutoff_time = Utc::now() - retention_duration;

    if !log_dir.exists() {
        return Ok(());
    }

    // Collect all log files
    let mut log_files: Vec<(usize, PathBuf, std::fs::Metadata, chrono::DateTime<Utc>)> = Vec::new();

    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only process files that look like our log files
        if let Some(file_name) = path.file_name() {
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with(LOG_FILE_NAME) {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        let modified_time: chrono::DateTime<Utc> = modified.into();
                        // Extract index from filename (e.g., "codesearch.log.1" -> 1, "codesearch.log" -> 0)
                        let index = if file_name == LOG_FILE_NAME {
                            0
                        } else if let Some(suffix) = file_name.strip_prefix(&format!("{}.", LOG_FILE_NAME)) {
                            suffix.parse().unwrap_or(0)
                        } else {
                            0
                        };
                        log_files.push((index, path, metadata, modified_time));
                    }
                }
            }
        }
    }

    // Sort by modified time (oldest first)
    log_files.sort_by(|a, b| a.3.cmp(&b.3));

    let mut removed_count = 0;
    for (index, path, _metadata, modified_time) in log_files {
        // Remove files older than retention period
        if modified_time < cutoff_time {
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!("Failed to remove old log file {:?}: {}", path, e);
            } else {
                tracing::debug!("Removed old log file {:?} (modified: {})", path, modified_time);
                removed_count += 1;
            }
        }
    }

    if removed_count > 0 {
        tracing::info!("Removed {} old log files (older than {} days)", removed_count, config.retention_days);
    }

    Ok(())
}

/// Initialize the logger
///
/// # Arguments
/// * `db_path` - Path to the database directory (logs will be stored in db_path/logs/)
/// * `log_level` - Log level to use
/// * `quiet` - If true, suppress console output (log only to file)
///
/// # Returns
/// Returns the log directory path and rotation configuration
pub fn init_logger(
    db_path: &Path,
    log_level: LogLevel,
    quiet: bool,
) -> Result<(PathBuf, LogRotationConfig)> {
    let log_dir = get_log_dir(db_path);
    ensure_log_dir(&log_dir)?;

    let config = LogRotationConfig::from_env();

    // Rotate if needed before creating new appender
    rotate_if_needed(&log_dir, &config)?;

    // Create file appender with DAILY rotation (size-based is handled by background task)
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &log_dir, LOG_FILE_NAME);

    // Create subscriber
    let env_filter = EnvFilter::new(log_level.as_str())
        // Filter verbose debug logs from dependencies
        .add_directive(
            "tantivy=warn,arroy=warn,ort=warn"
                .parse()
                .unwrap_or_else(|_| "warn".parse().unwrap()),
        );

    let subscriber = tracing_subscriber::registry().with(env_filter);

    if quiet {
        // File-only logging
        subscriber
            .with(
                fmt::layer()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(false),
            )
            .try_init()?;
    } else {
        // Console + file logging (both to stderr and file)
        subscriber
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
            .try_init()?;
    }

    tracing::info!(
        "Logger initialized: level={}, log_dir={:?}, max_size_mb={}, max_files={}, retention_days={}",
        log_level.as_str(),
        log_dir,
        config.max_size_mb,
        config.max_files,
        config.retention_days,
    );

    Ok((log_dir, config))
}

/// Start periodic log cleanup task
///
/// This task runs every 24 hours (configurable via CODESEARCH_LOG_CLEANUP_INTERVAL_HOURS)
/// and removes old log files based on retention_days.
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

        let cleanup_interval = Duration::hours(cleanup_interval_hours as i64).to_std().unwrap();

        tracing::info!(
            "Log cleanup task started: interval={}h, retention_days={}",
            cleanup_interval_hours,
            config.retention_days
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(cleanup_interval) => {
                    // Check for rotation
                    if let Err(e) = rotate_if_needed(&log_dir, &config) {
                        tracing::error!("Failed to rotate log file: {}", e);
                    }

                    // Clean up old logs
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
    use std::fs;
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
    fn test_log_level_as_tracing_level() {
        assert_eq!(LogLevel::Error.as_tracing_level(), Level::ERROR);
        assert_eq!(LogLevel::Warn.as_tracing_level(), Level::WARN);
        assert_eq!(LogLevel::Info.as_tracing_level(), Level::INFO);
        assert_eq!(LogLevel::Debug.as_tracing_level(), Level::DEBUG);
        assert_eq!(LogLevel::Trace.as_tracing_level(), Level::TRACE);
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
        assert!(config.max_size_mb > 0);
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
    fn test_rotate_if_needed() {
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path();

        // Create a small log file (should NOT rotate)
        let current_path = log_dir.join(LOG_FILE_NAME);
        let mut file = File::create(&current_path).unwrap();
        write!(file, "small file").unwrap();

        let config = LogRotationConfig {
            max_size_mb: 10,
            max_files: 5,
            retention_days: 5,
        };

        let result = rotate_if_needed(log_dir, &config);
        assert!(result.is_ok());
        assert!(current_path.exists());

        // Create a large log file (should rotate)
        let large_content = "x".repeat(11 * 1024 * 1024); // 11 MB
        let mut file = File::create(&current_path).unwrap();
        write!(file, large_content).unwrap();

        let result = rotate_if_needed(log_dir, &config);
        assert!(result.is_ok());
        assert!(!current_path.exists());

        // Check that rotated file exists
        let rotated_path = log_dir.join(format!("{}.1", LOG_FILE_NAME));
        assert!(rotated_path.exists());
    }

    #[test]
    fn test_cleanup_old_logs() {
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path();

        // Create test log files
        let current_path = log_dir.join(LOG_FILE_NAME);
        let mut file = File::create(&current_path).unwrap();
        write!(file, "current").unwrap();

        let rotated_path = log_dir.join(format!("{}.1", LOG_FILE_NAME));
        let mut file = File::create(&rotated_path).unwrap();
        write!(file, "old").unwrap();

        // Make rotated file old by setting its modified time
        let old_time = Utc::now() - Duration::days(10);
        fs::set_file_times(&rotated_path, old_time.into(), old_time.into()).unwrap();

        let config = LogRotationConfig {
            max_size_mb: 10,
            max_files: 5,
            retention_days: 5,
        };

        let result = cleanup_old_logs(log_dir, &config);
        assert!(result.is_ok());

        // Current file should still exist
        assert!(current_path.exists());

        // Old file should be removed
        assert!(!rotated_path.exists());
    }
}
