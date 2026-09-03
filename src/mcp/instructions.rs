/// MCP server instructions template (pre-substitution). Kept as a named const so
/// the line-count and deprecated-alias tests can validate it directly without
/// fragile `include_str!` source-text searching or instantiating the service.
/// Substitution uses `str::replace` (not `format!`) because `format!` requires a
/// literal; the placeholders are unique tokens that don't appear in the prose.
/// See `test_instructions_max_50_lines` / `test_no_deprecated_tool_aliases`.
pub(crate) const INSTRUCTIONS_TEMPLATE: &str = r#"codesearch — semantic code search + symbol impact analysis.

WHEN TO USE codesearch (prefer over grep/glob):
  Good for: semantic or cross-file lookup, unknown file paths, symbol navigation,
    "where is X implemented", "find usages of Y", "how does Z flow through the code"
  Not for: a single known file (just read it), trivial one-line edits,
    exact literal patterns where plain grep is faster

SERVICE-MODE NOTES (codesearch serve, esp. on another host):
  - Paths come from the SERVER's filesystem. Use get_chunk to read content;
    don't try to open returned paths locally.
  - Not every directory is indexed (e.g. .venv, node_modules, build/). If a
    search returns nothing, the dir may be unindexed — ask, don't grep blindly.

PICK THE RIGHT TOOL FOR THE TASK:
  "who calls X?" / "what breaks if I rename X?"
    → find_impact (precise SCIP call-graph; if no backend for the language, it says so → then use find kind="usages")
  "find code about X" / "how does X work" / "show me X"
    → search(mode="semantic") — concepts + synonyms + identifiers
  exact syntax like Vec<T> / foo = null / a::b
    → search(mode="literal", regex=true) — patterns semantic can't match
  "where is X defined?" / "what does file X import?"
    → find(kind="definition" | "imports")
  "show all symbols in file X" / "code like chunk Y"
    → explore(kind="outline" | "similar")
  read chunk content → get_chunk(chunk_id)
  index health / repo list → status

RULES:
  - search(semantic) is the DEFAULT for code lookup. Don't skip it.
  - For "who calls X" / impact analysis, try find_impact first; fall back to find(kind="usages") only if find_impact reports no backend.
  - NEVER use literal as first search unless you need exact syntax.
  - project or group is REQUIRED in multi-repo mode.

Mode: {mode}
Project: {project}
Database: {db} ({exists})
Model: {model} ({dims}d)
"#;
