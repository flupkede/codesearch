# MCP Tool Description Improvements for Codesearch

**Problem:** Agents don't know which tool to use for which query type, leading to
semantic_search being called for exact name lookups (where it scores 0.00) instead
of find_references (which scores 0.90+).

See full analysis in the outputs file.
