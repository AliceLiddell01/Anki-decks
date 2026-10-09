//! Регрессии идентичности свидетельств и переноса независимых находок.

use std::collections::BTreeMap;
use std::path::Path;

use anki_repo::code_review::learning;
use anki_repo::code_review::learning::import::{ImportInputsHint, LoadedReview};
use anki_repo::code_review::learning::model::TrustLevel;
use anki_repo::code_review::model::{
    CandidateEvidence, CandidateOrigin, REVIEW_SCHEMA_VERSION, ReviewPack, ReviewScope,
};
use anki_repo::code_review::review_queue;
use anki_repo::code_review::scope::GitTarget;
use anki_repo::code_review::semantic_triage::{self, FindingProvenance, SemanticFinding, Severity};
use anki_repo::error::ErrorCode;

use crate::common::TempDir;

fn loaded(head: &str, description: Option<&str>, linked: bool) -> LoadedReview {
    let pack = ReviewPack {
        schema_version: REVIEW_SCHEMA_VERSION,
        target: GitTarget {
            repository_id: learning::import::sha256_hex(b"synthetic repository"),
            base_sha: "a".repeat(40),
            head_sha: head.to_owned(),
            merge_base_sha: "a".repeat(40),
        },
        scope: ReviewScope {
            merge_base_sha: "a".repeat(40),
            text_image_limit_bytes: 1024,
            files: Vec::new(),
        },
        diagnostics: Vec::new(),
        candidates: if linked {
            vec![CandidateEvidence {
                id: "candidate".into(),
                detector: "error_path".into(),
                path: "src/service.rs".into(),
                line: Some(17),
                column: Some(5),
                snippet: Some("return Err(error);".into()),
                origin: CandidateOrigin::IntroducedOrChanged,
                signals: vec!["error_path".into()],
                source: "synthetic".into(),
                metadata: BTreeMap::new(),
            }]
        } else {
            Vec::new()
        },
        language: anki_repo::code_review::language::LanguageScan {
            schema_version: anki_repo::code_review::language::LANGUAGE_SCHEMA_VERSION,
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
    let digest = learning::import::sha256_hex(&serde_json::to_vec(&pack).unwrap());
    let queue = review_queue::build(&pack, &digest, &BTreeMap::new()).unwrap();
    let mut triage = semantic_triage::initialize(&pack, &digest);
    if let Some(description) = description {
        triage.findings.push(SemanticFinding {
            id: "finding".into(),
            severity: Severity::Major,
            title: "Проверяемый дефект".into(),
            description: description.into(),
            provenance: if linked {
                FindingProvenance::CandidateAssisted
            } else {
                FindingProvenance::Independent
            },
            candidate_ids: if linked {
                vec!["candidate".into()]
            } else {
                Vec::new()
            },
        });
    }
    if linked && description.is_some() {
        triage.unreviewed_candidate_ids.clear();
        triage
            .individual_decisions
            .push(semantic_triage::CandidateDecision {
                candidate_id: "candidate".into(),
                disposition: semantic_triage::Disposition::Confirmed,
                reason_code: semantic_triage::ReasonCode::ExpectedFailurePath,
                explanation: "Свидетельство локализует подтверждённый дефект".into(),
                finding_ids: vec!["finding".into()],
            });
    }
    semantic_triage::canonicalize(&mut triage);
    // Фикстура проверяет импорт подготовленного значения. AST-загрузка и уровень
    // доверия отдельно покрыты контрактами loader на временных Git-образах.
    LoadedReview {
        queue_sha256: learning::import::sha256_hex(&serde_json::to_vec(&queue).unwrap()),
        review_pack_sha256: digest,
        triage: Some((
            triage.clone(),
            learning::import::sha256_hex(&serde_json::to_vec(&triage).unwrap()),
        )),
        pack,
        queue,
        trust: TrustLevel::AstAuthenticated,
        limitations: Vec::new(),
        inputs_hint: ImportInputsHint::default(),
    }
}

fn store(path: &Path) -> learning::LearningStore {
    learning::LearningStore::open(learning::StoreOptions::at(path.join("state.sqlite"))).unwrap()
}

fn import(store: &learning::LearningStore, loaded: &LoadedReview) -> learning::ImportRecord {
    learning::import_history(store, loaded, &learning::ImportRequest::default()).unwrap()
}

fn links(store: &learning::LearningStore) -> usize {
    learning::export_history(store)
        .unwrap()
        .archive
        .case_links
        .len()
}

#[test]
fn same_structural_class_and_nearby_ranges_do_not_identify_different_bugs() {
    let temporary = TempDir::new("finding-identity");
    let store = store(temporary.path());
    let first = loaded(
        &"b".repeat(40),
        Some("src/service.rs:17: ошибка отмены запроса"),
        true,
    );
    let second = loaded(
        &"c".repeat(40),
        Some("src/service.rs:17: ошибка обработки тайм-аута"),
        true,
    );
    import(&store, &first);
    import(&store, &second);
    assert_eq!(links(&store), 0);
    let archive = learning::export_history(&store).unwrap().archive;
    assert_ne!(archive.findings[0].signature, archive.findings[1].signature);
}

#[test]
fn repeated_full_evidence_remains_linkable_without_candidate_identity() {
    let temporary = TempDir::new("independent-evidence-repeat");
    let store = store(temporary.path());
    let description = "src/service.rs:17: отмена запроса пропускает закрытие ресурса";
    import(&store, &loaded(&"b".repeat(40), Some(description), false));
    import(&store, &loaded(&"c".repeat(40), Some(description), false));
    assert_eq!(links(&store), 1);
    let archive = learning::export_history(&store).unwrap().archive;
    assert!(archive.case_links[0].basis.contains("свидетельства"));
    assert!(archive.finding_links.is_empty());
}

#[test]
fn independent_evidence_and_provenance_survive_search_export_and_restore() {
    let source_dir = TempDir::new("independent-source");
    let destination_dir = TempDir::new("independent-destination");
    let source = store(source_dir.path());
    let destination = store(destination_dir.path());
    let description = "src/service.rs:17: отмена запроса пропускает закрытие ресурса";
    let record = import(&source, &loaded(&"b".repeat(40), Some(description), false));
    assert_eq!(record.observations.findings_independent, 1);
    assert_eq!(record.observations.individual_decisions, 0);
    let archive = learning::export_history(&source).unwrap().archive;
    assert_eq!(archive.findings[0].description, description);
    assert_eq!(archive.findings[0].provenance, "independent");
    assert!(archive.finding_links.is_empty());
    learning::restore_history(&destination, &archive).unwrap();
    let results = learning::search_history(
        &destination,
        &learning::search::SearchQuery {
            text: Some("src/service.rs:17".into()),
            provenance: Some("independent".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(results.cases.len(), 1);
    assert_eq!(results.cases[0].finding_id.as_deref(), Some("finding"));
    assert!(results.cases[0].candidate_id.is_none());
    assert!(results.cases[0].unit_id.is_none());
    assert_eq!(
        learning::export_history(&destination)
            .unwrap()
            .archive
            .findings,
        archive.findings
    );
}

#[test]
fn classifier_features_use_the_declared_rules_and_keep_legacy_unknown_separate() {
    let temporary = TempDir::new("classifier-stamp");
    let store = store(temporary.path());
    let current = loaded(&"b".repeat(40), None, true);
    let record = import(&store, &current);
    let archive = learning::export_history(&store).unwrap().archive;
    let features = &archive.units[0].feature_map;
    assert_eq!(
        features.get("classifier_compatibility"),
        Some(&record.inputs.classifier_digest)
    );
    let mut legacy = loaded(&"c".repeat(40), None, true);
    legacy.queue.classifier_rules_version = None;
    assert_ne!(
        learning::import::classifier_digest(&legacy.pack, &legacy.queue),
        record.inputs.classifier_digest
    );
    legacy.queue.classifier_rules_version = Some(review_queue::CLASSIFIER_RULES_VERSION + 1);
    assert_ne!(
        learning::import::classifier_digest(&legacy.pack, &legacy.queue),
        record.inputs.classifier_digest
    );
}

#[test]
fn request_prevalidation_rejects_execution_variant_mismatch_without_database() {
    let mut loaded = loaded(&"b".repeat(40), None, false);
    loaded.inputs_hint.workspace_variant = Some(format!("snapshot-{}", "a".repeat(32)));
    let error =
        learning::import::validate_import_request(&loaded, &learning::ImportRequest::default())
            .unwrap_err();
    assert_eq!(error.code, ErrorCode::BaselineMismatch);
}

#[test]
fn early_review_id_without_trust_remains_an_idempotent_import() {
    let temporary = TempDir::new("legacy-review-id");
    let store = store(temporary.path());
    let mut loaded = loaded(&"b".repeat(40), None, false);
    loaded.queue.classifier_rules_version = None;
    let record = import(&store, &loaded);
    let target = &loaded.pack.target;
    let digest = learning::import::sha256_hex(
        format!(
            "{}\n{}\n{}\n{}\nroot\n{}\n{}\n{}\n{}\n",
            target.repository_id,
            target.base_sha,
            target.head_sha,
            target.merge_base_sha,
            record.inputs.analyzer_digest,
            record.inputs.classifier_digest,
            loaded.queue_sha256,
            loaded.triage.as_ref().unwrap().1,
        )
        .as_bytes(),
    );
    let legacy_id = format!("review-{}", &digest[..32]);
    let connection = rusqlite::Connection::open(temporary.path().join("state.sqlite")).unwrap();
    connection
        .execute(
            "UPDATE learning_import SET review_id = ?1 WHERE review_id = ?2",
            [&legacy_id, &record.review_id],
        )
        .unwrap();
    let repeated =
        learning::import::import_with_outcome(&store, &loaded, &learning::ImportRequest::default())
            .unwrap();
    assert_eq!(repeated.status, learning::ImportStatus::NoopExisting);
    assert_eq!(repeated.record.review_id, legacy_id);
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path, content: &str) -> String {
    std::fs::write(root.join("source.rs"), content).unwrap();
    git(root, &["add", "source.rs"]);
    git(root, &["commit", "-qm", content]);
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn only_proven_git_ancestry_creates_a_revision_line() {
    let repository = TempDir::new("import-ancestry-repository");
    let storage = TempDir::new("import-ancestry-storage");
    let root = repository.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Тест"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    let base = commit(root, "base");
    let first_head = commit(root, "first");
    let second_head = commit(root, "descendant");
    git(root, &["checkout", "-q", "--detach", &base]);
    let sibling_head = commit(root, "sibling");
    let store = store(storage.path());
    let mut first = loaded(&first_head, None, true);
    first.inputs_hint.repository_root = Some(root.to_path_buf());
    let first_record = import(&store, &first);
    let mut second = loaded(&second_head, None, true);
    second.inputs_hint.repository_root = Some(root.to_path_buf());
    let second_record = import(&store, &second);
    assert_eq!(
        second_record.revision_of.as_deref(),
        Some(first_record.review_id.as_str())
    );
    let mut sibling = loaded(&sibling_head, None, true);
    sibling.inputs_hint.repository_root = Some(root.to_path_buf());
    let sibling_record = import(&store, &sibling);
    assert!(sibling_record.revision_of.is_none());
    assert_eq!(
        learning::show_import(&store, &first_record.review_id)
            .unwrap()
            .superseded_by
            .as_deref(),
        Some(second_record.review_id.as_str())
    );
    assert!(
        learning::show_import(&store, &second_record.review_id)
            .unwrap()
            .superseded_by
            .is_none()
    );
}

#[test]
fn truncated_evidence_is_reported_and_cannot_prove_a_repeat() {
    let temporary = TempDir::new("truncated-independent-evidence");
    let store = store(temporary.path());
    let description = format!("src/service.rs:17: {}", "обоснование ".repeat(200));
    let first = import(&store, &loaded(&"b".repeat(40), Some(&description), false));
    let second = import(&store, &loaded(&"c".repeat(40), Some(&description), false));
    for record in [first, second] {
        assert!(
            record
                .limitations
                .iter()
                .any(|text| text.contains("ограниченный текст"))
        );
    }
    assert_eq!(links(&store), 0);
    let archive = learning::export_history(&store).unwrap().archive;
    assert!(archive.findings.iter().all(|finding| {
        finding.description.len() <= learning::import::MAX_STORED_TEXT_BYTES
            && finding.signature.starts_with("finding-incomplete-v2-")
    }));
}
