use super::*;

#[test]
fn file_validation_is_nondestructive_and_paths_have_one_spelling() {
    for path in [
        "/a", "../a", "a/../b", "a//b", "a\\b", "./a", "a/", "C:/a", "a\0b",
    ] {
        assert!(validate_path(path, false).is_err(), "accepted {path:?}");
    }
    assert!(validate_path("src/main.rs", false).is_ok());
    assert!(validate_path("src/", true).is_ok());
    assert!(validate_path("", true).is_ok());
    for revision in [-1, i64::MAX] {
        assert!(FileUpdate {
            path: "a.rs".into(),
            expected_revision: revision,
            source_revision: "abc".into(),
            content: None
        }
        .validate()
        .is_err());
    }
    let mut update = FileUpdate {
        path: "a.rs".into(),
        expected_revision: 0,
        source_revision: "abc".into(),
        content: Some("ok".into()),
    };
    assert!(update.validate().is_ok());
    update.content = Some("x".repeat(MAX_FILE_BYTES + 1));
    assert!(update.validate().is_err());
}

#[test]
fn retrieval_requires_bounded_explicit_scope() {
    let mut search = SearchRequest {
        repositories: vec!["example".into()],
        query: "needle".into(),
        mode: SearchMode::Hybrid,
        limit: 10,
        path_prefix: None,
    };
    assert!(search.validate().is_ok());
    for limit in [0, MAX_RESULTS + 1] {
        search.limit = limit;
        assert!(search.validate().is_err());
    }
    search.limit = 10;
    search.repositories.clear();
    assert!(search.validate().is_err());
    search.repositories = vec!["example".into()];
    search.query = " ".into();
    assert!(search.validate().is_err());
}

#[test]
fn cosine_vectors_reject_corrupt_and_zero_values() {
    for vector in [
        vec![],
        vec![0.0, 0.0],
        vec![f32::NAN, 1.0],
        vec![f32::INFINITY, 1.0],
        vec![1.0],
    ] {
        assert!(validate_vector(&vector, 2).is_err());
    }
    assert!(validate_vector(&[1.0, -1.0], 2).is_ok());
}

#[test]
fn hybrid_uses_ranks_not_incompatible_scores_and_has_stable_ties() {
    let hit = |id, score| SearchHit {
        id,
        repository: "r".into(),
        path: "a".into(),
        content: "x".into(),
        start_line: 1,
        end_line: 1,
        file_revision: 1,
        source_revision: "s".into(),
        score,
    };
    let fused = fuse(
        vec![hit(2, 900.0), hit(1, 1.0)],
        vec![hit(1, 0.99), hit(3, 0.5)],
        3,
    );
    assert_eq!(
        fused.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}
