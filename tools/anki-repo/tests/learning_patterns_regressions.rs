//! Регрессии линий наблюдения, совместимости признаков и снимка рекомендаций.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

use anki_repo::code_review::learning::model::ReviewedOutcome;
use anki_repo::code_review::learning::model::{SupportLevel, TrustLevel};
use anki_repo::code_review::learning::{
    self, ImportInputsHint, LearningStore, LoadedReview, StoreOptions,
};
use anki_repo::code_review::model::{
    CandidateEvidence, CandidateOrigin, ReviewFile, ReviewPack, ReviewScope,
};
use anki_repo::code_review::review_queue::{self, ClassificationBasis, CodeRole, SyntaxContext};
use anki_repo::code_review::scope::{FileCategory, FileStatus, FileSurface, GitTarget, ImageState};
use anki_repo::error::ErrorCode;
use rusqlite::params;

use crate::common::TempDir;

fn loaded_fixture() -> LoadedReview {
    loaded_fixture_with_candidates(2)
}

fn loaded_fixture_with_candidates(candidate_count: usize) -> LoadedReview {
    let pack = ReviewPack {
        schema_version: 1,
        target: GitTarget {
            repository_id: "shared-repository".into(),
            base_sha: "base".into(),
            head_sha: "head".into(),
            merge_base_sha: "base".into(),
        },
        scope: ReviewScope {
            merge_base_sha: "base".into(),
            text_image_limit_bytes: 1024,
            files: vec![ReviewFile {
                path: "src/tests.rs".into(),
                previous_path: None,
                status: FileStatus::Modified,
                additions: None,
                deletions: None,
                category: FileCategory::Rust,
                surfaces: vec![FileSurface::Tests],
                binary: false,
                base_state: ImageState::Text,
                base_size: 0,
                base_object_id: None,
                base_changed_lines: Vec::new(),
                post_state: ImageState::Text,
                post_size: 64,
                post_object_id: None,
                post_changed_lines: Vec::new(),
            }],
        },
        diagnostics: Vec::new(),
        candidates: (0..candidate_count)
            .map(|index| CandidateEvidence {
                id: format!("case-{index}"),
                detector: "error_path".into(),
                path: "src/tests.rs".into(),
                line: Some(index + 1),
                column: Some(1),
                snippet: None,
                origin: CandidateOrigin::IntroducedOrChanged,
                signals: vec!["unwrap".into()],
                source: "synthetic".into(),
                metadata: BTreeMap::new(),
            })
            .collect(),
        language: anki_repo::code_review::language::LanguageScan {
            schema_version: 1,
            files: Vec::new(),
            candidates: Vec::new(),
            skipped: Vec::new(),
        },
        dependencies: Vec::new(),
        tests: Vec::new(),
        suppressions: Vec::new(),
        risk_surfaces: Vec::new(),
        tool_runs: Vec::new(),
    };
    let contexts = fixture_contexts(&pack);
    let digest = learning::import::sha256_hex(&serde_json::to_vec(&pack).unwrap());
    let queue = review_queue::build(&pack, &digest, &contexts).unwrap();
    assert_eq!(queue.units.len(), 1);
    assert!(queue.units[0].is_group());
    let queue_digest = learning::import::sha256_hex(&serde_json::to_vec(&queue).unwrap());
    LoadedReview {
        pack,
        review_pack_sha256: digest,
        queue,
        queue_sha256: queue_digest,
        triage: None,
        trust: TrustLevel::AstAuthenticated,
        limitations: Vec::new(),
        inputs_hint: ImportInputsHint::default(),
    }
}

