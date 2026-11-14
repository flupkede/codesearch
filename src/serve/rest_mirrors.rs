//! REST mirrors of the MCP tools (federation-friendly HTTP+JSON).
//!
//! A remote `codesearch serve` can be queried for federation WITHOUT an MCP
//! session: each handler constructs a throwaway `CodesearchService` bound to
//! the live `ServeState`, invokes the `#[tool]` method, and returns the
//! tool's JSON payload unwrapped from `CallToolResult`. Protected by serve's
//! `require_auth_for_network` layer (same as /status, /mcp) — no separate
//! auth code needed. Moved here from the mcp module: the handlers extract
//! `Arc<ServeState>` and call the private `#[tool]` methods, both CLI-side.

use axum::extract::{Path as AxumPath, Query as AxumQuery, State as AxumState};
use axum::http::StatusCode;
use axum::response::Json as AxumJson;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

use crate::mcp::types::{
    ExploreRequest, FindImpactRequest, FindRequest, GetChunkRequest, SearchRequest,
};
use crate::mcp::{CodesearchService, McpError};
use crate::serve::ServeState;

// REST API handlers (federation-friendly HTTP+JSON mirror of MCP tools).
//
// Expose the same logic over plain HTTP so a remote codesearch serve can be
// queried for federation WITHOUT an MCP session. Each handler constructs a
// throwaway `CodesearchService` bound to the live `ServeState`, invokes the
// existing `#[tool]` method, and returns the tool's JSON payload unwrapped
// from `CallToolResult`. Protected by serve's `require_auth_for_network`
// layer (same as /status, /mcp) — no separate auth code needed.
// ════════════════════════════════════════════════════════════════

type RestResponse = AxumJson<serde_json::Value>;
type RestError = (StatusCode, AxumJson<serde_json::Value>);

/// Unwrap a `CallToolResult` into the JSON a federation client wants.
///
/// `CallToolResult` carries its payload as `ContentBlock::text(json_string)`. The
/// normal case for search/find/explore/get_chunk is a single text item whose
/// value parses as JSON, so we parse it back and return the structured value
/// (clients get clean objects instead of a JSON-in-string). When the tool set
/// `is_error` (e.g. a `scope_required` error) we still parse the body but mark
/// it with `"_mcp_is_error": true` so a federation caller can distinguish tool
/// errors from HTTP errors. Non-JSON payloads fall back to `{content, is_error}`.
pub(crate) fn call_tool_result_to_json(result: CallToolResult) -> serde_json::Value {
    let is_error = result.is_error.unwrap_or(false);
    let val = serde_json::to_value(&result).unwrap_or_else(|_| serde_json::json!({}));
    let text = val
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut parsed) => {
            if is_error {
                if let Some(obj) = parsed.as_object_mut() {
                    obj.insert("_mcp_is_error".into(), serde_json::json!(true));
                }
            }
            parsed
        }
        Err(_) => serde_json::json!({ "content": text, "is_error": is_error }),
    }
}

/// Build a per-request `CodesearchService` bound to the live serve state.
fn make_service(state: &std::sync::Arc<ServeState>) -> Result<CodesearchService, RestError> {
    CodesearchService::new_for_serve(state.clone()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            AxumJson(serde_json::json!({"error": format!("failed to build service: {e}")})),
        )
    })
}

/// Map an MCP-layer error to an HTTP 500. `McpError` (= `rmcp::ErrorData`)
/// derives `Serialize`, so round-trip it through a JSON value for the body.
fn mcp_err_to_http(e: McpError) -> RestError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        AxumJson(serde_json::json!({
            "error": serde_json::to_value(&e).unwrap_or(serde_json::Value::Null)
        })),
    )
}

pub(crate) async fn rest_search_handler(
    AxumState(state): AxumState<std::sync::Arc<ServeState>>,
    AxumJson(req): AxumJson<SearchRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .search(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_find_handler(
    AxumState(state): AxumState<std::sync::Arc<ServeState>>,
    AxumJson(req): AxumJson<FindRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .find(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_explore_handler(
    AxumState(state): AxumState<std::sync::Arc<ServeState>>,
    AxumJson(req): AxumJson<ExploreRequest>,
) -> Result<RestResponse, RestError> {
    let service = make_service(&state)?;
    let result = service
        .explore(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

pub(crate) async fn rest_get_chunk_handler(
    AxumState(state): AxumState<std::sync::Arc<ServeState>>,
    AxumPath(chunk_id): AxumPath<u32>,
    AxumQuery(params): AxumQuery<std::collections::HashMap<String, String>>,
) -> Result<RestResponse, RestError> {
    let req = GetChunkRequest {
        chunk_id,
        chunk_ref: params.get("chunk_ref").cloned(),
        context_lines: params.get("context_lines").and_then(|s| s.parse().ok()),
        project: params.get("project").cloned(),
        cwd: None,
        group: params.get("group").cloned(),
    };
    let service = make_service(&state)?;
    let result = service
        .get_chunk(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(AxumJson(call_tool_result_to_json(result)))
}

/// REST mirror of the `find_impact` MCP tool: POST a `FindImpactRequest`
/// body, receive the tool's JSON payload. Read-only, same auth class as
/// the other REST mirrors (see `FIND_IMPACT_PATH`).
pub(crate) async fn rest_find_impact_handler(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::serve::ServeState>>,
    axum::Json(req): axum::Json<FindImpactRequest>,
) -> Result<axum::response::Json<serde_json::Value>, RestError> {
    let service = make_service(&state)?;
    let result = service
        .find_impact(Parameters(req))
        .await
        .map_err(mcp_err_to_http)?;
    Ok(axum::Json(call_tool_result_to_json(result)))
}
