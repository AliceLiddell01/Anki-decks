//! Регрессии принадлежности обратной связи и поиска по действующим исходам.

use anki_repo::code_review::learning::model::{ObservationCounts, ReviewedOutcome};
use anki_repo::code_review::learning::store::{LearningStore, StoreOptions};
use anki_repo::code_review::learning::{self, FeedbackAction, FeedbackEvent, FeedbackKind};
use anki_repo::error::ErrorCode;

use crate::common::TempDir;

fn fixture() -> (TempDir, LearningStore) {
    let temp = TempDir::new("feedback-search-regressions");
    let store = LearningStore::open(StoreOptions::at(temp.path().join("history.sqlite"))).unwrap();
    store.write(|write| {
        write.execute(
            "INSERT INTO learning_import (
                review_id, repository_id, base_sha, head_sha, merge_base_sha,
                workspace_variant, review_pack_sha256, queue_sha256,
                review_schema_version, queue_schema_version, analyzer_digest, classifier_digest,
                trust, outcome, limitations_json, revision, imported_at, observations_json, identity_key
             ) VALUES ('review', 'repository', 'base', 'head', 'base', 'root', 'pack', 'queue',
                       1, 1, 'analyzer', 'classifier', 'ast_authenticated', ?1, '[]',
                       1, 1, ?2, 'identity')",
            rusqlite::params![
                ReviewedOutcome::FullyReviewed.as_str(),
                serde_json::to_string(&ObservationCounts::default()).unwrap(),
            ],
        )?;
        for unit in ["first", "second"] {
            write.execute(
                "INSERT INTO learning_unit (
                    review_id, unit_id, kind, candidate_count, priority, representatives_json,
                    disposition, detector, source, role, code_role, surfaces_json, signature, feature_json
                 ) VALUES ('review', ?1, 'individual', 1, 'normal', '[]', 'confirmed',
                           'detector', 'source', 'role', 'role', '[]', 'signature', '{}')",
                [unit],
            )?;
            write.execute(
                "INSERT INTO learning_candidate (
                    review_id, candidate_id, unit_id, detector, source, path, path_family,
                    origin, execution, classification_json
                 ) VALUES ('review', ?1, ?1, 'detector', 'source', 'src/lib.rs', 'src',
                           'introduced_or_changed', 'production', '{}')",
                [unit],
            )?;
        }
        for (case, unit, original) in [
            ("case-first", "first", "confirmed"),
            ("case-second", "second", "acceptable"),
        ] {
            write.execute(
                "INSERT INTO learning_search (case_id, review_id, unit_id, candidate_id,
                     kind, disposition, text)
                 VALUES (?1, 'review', ?2, ?2, 'decision', ?3, 'проверяемый пример')",
                rusqlite::params![case, unit, original],
            )?;
        }
        Ok(())
    }).unwrap();
    (temp, store)
}

fn revision(id: &str) -> FeedbackEvent {
    FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: id.into(),
        review_id: "review".into(),
        unit_id: "first".into(),
        candidate_id: Some("first".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action: FeedbackAction::Append,
        supersedes_event_id: None,
        effective_disposition: Some("false_positive".into()),
        usefulness: None,
        explanation: "Исправленный содержательный исход".into(),
        provenance: "reviewer".into(),
        recorded_at: 1,
    }
}

