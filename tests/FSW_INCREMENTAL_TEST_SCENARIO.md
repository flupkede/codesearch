# FSW + Incremental Indexing Test Scenario

## Overview

This test verifies that the File System Watcher (FSW) correctly detects file changes, updates the index incrementally, and that the MCP tools reflect these changes immediately.

**CRITICAL:** This test uses ONLY MCP tools. NO codesearch CLI commands should be executed during this test. The FSW must handle all index updates automatically.

## Prerequisites

- codesearch MCP server running (via OpenCode or Claude Code)
- An indexed project with a working `.codesearch.db` directory
- FSW must be enabled and running (it starts automatically with MCP server)

## Test Steps

### Step 1: Initial State Verification

Before making any changes, record the current baseline using MCP tools only.

```javascript
// Get initial index status
codesearch_index_status()

// Get file chunks for the file we'll modify
codesearch_get_file_chunks({
  path: "src/index/mod.rs",
  compact: true
})
```

Record:
- Chunk count from index status
- Last chunk's end_line from get_file_chunks
- Total chunk count for the specific file

### Step 2: Make File Changes

Add a unique test string to a tracked file. Use a timestamp or UUID to ensure uniqueness.

**Example - Add comment to `src/index/mod.rs`:**

```rust
// FSW_TEST - Unique test string for File System Watcher verification: FSW_TEST_20250209_UNIQUE_STRING_ABCD123
```

**Add this line at the end of the file, after the last existing line.**

**Verify the change exists:**
- Open the file in your editor
- Confirm the new line is present
- Note the exact line number

### Step 3: Wait for FSW Detection

The FSW has a debounce interval (typically 2-5 seconds). Wait for the file system watcher to detect and process the change.

**Wait 10-15 seconds** to ensure:
1. FSW detects the file modification (mtime change)
2. FSW debounces to avoid multiple rapid updates
3. FSW runs incremental index on changed files only
4. Index is updated and ready for queries

**Do NOT run any codesearch CLI commands during this wait.**

### Step 4: Verify Index Update Using MCP Tools

Use MCP tools to verify the change is now in the index.

**4a. Semantic Search**

```javascript
codesearch_semantic_search({
  query: "FSW_TEST unique string file system watcher verification",
  limit: 5,
  compact: true
})
```

**Expected Result:**
- ✅ Should find the modified file in results
- ✅ Path should point to the file you modified
- ✅ Score should indicate relevance (>0.5 is good)
- ✅ Result should be within top 5 matches

**4b. Get File Chunks**

```javascript
codesearch_get_file_chunks({
  path: "src/index/mod.rs",
  compact: true
})
```

**Expected Result:**
- ✅ Total chunk count should have increased (or last chunk end_line increased)
- ✅ Last chunk's end_line should be > original baseline
- ✅ The file structure should include the new content

**4c. Index Status**

```javascript
codesearch_index_status()
```

**Expected Result:**
- ✅ Chunk count may have increased (depending on chunking)
- ✅ Database should show recent update

### Step 5: Find References (Optional)

If the change includes a searchable symbol/function name:

```javascript
codesearch_find_references({
  symbol: "FSW_TEST",
  limit: 10
})
```

**Expected Result:**
- ✅ Should find the new symbol reference
- ✅ Should show the file path and line number
- ✅ Result count should be >= 1

### Step 6: Revert Changes

Remove the test string to verify deletion is also detected by FSW.

**Undo the change:**
- Delete the test line from the file
- Save the file
- Confirm file is back to original state

**Do NOT run `git checkout` or any CLI commands to revert - use your editor only.**

### Step 7: Wait for FSW Detection Again

Wait for FSW to detect the file deletion/update:

**Wait 10-15 seconds** for:
1. FSW detects file modification
2. FSW debounces
3. FSW runs incremental index
4. Index reflects the deletion

**Do NOT run any codesearch CLI commands during this wait.**

### Step 8: Verify Deletion in Index

Use MCP tools to verify the change is gone.

**8a. Semantic Search**

