use super::{is_idle, resolve_proxy_idle_disconnect_secs};
use crate::constants::DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS;
use std::time::{Duration, Instant};

#[test]
fn not_idle_before_the_threshold_elapses() {
    let last = Instant::now();
    let now = last + Duration::from_secs(59);
    assert!(!is_idle(last, 60, now));
}

#[test]
fn idle_once_the_threshold_is_reached() {
    let last = Instant::now();
    assert!(is_idle(last, 60, last + Duration::from_secs(60)));
    assert!(is_idle(last, 60, last + Duration::from_secs(600)));
}

#[test]
fn zero_threshold_disables_idle_disconnect() {
    let last = Instant::now();
    // Even an absurdly long idle period must not trigger a close.
    assert!(!is_idle(last, 0, last + Duration::from_secs(86_400)));
}

#[test]
fn clock_going_backwards_is_not_idle() {
    // saturating_duration_since floors at zero instead of panicking.
    let last = Instant::now() + Duration::from_secs(10);
    assert!(!is_idle(last, 60, Instant::now()));
}

#[test]
fn explicit_value_wins_over_env_and_default() {
    // Explicit takes precedence without consulting the environment, so this
    // stays correct regardless of what other tests set.
    assert_eq!(resolve_proxy_idle_disconnect_secs(Some(5)), 5);
    assert_eq!(resolve_proxy_idle_disconnect_secs(Some(0)), 0);
}

#[test]
fn falls_back_to_the_documented_default() {
    // No explicit value and (in the normal test environment) no env override.
    if std::env::var(crate::constants::MCP_PROXY_IDLE_DISCONNECT_SECS_ENV).is_err() {
        assert_eq!(
            resolve_proxy_idle_disconnect_secs(None),
            DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS
        );
    }
}
