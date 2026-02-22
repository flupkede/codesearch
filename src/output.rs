//! Output control for quiet mode and JSON output
//!
//! Provides a global quiet mode flag to suppress non-essential output.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quiet_mode_toggle() {
        // Initial state
        set_quiet(false);
        assert!(!is_quiet());

        // Enable
        set_quiet(true);
        assert!(is_quiet());

        // Disable
        set_quiet(false);
        assert!(!is_quiet());
    }

    #[test]
    fn test_print_info_not_quiet() {
        set_quiet(false);
        assert!(!is_quiet());

        // Test that print_info doesn't panic when quiet mode is off
        print_info(format_args!("info message"));

        // Reset
        set_quiet(false);
    }

    #[test]
    fn test_print_info_quiet() {
        set_quiet(true);
        assert!(is_quiet());

        // Test that print_info doesn't panic when quiet mode is on
        print_info(format_args!("suppressed info message"));

        // Reset
        set_quiet(false);
    }

    #[test]
    fn test_print_warn_not_quiet() {
        set_quiet(false);
        assert!(!is_quiet());

        // Test that print_warn doesn't panic when quiet mode is off
        print_warn(format_args!("warning message"));

        // Reset
        set_quiet(false);
    }

    #[test]
    fn test_print_warn_quiet() {
        set_quiet(true);
        assert!(is_quiet());

        // Test that print_warn doesn't panic when quiet mode is on
        print_warn(format_args!("suppressed warning message"));

        // Reset
        set_quiet(false);
    }

    #[test]
    fn test_multiple_print_calls() {
        set_quiet(false);
        print_info(format_args!("first"));
        print_warn(format_args!("second"));

        set_quiet(true);
        print_info(format_args!("suppressed first"));
        print_warn(format_args!("suppressed second"));

        set_quiet(false);
    }
}

/// Global quiet mode flag
static QUIET_MODE: AtomicBool = AtomicBool::new(false);

/// Enable quiet mode (suppresses informational output)
pub fn set_quiet(quiet: bool) {
    QUIET_MODE.store(quiet, Ordering::SeqCst);
}

/// Check if quiet mode is enabled
pub fn is_quiet() -> bool {
    QUIET_MODE.load(Ordering::SeqCst)
}

/// Print a message only if not in quiet mode (non-macro version for better compatibility)
/// Uses stderr to avoid corrupting stdout-based protocols (MCP, JSON output)
pub fn print_info(args: std::fmt::Arguments<'_>) {
    if !is_quiet() {
        eprintln!("{}", args);
    }
}

/// Print a warning to stderr only if not in quiet mode (non-macro version)
#[allow(dead_code)] // Used by warn_print! macro
pub fn print_warn(args: std::fmt::Arguments<'_>) {
    if !is_quiet() {
        eprintln!("{}", args);
    }
}

/// Print a message only if not in quiet mode
#[macro_export]
macro_rules! info_print {
    ($($arg:tt)*) => {
        $crate::output::print_info(format_args!($($arg)*));
    };
}

/// Print to stderr only if not in quiet mode (for warnings)
#[macro_export]
macro_rules! warn_print {
    ($($arg:tt)*) => {
        $crate::output::print_warn(format_args!($($arg)*));
    };
}