#[test]
fn recommendation_artifact_bounds_group_candidate_ids_and_reports_the_full_count() {
    let loaded = loaded_fixture_with_candidates(5);
    let document =
        learning::recommend(None, &loaded, &learning::RecommendRequest::default()).unwrap();
    assert_eq!(document.schema_version, 2);
    let recommendation = &document.recommendations[0];
    assert_eq!(recommendation.candidate_count, 5);
    assert!(recommendation.candidate_ids.len() <= 3);
    assert_eq!(
        recommendation.candidate_ids,
        recommendation.representative_candidate_ids
    );
    assert!(recommendation.candidate_ids_truncated);
    let serialized = serde_json::to_value(recommendation).unwrap();
    assert_eq!(serialized["candidate_count"], 5);
    assert_eq!(serialized["candidate_ids_truncated"], true);
    assert!(serialized["candidate_ids"].as_array().unwrap().len() <= 3);
}

fn fixture_contexts(pack: &ReviewPack) -> BTreeMap<String, SyntaxContext> {
    pack.candidates
        .iter()
        .map(|candidate| {
            (
                candidate.id.clone(),
                SyntaxContext {
                    execution: Some(FileSurface::Tests),
                    code_role: CodeRole::TestSetup,
                    text_role: None,
                    signature: Some("method:unwrap".into()),
                    basis: ClassificationBasis::SyntaxContext,
                },
            )
        })
        .collect()
}

/// Записи на уровне persisted-контракта: одинаковый structural unit_id
/// намеренно появляется в разных Git-диапазонах одного репозитория.
fn seed_case(
    store: &LearningStore,
    loaded: &LoadedReview,
    id: &str,
    head: &str,
    revision_of: Option<&str>,
    disposition: &str,
    classifier: Option<&str>,
) {
    let unit = &loaded.queue.units[0];
    let compatibility = classifier
        .map(str::to_owned)
        .unwrap_or_else(|| learning::import::classifier_digest(&loaded.pack, &loaded.queue));
    let features =
        learning::patterns::unit_features_with_classifier(&unit.signature, &compatibility);
    let feature_json = serde_json::to_string(&features).unwrap();
    let signature = learning::patterns::feature_signature(&features);
    store.write(|write| {
        write.execute(
            "INSERT INTO learning_import (review_id,repository_id,base_sha,head_sha,merge_base_sha,workspace_variant,review_pack_sha256,queue_sha256,review_schema_version,queue_schema_version,analyzer_digest,classifier_digest,trust,outcome,limitations_json,revision_of,revision,imported_at,observations_json,identity_key)
             VALUES (?1,'shared-repository','base',?2,'base','root','pack','queue',1,?5,'analyzer',?3,'ast_authenticated',?6,'[]',?4,1,86400,?7,?1)",
            params![
                id,
                head,
                compatibility,
                revision_of,
                loaded.queue.schema_version,
                ReviewedOutcome::FullyReviewed.as_str(),
                serde_json::to_string(&learning::model::ObservationCounts::default()).unwrap(),
            ],
        )?;
        write.execute(
            "INSERT INTO learning_unit (review_id,unit_id,kind,candidate_count,priority,representatives_json,disposition,detector,source,role,code_role,surfaces_json,signature,feature_json)
             VALUES (?1,?2,'group',2,'normal','[]',?3,'error_path','synthetic','error_path','unknown','[]',?4,?5)",
            params![id, unit.id, disposition, signature, feature_json],
        )?;
        Ok(())
    }).unwrap();
}

fn only_support(store: &LearningStore) -> learning::model::SupportSummary {
    let report =
        learning::pattern_report(store, &learning::patterns::PatternQuery::default()).unwrap();
    assert_eq!(report.rules.len(), 1);
    report.rules[0].support.clone()
}

#[test]
fn independent_ranges_with_the_same_structural_unit_keep_contradictions() {
    let dir = TempDir::new("patterns-independent-ranges");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    seed_case(&store, &loaded, "first", "head-a", None, "confirmed", None);
    seed_case(
        &store,
        &loaded,
        "second",
        "head-b",
        None,
        "acceptable",
        None,
    );
    let two = only_support(&store);
    assert_eq!(two.support_units, 2);
    assert_eq!((two.confirmed_units, two.acceptable_units), (1, 1));
    assert_eq!(two.contradicting_unit_ids.len(), 1);
    seed_case(&store, &loaded, "third", "head-c", None, "confirmed", None);
    let three = only_support(&store);
    assert_eq!(three.support_units, 3);
    assert_eq!(three.level, SupportLevel::Contradictory);
}

