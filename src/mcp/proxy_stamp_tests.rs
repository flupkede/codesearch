//! Unit tests for the proxy SEP-2322 re-stamp helpers.
//!
//! The wire-shape guard in `tests/mcp_protocol_bridge.rs` runs `--mode local`
//! and can never reach these helpers (they only fire on the proxy path), so
//! the stamp behavior is pinned here directly: a hub-stripped result gets
//! `"complete"` back, and non-`Complete` `CallToolResponse` variants keep
//! their own semantics.

use super::{restamp_complete, restamp_complete_response};
use rmcp::model::{
    CallToolResponse, CallToolResult, InputRequiredResult, ListToolsResult, ResultType,
};

#[test]
fn restamp_complete_sets_stripped_discriminator() {
    // hub stripped result_type for the hub<->proxy session
    let result = ListToolsResult {
        result_type: None,
        ..Default::default()
    };
    let result = restamp_complete(result);
    assert_eq!(result.result_type, Some(ResultType::COMPLETE));
}

#[test]
fn restamp_complete_response_stamps_complete_variant() {
    let mut inner = CallToolResult::success(vec![]);
    inner.result_type = None; // hub stripped it
    let response = restamp_complete_response(CallToolResponse::Complete(inner));
    match response {
        CallToolResponse::Complete(result) => {
            assert_eq!(result.result_type, Some(ResultType::COMPLETE));
        }
        other => panic!("expected Complete variant, got {other:?}"),
    }
}

#[test]
fn restamp_complete_response_leaves_non_complete_variants_alone() {
    let input_required =
        CallToolResponse::InputRequired(InputRequiredResult::from_request_state("opaque"));
    let stamped = restamp_complete_response(input_required);
    match stamped {
        CallToolResponse::InputRequired(result) => {
            assert_eq!(result.result_type, ResultType::INPUT_REQUIRED);
        }
        other => panic!("expected InputRequired variant to pass through, got {other:?}"),
    }
}
