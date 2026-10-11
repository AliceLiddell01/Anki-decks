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

fn fixture_with_outcomes(outcomes: &[Option<&str>]) -> LoadedReview {
    use anki_repo::code_review::semantic_triage::{
        self, CandidateDecision, Disposition, FindingProvenance, ReasonCode, SemanticFinding,
        Severity,
    };
    let mut loaded = loaded_fixture_with_candidates(outcomes.len());
    let mut triage = semantic_triage::initialize(&loaded.pack, &loaded.review_pack_sha256);
    for (index, outcome) in outcomes.iter().enumerate() {
        let Some(outcome) = outcome else { continue };
        let candidate_id = format!("case-{index}");
        triage
            .unreviewed_candidate_ids
            .retain(|id| id != &candidate_id);
        let confirmed = *outcome == "confirmed";
        let finding_id = format!("finding-{index}");
        triage.individual_decisions.push(CandidateDecision {
            candidate_id: candidate_id.clone(),
            disposition: if confirmed {
                Disposition::Confirmed
            } else {
                Disposition::Acceptable
            },
            reason_code: ReasonCode::Other,
            explanation: "Исход проверен индивидуально внутри structural group".into(),
            finding_ids: if confirmed {
                vec![finding_id.clone()]
            } else {
                Vec::new()
            },
        });
        if confirmed {
            triage.findings.push(SemanticFinding {
                id: finding_id,
                severity: Severity::Major,
                title: "Подтверждённый дефект".into(),
                description: "src/tests.rs:2: воспроизводимый сбой отдельного кандидата".into(),
                provenance: FindingProvenance::DirectCandidate,
                candidate_ids: vec![candidate_id],
            });
        }
    }
    semantic_triage::canonicalize(&mut triage);
    semantic_triage::validate(&triage, &loaded.pack, &loaded.review_pack_sha256).unwrap();
    let digest = learning::import::sha256_hex(&serde_json::to_vec(&triage).unwrap());
    loaded.triage = Some((triage, digest));
    loaded
}

