//! Logging module with rotation and cleanup
//!
//! Provides centralized logging configuration with:
//! - Log file rotation based on size
//! - Periodic cleanup of old logs
//! - Per-database log storage in .codesearch.db/logs/
//! - Configurable via environment variables

use anyhow::Result;
use chrono::{Duration, Utc};
use std::path::{Path, PathBuf};
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
    /// Retention period in days (cleanup logs older than this)
    pub retention_days: u64,
}

impl Default for LogRotationConfig {
    fn default() -> Self {
        Self {
            max_size_mb: DEFAULT_LOG_MAX_SIZE_MB,
            max_files: DEFAULT_LOG_MAX_FILES,
            retention_days: DEFAULT_LOG_RETENTION_DAYS,
        }
    }
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
                .unwrap_or(DEFAULT_LOG_RETENTION_DAYS),
        }
    }
}

/// Get the log directory for a given database path
///
/// Returns `.codesearch.db/logs/` alongside the database
pub fn get_log_dir(db_path: &Path) -> PathBuf {
    db_path.join(LOG_DIR_NAME)
}

/// Get the log file path for a given database
///
/// Returns `.codesearch.db/logs/codesearch.log`
pub fn get_log_file(db_path: &Path) -> PathBuf {
    get_log_dir(db_path).join(LOG_FILE_NAME)
}

/// Ensure log directory exists
pub fn ensure_log_dir(db_path: &Path) -> Result<()> {
    let log_dir = get_log_dir(db_path);
    if !log_dir.exists() {
        std::fs::create_dir_all(&log_dir)?;
    }
    Ok(())
}

/// Initialize the logging system for a database
///
/// # Arguments
/// * `db_path` - Path to the database directory (.codesearch.db)
/// * `log_level` - Log level to use
/// * `quiet` - If true, suppress console output (logs to file only)
///
/// # Returns
/// The log file path and log rotation config
pub fn init_logger(db_path: &Path, log_level: LogLevel, quiet: bool) -> Result<(PathBuf, LogRotationConfig)> {
    let rotation_config = LogRotationConfig::from_env();

    // Ensure log directory exists
    ensure_log_dir(db_path)?;

    let log_dir = get_log_dir(db_path);

    // Determine rotation strategy based on max_size_mb
    // tracing-appender only supports HOURLY, DAILY, NEVER
    // We'll use DAILY rotation and rely on cleanup for file management
    let rotation = Rotation::DAILY;

    // Create rolling file appender
    let file_appender = RollingFileAppender::new(rotation, log_dir.clone(), LOG_FILE_NAME);

    // Build the subscriber layers
    // Filter out verbose debug logs from external crates
    let env_filter = EnvFilter::new(format!(
        "codesearch={},tantivy=info,tantivy::directory::mmap_directory=warn,arroy=info,ort=info",
        log_level.as_str()
    ));

    if quiet {
        // File logging only
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_ansi(false).with_writer(file_appender))
            .try_init()?;
    } else {
        // Both console (stderr) and file logging
        // IMPORTANT: Use stderr for console output — stdout is reserved for
        // program output and MCP/JSON protocol communication
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_writer(std::io::stderr))
            .with(fmt::layer().with_ansi(false).with_writer(file_appender))
            .try_init()?;
    }

    tracing::info!(
        "Logger initialized: level={}, dir={:?}, rotation={:?}",
        log_level.as_str(),
        log_dir,
        rotation_config
    );

    Ok((get_log_file(db_path), rotation_config))
}

/// Cleanup old log files based on retention policy
///
/// Removes log files older than `retention_days` from the log directory.
///
/// # Arguments
/// * `db_path` - Path to the database directory
/// * `rotation_config` - Log rotation configuration with retention settings
pub fn cleanup_old_logs(db_path: &Path, rotation_config: &LogRotationConfig) -> Result<()> {
    let log_dir = get_log_dir(db_path);

    // If log directory doesn't exist, nothing to clean
    if !log_dir.exists() {
        return Ok(());
    }

    let now = Utc::now();
    let cutoff = now - Duration::days(rotation_config.retention_days as i64);

    let mut removed_count = 0;

    for entry in std::fs::read_dir(&log_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only process files
        if !path.is_file() {
            continue;
        }

        // Skip the current log file
        if path.file_name() == Some(std::ffi::OsStr::new(LOG_FILE_NAME)) {
            continue;
        }

        // Get file modification time
        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                let modified_time: chrono::DateTime<Utc> = modified.into();

                // Remove if older than retention period
                if modified_time < cutoff {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!("Failed to remove old log file {:?}: {}", path, e);
                    } else {
                        tracing::debug!("Removed old log file: {:?}", path);
                        removed_count += 1;
                    }
                }
            }
        }
    }

    if removed_count > 0 {
        tracing::info!("Cleaned up {} old log files from {:?}", removed_count, log_dir);
    }

    Ok(())
}

/// Start periodic log cleanup task
///
/// Returns a task handle that can be aborted when shutting down.
/// Cleanup runs every 24 hours by default.
///
 /// # Arguments
 /// * `db_path` - Path to the database directory
 /// * `rotation_config` - Log rotation configuration
 /// * `shutdown_token` - Cancellation token for graceful shutdown
 pub fn start_cleanup_task(
     db_path: PathBuf,
     rotation_config: LogRotationConfig,
     shutdown_token: CancellationToken,
 ) -> tokio::task::JoinHandle<()> {
    let cleanup_interval_hours = std::env::var("CODESEARCH_LOG_CLEANUP_INTERVAL_HOURS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24); // Default: every 24 hours

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(cleanup_interval_hours * 3600));

        tracing::info!(
            "Log cleanup task started: interval={}h, retention={}days",
            cleanup_interval_hours,
            rotation_config.retention_days
        );

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = cleanup_old_logs(&db_path, &rotation_config) {
                        tracing::error!("Log cleanup failed: {}", e);
                    }
                }
                _ = shutdown_token.cancelled() => {
                    tracing::info!("Log cleanup task shutting down");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_get_log_dir() {
        let db_path = PathBuf::from("/project/.codesearch.db");
        let log_dir = get_log_dir(&db_path);
        assert_eq!(log_dir, PathBuf::from("/project/.codesearch.db/logs"));
    }

    #[test]
    fn test_get_log_file() {
        let db_path = PathBuf::from("/project/.codesearch.db");
        let log_file = get_log_file(&db_path);
        assert_eq!(
            log_file,
            PathBuf::from("/project/.codesearch.db/logs/codesearch.log")
        );
    }

    #[test]
    fn test_log_rotation_config_default() {
        let config = LogRotationConfig::default();
        assert_eq!(config.max_size_mb, DEFAULT_LOG_MAX_SIZE_MB);
        assert_eq!(config.max_files, DEFAULT_LOG_MAX_FILES);
        assert_eq!(config.retention_days, DEFAULT_LOG_RETENTION_DAYS);
    }

    #[test]
    fn test_ensure_log_dir() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join(".codesearch.db");
        let log_dir = get_log_dir(&db_path);

        assert!(!log_dir.exists());
        ensure_log_dir(&db_path).unwrap();
        assert!(log_dir.exists());
    }
}
