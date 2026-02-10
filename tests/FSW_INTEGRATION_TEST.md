# FSW Incremental Indexing Integration Test

## Overview

This integration test verifies that the File System Watcher (FSW) correctly detects file changes and updates the index incrementally using ONLY MCP tools.

**CRITICAL RULES:**
- ❌ NO codesearch CLI commands (index, serve, stats, etc.)
- ❌ NO manual database operations
- ❌ NO starting/stopping MCP server (already running)
- ✅ ONLY MCP tool calls (semantic_search, find_references, get_file_chunks, index_status)
- ✅ Test adds/removes real files from the codebase
- ✅ FSW must auto-update index (no manual intervention)

## Test Data Location

Test code is located at: `tests/test_fsw_project/lib.rs`

Additional test file for individual file deletion: `tests/test_fsw_project/utils.rs`

These files contain:
- Real methods with actual logic and dependencies
- Text strings for FTS search (unique test strings)
- Code structures for semantic search (functions, structs, traits)
- Dependencies between modules (auth, data_processing, network, utils)

## Unique Search Targets

### Text Search Strings (for semantic_search and FTS):
1. `AUTH_TEST_UNIQUE_STRING_FOR_TEXT_SEARCH_20240209_ABC123` - in UserCredentials struct (lib.rs)
2. `AUTHENTICATE_USER_METHOD_UNIQUE_TEXT_STRING_XYZ789` - in authenticate_user method (lib.rs)
3. `DATA_PROCESSING_TEST_STRING_FOR_SEARCH_20240209_DEF456` - in DataRecord struct (lib.rs)
4. `NETWORK_SERVICE_TEST_UNIQUE_TEXT_20240209_GHI789` - in HttpResponse struct (lib.rs)
5. `VALIDATE_EMAIL_FUNCTION_UNIQUE_STRING_JKL012` - in validate_email function (lib.rs)
6. `UTILS_FILE_DELETE_TEST_STRING_20240209_MNO345` - ONLY in utils.rs (for individual file deletion test)

### Code/Method Search Targets (for semantic_search and find_references):
1. `authenticate_user` - Authentication method with real logic (lib.rs)
2. `DataProcessor::new` - Constructor with dependencies (lib.rs)
3. `NetworkService::handle_request` - Request handling method (lib.rs)
4. `validate_email` - Email validation with regex (lib.rs)
5. `Middleware::process` - Trait method for request processing (lib.rs)
6. `sanitize_input` - Input sanitization function (lib.rs)
7. `format_duration` - Duration formatting function (lib.rs)

### Code/Method Search Targets (for semantic_search and find_references):
1. `authenticate_user` - Authentication method with real logic
2. `DataProcessor::new` - Constructor with dependencies
3. `NetworkService::handle_request` - Request handling method
4. `validate_email` - Email validation with regex
5. `Middleware::process` - Trait method for request processing

## Test Procedure

### Step 1: Verify Test File Does Not Exist Yet

