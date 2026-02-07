# MCP Help System Implementation Summary

## Questions Answered

### 1. Can we add `--help` to the mcp command?

**Yes, but it's already available!**

Since `mcp` is a clap subcommand, users can run:

```bash
codesearch mcp --help
```

This displays:
```text
Start MCP server for Claude Code integration

Usage: codesearch [OPTIONS] mcp [PATH]

Arguments:
  [PATH]  Path to project (defaults to current directory)

Options:
  -h, --help     Print help
  -v, --verbose  Enable verbose output
  -q, --quiet    Suppress informational output
```

### 2. Is there a specific tool an agent calls to get help from an MCP?

**No standard "help" tool exists in the MCP protocol.**

However, MCP servers have an `instructions` field in their server info that's automatically displayed when the AI assistant connects to the server.

## Implementation Details

### Before Enhancement

The original MCP server had minimal instructions:

```rust
instructions: Some(
    "codesearch is a semantic code search tool. Use semantic_search to find code \
     by meaning, get_file_chunks to see all chunks in a file, and index_status \
     to check if the index is ready."
        .to_string(),
),
```

### After Enhancement

I've expanded the instructions to include comprehensive help:

```rust
instructions: Some(
    format!(r#"
codesearch - Semantic Code Search MCP Server

OVERVIEW:
codesearch provides fast, local semantic code search using natural language queries.
Search your codebase by meaning, not just by keywords.

AVAILABLE TOOLS:

1. semantic_search(query, limit=10)
   Search the codebase using natural language queries.
   Query examples:
     - "where do we handle user authentication?"
     - "how is error logging implemented?"
     - "functions that process payment data"
     - "database connection management"
   Returns: Array of matches with path, line numbers, code content, and relevance scores.

2. get_file_chunks(path)
   Get all indexed chunks from a specific file.
   Useful for understanding the complete structure of a file.
   Returns: All chunks from the file with full context.

3. index_status()
   Check if the index exists and get database statistics.
   Use this before searching to verify the index is ready.
   Returns: Index status, total chunks, files, model info, and dimensions.

USAGE PATTERNS:

Understanding a New Codebase:
  1. Check index_status() to verify index is ready
  2. Search for core concepts: semantic_search("main application entry point")
  3. Explore patterns: semantic_search("error handling strategy")
  4. Get detailed view: get_file_chunks("src/main.rs")

Finding Implementation Patterns:
  - semantic_search("how are API endpoints defined?")
  - semantic_search("database model definitions")
  - get_file_chunks("src/models/user.rs")

Debugging and Analysis:
  - semantic_search("error handling for database operations")
  - semantic_search("user input validation")

Implementing New Features:
  - semantic_search("authentication handling code") - Find reference implementations
  - semantic_search("configuration management") - Understand patterns
  - get_file_chunks("src/config.rs") - See detailed implementation

BEST PRACTICES:

✓ Use natural language queries describing concepts, not exact terms
✓ Check index_status() before searching
✓ Use specific queries with context (e.g., "API layer error handling" vs "error handling")
✓ Combine semantic_search() with get_file_chunks() for detailed analysis
✓ Start with broader queries, then narrow down

✗ Avoid short, vague queries like "auth" or "db" (use grep for exact matches)
✗ Don't expect exact string matching (that's what grep is for)

PERFORMANCE:
- Search speed: ~75ms after initial model load
- First search: ~2-3s (model loading time)
- Indexing: 30-60s for initial, incremental updates are instant

SETUP:
If this MCP server doesn't find an index, the user needs to run:
  codesearch index

For detailed documentation, visit: https://github.com/yxanul/codesearch

Current database: {db}
Model: {model}
Dimensions: {dims}
"#,
        db = self.db_path.display(),
        model = self.model_type.short_name(),
        dims = self.dimensions
    )
),
```

## How AI Assistants Access MCP Help

### Automatic Display

When Claude Code (or other MCP-compatible assistant) connects to the codesearch MCP server, it automatically:

1. Calls the server's `info` endpoint
2. Receives the `instructions` field
3. Displays this to the user or uses it internally to understand available tools

### No Explicit Help Call Needed

Unlike CLI tools where you type `--help`, MCP help is:
- Automatically provided on connection
- Available through the assistant's UI
- Can be queried by asking: "What tools does codesearch provide?"

### Practical Usage

In Claude Code, you might ask:

