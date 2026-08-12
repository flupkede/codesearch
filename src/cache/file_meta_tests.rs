use super::*;
use tempfile::tempdir;

// ── safe_canonicalize / strip_unc_prefix ────────────────────────────────

#[test]
fn strip_unc_prefix_removes_windows_unc() {
    let unc = PathBuf::from(r"\\?\C:\WorkArea\AI\foo");
    let stripped = strip_unc_prefix(unc);
    assert_eq!(stripped, PathBuf::from(r"C:\WorkArea\AI\foo"));
}

#[test]
fn strip_unc_prefix_is_idempotent_on_plain_path() {
    let plain = PathBuf::from(r"C:\WorkArea\AI\foo");
    let result = strip_unc_prefix(plain.clone());
    assert_eq!(result, plain);
}

#[test]
fn strip_unc_prefix_is_idempotent_on_unix_path() {
    let unix = PathBuf::from("/home/user/project");
    let result = strip_unc_prefix(unix.clone());
    assert_eq!(result, unix);
}

/// `safe_canonicalize` on an existing directory must return a plain path
/// (no `\\?\` prefix) that `Path::exists()` confirms is reachable.
/// This is the core regression guard for the class of bugs where UNC paths
/// caused `.join(".codesearch.db").exists()` to return false.
#[test]
fn safe_canonicalize_on_existing_dir_returns_plain_path() {
    let tmp = tempdir().unwrap();
    let result = safe_canonicalize(tmp.path()).unwrap();
    let s = result.to_string_lossy();
    assert!(
        !s.starts_with(r"\\?\"),
        "safe_canonicalize must strip UNC prefix, got: {}",
        s
    );
    // The returned path must still be a valid, accessible directory.
    assert!(
        result.exists(),
        "safe_canonicalize result must exist: {}",
        s
    );
    // A sub-path join must also be resolvable — this is what was broken.
    let sub = result.join("dummy_check");
    // exists() returns false (dir doesn't exist) but must NOT panic or error
    let _ = sub.exists();
}

#[test]
fn safe_canonicalize_on_nonexistent_path_returns_error() {
    let nonexistent = PathBuf::from(r"C:\this\path\does\not\exist\ever");
    assert!(
        safe_canonicalize(&nonexistent).is_err(),
        "safe_canonicalize must propagate canonicalize() errors"
    );
}

// ── translate_msys_path ─────────────────────────────────────────────────
//
// Regression guard for the `<repo>-propagate-tmp` path-pollution defect:
// an agent-supplied POSIX path (`/c/Users/...`) slipped past canonicalize
// and got materialised on Windows as `<current-drive>:\c\Users\...`, creating
// junk `C:\c\Users\...` directories. translate_msys_path closes that hole.

#[cfg(windows)]
#[test]
fn translate_msys_path_converts_single_letter_drive() {
    let cases: &[(&str, &str)] = &[
        // lowercase drive → uppercase
        ("/c/Users/foo", "C:/Users/foo"),
        // uppercase drive → unchanged
        ("/D/data/repo", "D:/data/repo"),
        // bare drive root
        ("/c", "C:/"),
        // drive root with trailing slash
        ("/c/", "C://"),
        // deeply nested
        ("/z/a/b/c/d/e/f", "Z:/a/b/c/d/e/f"),
    ];
    for (input, expected) in cases {
        let got = translate_msys_path(&PathBuf::from(input));
        assert_eq!(
            got,
            PathBuf::from(*expected),
            "translate_msys_path({:?}): expected {:?}, got {:?}",
            input,
            expected,
            got
        );
    }
}

#[cfg(windows)]
#[test]
fn translate_msys_path_leaves_non_drive_paths_untouched() {
    // These look like POSIX paths but are NOT single-letter-drive MSYS paths
    // — they must be left as-is so genuine Unix-isms (/usr, /etc, multi-char)
    // aren't accidentally rewritten.
    let cases: &[&str] = &[
        "/usr/bin/foo",    // multi-char first segment
        "/ab/foo",         // two-letter drive → not a Windows drive
        "/home/user",      // multi-char
        "/1/foo",          // digit, not a letter
        "/_foo",           // underscore, not a letter
        "relative/path",   // not absolute
        "relative/c/path", // relative despite single-letter segment
        ".",               // bare relative
        "",                // empty
    ];
    for input in cases {
        let got = translate_msys_path(&PathBuf::from(*input));
        assert_eq!(
            got,
            PathBuf::from(*input),
            "translate_msys_path({:?}) must be a no-op, got {:?}",
            input,
            got
        );
    }
}

#[cfg(windows)]
#[test]
fn translate_msys_path_leaves_existing_windows_paths_untouched() {
    let cases: &[&str] = &[
        r"C:\Users\foo",
        "C:/Users/foo",
        r"\\?\C:\Users\foo", // UNC verbatim
        r"D:\",
    ];
    for input in cases {
        let got = translate_msys_path(&PathBuf::from(*input));
        assert_eq!(
            got,
            PathBuf::from(*input),
            "translate_msys_path must not touch existing Windows paths: {:?} → {:?}",
            input,
            got
        );
    }
}

#[cfg(not(windows))]
#[test]
fn translate_msys_path_is_noop_on_unix() {
    // On Unix `/c/Users/foo` is a legitimate absolute path, not an MSYS-ism.
    assert_eq!(
        translate_msys_path(&PathBuf::from("/c/Users/foo")),
        PathBuf::from("/c/Users/foo")
    );
    assert_eq!(
        translate_msys_path(&PathBuf::from("/home/user")),
        PathBuf::from("/home/user")
    );
}

// ── normalize_user_path ─────────────────────────────────────────────────
//
// Single helper used by every "safe_canonicalize(...).unwrap_or_else(_)"
// fallback site (register, unregister_path, alias_for_path, scan_for_remote,
// resolve_database_with_message, try_delegate_*_to_serve, run_serve). Tests
// pin both the translate + UNC-strip composition and the contract that
// makes it safe to call on already-clean paths.

#[cfg(windows)]
#[test]
fn normalize_user_path_translates_msys_and_strips_unc() {
    // MSYS path → translated, no UNC to strip (input is not yet canonical).
    assert_eq!(
        normalize_user_path(&PathBuf::from("/c/Users/foo")),
        PathBuf::from("C:/Users/foo")
    );
    // Already-canonical UNC path → UNC stripped, no translate needed
    // (first byte is `\`, not `/`).
    assert_eq!(
        normalize_user_path(&PathBuf::from(r"\\?\C:\Users\foo")),
        PathBuf::from(r"C:\Users\foo")
    );
    // Already-clean Windows path → idempotent.
    assert_eq!(
        normalize_user_path(&PathBuf::from(r"C:\Users\foo")),
        PathBuf::from(r"C:\Users\foo")
    );
}

#[cfg(not(windows))]
#[test]
fn normalize_user_path_only_strips_unc_on_unix() {
    // No MSYS translation on Unix; UNC strip is a no-op on a non-Windows
    // path but the function must still be safe to call.
    assert_eq!(
        normalize_user_path(&PathBuf::from("/c/Users/foo")),
        PathBuf::from("/c/Users/foo")
    );
    assert_eq!(
        normalize_user_path(&PathBuf::from("/home/user")),
        PathBuf::from("/home/user")
    );
}

#[cfg(windows)]
#[test]
fn test_normalize_path_windows_forms() {
    // Previously 5 separate #[cfg(windows)] #[test]s (strips_unc_prefix,
    // converts_backslashes, mixed_separators, deeply_nested,
    // consecutive_backslashes); consolidated into one table-driven test over
    // normalize_path equality cases.
    let cases: &[(&str, &str)] = &[
        (
            r"\\?\C:\WorkArea\AI\codesearch\src\main.rs",
            "C:/WorkArea/AI/codesearch/src/main.rs",
        ),
        (
            r"C:\WorkArea\AI\codesearch\src\main.rs",
            "C:/WorkArea/AI/codesearch/src/main.rs",
        ),
        (
            r"C:\Users\project/src/lib.rs",
            "C:/Users/project/src/lib.rs",
        ),
        (
            r"\\?\C:\Very\Deep\Nested\Path\To\Some\File.rs",
            "C:/Very/Deep/Nested/Path/To/Some/File.rs",
        ),
        (
            r"C:\\Double\\Backslashes\\file.rs",
            "C://Double//Backslashes//file.rs",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            normalize_path(Path::new(input)),
            *expected,
            "normalize_path({input:?}) expected {expected:?}"
        );
    }
}

#[test]
fn test_normalize_path_forward_slashes_unchanged() {
    let path = Path::new("C:/WorkArea/AI/codesearch/src/main.rs");
    let result = normalize_path(path);
    // On Windows, Path::new with forward slashes may or may not convert them
    // The important thing is the result is consistent
    assert!(!result.contains('\\'));
    assert!(!result.starts_with(r"\\?\"));
}

#[cfg(windows)]
#[test]
fn test_normalize_path_str_windows_forms() {
    // Previously 2 separate #[cfg(windows)] #[test]s (strips_unc,
    // mixed_separators); consolidated into one table-driven test.
    let cases: &[(&str, &str)] = &[
        (r"\\?\C:\foo\bar.rs", "C:/foo/bar.rs"),
        (
            r"C:\Users\project/src/lib.rs",
            "C:/Users/project/src/lib.rs",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            normalize_path_str(input),
            *expected,
            "normalize_path_str({input:?}) expected {expected:?}"
        );
    }
}

#[test]
fn test_normalize_path_unix_style() {
    // Unix/Linux/macOS paths should remain unchanged
    let path = Path::new("/home/user/project/src/main.rs");
    assert_eq!(normalize_path(path), "/home/user/project/src/main.rs");
}

/// Aikido 30641757 (priority 46): on Unix, a file whose name literally
/// contains a backslash (`foo\bar.rs`) is distinct from a file in a
/// subdirectory (`foo/bar.rs`). Both must NOT collapse to the same key.
#[cfg(not(windows))]
#[test]
fn test_normalize_path_preserves_unix_backslash_filenames() {
    // Subdirectory file — forward slash is the separator.
    let subdir = normalize_path(Path::new("foo/bar.rs"));
    // Literal-backslash filename — backslash is part of the name on Unix.
    let literal = normalize_path(Path::new("foo\\bar.rs"));
    assert_ne!(
        subdir, literal,
        "Unix must NOT collapse `foo/bar.rs` and `foo\\bar.rs` into the same key"
    );
    assert_eq!(subdir, "foo/bar.rs");
    assert_eq!(literal, "foo\\bar.rs");
}

#[test]
fn test_normalize_path_already_normalized() {
    // Already normalized paths should remain unchanged
    let path = Path::new("C:/WorkArea/AI/codesearch/src/main.rs");
    assert_eq!(
        normalize_path(path),
        "C:/WorkArea/AI/codesearch/src/main.rs"
    );
}

#[cfg(windows)]
#[test]
fn test_migrate_paths_normalizes_keys() {
    let mut store = FileMetaStore::new("test-model".to_string(), 384);
    // Insert with non-normalized key (simulating old format)
    store.files.insert(
        r"C:\WorkArea\src\main.rs".to_string(),
        FileMeta {
            hash: "abc123".to_string(),
            mtime: 1000,
            size: 100,
            chunk_count: 2,
            chunk_ids: vec![1, 2],
        },
    );
    store.files.insert(
        r"\\?\C:\WorkArea\src\lib.rs".to_string(),
        FileMeta {
            hash: "def456".to_string(),
            mtime: 2000,
            size: 200,
            chunk_count: 3,
            chunk_ids: vec![3, 4, 5],
        },
    );

    store.migrate_paths();

    // Both should be normalized
    assert!(store.files.contains_key("C:/WorkArea/src/main.rs"));
    assert!(store.files.contains_key("C:/WorkArea/src/lib.rs"));
    // Old keys should be gone
    assert!(!store.files.contains_key(r"C:\WorkArea\src\main.rs"));
    assert!(!store.files.contains_key(r"\\?\C:\WorkArea\src\lib.rs"));
}

#[test]
fn test_file_meta_store() {
    let dir = tempdir().unwrap();
    let db_path = dir.path();

    let mut store = FileMetaStore::new("test-model".to_string(), 384);

    // Create a test file
    let test_file = dir.path().join("test.txt");
    fs::write(&test_file, "hello world").unwrap();

    // Check new file
    let (needs_reindex, old_chunks) = store.check_file(&test_file).unwrap();
    assert!(needs_reindex);
    assert!(old_chunks.is_empty());

    // Update metadata
    store.update_file(&test_file, vec![1, 2, 3]).unwrap();

    // Check again - should not need reindex
    let (needs_reindex, _) = store.check_file(&test_file).unwrap();
    assert!(!needs_reindex);

    // Modify file
    fs::write(&test_file, "hello world modified").unwrap();

    // Now should need reindex
    let (needs_reindex, old_chunks) = store.check_file(&test_file).unwrap();
    assert!(needs_reindex);
    assert_eq!(old_chunks, vec![1, 2, 3]);

    // Save and load
    store.save(db_path).unwrap();
    let loaded = FileMetaStore::load_or_create(db_path, "test-model", 384).unwrap();
    assert_eq!(loaded.files.len(), 1);
}

// =========================================================================
// Path comparison tests — verify that different path formats match correctly
// These test the exact bug patterns that have caused issues in production.
// =========================================================================

#[cfg(windows)]
#[test]
fn test_path_comparison_normalizes_equivalently() {
    // Previously 4 separate #[test]s (path_comparison_unc_vs_normal,
    // path_comparison_backslash_vs_forward, path_str_comparison_unc_vs_normal,
    // path_comparison_stored_vs_walker); consolidated into one table-driven
    // test asserting that pathologically different spellings of the same path
    // normalize to an identical key (the production bug class for path matching).
    let path_pairs: &[(&str, &str)] = &[
        // UNC-prefixed vs backslash form
        (r"\\?\C:\WorkArea\src\main.rs", r"C:\WorkArea\src\main.rs"),
        // backslash vs forward-slash form
        (r"C:\WorkArea\src\main.rs", "C:/WorkArea/src/main.rs"),
        // stored (forward) vs walked (UNC) form
        (
            "C:/WorkArea/AI/codesearch/src/main.rs",
            r"\\?\C:\WorkArea\AI\codesearch\src\main.rs",
        ),
    ];
    for (a, b) in path_pairs {
        let na = normalize_path(Path::new(a));
        let nb = normalize_path(Path::new(b));
        assert_eq!(
            na, nb,
            "normalize_path({a:?}) vs normalize_path({b:?}) diverged"
        );
    }

    // normalize_path_str UNC vs normal
    assert_eq!(
        normalize_path_str(r"\\?\C:\WorkArea\src\main.rs"),
        normalize_path_str(r"C:\WorkArea\src\main.rs"),
        "normalize_path_str UNC vs normal diverged"
    );
}

#[cfg(windows)]
#[test]
fn test_path_filter_starts_with() {
    // Simulates: --filter-path src/ matching against stored paths
    let filter = normalize_path_str("src/");
    let stored = normalize_path_str("src/main.rs");
    assert!(stored.starts_with(&filter));

    // Backslash filter should also work
    let filter_bs = normalize_path_str(r"src\");
    assert!(stored.starts_with(&filter_bs));
}

#[cfg(windows)]
#[test]
fn test_path_filter_with_unc_prefix() {
    // Agent sends UNC path as filter, stored paths are normalized
    let filter = normalize_path_str(r"\\?\C:\WorkArea\src");
    let stored = normalize_path_str("C:/WorkArea/src/main.rs");
    assert!(stored.starts_with(&filter));
}

#[test]
fn test_normalize_idempotent() {
    // Normalizing an already-normalized path should produce the same result
    let original = "C:/WorkArea/AI/codesearch/src/main.rs";
    let once = normalize_path_str(original);
    let twice = normalize_path_str(&once);
    assert_eq!(once, twice, "normalize_path_str must be idempotent");
}

#[test]
fn test_normalize_path_equals_normalize_path_str() {
    // Both functions must produce identical output for the same input
    let input = r"\\?\C:\WorkArea\AI\src\main.rs";
    let from_path = normalize_path(Path::new(input));
    let from_str = normalize_path_str(input);
    assert_eq!(from_path, from_str);
}

#[cfg(windows)]
#[test]
fn test_normalize_path_relative_strips_project_root() {
    let root = normalize_path_str(r"C:\WorkArea\AI\codesearch");
    let relative = normalize_path_relative(r"\\?\C:\WorkArea\AI\codesearch\src\main.rs", &root);
    assert_eq!(relative, "src/main.rs");
}

#[test]
fn test_normalize_path_relative_keeps_path_when_root_not_matching() {
    let root = normalize_path_str("/repo");
    let relative = normalize_path_relative("/other/place/src/main.rs", &root);
    assert_eq!(relative, "/other/place/src/main.rs");
}

#[test]
fn test_normalize_path_relative_trims_dot_slash_for_relative_input() {
    let root = normalize_path_str("C:/WorkArea/AI/codesearch");
    let relative = normalize_path_relative("./src/lib.rs", &root);
    assert_eq!(relative, "src/lib.rs");
}

#[test]
fn test_normalize_filter_path_trims_prefix_and_suffix() {
    assert_eq!(normalize_filter_path("./src/"), "src/");
}

#[cfg(windows)]
#[test]
fn test_path_matches_filter_with_absolute_windows_path() {
    let root = normalize_path_str(r"C:\WorkArea\AI\codesearch");
    let filter = normalize_filter_path("src/");
    assert!(path_matches_filter(
        r"\\?\C:\WorkArea\AI\codesearch\src\main.rs",
        &filter,
        &root,
    ));
}

#[test]
fn test_path_matches_filter_with_non_matching_prefix() {
    let root = normalize_path_str("/repo");
    let filter = normalize_filter_path("src/");
    assert!(!path_matches_filter("/repo/tests/main.rs", &filter, &root));
}

#[test]
fn test_path_matches_filter_does_not_match_partial_directory_name() {
    let root = normalize_path_str("/repo");
    let filter = normalize_filter_path("src/");
    assert!(!path_matches_filter("/repo/src2/main.rs", &filter, &root));
}

#[test]
fn test_path_matches_filter_matches_exact_directory_name() {
    let root = normalize_path_str("/repo");
    let filter = normalize_filter_path("src");
    assert!(path_matches_filter("/repo/src/main.rs", &filter, &root));
}
