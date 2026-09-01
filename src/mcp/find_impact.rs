use super::types::FindImpactRequest;
use super::CodesearchService;
use crate::symbols::SymbolReference;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router, ErrorData as McpError,
};
use std::path::{Path, PathBuf};

/// Resolve the `find_impact` wall-clock budget: env var →
/// `DEFAULT_FIND_IMPACT_BUDGET_SECS`. `0` disables the budget (unbounded
/// lookup, the pre-budget behaviour). Mirrors
/// `resolve_proxy_idle_disconnect_secs`.
fn resolve_find_impact_budget_secs() -> u64 {
    std::env::var(crate::constants::FIND_IMPACT_BUDGET_SECS_ENV)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(crate::constants::DEFAULT_FIND_IMPACT_BUDGET_SECS)
}

/// Outcome of a budget-bounded `find_impact` lookup.
pub(crate) enum ImpactLookupOutcome {
    /// The lookup finished within the budget (or the budget is disabled).
    /// `Err` preserves the store/helper failure for the caller to report.
    Done(Result<Vec<SymbolReference>, anyhow::Error>),
    /// The budget overran; the lookup keeps running in the background.
    Busy {
        /// What is still running (goes into the busy envelope verbatim).
        state: String,
        /// Wall-clock time actually waited before giving up.
        waited_ms: u64,
    },
}

/// Race a `find_impact` lookup against its wall-clock budget.
///
/// `lookup` is the already-offloaded lookup future (the handler runs the
/// blocking SCIP call on `spawn_blocking`); it is NOT cancelled on overrun —
/// dropping the future abandons it while the detached blocking task keeps
/// running, so its reference-cache writes still land in LMDB and the retry
/// hinted by the busy answer is served warm. `budget_secs == 0` disables the
/// race entirely. Kept generic over the future so tests can plant a sleeping
/// handler instead of a real SCIP helper.
pub(crate) async fn find_impact_with_budget<F>(
    budget_secs: u64,
    state: String,
    lookup: F,
) -> ImpactLookupOutcome
where
    F: std::future::Future<Output = Result<Vec<SymbolReference>, anyhow::Error>>,
{
    if budget_secs == 0 {
        return ImpactLookupOutcome::Done(lookup.await);
    }
    let started = std::time::Instant::now();
    match tokio::time::timeout(std::time::Duration::from_secs(budget_secs), lookup).await {
        Ok(result) => ImpactLookupOutcome::Done(result),
        Err(_elapsed) => ImpactLookupOutcome::Busy {
            state,
            waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
    }
}

/// Collapse exact-duplicate references from the SCIP find-refs output.
///
/// The helper can emit multiple occurrences of the same symbol at the same
/// file:line (declaration plus multiple roles on one line), which reaches the
/// agent as visible noise (observed live: a definition listed 5×). Two
/// references that are identical in ALL of (file, line range, kind) carry no
/// information the agent could act on separately — `SymbolReference` has no
/// column, so two genuinely distinct same-line calls are indistinguishable
/// from duplicates and collapse too, which is the right call for a caller
/// that wants "where is this used", not occurrence counts. Order is stable.
fn dedupe_references(
    refs: Vec<crate::symbols::SymbolReference>,
) -> Vec<crate::symbols::SymbolReference> {
    let mut seen: std::collections::HashSet<(String, u32, u32, String)> =
        std::collections::HashSet::with_capacity(refs.len());
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        let key = (
            r.file.to_string_lossy().into_owned(),
            r.start_line,
            r.end_line,
            r.kind.clone(),
        );
        if seen.insert(key) {
            out.push(r);
        }
    }
    out
}