```javascript
// Try to find test file - should NOT exist
codesearch_semantic_search({
  query: "AUTH_TEST_UNIQUE_STRING_FOR_TEXT_SEARCH_20240209_ABC123",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ NO results (test file not indexed yet)

---

### Step 2: Create Test Files

The test files should exist in `tests/test_fsw_project/`:

```bash
# Check files exist
ls -la tests/test_fsw_project/
# Should show: lib.rs, utils.rs
```

Files to create:
- `tests/test_fsw_project/lib.rs` - Full Rust library with all modules (auth, data_processing, network)
- `tests/test_fsw_project/utils.rs` - Utility module with helper functions (contains UTILS_FILE_DELETE_TEST_STRING)

Both files contain unique search strings for testing file-specific deletion.

---

### Step 3: Wait for FSW to Detect and Index

Wait 10-15 seconds for FSW to:
1. Detect the new file
2. Debounce (wait for no more changes)
3. Run incremental index
4. Update the search index

**Do NOT run any codesearch CLI commands.**

---

### Step 4: Verify File is Indexed

#### 4a. Text Search - Find Unique Strings

```javascript
// Test string 1 - UserCredentials
codesearch_semantic_search({
  query: "AUTH_TEST_UNIQUE_STRING_FOR_TEXT_SEARCH_20240209_ABC123",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `tests/test_fsw_project/lib.rs` in results

```javascript
// Test string 2 - authenticate_user method
codesearch_semantic_search({
  query: "AUTHENTICATE_USER_METHOD_UNIQUE_TEXT_STRING_XYZ789",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `tests/test_fsw_project/lib.rs` in results

```javascript
// Test string 3 - DataRecord
codesearch_semantic_search({
  query: "DATA_PROCESSING_TEST_STRING_FOR_SEARCH_20240209_DEF456",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `tests/test_fsw_project/lib.rs` in results

#### 4b. Code Search - Find Methods

```javascript
// Find authenticate_user method
codesearch_semantic_search({
  query: "authenticate user with username password validation",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `tests/test_fsw_project/lib.rs::auth::AuthService::authenticate_user`

```javascript
// Find DataProcessor
codesearch_semantic_search({
  query: "data processor with batch size aggregation mode",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `tests/test_fsw_project/lib.rs::data_processing::DataProcessor`

#### 4c. Find References - Method Call Sites

```javascript
// Find all references to authenticate_user
codesearch_find_references({
  symbol: "authenticate_user",
  limit: 10
})
```

**Expected Result:** ✅ Finds at least 1 reference in `tests/test_fsw_project/lib.rs`

```javascript
// Find all references to validate_email
codesearch_find_references({
  symbol: "validate_email",
  limit: 10
})
```

**Expected Result:** ✅ Finds at least 1 reference in `tests/test_fsw_project/lib.rs`

#### 4d. Get File Chunks - Verify Structure

```javascript
codesearch_get_file_chunks({
  path: "tests/test_fsw_project/lib.rs",
  compact: true
})
```

**Expected Result:** ✅ Returns multiple chunks with signatures for:
- `auth::UserCredentials`
- `auth::AuthService::new`
- `auth::AuthService::register_user`
- `auth::AuthService::authenticate_user`
- `auth::AuthService::validate_session`
- `data_processing::DataRecord`
- `data_processing::DataProcessor`
- `data_processing::DataProcessor::new`
- `network::HttpResponse`
- `network::HttpRequest`
- `network::NetworkService`
- `network::NetworkService::handle_request`
- `utils::validate_email`
- `utils::sanitize_input`
- `utils::format_duration`
- `utils::levenshtein_distance`

#### 4e. Index Status Check

```javascript
codesearch_index_status()
```

**Expected Result:** ✅ Chunk count has increased (from baseline)

---

### Step 5: Search for Specific Functionality

#### 5a. Search for Authentication Logic

```javascript
codesearch_semantic_search({
  query: "password validation hash verification authentication",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `auth::AuthService::authenticate_user` method

#### 5b. Search for Data Aggregation

```javascript
codesearch_semantic_search({
  query: "sum average min max aggregation batch processing",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `data_processing::DataProcessor::process_batch` method

#### 5c. Search for Middleware

```javascript
codesearch_semantic_search({
  query: "middleware trait process request authentication logging",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `network::Middleware::process` and implementations

#### 5d. Search for Utility Functions

```javascript
codesearch_semantic_search({
  query: "email validation regex pattern",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `utils::validate_email` function

```javascript
codesearch_semantic_search({
  query: "string distance levenshtein algorithm",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `utils::levenshtein_distance` function

---

### Step 6: Verify Search Accuracy

Each search should return results with:
- ✅ Path pointing to `tests/test_fsw_project/lib.rs`
- ✅ Meaningful scores (> 0.3 indicates relevance)
- ✅ Correct signatures (method names, struct names)

---

### Step 7: Delete Single Test File (Individual File Deletion Test)

**NEW TEST:** Verify FSW handles individual file deletions correctly (not just folder deletions).

First verify utils.rs content is searchable:

```javascript
// Verify utils.rs specific string
codesearch_semantic_search({
  query: "UTILS_FILE_DELETE_TEST_STRING_20240209_MNO345",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Finds `tests/test_fsw_project/utils.rs`

Now delete only utils.rs (NOT the entire folder):

```bash
# Delete only utils.rs
rm -f tests/test_fsw_project/utils.rs

# Verify lib.rs still exists
ls -la tests/test_fsw_project/
# Should show: lib.rs (but NOT utils.rs)
```

---

### Step 8: Wait for FSW to Detect Single File Deletion

Wait 10-15 seconds for FSW to:
1. Detect the utils.rs file deletion
2. Debounce
3. Run incremental index
4. Remove only utils.rs content (keep lib.rs)

**Do NOT run any codesearch CLI commands.**

---

### Step 9: Verify Single File Deletion

#### 9a. Verify utils.rs content is gone

```javascript
// Should NOT find utils.rs specific string
codesearch_semantic_search({
  query: "UTILS_FILE_DELETE_TEST_STRING_20240209_MNO345",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ NO results (utils.rs removed)

#### 9b. Verify lib.rs content still exists

```javascript
// Should still find lib.rs strings
codesearch_semantic_search({
  query: "AUTH_TEST_UNIQUE_STRING_FOR_TEXT_SEARCH_20240209_ABC123",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Still finds `tests/test_fsw_project/lib.rs`

```javascript
// Should still find lib.rs methods
codesearch_semantic_search({
  query: "authenticate user with username password validation",
  limit: 5,
  compact: true
})
```

**Expected Result:** ✅ Still finds `tests/test_fsw_project/lib.rs`

#### 9c. Get File Chunks - Verify utils.rs gone, lib.rs still exists

```javascript
// utils.rs should be gone
codesearch_get_file_chunks({
  path: "tests/test_fsw_project/utils.rs",
  compact: true
})
```

**Expected Result:** ❌ Returns empty or error (file removed from index)

```javascript
// lib.rs should still exist
codesearch_get_file_chunks({
  path: "tests/test_fsw_project/lib.rs",
  compact: true
})
```

**Expected Result:** ✅ Returns chunks from lib.rs

#### 9d. Index Status Check

```javascript
codesearch_index_status()
```

**Expected Result:** ✅ Chunk count decreased (utils.rs removed, lib.rs still present)

---

### Step 10: Delete Entire Test Folder (Directory Deletion Test)

Now remove the test file to verify FSW handles deletions:

```bash
# Delete the test file
rm -f tests/test_fsw_project/lib.rs
rm -rf tests/test_fsw_project/
```

**Verify deletion:**
```bash
ls -la tests/test_fsw_project/
# Should show "No such file or directory"
```

---

### Step 11: Wait for FSW to Detect Folder Deletion

Wait 10-15 seconds for FSW to:
1. Detect the folder deletion
2. Debounce
3. Run incremental index
4. Remove all files from folder from search index

**Do NOT run any codesearch CLI commands.**

---

### Step 12: Verify Folder is Removed from Index

#### 9a. Text Search - Confirm Unique Strings Gone

```javascript
// Test string 1 - Should NOT find
codesearch_semantic_search({
  query: "AUTH_TEST_UNIQUE_STRING_FOR_TEXT_SEARCH_20240209_ABC123",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ NO results (file removed from index)

```javascript
// Test string 2 - Should NOT find
codesearch_semantic_search({
  query: "AUTHENTICATE_USER_METHOD_UNIQUE_TEXT_STRING_XYZ789",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ NO results (file removed from index)

```javascript
// Test string 3 - Should NOT find
codesearch_semantic_search({
  query: "DATA_PROCESSING_TEST_STRING_FOR_SEARCH_20240209_DEF456",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ NO results (file removed from index)

#### 9b. Code Search - Confirm Methods Gone

```javascript
// Should NOT find authenticate_user
codesearch_semantic_search({
  query: "authenticate user with username password validation",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ Does NOT return `tests/test_fsw_project/lib.rs`

```javascript
// Should NOT find DataProcessor
codesearch_semantic_search({
  query: "data processor with batch size aggregation mode",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ Does NOT return `tests/test_fsw_project/lib.rs`

#### 9c. Find References - Confirm References Gone

```javascript
// Should NOT find references to authenticate_user from test file
codesearch_find_references({
  symbol: "authenticate_user",
  limit: 10
})
```

**Expected Result:** ❌ Results do NOT include `tests/test_fsw_project/lib.rs`

```javascript
// Should NOT find references to validate_email from test file
codesearch_find_references({
  symbol: "validate_email",
  limit: 10
})
```

**Expected Result:** ❌ Results do NOT include `tests/test_fsw_project/lib.rs`

#### 9d. Get File Chunks - Confirm File Gone

```javascript
codesearch_get_file_chunks({
  path: "tests/test_fsw_project/lib.rs",
  compact: true
})
```

**Expected Result:** ❌ Returns empty or error (file not in index)

#### 9e. Index Status Check

```javascript
codesearch_index_status()
```

**Expected Result:** ✅ Chunk count should match baseline (before test file was added)

---

### Step 13: Search for Removed Functionality

```javascript
// Should NOT find authentication logic from test file
codesearch_semantic_search({
  query: "password validation hash verification authentication",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ Does NOT return results from `tests/test_fsw_project/lib.rs`

```javascript
// Should NOT find middleware from test file
codesearch_semantic_search({
  query: "middleware trait process request authentication logging",
  limit: 5,
  compact: true
})
```

**Expected Result:** ❌ Does NOT return results from `tests/test_fsw_project/lib.rs`

---

## Test Report Format

After completing all steps, the test should report:

```
# FSW Incremental Indexing Test Report

## Test Steps Executed: ✅

### Step 1: Verify test file does not exist
- Status: PASSED ✅
- Details: No results for test strings

### Step 2: Create test file
- Status: PASSED ✅
- File: tests/test_fsw_project/lib.rs
- Size: ~600 lines of real code

### Step 3: Wait for FSW detection
- Wait time: 15 seconds
- Status: PASSED ✅

### Step 4: Verify file indexed
#### 4a. Text search (3 unique strings): PASSED ✅
- AUTH_TEST_UNIQUE_STRING: Found ✅
- AUTHENTICATE_USER_METHOD_UNIQUE: Found ✅
- DATA_PROCESSING_TEST_STRING: Found ✅

#### 4b. Code search (2 methods): PASSED ✅
- authenticate_user: Found ✅
- DataProcessor::new: Found ✅

#### 4c. Find references (2 symbols): PASSED ✅
- authenticate_user: Found ✅
- validate_email: Found ✅

#### 4d. Get file chunks: PASSED ✅
- Chunks found: 20+ ✅
- All expected structures present ✅

#### 4e. Index status: PASSED ✅
- Chunk count increased ✅

### Step 5: Search specific functionality (5 searches): PASSED ✅
- Authentication logic: Found ✅
- Data aggregation: Found ✅
- Middleware: Found ✅
- Email validation: Found ✅
- Levenshtein distance: Found ✅

### Step 6: Verify search accuracy: PASSED ✅
- All results point to correct file ✅
- All scores meaningful ✅
- All signatures correct ✅

### Step 7: Delete single test file (utils.rs)
- Status: PASSED ✅
- utils.rs removed, lib.rs still exists ✅

### Step 8: Wait for FSW detection (single file)
- Wait time: 15 seconds
- Status: PASSED ✅

### Step 9: Verify single file deletion
#### 9a. utils.rs strings gone: PASSED ✅
- UTILS_FILE_DELETE_TEST_STRING: Gone ✅

#### 9b. lib.rs still exists: PASSED ✅
- lib.rs strings: Found ✅
- lib.rs methods: Found ✅

#### 9c. File chunks check: PASSED ✅
- utils.rs: Gone ✅
- lib.rs: Found ✅

#### 9d. Index status: PASSED ✅
- Chunk count decreased correctly ✅

### Step 10: Delete entire folder
- Status: PASSED ✅
- Folder removed successfully ✅

### Step 11: Wait for FSW detection (folder)
- Wait time: 15 seconds
- Status: PASSED ✅

### Step 12: Verify folder removed from index
#### 9a. Text search (3 strings): PASSED ✅
- AUTH_TEST_UNIQUE_STRING: Gone ✅
- AUTHENTICATE_USER_METHOD_UNIQUE: Gone ✅
- DATA_PROCESSING_TEST_STRING: Gone ✅

#### 9b. Code search (2 methods): PASSED ✅
- authenticate_user: Gone ✅
- DataProcessor::new: Gone ✅

#### 9c. Find references (2 symbols): PASSED ✅
- authenticate_user: Gone ✅
- validate_email: Gone ✅

#### 9d. Get file chunks: PASSED ✅
- File not in index ✅

#### 9e. Index status: PASSED ✅
- Chunk count back to baseline ✅

### Step 13: Search removed functionality (2 searches): PASSED ✅
- Authentication logic: Gone ✅
- Middleware: Gone ✅

## Overall Result: PASSED ✅

All 13 steps completed successfully. FSW correctly:
1. Detected file addition (2 files)
2. Indexed new content incrementally
3. Made content searchable via all MCP tools
4. Detected individual file deletion (utils.rs)
5. Removed only utils.rs from index, kept lib.rs
6. Detected folder deletion (test_fsw_project/)
7. Removed all folder content from index
8. Updated search results correctly

## Test Metrics
- Total searches: 25+
- Successful searches: 25+ (100%)
- Files added: 2 (lib.rs, utils.rs)
- Files removed: 2 (utils.rs individually, then folder with lib.rs)
- Unique strings tested: 6
- Methods tested: 7
- References tested: 4
- Total wait time: 45 seconds
- Total test time: ~3 minutes
```

---

## Troubleshooting

### Test File Not Indexed After Waiting

**Symptom:** Semantic search doesn't find test file after 15+ seconds

**This is a BUG - FSW should have auto-updated the index!**

**Do NOT run `codesearch index` - that defeats the purpose of this test.**

**Debug:**
1. Check if MCP server is running (it should be if you're using this agent)
2. Look for FSW errors in MCP server output
3. Verify file exists: `ls -la tests/test_fsw_project/lib.rs`

**Report bug if:**
- File exists but never appears in search
- No error messages shown
- Takes > 30 seconds to appear

### Content Still Found After Deletion

**Symptom:** Search still finds test file content after deletion

**This is a BUG - FSW should have removed it from index!**

**Debug:**
1. Verify file is deleted: `ls -la tests/test_fsw_project/`
2. Wait additional 10 seconds
3. Try different search queries

**Report bug if:**
- File is deleted but content still searchable
- Takes > 30 seconds to disappear
- Index status doesn't update

### Partial Results

**Symptom:** Some searches find content, others don't

**Possible Causes:**
- Index partially updated (FSW still processing)
- Different search modes return different results
- Timing issue (searched too soon)

**Solution:**
- Wait additional 5-10 seconds
- Re-run failed searches
- Check index status

---

## Notes

- This test validates FSW + MCP integration end-to-end
- Test file contains 600+ lines of real, realistic code
- All searches use MCP tools only - no CLI commands
- FSW must handle ALL index updates automatically
- No manual intervention during test
- Test passes only if ALL 10 steps succeed

---

## Execution Instructions

To run this test:

1. Ensure MCP server is running (OpenCode agent)
2. Follow each step in order
3. Use EXACT search queries provided
4. Wait specified time after file operations
5. Report results in Test Report Format
6. Do NOT skip any steps
7. Do NOT use any codesearch CLI commands

**Estimated Time:** 2-3 minutes
**Success Rate:** All 10 steps must pass
**Critical Failure:** Any step fails = FSW bug