#[test]
fn feedback_candidate_must_belong_to_the_selected_unit() {
    let (_temp, store) = fixture();
    let mut event = revision("wrong-owner");
    event.candidate_id = Some("second".into());
    assert_eq!(
        learning::feedback::record_feedback(&store, &event)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(
        learning::feedback::list_events(&store, "review", "first")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn supersede_and_retract_require_current_same_kind_assertions() {
    let (_temp, store) = fixture();
    learning::feedback::record_feedback(&store, &revision("initial")).unwrap();
    let mut replacement = revision("replacement");
    replacement.action = FeedbackAction::Supersede;
    replacement.supersedes_event_id = Some("initial".into());
    learning::feedback::record_feedback(&store, &replacement).unwrap();
    let mut repeated = replacement.clone();
    repeated.event_id = "repeated".into();
    assert_eq!(
        learning::feedback::record_feedback(&store, &repeated)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let mut foreign_kind = revision("foreign-kind");
    foreign_kind.action = FeedbackAction::Retract;
    foreign_kind.supersedes_event_id = Some("replacement".into());
    foreign_kind.kind = FeedbackKind::RecommendationUsefulness;
    foreign_kind.effective_disposition = None;
    assert_eq!(
        learning::feedback::record_feedback(&store, &foreign_kind)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let mut append_with_target = revision("append-target");
    append_with_target.supersedes_event_id = Some("replacement".into());
    assert_eq!(
        learning::feedback::record_feedback(&store, &append_with_target)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    let mut retract = revision("retract");
    retract.action = FeedbackAction::Retract;
    retract.supersedes_event_id = Some("replacement".into());
    retract.effective_disposition = None;
    learning::feedback::record_feedback(&store, &retract).unwrap();
    let mut retract_retract = retract.clone();
    retract_retract.event_id = "retract-retract".into();
    retract_retract.supersedes_event_id = Some("retract".into());
    assert_eq!(
        learning::feedback::record_feedback(&store, &retract_retract)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(
        learning::feedback::outcome(&store, "review", "first")
            .unwrap()
            .effective_event_ids
            .is_empty()
    );
}

#[test]
fn a_foreign_kind_correction_cannot_deactivate_semantic_state() {
    let (_temp, store) = fixture();
    learning::feedback::record_feedback(&store, &revision("semantic")).unwrap();
    // Имитируем старую повреждённую запись в SQLite: читатель тоже обязан
    // отделять оценки полезности от семантических исправлений.
    store
        .write(|write| {
            write.execute(
                "INSERT INTO learning_feedback (
                event_id, review_id, unit_id, kind, action, supersedes_event_id,
                retracted_event_id, explanation, provenance, recorded_at
             ) VALUES ('foreign', 'review', 'first', 'recommendation_usefulness', 'retract',
                       'semantic', 'semantic', 'Отзыв оценки', 'reviewer', 2)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        learning::feedback::outcome(&store, "review", "first")
            .unwrap()
            .effective_disposition
            .as_deref(),
        Some("false_positive")
    );
    let search = learning::search_history(
        &store,
        &learning::SearchQuery {
            disposition: Some("false_positive".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(search.matched, 1);
    assert_eq!(
        search.cases[0].disposition.as_deref(),
        Some("false_positive")
    );
}

#[test]
fn search_filter_rows_and_counts_follow_revision_then_retraction() {
    let (_temp, store) = fixture();
    learning::feedback::record_feedback(&store, &revision("semantic")).unwrap();
    let unfiltered = learning::search_history(&store, &learning::SearchQuery::default()).unwrap();
    assert_eq!(unfiltered.matched, 2);
    assert_eq!(
        unfiltered
            .cases
            .iter()
            .find(|case| case.case_id == "case-first")
            .unwrap()
            .disposition
            .as_deref(),
        Some("false_positive")
    );
    for (disposition, expected) in [("false_positive", 1), ("confirmed", 0), ("acceptable", 1)] {
        let page = learning::search_history(
            &store,
            &learning::SearchQuery {
                disposition: Some(disposition.into()),
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page.matched, expected);
        assert_eq!(page.cases.len(), expected);
        assert!(!page.has_more);
        assert!(
            page.cases
                .iter()
                .all(|case| case.disposition.as_deref() == Some(disposition))
        );
    }
    // Два символа заставляют использовать подстрочный путь даже при наличии FTS5.
    let page = learning::search_history(
        &store,
        &learning::SearchQuery {
            text: Some("пр".into()),
            disposition: Some("false_positive".into()),
            offset: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page.matched, 1);
    assert!(page.cases.is_empty());
    assert!(!page.has_more);
    let mut retract = revision("retract");
    retract.action = FeedbackAction::Retract;
    retract.supersedes_event_id = Some("semantic".into());
    retract.effective_disposition = None;
    learning::feedback::record_feedback(&store, &retract).unwrap();
    let original = learning::search_history(
        &store,
        &learning::SearchQuery {
            disposition: Some("confirmed".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(original.matched, 1);
    assert_eq!(original.cases[0].disposition.as_deref(), Some("confirmed"));
    assert_eq!(
        learning::search_history(
            &store,
            &learning::SearchQuery {
                disposition: Some("false_positive".into()),
                ..Default::default()
            }
        )
        .unwrap()
        .matched,
        0
    );
}
