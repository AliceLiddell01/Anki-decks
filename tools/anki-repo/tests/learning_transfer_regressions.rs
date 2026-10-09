//! Регрессии переносимой истории: решения, связи, атомарность и снимок чтения.

use std::collections::BTreeMap;
use std::path::Path;

use anki_repo::code_review::learning::model::{
    ExportedCandidate, ExportedDecision, ExportedFinding, ExportedFindingLink, ExportedSearchCase,
    FeedbackAction, FeedbackEvent, FeedbackKind, ImportInputs, ImportRecord, LearningExport,
    ObservationCounts, ReviewUnitKind, ReviewUnitRecord, ReviewedOutcome, TrustLevel,
};
use anki_repo::code_review::learning::{self, LearningStore, StoreOptions};
use anki_repo::error::ErrorCode;
use sha2::{Digest, Sha256};

use crate::common::TempDir;

fn open(path: &Path) -> LearningStore {
    LearningStore::open(StoreOptions::at(path.join("history.sqlite"))).unwrap()
}

/// Сериализация тела повторяет порядок форматного контракта, включая пустой
/// search у v1. Неизвестные старым версиям поля в их digest не добавляются.
fn seal(archive: &mut LearningExport) {
    let mut names = vec!["reviews", "units", "candidates"];
    if archive.manifest.export_schema_version >= 4 {
        names.push("decisions");
    }
    names.extend([
        "findings",
        "finding_links",
        "case_links",
        "feedback_events",
        "policy_proposals",
        "search",
    ]);
    // Объекты внутри тела сериализуются в порядке полей Rust-моделей,
    // поэтому сериализуем каждую коллекцию исходного архива отдельно.
    let values = [
        serde_json::to_string(&archive.reviews).unwrap(),
        serde_json::to_string(&archive.units).unwrap(),
        serde_json::to_string(&archive.candidates).unwrap(),
        serde_json::to_string(&archive.decisions).unwrap(),
        serde_json::to_string(&archive.findings).unwrap(),
        serde_json::to_string(&archive.finding_links).unwrap(),
        serde_json::to_string(&archive.case_links).unwrap(),
        serde_json::to_string(&archive.feedback_events).unwrap(),
        serde_json::to_string(&archive.policy_proposals).unwrap(),
        serde_json::to_string(&archive.search).unwrap(),
    ];
    let values: Vec<_> = values
        .into_iter()
        .enumerate()
        .filter(|(index, _)| *index != 3 || archive.manifest.export_schema_version >= 4)
        .map(|(_, value)| value)
        .collect();
    let body = names
        .iter()
        .zip(values)
        .map(|(name, value)| format!("\"{name}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    let bytes = format!("{{{body}}}");
    archive.manifest.payload_sha256 = format!("{:x}", Sha256::digest(bytes.as_bytes()));
    archive.manifest.reviews = archive.reviews.len();
    archive.manifest.units = archive.units.len();
    archive.manifest.findings = archive.findings.len();
    archive.manifest.feedback_events = archive.feedback_events.len();
    archive.manifest.search_cases = archive.search.len();
}

fn fixture(store: &LearningStore, id: &str) -> LearningExport {
    let mut archive = learning::transfer::export_history(store).unwrap().archive;
    assert!(archive.reviews.is_empty());
    archive.reviews.push(ImportRecord {
        review_id: id.into(),
        repository_id: "synthetic-repository".into(),
        base_sha: format!("base-{id}"),
        head_sha: format!("head-{id}"),
        merge_base_sha: format!("base-{id}"),
        workspace_variant: "root".into(),
        workspace_label: None,
        inputs: ImportInputs {
            review_pack_sha256: format!("pack-{id}"),
            ..ImportInputs::default()
        },
        trust: TrustLevel::AstAuthenticated,
        outcome: ReviewedOutcome::FullyReviewed,
        limitations: vec![],
        revision_of: None,
        superseded_by: None,
        revision: 1,
        imported_at: 1,
        observations: ObservationCounts::default(),
    });
    for (unit_id, kind, candidates) in [
        (
            "individual",
            ReviewUnitKind::Individual,
            vec!["candidate-a"],
        ),
        (
            "group",
            ReviewUnitKind::Group,
            vec!["candidate-b", "candidate-c"],
        ),
    ] {
        archive.units.push(ReviewUnitRecord {
            review_id: id.into(),
            unit_id: unit_id.into(),
            kind,
            candidate_count: candidates.len(),
            priority: "normal".into(),
            representative_candidate_ids: vec![candidates[0].into()],
            disposition: Some("confirmed".into()),
            reason_code: Some("other".into()),
            detector: "error_path".into(),
            source: "synthetic".into(),
            role: "production".into(),
            code_role: "implementation".into(),
            surfaces: vec!["production".into()],
            feature_map: BTreeMap::from([("role".into(), "production".into())]),
        });
        for candidate in &candidates {
            archive.candidates.push(ExportedCandidate {
                review_id: id.into(),
                candidate_id: (*candidate).into(),
                unit_id: unit_id.into(),
                detector: "error_path".into(),
                source: "synthetic".into(),
                path: "src/lib.rs".into(),
                path_family: "src".into(),
                origin: "introduced_or_changed".into(),
                snippet: None,
                classification_json: "{}".into(),
            });
        }
        archive.decisions.push(ExportedDecision {
            review_id: id.into(),
            decision_id: format!("decision-{unit_id}"),
            kind: kind.as_str().into(),
            disposition: "confirmed".into(),
            reason_code: "other".into(),
            explanation: "Семантическая проверка подтверждает нарушение контракта.".into(),
            candidate_count: candidates.len(),
            covered_candidate_ids: candidates.into_iter().map(str::to_owned).collect(),
        });
    }
    archive.findings.push(ExportedFinding {
        review_id: id.into(),
        finding_id: "linked-finding".into(),
        severity: "major".into(),
        provenance: "direct_candidate".into(),
        title: "Нарушение контракта".into(),
        description: "Проверенное объяснение с доказательством.".into(),
        signature: "finding-signature".into(),
    });
    for candidate in ["candidate-a", "candidate-b"] {
        archive.finding_links.push(ExportedFindingLink {
            review_id: id.into(),
            candidate_id: candidate.into(),
            finding_id: "linked-finding".into(),
        });
    }
    archive.search.push(ExportedSearchCase {
        case_id: format!("search-{id}"),
        review_id: id.into(),
        unit_id: "individual".into(),
        candidate_id: Some("candidate-a".into()),
        finding_id: Some("linked-finding".into()),
        kind: "finding".into(),
        disposition: Some("confirmed".into()),
        severity: Some("major".into()),
        provenance: Some("direct_candidate".into()),
        text: "Проверенное объяснение".into(),
    });
    seal(&mut archive);
    learning::transfer::verify_export(&archive).unwrap();
    archive
}

fn event(id: &str, action: FeedbackAction, target: Option<&str>) -> FeedbackEvent {
    FeedbackEvent {
        schema_version: learning::feedback::FEEDBACK_SCHEMA_VERSION,
        event_id: id.into(),
        review_id: "review".into(),
        unit_id: "individual".into(),
        candidate_id: Some("candidate-a".into()),
        kind: FeedbackKind::SemanticOutcomeRevision,
        action,
        supersedes_event_id: target.map(str::to_owned),
        effective_disposition: if action == FeedbackAction::Retract {
            None
        } else {
            Some("false_positive".into())
        },
        usefulness: None,
        explanation: "Ревьюер уточнил исход.".into(),
        provenance: "reviewer".into(),
        recorded_at: 2,
    }
}

#[test]
fn decisions_roundtrip_preserves_group_coverage_findings_search_and_local_children() {
    let source_dir = TempDir::new("transfer-decisions-source");
    let source = open(source_dir.path());
    let mut input = fixture(&source, "review");
    input
        .feedback_events
        .push(event("correction", FeedbackAction::Append, None));
    seal(&mut input);
    learning::transfer::restore_history(&source, &input).unwrap();
    let archive = learning::transfer::export_history(&source).unwrap().archive;
    let mut expected_decisions = input.decisions.clone();
    expected_decisions.sort_by(|left, right| {
        (&left.review_id, &left.decision_id).cmp(&(&right.review_id, &right.decision_id))
    });
    assert_eq!(archive.decisions, expected_decisions);
    assert_eq!(archive.manifest.export_schema_version, 4);
    let destination_dir = TempDir::new("transfer-decisions-destination");
    let destination = open(destination_dir.path());
    learning::transfer::restore_history(&destination, &archive).unwrap();
    assert_eq!(
        learning::feedback::outcome(&source, "review", "individual").unwrap(),
        learning::feedback::outcome(&destination, "review", "individual").unwrap()
    );
    let query = learning::patterns::PatternQuery {
        limit: 10,
        now: Some(1),
        ..Default::default()
    };
    let source_rules = learning::patterns::pattern_report(&source, &query)
        .unwrap()
        .rules;
    let restored_rules = learning::patterns::pattern_report(&destination, &query)
        .unwrap()
        .rules;
    assert_eq!(source_rules, restored_rules);
    assert!(
        source_rules
            .iter()
            .any(|rule| rule.support.confirmed_findings > 0)
    );
    let search = learning::search::SearchQuery {
        text: Some("объяснение".into()),
        ..Default::default()
    };
    assert_eq!(
        learning::search_history(&source, &search).unwrap().cases,
        learning::search_history(&destination, &search)
            .unwrap()
            .cases
    );
    destination
        .write(|write| {
            write.execute(
                "UPDATE learning_decision SET explanation = 'Локальное уточнение'",
                [],
            )?;
            write.execute("UPDATE learning_candidate SET path = 'src/local.rs'", [])?;
            Ok(())
        })
        .unwrap();
    let before = learning::transfer::export_history(&destination)
        .unwrap()
        .archive;
    let repeated = learning::transfer::restore_history(&destination, &archive).unwrap();
    assert_eq!(repeated.unchanged_reviews, 1);
    assert_eq!(
        before,
        learning::transfer::export_history(&destination)
            .unwrap()
            .archive
    );
}

#[test]
fn legacy_versions_keep_original_digest_and_report_missing_decisions_without_fabrication() {
    let empty_dir = TempDir::new("transfer-legacy-source");
    let empty = open(empty_dir.path());
    for version in [1, 2, 3] {
        let mut archive = fixture(&empty, "review");
        archive.manifest.export_schema_version = version;
        archive.decisions.clear();
        if version == 1 {
            archive.search.clear();
        }
        seal(&mut archive);
        let mut document = serde_json::to_value(&archive).unwrap();
        document.as_object_mut().unwrap().remove("decisions");
        if version == 1 {
            document.as_object_mut().unwrap().remove("search");
            document["manifest"]
                .as_object_mut()
                .unwrap()
                .remove("search_cases");
        }
        let old: LearningExport = serde_json::from_value(document).unwrap();
        let directory = TempDir::new("transfer-legacy-destination");
        let destination = open(directory.path());
        let result = learning::transfer::restore_history(&destination, &old).unwrap();
        assert!(
            result
                .limitations
                .iter()
                .any(|text| text.contains("не содержит исходных semantic decisions"))
        );
        assert!(
            learning::transfer::export_history(&destination)
                .unwrap()
                .archive
                .decisions
                .is_empty()
        );
    }
}

#[test]
fn invalid_archives_with_valid_digest_fail_without_changing_existing_history() {
    let directory = TempDir::new("transfer-invalid");
    let store = open(directory.path());
    let original = fixture(&store, "review");
    learning::transfer::restore_history(&store, &original).unwrap();
    let before = learning::transfer::export_history(&store).unwrap().archive;
    let mut mutations = vec![];
    let mut archive = original.clone();
    archive.units[0].feature_map.insert(
        "classifier_compatibility".into(),
        "forged-classifier".into(),
    );
    mutations.push(archive);
    let mut archive = original.clone();
    archive.candidates[0].unit_id = "absent".into();
    mutations.push(archive);
    let mut archive = original.clone();
    let mut wrong_candidate = event("wrong-candidate", FeedbackAction::Append, None);
    wrong_candidate.candidate_id = Some("candidate-b".into());
    archive.feedback_events.push(wrong_candidate);
    mutations.push(archive);
    let mut archive = original.clone();
    archive.decisions[0].covered_candidate_ids = vec!["missing".into()];
    mutations.push(archive);
    let mut archive = original.clone();
    archive.decisions.push(archive.decisions[0].clone());
    mutations.push(archive);
    let mut archive = original.clone();
    let mut cross_kind = event("foreign-kind", FeedbackAction::Supersede, Some("semantic"));
    cross_kind.kind = FeedbackKind::RecommendationUsefulness;
    cross_kind.effective_disposition = None;
    cross_kind.usefulness = Some("useful".into());
    archive.feedback_events = vec![event("semantic", FeedbackAction::Append, None), cross_kind];
    mutations.push(archive);
    let mut archive = original.clone();
    archive.feedback_events = vec![event(
        "missing-target",
        FeedbackAction::Supersede,
        Some("absent"),
    )];
    mutations.push(archive);
    let mut archive = original.clone();
    archive.feedback_events = vec![
        event("semantic", FeedbackAction::Append, None),
        event("first", FeedbackAction::Supersede, Some("semantic")),
        event("second", FeedbackAction::Retract, Some("semantic")),
    ];
    mutations.push(archive);
    let mut archive = original.clone();
    let mut quarantined_revision = archive.reviews[0].clone();
    quarantined_revision.review_id = "quarantined-revision".into();
    quarantined_revision.trust = TrustLevel::StructureOnlyQuarantine;
    quarantined_revision.revision_of = Some("review".into());
    quarantined_revision.superseded_by = None;
    quarantined_revision.head_sha = "quarantined-head".into();
    quarantined_revision.inputs.review_pack_sha256 = "quarantined-pack".into();
    archive.reviews[0].superseded_by = Some(quarantined_revision.review_id.clone());
    archive.reviews.push(quarantined_revision);
    mutations.push(archive);
    for mut invalid in mutations {
        seal(&mut invalid);
        assert_eq!(
            learning::transfer::restore_history(&store, &invalid)
                .unwrap_err()
                .code,
            ErrorCode::LearningExportInvalid
        );
        assert_eq!(
            learning::transfer::export_history(&store).unwrap().archive,
            before
        );
    }
}

#[test]
fn restore_validates_same_kind_replacement_retraction_without_array_order_dependency() {
    let directory = TempDir::new("transfer-correction-chain");
    let store = open(directory.path());
    let mut archive = fixture(&store, "review");
    archive.feedback_events = vec![
        event("retract", FeedbackAction::Retract, Some("replacement")),
        event("replacement", FeedbackAction::Supersede, Some("first")),
        event("first", FeedbackAction::Append, None),
    ];
    seal(&mut archive);
    learning::transfer::restore_history(&store, &archive).unwrap();
    let outcome = learning::feedback::outcome(&store, "review", "individual").unwrap();
    assert_eq!(outcome.original_disposition.as_deref(), Some("confirmed"));
    assert_eq!(outcome.effective_disposition, None);
    assert!(outcome.effective_event_ids.is_empty());
}

#[test]
fn global_feedback_collision_rolls_back_all_new_reviews_and_preserves_local_audit() {
    let source_dir = TempDir::new("transfer-collision-source");
    let source = open(source_dir.path());
    let mut archive = fixture(&source, "incoming");
    let mut incoming = event("shared-event", FeedbackAction::Append, None);
    incoming.review_id = "incoming".into();
    archive.feedback_events.push(incoming);
    seal(&mut archive);
    let destination_dir = TempDir::new("transfer-collision-destination");
    let destination = open(destination_dir.path());
    let mut local = fixture(&destination, "review");
    local
        .feedback_events
        .push(event("shared-event", FeedbackAction::Append, None));
    seal(&mut local);
    learning::transfer::restore_history(&destination, &local).unwrap();
    let before = learning::transfer::export_history(&destination)
        .unwrap()
        .archive;
    assert_eq!(
        learning::transfer::restore_history(&destination, &archive)
            .unwrap_err()
            .code,
        ErrorCode::LearningConflict
    );
    assert_eq!(
        before,
        learning::transfer::export_history(&destination)
            .unwrap()
            .archive
    );
}

#[test]
fn export_generation_and_tables_share_a_snapshot_during_concurrent_writes() {
    let source_dir = TempDir::new("transfer-snapshot-source");
    let source = open(source_dir.path());
    let archives: Vec<_> = (0..40)
        .map(|index| fixture(&source, &format!("review-{index}")))
        .collect();
    let destination_dir = TempDir::new("transfer-snapshot-destination");
    let destination = open(destination_dir.path());
    let database = destination_dir.path().join("history.sqlite");
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let writer = LearningStore::open(StoreOptions::at(database)).unwrap();
        for archive in archives {
            learning::transfer::restore_history(&writer, &archive).unwrap();
        }
        finished_tx.send(()).unwrap();
    });
    loop {
        let archive = learning::transfer::export_history(&destination)
            .unwrap()
            .archive;
        assert_eq!(
            archive.manifest.generation.trusted_reviews,
            archive.reviews.len()
        );
        assert_eq!(
            archive.manifest.generation.trusted_units,
            archive.units.len()
        );
        assert_eq!(
            archive.manifest.generation.revision,
            archive.reviews.len() as u64
        );
        if finished_rx.try_recv().is_ok() {
            break;
        }
    }
    writer.join().unwrap();
    assert_eq!(
        learning::transfer::export_history(&destination)
            .unwrap()
            .archive
            .reviews
            .len(),
        40
    );
}