```javascript
codesearch_semantic_search({
  query: "FSW_TEST unique string file system watcher verification",
  limit: 5,
  compact: true
})
```

**Expected Result:**
- ✅ Should NOT find the modified file in results for this query
- ✅ Results should show different files or fewer results
- ✅ The previously found result should be gone

**8b. Get File Chunks**

```javascript
codesearch_get_file_chunks({
  path: "src/index/mod.rs",
  compact: true
})
```

**Expected Result:**
- ✅ Total chunk count should match original baseline
- ✅ Last chunk's end_line should match original baseline
- ✅ File structure should be back to original state

**8c. Index Status**

```javascript
codesearch_index_status()
```

**Expected Result:**
- ✅ Chunk count should match original baseline
- ✅ Database should show recent update

### Step 9: Verify Reference Cleanup (If Step 5 was performed)

```javascript
codesearch_find_references({
  symbol: "FSW_TEST",
  limit: 10
})
```

**Expected Result:**
- ✅ Should NOT find any references
- ✅ Should return empty or no results

## Success Criteria

The test **PASSES** only if ALL of the following are true:

✅ **Step 1:** Initial baseline recorded via MCP tools
✅ **Step 2:** File change successfully made (verified manually)
✅ **Step 4a:** Semantic search finds the change after waiting
✅ **Step 4b:** File chunks show increased line count
✅ **Step 4c:** Index status shows recent update
✅ **Step 5:** Reference search finds the symbol (if applicable)
✅ **Step 6:** Change successfully reverted (verified manually)
✅ **Step 8a:** Semantic search NO LONGER finds the change after waiting
✅ **Step 8b:** File chunks show original line count (back to baseline)
✅ **Step 8c:** Index status reflects deletion
✅ **Step 9:** Reference search returns no results (if applicable)

## Expected Behavior

### What SHOULD Happen

1. **File is modified** → FSW detects within 2-5 seconds
2. **FSW debounces** → Waits for no more changes for ~2 seconds
3. **Incremental index runs** → Only the changed file is re-processed
4. **Index updates** → Search results immediately reflect the change
5. **File is reverted** → FSW detects and re-indexes
6. **Search results update** → Old content is removed from index

### What MUST NOT Happen

❌ Running `codesearch index` or any CLI commands
❌ Waiting indefinitely without seeing changes
❌ Changes not appearing in search results
❌ Need to manually refresh or restart the MCP server

## Troubleshooting

### Change Not Found After Waiting

**Symptoms:** Semantic search doesn't find the new content after 15+ seconds

**This is a BUG - FSW should have updated the index automatically!**