#[tool_router(router = find_impact_router, vis = "pub(crate)")]
impl CodesearchService {
    /// Symbol impact analysis — returns transitive call-sites of a symbol with file/line precision.
    ///
    /// The recommended tool for "who calls X?" / "what breaks if I rename X?". Uses
    /// language-specific semantic analysis (SCIP) to find all references, enabling agents
    /// to plan refactors with IDE-class accuracy instead of text-matching grep heuristics.
    /// Precision backends ship per language: C# (bundled `scip-csharp` helper,
    /// `-with-csharp` releases) and TypeScript (`scip-typescript`, resolved via `npx`
    /// or `CODESEARCH_SCIP_TYPESCRIPT`). If no backend is installed for the target
    /// language, the response reports it — fall back to `find` with `kind="usages"`
    /// (lexical) only then.
    #[tool(
        description = "Symbol impact analysis — find all references to a symbol with IDE-class precision (SCIP).\n\nThe right tool for \"who calls X?\" / \"what breaks if I rename X?\". Returns transitive call-sites with file/line precision, enabling agents to plan refactors without missing a caller. More accurate than text-based `find kind=\"usages\"` because it understands language semantics.\n\nInput variants:\n- By name: `{ \"symbol_name\": \"FieldDefinition.Validate\", \"project\": \"myrepo\" }`\n- By position: `{ \"file\": \"src/Validation/FieldDefinition.cs\", \"line\": 42, \"project\": \"myrepo\" }`\n\nPrecision backends (SCIP) ship per language; C# (bundled `scip-csharp` helper, `-with-csharp` releases) and TypeScript (via `npx` or `CODESEARCH_SCIP_TYPESCRIPT`) are available today. For Rust/Python/Go/etc., use `find` with `kind=\"usages\"` as a text-based fallback until SCIP backends for those languages ship.\n\nOn a busy answer (`\"busy\": true`): sleep `retry_after_seconds` and retry the SAME call. Busy is progress, not failure — never fall back to text search on busy.\n\nIMPORTANT (multi-repo): always specify `project` (single repo). Omitting `project` in multi-repo mode returns a `scope_required` error."
    )]
    async fn find_impact(
        &self,
        Parameters(request): Parameters<FindImpactRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 find_impact(symbol_name={:?}, file={:?}, line={:?}, language={:?}, project={:?})",
            request.symbol_name,
            request.file,
            request.line,
            request.language,
            request.project,
        );

        // Validate input: must provide either symbol_name or file+line
        let has_name = request
            .symbol_name
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_position = request.file.is_some() && request.line.is_some();
        if !has_name && !has_position {
            return Ok(CallToolResult::success(vec![Content::text(
                "Must provide either `symbol_name` or both `file` and `line` for position-based lookup.".to_string(),
            )]));
        }

        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find_impact")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Determine project root and db_path for the symbol index
        let (project_root, db_path) = if let Some(ref alias) = ctx.project_alias {
            let root = ctx
                .alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone());
            // The symbol index DB lives alongside the vector DB
            let db = root.join(crate::constants::DB_DIR_NAME);
            (root, db)
        } else {
            // Single-repo / stdio mode: use the service's own paths
            (self.project_path.clone(), self.db_path.clone())
        };

        // Use the shared symbol indexer registry
        let registry = &self.symbol_registry;

        // Determine which language to use
        let language = request.language.clone().or_else(|| {
            // Auto-detect from file extension
            request.file.as_ref().and_then(|f| {
                let ext = Path::new(f).extension()?.to_str()?.to_lowercase();
                match ext.as_str() {
                    "cs" => Some(crate::constants::LANG_CSHARP.to_string()),
                    "ts" | "tsx" | "mts" | "cts" => {
                        Some(crate::constants::LANG_TYPESCRIPT.to_string())
                    }
                    _ => None,
                }
            })
        });

        let indexer: &dyn crate::symbols::SymbolIndexer = match language {
            Some(ref lang) => match registry.get(lang) {
                Some(i) => i,
                None => {
                    let available = registry.available_languages();
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "No symbol indexer for language '{}'. Available languages: {:?}",
                        lang, available
                    ))]));
                }
            },
            None => {
                // No language specified and couldn't auto-detect — try all installed
                let installed = registry.installed_languages();
                if installed.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No symbol indexers installed. Install the `scip-csharp` helper for C# support, or `scip-typescript` (via npx) for TypeScript support.".to_string(),
                    )]));
                }
                // Use the first installed language (MVP: C# or TypeScript)
                match registry.get(&installed[0]) {
                    Some(i) => i,
                    None => {
                        unreachable!("installed_languages() returned a language with no indexer")
                    }
                }
            }
        };

        // Check if the helper is available
        if !indexer.is_available() {
            let error = crate::symbols::SymbolIndexError {
                error: format!(
                    "Symbol indexer for '{}' is not available. The helper binary is not installed.",
                    indexer.language()
                ),
                available_languages: registry.available_languages(),
                hint_for_agent: format!(
                    "Install the `-with-csharp` release variant, or set {} to the helper path.",
                    crate::constants::SCIP_CSHARP_HELPER_ENV
                ),
            };
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string(&error).unwrap_or_else(|_| error.error.clone()),
            )]));
        }

        // Perform the lookup under an internal wall-clock budget.
        //
        // `find_references` may invoke `scip-csharp find-refs` on a cache miss
        // (lazy Opt-2 reference resolution). That subprocess can take several minutes
        // on a large solution. The call therefore runs on `spawn_blocking` (it never
        // blocks an async worker thread) and is raced against
        // CODESEARCH_FIND_IMPACT_BUDGET_SECS: on overrun the caller gets a structured
        // busy answer instead of the MCP client winning the timeout race. The
        // blocking task is abandoned, not cancelled — its cache writes still land
        // in LMDB, so the hinted retry is served warm; the lookup tracker
        // (find_impact_tracker) makes that retry observe progress or the warm
        // result explicitly instead of re-running the helper.
        let language_for_lookup = indexer.language().to_string();
        let symbol_name_for_lookup = request.symbol_name.clone();
        let line_for_lookup = request.line;
        let file_for_pos = if !has_name {
            Some(self.normalize_symbol_query_path(
                &project_root,
                Path::new(request.file.as_ref().unwrap()),
            ))
        } else {
            None
        };
        let what = if has_name {
            format!("'{}'", symbol_name_for_lookup.as_deref().unwrap_or("?"))
        } else {
            format!(
                "{}:{}",
                request.file.as_deref().unwrap_or("?"),
                line_for_lookup.unwrap_or(0)
            )
        };
        let busy_state = format!(
            "resolving {} via the {} SCIP helper (cold reference cache)",
            what, language_for_lookup
        );
        let budget_secs = resolve_find_impact_budget_secs();

        // Index fingerprint: the repository HEAD at response time. The
        // non-fatal git read is offloaded like every blocking call; a
        // failed read simply omits the field. Drift against
        // `index_head_sha` is surfaced, never auto-reindexed (deliberate:
        // reindexing a large solution on every branch switch would thrash).
        let head_root = project_root.clone();
        let current_head_sha =
            tokio::task::spawn_blocking(move || crate::symbols::current_git_head(&head_root))
                .await
                .unwrap_or(None);

        // Shared result construction: the warm-retry path (below) must be
        // byte-identical to a budget-fast completion, so both build the
        // response through this one closure.
        let build_impact =
            |references: Vec<crate::symbols::SymbolReference>| crate::symbols::FindImpactResult {
                symbol: request.symbol_name.clone().unwrap_or_else(|| {
                    format!(
                        "{}:{}",
                        request.file.as_deref().unwrap_or("?"),
                        request.line.unwrap_or(0)
                    )
                }),
                references: dedupe_references(references),
                index_age_seconds: indexer.index_age(&db_path),
                language: indexer.language().to_string(),
                scope: ctx
                    .project_alias
                    .map(|a| format!("project:{}", a))
                    .unwrap_or_else(|| "local".to_string()),
                index_head_sha: indexer.index_head_sha(&db_path),
                current_head_sha: current_head_sha.clone(),
            };

        // Background continuation: consult the tracker before starting a
        // (potentially cold, minutes-long) lookup. A retry of an overran
        // lookup observes progress or the warm result instead of racing a
        // second identical helper subprocess against the cold cache.
        let tracker_key: find_impact_tracker::LookupKey = (db_path.clone(), what.clone());
        match find_impact_tracker::IMPACT_LOOKUP_TRACKER.check(&tracker_key) {
            Some(find_impact_tracker::TrackedStatus::Running { elapsed_ms }) => {
                tracing::info!(
                    "find_impact retry: lookup still running ({}ms elapsed): {}",
                    elapsed_ms,
                    busy_state
                );
                let busy = crate::symbols::SymbolLookupBusy {
                    busy: true,
                    state: busy_state.clone(),
                    waited_ms: elapsed_ms,
                    advice: format!(
                        "still running ({}s elapsed); retry the same call in ~{}s",
                        elapsed_ms / 1000,
                        budget_secs.max(1)
                    ),
                    retry_after_seconds: budget_secs.max(1),
                };
                let json =
                    serde_json::to_string(&busy).unwrap_or_else(|_| "{\"busy\":true}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            Some(find_impact_tracker::TrackedStatus::Done(Ok(references))) => {
                tracing::info!(
                    "find_impact retry: serving warm result ({} references) from the finished background lookup: {}",
                    references.len(),
                    busy_state
                );
                let impact = build_impact(references);
                let json = serde_json::to_string(&impact).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            Some(find_impact_tracker::TrackedStatus::Done(Err(chain))) => {
                // Same classification as a fresh failure: the tracked chain
                // is already `{:#}`-rendered, the index age decides the class.
                let failure = crate::symbols::SymbolLookupFailure::classify(
                    chain,
                    indexer.index_age(&db_path),
                );
                let json =
                    serde_json::to_string(&failure).unwrap_or_else(|_| failure.error.clone());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
            None => {}
        }

        let registry_for_lookup = self.symbol_registry.clone();
        let db_path_for_lookup = db_path.clone();
        let file_for_lookup = file_for_pos;
        let lookup_entry = find_impact_tracker::IMPACT_LOOKUP_TRACKER.register(tracker_key.clone());
        let lookup_entry_in_task = lookup_entry;
        let lookup = async move {
            tokio::task::spawn_blocking(move || {
                let indexer = registry_for_lookup
                    .get(&language_for_lookup)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "symbol indexer for '{}' disappeared mid-request",
                            language_for_lookup
                        )
                    })?;
                let result = if has_name {
                    indexer.find_references(
                        &db_path_for_lookup,
                        symbol_name_for_lookup.as_deref().unwrap_or(""),
                    )
                } else {
                    indexer.find_references_by_position(
                        &db_path_for_lookup,
                        &file_for_lookup.unwrap_or_default(),
                        line_for_lookup.unwrap_or(0),
                    )
                };
                // Record INSIDE the blocking task: the handler's awaiting
                // future is dropped at budget overrun, but this detached
                // task survives and the recorded outcome is what the hinted
                // retry observes. (`anyhow::Error` is not `Clone`, so the
                // failure side is recorded as its rendered `{:#}` chain.)
                let recorded = result.as_ref().map_err(|e| format!("{e:#}")).cloned();
                lookup_entry_in_task.finish(recorded);
                result
            })
            .await
            .map_err(|e| anyhow::anyhow!("symbol lookup task failed: {e:#}"))?
        };

        match find_impact_with_budget(budget_secs, busy_state, lookup).await {
            ImpactLookupOutcome::Done(Ok(references)) => {
                // Completed within the budget: nothing is in flight, so a
                // later lookup must consult the real cache, not the tracker.
                find_impact_tracker::IMPACT_LOOKUP_TRACKER.remove(&tracker_key);
                let impact = build_impact(references);
                let json = serde_json::to_string(&impact).unwrap_or_else(|_| "{}".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            ImpactLookupOutcome::Done(Err(e)) => {
                find_impact_tracker::IMPACT_LOOKUP_TRACKER.remove(&tracker_key);
                // Typed failure envelope (busy/stale/failed must stay
                // machine-branchable; the index age decides stale vs failed).
                let failure = crate::symbols::SymbolLookupFailure::classify(
                    format!("{e:#}"),
                    indexer.index_age(&db_path),
                );
                let json =
                    serde_json::to_string(&failure).unwrap_or_else(|_| failure.error.clone());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            ImpactLookupOutcome::Busy { state, waited_ms } => {
                tracing::warn!(
                    "find_impact budget overrun after {}ms (budget {}s): {} — answering busy, lookup continues in background",
                    waited_ms,
                    budget_secs,
                    state
                );
                let busy = crate::symbols::SymbolLookupBusy {
                    busy: true,
                    state,
                    waited_ms,
                    advice: format!(
                        "retry the same call in ~{}s; the lookup keeps running in the background and the retry is served from cache once it completes",
                        budget_secs.max(1)
                    ),
                    retry_after_seconds: budget_secs.max(1),
                };
                let json =
                    serde_json::to_string(&busy).unwrap_or_else(|_| "{\"busy\":true}".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
        }
    }
}

#[cfg(test)]
#[path = "find_impact_tests.rs"]
mod find_impact_tests;

// `#[path]` is load-bearing: a plain `mod` from find_impact.rs would resolve
// under src/mcp/find_impact/, and the file stays at src/mcp/.
#[path = "find_impact_tracker.rs"]
mod find_impact_tracker;