#[test]
fn negative_stored_event_time_is_rejected_without_normalization() {
    let directory = TempDir::new("transfer-negative-time");
    let store = open(directory.path());
    let mut archive = fixture(&store, "review");
    archive
        .feedback_events
        .push(event("correction", FeedbackAction::Append, None));
    seal(&mut archive);
    learning::transfer::restore_history(&store, &archive).unwrap();
    store
        .write(|write| {
            write.execute("UPDATE learning_feedback SET recorded_at = -1", [])?;
            Ok(())
        })
        .unwrap();
    let generation = learning::import::generation(&store).unwrap();
    assert_eq!(
        learning::transfer::export_history(&store).unwrap_err().code,
        ErrorCode::LearningCorrupt
    );
    assert_eq!(
        learning::transfer::restore_history(&store, &archive)
            .unwrap_err()
            .code,
        ErrorCode::LearningCorrupt
    );
    assert_eq!(generation, learning::import::generation(&store).unwrap());
}

#[test]
fn global_search_collision_rolls_back_new_reviews_without_replacing_local_text() {
    let source_dir = TempDir::new("transfer-search-collision-source");
    let source = open(source_dir.path());
    let mut archive = fixture(&source, "incoming");
    archive.search[0].case_id = "shared-case".into();
    seal(&mut archive);
    let destination_dir = TempDir::new("transfer-search-collision-destination");
    let destination = open(destination_dir.path());
    let mut local = fixture(&destination, "review");
    local.search[0].case_id = "shared-case".into();
    local.search[0].text = "Локальное доказательство".into();
    seal(&mut local);
    learning::transfer::restore_history(&destination, &local).unwrap();
    let before = learning::transfer::export_history(&destination)
        .unwrap()
        .archive;
    assert_eq!(
        learning::transfer::restore_history(&destination, &archive)
            .unwrap_err()
            .code,
        ErrorCode::LearningExportInvalid
    );
    assert_eq!(
        before,
        learning::transfer::export_history(&destination)
            .unwrap()
            .archive
    );
}