#[test]
fn exact_ranges_and_proven_revision_chains_count_once() {
    let dir = TempDir::new("patterns-proof-chain");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    seed_case(&store, &loaded, "first", "head-a", None, "confirmed", None);
    seed_case(&store, &loaded, "reread", "head-a", None, "confirmed", None);
    seed_case(
        &store,
        &loaded,
        "successor",
        "head-b",
        Some("first"),
        "acceptable",
        None,
    );
    store
        .write(|write| {
            write.execute(
                "UPDATE learning_import SET revision=2 WHERE review_id='successor'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let support = only_support(&store);
    assert_eq!(support.support_units, 1);
    assert_eq!(support.revised_units, 2);
    assert_eq!(support.acceptable_units, 1);
    assert_eq!(support.confirmed_units, 0);
}

#[test]
fn three_independent_ranges_reach_the_support_threshold() {
    let dir = TempDir::new("patterns-threshold");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    for (id, head) in [
        ("first", "head-a"),
        ("second", "head-b"),
        ("third", "head-c"),
    ] {
        seed_case(&store, &loaded, id, head, None, "confirmed", None);
    }
    let support = only_support(&store);
    assert_eq!(support.support_units, 3);
    assert_eq!(support.level, SupportLevel::Supported);
}

#[test]
fn report_abstention_and_total_rules_use_the_unlimited_rule_set() {
    let dir = TempDir::new("patterns-report-limit");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    for index in 0..4 {
        seed_case(
            &store,
            &loaded,
            &format!("contradictory-{index}"),
            &format!("head-c{index}"),
            None,
            if index == 3 {
                "false_positive"
            } else {
                "confirmed"
            },
            Some("contradictory-classifier"),
        );
    }
    for index in 0..3 {
        seed_case(
            &store,
            &loaded,
            &format!("supported-{index}"),
            &format!("head-s{index}"),
            None,
            "confirmed",
            Some("supported-classifier"),
        );
    }

    let report = learning::pattern_report(
        &store,
        &learning::patterns::PatternQuery {
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.rules.len(), 1);
    assert_eq!(report.rules_total, 2);
    assert_eq!(report.rules[0].support.level, SupportLevel::Contradictory);
    assert!(!report.abstained);
}

#[test]
fn classifier_versions_partition_identical_structure_and_legacy_provenance() {
    let dir = TempDir::new("patterns-classifier-version");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    seed_case(
        &store,
        &loaded,
        "current",
        "head-a",
        None,
        "confirmed",
        None,
    );
    seed_case(
        &store,
        &loaded,
        "older",
        "head-b",
        None,
        "acceptable",
        Some("older-classifier"),
    );
    // Старый формат feature_json не содержит stamp; provenance не переписывается
    // текущей сборкой и не смешивает его с новыми признаками.
    let legacy = serde_json::to_string(&learning::patterns::unit_features(
        &loaded.queue.units[0].signature,
    ))
    .unwrap();
    store
        .write(|write| {
            write.execute(
                "UPDATE learning_unit SET feature_json=?1 WHERE review_id='older'",
                [legacy],
            )?;
            Ok(())
        })
        .unwrap();
    let report =
        learning::pattern_report(&store, &learning::patterns::PatternQuery::default()).unwrap();
    assert_eq!(report.rules.len(), 2);
    assert!(
        report
            .rules
            .iter()
            .all(|rule| rule.support.support_units == 1)
    );
    let current_digest = learning::import::classifier_digest(&loaded.pack, &loaded.queue);
    let mut unrelated = loaded.pack.clone();
    unrelated.target.head_sha = "different-head".into();
    unrelated.candidates[0].snippet = Some("different content".into());
    assert_eq!(
        current_digest,
        learning::import::classifier_digest(&unrelated, &loaded.queue)
    );
    let mut legacy_queue = loaded.queue.clone();
    legacy_queue.classifier_rules_version = None;
    legacy_queue.schema_version = review_queue::LEGACY_QUEUE_SCHEMA_VERSION;
    assert_ne!(
        current_digest,
        learning::import::classifier_digest(&loaded.pack, &legacy_queue)
    );
    let document = learning::recommend(
        Some(&store),
        &loaded,
        &learning::RecommendRequest::default(),
    )
    .unwrap();
    assert_eq!(
        document.recommendations[0]
            .support
            .as_ref()
            .unwrap()
            .support_units,
        1
    );
}

#[test]
fn recommendations_check_expected_generation_and_history_inside_the_snapshot() {
    let dir = TempDir::new("recommend-generation");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    seed_case(&store, &loaded, "first", "head-a", None, "confirmed", None);
    let generation = learning::import::generation(&store).unwrap().revision;
    let request = learning::RecommendRequest {
        history_revision: Some(generation),
        require_history: true,
        ..Default::default()
    };
    let document = learning::recommend(Some(&store), &loaded, &request).unwrap();
    assert_eq!(document.generation.revision, generation);
    assert_eq!(
        document.recommendations[0]
            .support
            .as_ref()
            .unwrap()
            .confirmed_units,
        1
    );
    store
        .write(|write| {
            write.execute("UPDATE learning_unit SET disposition='acceptable'", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        learning::recommend(Some(&store), &loaded, &request)
            .unwrap_err()
            .code,
        ErrorCode::SourceChanged
    );
    assert_eq!(
        learning::recommend(None, &loaded, &request)
            .unwrap_err()
            .code,
        ErrorCode::SourceChanged
    );
    assert_eq!(
        learning::recommend(
            None,
            &loaded,
            &learning::RecommendRequest {
                require_history: true,
                ..Default::default()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );
}

#[test]
fn concurrent_recommendations_never_mix_generation_and_evidence() {
    let dir = TempDir::new("recommend-concurrent-snapshot");
    let database = dir.path().join("state.sqlite");
    let store = LearningStore::open(StoreOptions::at(database.clone())).unwrap();
    let loaded = loaded_fixture();
    seed_case(&store, &loaded, "first", "head-a", None, "confirmed", None);
    seed_case(&store, &loaded, "second", "head-b", None, "confirmed", None);
    store
        .write(|write| {
            write.execute(
                "UPDATE learning_import SET imported_at=259200 WHERE review_id='first'",
                [],
            )?;
            write.execute(
                "UPDATE learning_import SET imported_at=172800 WHERE review_id='second'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let initial = learning::import::generation(&store).unwrap().revision;
    let start = Arc::new(Barrier::new(2));
    let writer_start = Arc::clone(&start);
    let writer = std::thread::spawn(move || {
        let mut options = StoreOptions::at(database);
        options.create = false;
        let writer = LearningStore::open(options).unwrap();
        writer_start.wait();
        for index in 1..=80 {
            writer
                .write(|write| {
                    let outcome = if index % 2 == 0 {
                        "confirmed"
                    } else {
                        "acceptable"
                    };
                    write.execute(
                        "UPDATE learning_unit SET disposition=?1 WHERE review_id='first'",
                        [outcome],
                    )?;
                    let time = if index % 2 == 0 { 259200 } else { 86400 };
                    write.execute(
                        "UPDATE learning_import SET imported_at=?1 WHERE review_id='first'",
                        [time],
                    )?;
                    Ok(())
                })
                .unwrap();
            std::thread::yield_now();
        }
    });
    start.wait();
    for _ in 0..80 {
        let document = learning::recommend(
            Some(&store),
            &loaded,
            &learning::RecommendRequest::default(),
        )
        .unwrap();
        let support = document.recommendations[0].support.as_ref().unwrap();
        let confirmed = (document.generation.revision - initial).is_multiple_of(2);
        assert_eq!(support.confirmed_units, 1 + usize::from(confirmed));
        assert_eq!(support.acceptable_units, usize::from(!confirmed));
        let first = document.recommendations[0]
            .historical_cases
            .iter()
            .find(|case| case.review_id == "first")
            .unwrap();
        assert_eq!(first.age_days, u64::from(!confirmed));
    }
    writer.join().unwrap();
}

#[test]
fn structural_repeat_recommendation_explains_similarity_without_claiming_bug_identity() {
    let dir = TempDir::new("recommend-structural-analogy");
    let store = LearningStore::open(StoreOptions::at(dir.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    for id in [
        "a-first", "b-second", "c-third", "d-fourth", "z-repeat", "zz-other",
    ] {
        seed_case(
            &store,
            &loaded,
            id,
            &format!("head-{id}"),
            None,
            "confirmed",
            None,
        );
    }
    let units = serde_json::to_string(&vec![loaded.queue.units[0].id.clone()]).unwrap();
    store.write(|write| {
        for id in ["z-repeat", "zz-other"] {
            write.execute("INSERT INTO learning_finding (review_id,finding_id,severity,provenance,title,description,signature,linked_unit_ids_json) VALUES (?1,'finding','P2','independent',?1,'Разные ошибки одного класса','similar',?2)", params![id, units])?;
        }
        write.execute("INSERT INTO learning_case_link (review_id,finding_id,linked_review_id,linked_finding_id,kind,basis) VALUES ('z-repeat','finding','zz-other','finding','structural_repeat','Структурная аналогия; идентичность ошибки не доказана')", [])?;
        Ok(())
    }).unwrap();
    let limited_request = learning::RecommendRequest {
        case_limit: 1,
        ..Default::default()
    };
    let limited = learning::recommend(Some(&store), &loaded, &limited_request).unwrap();
    let full_request = learning::RecommendRequest {
        case_limit: 10,
        ..Default::default()
    };
    let full = learning::recommend(Some(&store), &loaded, &full_request).unwrap();
    let recommendation = &limited.recommendations[0];
    assert_eq!(recommendation.granularity, "look_for_related_finding");
    assert_eq!(
        recommendation.granularity, full.recommendations[0].granularity,
        "case_limit не меняет гранулярность рекомендации"
    );
    assert!(recommendation.reason.contains("структурно похожий случай"));
    assert!(
        recommendation
            .reason
            .contains("не доказывает повтор той же ошибки")
    );
    assert!(
        !recommendation
            .reason
            .contains("того же структурного дефекта")
    );
    assert!(!recommendation.reason.contains("unresolved"));
}

#[test]
fn legacy_queue_without_classifier_stamp_preserves_authenticity_but_not_compatibility() {
    let loaded = loaded_fixture();
    let contexts = fixture_contexts(&loaded.pack);
    let mut value = serde_json::to_value(&loaded.queue).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("classifier_rules_version");
    value["schema_version"] = serde_json::json!(review_queue::LEGACY_QUEUE_SCHEMA_VERSION);
    let legacy: review_queue::ReviewQueue = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.classifier_rules_version, None);
    review_queue::validate_with_syntax(
        &legacy,
        &loaded.pack,
        &loaded.review_pack_sha256,
        &contexts,
    )
    .unwrap();
    assert_ne!(
        learning::import::classifier_digest(&loaded.pack, &legacy),
        learning::import::classifier_digest(&loaded.pack, &loaded.queue)
    );
    let mut forged = loaded.queue.clone();
    forged.classifier_rules_version = Some(review_queue::CLASSIFIER_RULES_VERSION + 1);
    assert!(
        review_queue::validate_with_syntax(
            &forged,
            &loaded.pack,
            &loaded.review_pack_sha256,
            &contexts
        )
        .is_err()
    );
}