**Debug Steps:**
1. Check if MCP server is running (it should be if you're using OpenCode/Claude Code)
2. Check if the FSW process is active (look for file watcher logs)
3. Verify the file is not ignored (check `.gitignore`, `.codesearchignore`)
4. Check for any error messages in MCP server output

**Do NOT run `codesearch index` - this defeats the purpose of the FSW test.**

**Report the bug if:**
- FSW is running but changes don't appear in search
- No error messages are shown
- Changes take > 30 seconds to appear

### Database Lock Conflict

**Symptoms:** MCP tools fail with database lock errors

**Possible Causes:**
- Previous MCP session didn't clean up properly
- Multiple codesearch MCP instances running

**Solutions:**
1. Restart your AI coding agent (OpenCode/Claude Code)
2. This will kill any orphaned processes
3. The MCP server will restart cleanly

### File Not Indexed

**Symptoms:** File change made but never appears in search results

**Possible Causes:**
- File matches ignore patterns
- File is binary (not supported)
- File path is outside indexed directory

**Solutions:**
1. Choose a different test file (e.g., a `.rs` or `.ts` file in `src/`)
2. Verify the file is tracked by git (not in `.gitignore`)
3. Ensure file is not binary

## Expected Timing

| Operation | Expected Time |
|-----------|---------------|
| FSW detection | 2-5 seconds (debounce) |
| Incremental index | 1-3 seconds (single file) |
| Search response | <100ms |
| Full round-trip (modify → see in search) | ~10 seconds |
| Full round-trip (revert → disappear) | ~10 seconds |

## Test Automation (for Windows - PowerShell)

**Note:** This is optional. The test is designed to be run manually using MCP tools. This script is provided for convenience but is not required.

```powershell
# FSW Test Automation Script (PowerShell)
# Usage: .\test_fsw.ps1

$ErrorActionPreference = "Stop"

$TestFile = "src\index\mod.rs"
$TestString = "// FSW_TEST - $(Get-Date -Format 'yyyyMMddHHmmss')_UNIQUE_TEST"

Write-Host "=== FSW Test Start ===" -ForegroundColor Green

# Step 1: Get baseline using MCP tools (manual step)
Write-Host "Step 1: Get baseline using MCP tools:" -ForegroundColor Yellow
Write-Host "  Run: codesearch_index_status()"
Write-Host "  Run: codesearch_get_file_chunks({path: '$TestFile', compact: true})"
Write-Host ""
Read-Host "Press Enter when ready to continue"

# Step 2: Add change
Write-Host "Step 2: Adding test string to file..." -ForegroundColor Yellow
Add-Content -Path $TestFile -Value $TestString
Write-Host "  Added: $TestString"
Write-Host ""
Read-Host "Press Enter when ready to continue"

# Step 3: Wait for FSW
Write-Host "Step 3: Waiting for FSW (15 seconds)..." -ForegroundColor Yellow
Start-Sleep -Seconds 15

# Step 4: Verify using MCP tools
Write-Host "Step 4: Verify change is indexed using MCP tools:" -ForegroundColor Yellow
Write-Host "  Run: codesearch_semantic_search({query: 'FSW_TEST', limit: 5, compact: true})"
Write-Host "  Run: codesearch_get_file_chunks({path: '$TestFile', compact: true})"
Write-Host ""
Read-Host "Press Enter when ready to continue"

# Step 5: Find references (optional)
Write-Host "Step 5: Find references (optional):" -ForegroundColor Yellow
Write-Host "  Run: codesearch_find_references({symbol: 'FSW_TEST', limit: 10})"
Write-Host ""
Read-Host "Press Enter when ready to continue"

# Step 6: Revert
Write-Host "Step 6: Reverting change..." -ForegroundColor Yellow
$content = Get-Content $TestFile
$content = $content | Where-Object { $_ -ne $TestString }
$content | Set-Content $TestFile
Write-Host "  Change reverted"
Write-Host ""
Read-Host "Press Enter when ready to continue"

# Step 7: Wait for FSW
Write-Host "Step 7: Waiting for FSW (15 seconds)..." -ForegroundColor Yellow
Start-Sleep -Seconds 15

# Step 8: Verify deletion
Write-Host "Step 8: Verify change is gone using MCP tools:" -ForegroundColor Yellow
Write-Host "  Run: codesearch_semantic_search({query: 'FSW_TEST', limit: 5, compact: true})"
Write-Host "  Run: codesearch_get_file_chunks({path: '$TestFile', compact: true})"
Write-Host ""
Read-Host "Press Enter when ready to continue"

Write-Host "=== FSW Test Complete ===" -ForegroundColor Green
```

Save as `test_fsw.ps1` and run with PowerShell. Note that this script only modifies files - it does NOT run any codesearch CLI commands. All verification is done via MCP tools.

## Important Notes

1. **NEVER run `codesearch index` during this test** - that would defeat the purpose
2. The FSW must handle all index updates automatically
3. If changes don't appear after 15+ seconds, it's a BUG in FSW
4. This test validates the end-to-end FSW + MCP integration
5. The test verifies both addition and deletion of content
6. Only MCP tools are used for verification - no CLI commands

## Related Tests

- Unit test: `tests/test_fsw_incremental.rs` - Automated test for this scenario
- Integration test: `tests/integration_tests.rs` - General integration tests
- Manual test via `codesearch serve` - For manual FSW testing without MCP