```
> What can I do with codesearch?
> Show me help for the codesearch MCP server
> How do I search code with codesearch?
```

Claude will use the `instructions` to answer.

## Key Improvements Made

### 1. Comprehensive Tool Documentation
- Detailed descriptions of each tool
- Parameter specifications
- Return value explanations
- Usage examples

### 2. Usage Patterns
- Real-world workflows for common tasks
- Step-by-step examples
- Different use cases (understanding, debugging, implementing)

### 3. Best Practices
- Do's and don'ts for effective queries
- Performance considerations
- Common pitfalls to avoid

### 4. Dynamic Information
- Current database path
- Active model type
- Vector dimensions

### 5. Setup Instructions
- Quick start guide
- Troubleshooting hints
- Link to full documentation

## Comparison: CLI Help vs MCP Help

| Aspect | CLI (`codesearch mcp --help`) | MCP Instructions |
|--------|----------------------------|------------------|
| **When shown** | When user explicitly requests | On server connection |
| **Audience** | Humans setting up MCP | AI assistants |
| **Content** | Command syntax & flags | Tool usage & examples |
| **Updates** | Static | Can include runtime info |
| **User control** | Explicit (`--help`) | Automatic |

## Future Enhancements

### Potential Additions

1. **Interactive Help Tool**
   ```rust
   #[tool(description = "Get detailed help and usage examples for codesearch tools")]
   async fn help(&self) -> Result<CallToolResult, McpError> {
       // Return comprehensive help documentation
   }
   ```

2. **Tool-Specific Help**
   ```rust
   #[tool(description = "Get help for a specific tool")]
   async fn tool_help(&self, Parameters(req): Parameters<ToolHelpRequest>) -> Result<CallToolResult, McpError> {
       // Return detailed help for requested tool
   }
   ```

3. **Example Queries**
   ```rust
   #[tool(description = "Get example search queries for common scenarios")]
   async fn example_queries(&self) -> Result<CallToolResult, McpError> {
       // Return curated query examples
   }
   ```

### Why Not Implemented Yet?

- Current instructions cover most use cases
- Keep MCP interface simple (3 core tools)
- Can add if user feedback indicates need

## Testing the Help System

### 1. Build and Run

```bash
# Build the project
cargo build --release

# Start MCP server
./target/release/codesearch mcp /path/to/project
```

### 2. Verify Help Content

The help will be displayed when:
- You connect Claude Code to the MCP server
- You ask "What can codesearch do?"
- You request MCP server info

### 3. Check Dynamic Content

The help includes runtime information:
- Database path
- Model type (e.g., `minilm-l6-q`, `jina-code`)
- Vector dimensions (384, 768, 1024)

## Integration with Documentation

### File Hierarchy

```
codesearch/
├── README.md                              # General overview
├── AI_AGENT_CLI_INSTRUCTIONS.md          # CLI usage for AI agents
├── MCP_INSTRUCTIONS.md                    # Setup guide for MCP
├── MCP_HELP_SYSTEM.md                    # This file
└── src/mcp/mod.rs                        # Enhanced help in code
```

### Documentation Flow

1. **New User** → README.md → MCP_INSTRUCTIONS.md → Setup
2. **AI Agent** → AI_AGENT_CLI_INSTRUCTIONS.md → Integration
3. **Running MCP** → Automatic help on connection
4. **Need More** → Full documentation at GitHub

## Summary

### ✅ What We Have

1. **CLI Help**: `codesearch mcp --help` - Basic command help
2. **MCP Instructions**: Comprehensive help displayed on connection
3. **Dynamic Info**: Runtime data included in help
4. **Usage Patterns**: Real-world workflows documented

### 📝 What Works

- AI assistants automatically receive help on connection
- Users can query "What can codesearch do?"
- Examples and best practices included
- Performance and setup information provided

### 🚀 What's Possible

- Add interactive `help()` tool if needed
- Add `tool_help()` for tool-specific documentation
- Add `example_queries()` for curated query examples
- Expand based on user feedback

## Conclusion

The MCP help system is now comprehensive and user-friendly. AI assistants like Claude Code automatically receive detailed instructions when connecting to the codesearch MCP server, including:

- Available tools and their usage
- Real-world usage patterns
- Best practices and pitfalls
- Performance characteristics
- Setup instructions
- Dynamic runtime information

No explicit help call is needed - it's all automatic!
