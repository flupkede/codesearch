//! Output control for quiet mode and JSON output
//!
//! Provides a global quiet mode flag to suppress non-essential output.

use std::sync::atomic::{AtomicBool, Ordering};

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
pub fn print_info(args: std::fmt::Arguments<'_>) {
    if !is_quiet() {
        println!("{}", args);
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