#[test]
fn mixed_partial_group_preserves_decisions_across_read_patterns_recommendation_and_restore() {
    let permutations = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for permutation in permutations {
        let mut loaded = fixture_with_outcomes(&[Some("acceptable"), Some("confirmed"), None]);
        if let review_queue::UnitMembers::Group { candidate_ids, .. } =
            &mut loaded.queue.units[0].members
        {
            *candidate_ids = permutation
                .iter()
                .map(|index| format!("case-{index}"))
                .collect();
        }
        loaded.queue_sha256 =
            learning::import::sha256_hex(&serde_json::to_vec(&loaded.queue).unwrap());
        let temporary = TempDir::new("learning-mixed-partial");
        let source =
            LearningStore::open(StoreOptions::at(temporary.path().join("source.sqlite"))).unwrap();
        let record =
            learning::import_history(&source, &loaded, &learning::ImportRequest::default())
                .unwrap();
        assert_eq!(
            (
                record.observations.reviewed_units,
                record.observations.unresolved_units
            ),
            (0, 1)
        );
        assert_eq!(
            (
                record.observations.covered_candidates,
                record.observations.unreviewed_candidates
            ),
            (2, 1)
        );
        assert_eq!(
            (
                record.observations.acceptable_candidates,
                record.observations.confirmed_candidates
            ),
            (1, 1)
        );

        // Имитируем старую ошибочную материализацию: read API обязаны брать
        // полное покрытие аудита, а не доверять первому агрегированному исходу.
        let mut old_counts = record.observations.clone();
        old_counts.reviewed_units = 1;
        old_counts.unresolved_units = 0;
        source
            .write(|write| {
                write.execute(
                    "UPDATE learning_unit SET disposition = 'acceptable', reason_code = 'other'",
                    [],
                )?;
                write.execute(
                    "UPDATE learning_import SET observations_json = ?1",
                    [serde_json::to_string(&old_counts).unwrap()],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            learning::show_import(&source, &record.review_id)
                .unwrap()
                .observations,
            record.observations
        );
        assert_eq!(
            learning::list_imports(&source, false, 10).unwrap()[0].observations,
            record.observations
        );
        let (total, page) = learning::import::list_imports_page(&source, false, 0, 1, "").unwrap();
        assert_eq!(total, 1);
        assert_eq!(page[0].observations, record.observations);
        assert_eq!(
            learning::import_history(&source, &loaded, &learning::ImportRequest::default())
                .unwrap()
                .observations,
            record.observations
        );

        let support = only_support(&source);
        assert_eq!(
            (
                support.support_units,
                support.confirmed_units,
                support.acceptable_units,
                support.unresolved_units
            ),
            (1, 0, 0, 1)
        );
        assert_eq!(
            support.confirmed_findings, 1,
            "Индивидуально confirmed finding сохраняет собственный исход"
        );
        assert_eq!(support.level, SupportLevel::InsufficientEvidence);
        assert!(support.confirmed_share_lower_bound.is_none());
        assert!(support.contradicting_unit_ids.is_empty());
        let recommendations = learning::recommend(
            Some(&source),
            &loaded,
            &learning::RecommendRequest::default(),
        )
        .unwrap();
        assert_eq!(
            recommendations.recommendations[0]
                .support
                .as_ref()
                .unwrap()
                .level,
            SupportLevel::InsufficientEvidence
        );
        assert_eq!(
            recommendations.recommendations[0].historical_cases[0]
                .disposition
                .as_deref(),
            Some("mixed_partial")
        );

        let archive = learning::export_history(&source).unwrap().archive;
        assert_eq!(
            archive.units[0].disposition.as_deref(),
            Some("mixed_partial")
        );
        assert!(archive.units[0].reason_code.is_none());
        assert_eq!(archive.decisions.len(), 2);
        assert_eq!(archive.candidates.len(), 3);
        assert_eq!(archive.reviews[0].observations, record.observations);
        let destination =
            LearningStore::open(StoreOptions::at(temporary.path().join("restored.sqlite")))
                .unwrap();
        learning::restore_history(&destination, &archive).unwrap();
        assert_eq!(only_support(&destination), support);
        let restored = learning::export_history(&destination).unwrap().archive;
        assert_eq!(restored.units, archive.units);
        assert_eq!(restored.decisions, archive.decisions);
        assert_eq!(restored.candidates, archive.candidates);
        assert_eq!(
            learning::recommend(
                Some(&destination),
                &loaded,
                &learning::RecommendRequest::default()
            )
            .unwrap()
            .recommendations,
            recommendations.recommendations
        );
    }
}

#[test]
fn mixed_partial_and_unresolved_states_do_not_count_as_homogeneous_observations() {
    for (outcomes, expected) in [
        (
            vec![Some("acceptable"), Some("confirmed"), None],
            "mixed_partial",
        ),
        (
            vec![Some("acceptable"), Some("confirmed"), Some("acceptable")],
            "mixed",
        ),
        (vec![Some("acceptable"), None, None], "partial"),
        (vec![None, None, None], "unresolved"),
    ] {
        let loaded = fixture_with_outcomes(&outcomes);
        let temporary = TempDir::new("learning-outcome-states");
        let store =
            LearningStore::open(StoreOptions::at(temporary.path().join("state.sqlite"))).unwrap();
        let record =
            learning::import_history(&store, &loaded, &learning::ImportRequest::default()).unwrap();
        assert_eq!(
            (
                record.observations.reviewed_units,
                record.observations.unresolved_units
            ),
            (0, 1)
        );
        let archive = learning::export_history(&store).unwrap().archive;
        assert_eq!(archive.units[0].disposition.as_deref(), Some(expected));
        let support = only_support(&store);
        assert_eq!(
            (
                support.confirmed_units,
                support.acceptable_units,
                support.unresolved_units
            ),
            (0, 0, 1)
        );

        let event = learning::FeedbackEvent {
            schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
            event_id: "outcome-revision".into(),
            review_id: record.review_id.clone(),
            unit_id: loaded.queue.units[0].id.clone(),
            candidate_id: None,
            kind: learning::FeedbackKind::SemanticOutcomeRevision,
            action: learning::FeedbackAction::Append,
            supersedes_event_id: None,
            effective_disposition: Some("confirmed".into()),
            usefulness: None,
            explanation: "Правка агрегата не доказывает покрытие исходных кандидатов".into(),
            provenance: "reviewer".into(),
            recorded_at: 1,
        };
        learning::feedback::record_feedback(&store, &event).unwrap();
        assert_eq!(only_support(&store), support);
        assert_eq!(
            learning::feedback::outcome_distribution(&store)
                .unwrap()
                .get(expected),
            Some(&1)
        );
        let archive = learning::export_history(&store).unwrap().archive;
        assert_eq!(archive.feedback_events, vec![event]);
        let restored =
            LearningStore::open(StoreOptions::at(temporary.path().join("restored.sqlite")))
                .unwrap();
        learning::restore_history(&restored, &archive).unwrap();
        assert_eq!(only_support(&restored), support);
        assert_eq!(
            learning::export_history(&restored)
                .unwrap()
                .archive
                .feedback_events,
            archive.feedback_events
        );
    }
}

#[test]
fn full_homogeneous_group_decision_remains_one_reviewed_unit() {
    use anki_repo::code_review::semantic_triage::{self, Disposition, GroupDecision, ReasonCode};
    let mut loaded = loaded_fixture_with_candidates(3);
    let unit = &loaded.queue.units[0];
    let mut triage = semantic_triage::initialize(&loaded.pack, &loaded.review_pack_sha256);
    triage.unreviewed_candidate_ids.clear();
    triage.group_decisions.push(GroupDecision {
        id: unit.id.clone(),
        candidate_ids: unit.candidate_ids().to_vec(),
        representative_candidate_ids: unit.representative_candidate_ids().to_vec(),
        disposition: Disposition::Acceptable,
        reason_code: ReasonCode::Other,
        explanation: "Все кандидаты группы рассмотрены с единым исходом".into(),
        finding_ids: Vec::new(),
    });
    semantic_triage::canonicalize(&mut triage);
    loaded.triage = Some((
        triage.clone(),
        learning::import::sha256_hex(&serde_json::to_vec(&triage).unwrap()),
    ));
    let temporary = TempDir::new("learning-homogeneous-group");
    let store =
        LearningStore::open(StoreOptions::at(temporary.path().join("state.sqlite"))).unwrap();
    let record =
        learning::import_history(&store, &loaded, &learning::ImportRequest::default()).unwrap();
    assert_eq!(
        (
            record.observations.group_decisions,
            record.observations.covered_candidates,
            record.observations.reviewed_units,
            record.observations.unresolved_units
        ),
        (1, 3, 1, 0)
    );
    let support = only_support(&store);
    assert_eq!(
        (
            support.support_units,
            support.acceptable_units,
            support.unresolved_units
        ),
        (1, 1, 0)
    );
    let archive = learning::export_history(&store).unwrap().archive;
    assert_eq!(archive.units[0].disposition.as_deref(), Some("acceptable"));
    assert_eq!(archive.units[0].reason_code.as_deref(), Some("other"));
    assert_eq!(archive.decisions[0].covered_candidate_ids.len(), 3);
}

#[test]
fn history_pages_filter_before_limit_and_keep_deterministic_revision_order() {
    let temporary = TempDir::new("learning-history-pages");
    let store =
        LearningStore::open(StoreOptions::at(temporary.path().join("state.sqlite"))).unwrap();
    let loaded = loaded_fixture();
    for (id, head) in [
        ("first", "head-a"),
        ("second", "head-b"),
        ("third", "head-c"),
    ] {
        seed_case(&store, &loaded, id, head, None, "acceptable", None);
    }
    store.write(|write| {
        write.execute("UPDATE learning_import SET revision = 3, workspace_label = 'PR%-SPECIAL' WHERE review_id = 'third'", [])?;
        write.execute("UPDATE learning_import SET trust = 'structure_only_quarantine' WHERE review_id = 'second'", [])?;
        Ok(())
    }).unwrap();
    let (total, first) = learning::import::list_imports_page(&store, true, 0, 2, "").unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        first
            .iter()
            .map(|record| record.review_id.as_str())
            .collect::<Vec<_>>(),
        ["third", "first"]
    );
    let (total, second) = learning::import::list_imports_page(&store, true, 2, 2, "").unwrap();
    assert_eq!(total, 3);
    assert_eq!(second[0].review_id, "second");
    let (total, trusted) = learning::import::list_imports_page(&store, false, 0, 10, "").unwrap();
    assert_eq!(total, 2);
    assert_eq!(trusted.len(), 2);
    for (search, id) in [
        ("THIRD", "third"),
        ("HEAD-B", "second"),
        ("quarantine", "second"),
        ("pr%-special", "third"),
    ] {
        let (total, records) =
            learning::import::list_imports_page(&store, true, 0, 1, search).unwrap();
        assert_eq!(total, 1);
        assert_eq!(records[0].review_id, id);
    }
    let (total, absent) =
        learning::import::list_imports_page(&store, true, usize::MAX, 1, "").unwrap();
    assert_eq!(total, 3);
    assert!(absent.is_empty());
    let (total, zero) = learning::import::list_imports_page(&store, true, 0, 0, "").unwrap();
    assert_eq!(total, 3);
    assert!(zero.is_empty());
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
