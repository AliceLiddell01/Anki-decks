use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::AssetDomainPolicy;
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAbsenceEvidence, JpdbPitchAcquired, JpdbPitchFailure, JpdbPitchOutcome,
    JpdbPitchQuery, JpdbPitchRequest, JpdbPitchSelection, JpdbPitchStage, JpdbVocabularyCandidate,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanAttestation, HumanDecision, LifecycleState,
    Provenance, SemanticStatus, ValidationRecord,
};
use crate::pitch_accent::{
    PitchAccentCaptureRect, PitchAccentCoordinateSpace, PitchAccentDarkThemeProof,
    PitchAccentDomainMetadata, PitchAccentDomainPolicy, PitchAccentEvidence,
    PitchAccentGraphEvidence, PitchAccentImageValidator, PitchAccentProvider,
    PitchAccentRenderEvidence, PitchAccentRenderKind, PitchAccentResolvedForm,
};
use crate::pitch_batch::{
    PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchItemStatus, PitchBatchOwnerSnapshot,
};
use crate::validation::SemanticValidator;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn request(surface: &str, reading: Option<&str>) -> JpdbPitchRequest {
    JpdbPitchRequest::new(JpdbPitchQuery::new(surface, reading.map(str::to_owned)))
}

fn batch(batch_id: &str, surface: &str, reading: Option<&str>) -> PitchAccentBatch {
    PitchAccentBatch::new(
        batch_id,
        vec![request(surface, reading)],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap()
}

fn temp_store() -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "asset-store-pitch-batch-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn browser() -> crate::browser_runtime::BrowserRuntimeProvenance {
    crate::browser_runtime::BrowserRuntimeProvenance {
        product: "Chrome/140".into(),
        protocol_version: "1.3".into(),
        revision: "1234567".into(),
        user_agent: "Mozilla/5.0 Chrome/140".into(),
        js_version: "V8 14.0".into(),
        executable_source: crate::browser_runtime::BrowserExecutableSource::PathLookup,
    }
}

fn metadata(surface: &str, reading: &str, vocabulary_id: u64) -> PitchAccentDomainMetadata {
    let source_url = format!("https://jpdb.io/vocabulary/{vocabulary_id}/{surface}/{reading}#a");
    let rect = PitchAccentCaptureRect {
        x: 100.0,
        y: 200.0,
        width: 20.0,
        height: 10.0,
    };
    PitchAccentDomainMetadata {
        surface: surface.into(),
        reading: reading.into(),
        jpdb_vocabulary_id: vocabulary_id,
        evidence: PitchAccentEvidence {
            provider: PitchAccentProvider::Jpdb,
            source_url,
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: surface.into(),
                reading: reading.into(),
            }],
            graph_count: 1,
            render: PitchAccentRenderEvidence {
                kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                selector: ".pitch-accent-graph".into(),
                graphs: vec![PitchAccentGraphEvidence {
                    index: 0,
                    selector: ".pitch-accent-graph".into(),
                    viewport_rect: PitchAccentCaptureRect {
                        x: 100.0,
                        y: 100.0,
                        width: 20.0,
                        height: 10.0,
                    },
                    document_rect: rect,
                }],
                coordinate_space: PitchAccentCoordinateSpace::Document,
                viewport_width: 1280,
                viewport_height: 900,
                document_width: 1280,
                document_height: 1800,
                scroll_x: 0.0,
                scroll_y: 100.0,
                pixel_width: 60,
                pixel_height: 30,
                device_scale_factor: 3.0,
                page_scale_factor: 1.0,
                dark_theme: PitchAccentDarkThemeProof {
                    document_element_classes: vec!["dark-mode".into()],
                    prefers_color_scheme: "dark".into(),
                    computed_color_scheme: "dark".into(),
                    background_selector: ".subsection-pitch-accent".into(),
                    background_rgb: [24, 36, 48],
                },
                graph_union_rect: rect,
                capture_rect: rect,
            },
            browser: browser(),
        },
    }
}

fn png(alternate: bool) -> Vec<u8> {
    let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([24, 36, 48, 255]));
    let start = if alternate { 12 } else { 10 };
    for x in start..(start + 40) {
        image.put_pixel(x, 15, image::Rgba([235, 235, 235, 255]));
    }
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn acquired(surface: &str, alternate: bool) -> JpdbPitchOutcome {
    acquired_with_metadata(png(alternate), metadata(surface, "ゆうれい", 123))
}

fn acquired_with_metadata(bytes: Vec<u8>, metadata: PitchAccentDomainMetadata) -> JpdbPitchOutcome {
    JpdbPitchOutcome::Acquired {
        asset: Box::new(JpdbPitchAcquired { bytes, metadata }),
    }
}

fn verified_record(surface: &str, alternate: bool) -> AssetRecord {
    let bytes = png(alternate);
    verified_record_with_metadata(surface, bytes, metadata(surface, "ゆうれい", 123))
}

fn verified_record_with_metadata(
    surface: &str,
    bytes: Vec<u8>,
    metadata: PitchAccentDomainMetadata,
) -> AssetRecord {
    let identity = AssetIdentity::new("pitch_accent", surface).unwrap();
    let sha256 = sha256_hex(&bytes);
    let location = PitchAccentDomainPolicy
        .canonical_location(&identity, &sha256, DetectedFormat::Png)
        .unwrap();
    let mut record = AssetRecord {
        identity,
        storage_path: location.storage_path,
        consumer_filename: location.consumer_filename,
        sha256: sha256.clone(),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::Png,
        provenance: Provenance {
            source_kind: "jpdb_browser_render".into(),
            source_name: "jpdb-vocabulary-123.png".into(),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
    };
    let decision = PitchAccentImageValidator
        .validate(&record, &mut Cursor::new(&bytes))
        .unwrap();
    assert_eq!(decision.status, SemanticStatus::Verified);
    record.lifecycle = LifecycleState::Verified;
    record.validation = Some(ValidationRecord {
        status: decision.status,
        validator: PitchAccentImageValidator::validator_identity(),
        content_sha256: sha256,
        evidence: decision.evidence,
    });
    record
}

fn rejected_owner(mut record: AssetRecord) -> AssetRecord {
    record.lifecycle = LifecycleState::Quarantined;
    record.human_attestation = Some(HumanAttestation {
        identity: record.identity.clone(),
        content_sha256: record.sha256.clone(),
        decision: HumanDecision::Reject,
        reason: "synthetic exact-SHA rejection".into(),
    });
    record
}

fn record_provider_outcome(
    runtime: &mut PitchAccentBatchRuntime,
    batch: &mut PitchAccentBatch,
    surface: &str,
    outcome: JpdbPitchOutcome,
) -> String {
    let token = batch.item_token(surface).unwrap();
    assert!(runtime.record_outcome(batch, &token, outcome).unwrap());
    batch
        .item(surface)
        .unwrap()
        .current_candidate_sha256
        .clone()
        .unwrap_or_default()
}

#[test]
fn incompatible_surface_inputs_are_identity_conflicts_and_exact_duplicates_fold() {
    let first = request("幽霊", Some("ゆうれい"));
    let duplicate = first.clone();
    let collapsed = PitchAccentBatch::new(
        "dupe-fold",
        vec![first.clone(), duplicate],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(collapsed.items.len(), 1);

    let incompatible = PitchAccentBatch::new(
        "reading-conflict",
        vec![request("幽霊", Some("ゆうれい")), request("幽霊", None)],
        PitchAccentImageValidator::validator_identity(),
    );
    assert!(incompatible.is_err());
}

#[test]
fn legacy_batch_schema_without_exact_attempt_and_plan_identity_fails_closed() {
    let mut current = batch("legacy-batch-schema", "幽霊", Some("ゆうれい"));
    current.schema_version = 1;
    assert!(current.validate().is_err());

    let mut legacy =
        serde_json::to_value(batch("legacy-batch-json", "幽霊", Some("ゆうれい"))).unwrap();
    let root = legacy.as_object_mut().unwrap();
    root.insert("schema_version".into(), serde_json::json!(1));
    root.remove("original_plan");
    root["items"][0]
        .as_object_mut()
        .unwrap()
        .remove("current_candidate_attempt_index");
    assert!(serde_json::from_value::<PitchAccentBatch>(legacy).is_err());
}

#[test]
fn transient_retry_is_targeted_and_page_contract_failure_is_not_retryable() {
    let root = temp_store();
    let mut batch = batch("retry-lifecycle", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();

    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::DetailReadiness,
                        diagnostic: Some("request timed out".into()),
                    },
                },
            )
            .unwrap()
    );
    let timed_out_token = token;
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::TechnicalFailure
    );
    batch
        .retry("幽霊", "retry transient timeout".into())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_eq!(batch.item("幽霊").unwrap().generation, 1);
    assert_eq!(batch.item("幽霊").unwrap().attempts.len(), 1);
    assert!(
        !runtime
            .record_outcome(
                &mut batch,
                &timed_out_token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::Capture,
                        diagnostic: Some("stale pre-retry result".into()),
                    },
                },
            )
            .unwrap()
    );
    assert_eq!(batch.item("幽霊").unwrap().attempts.len(), 1);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Pending
    );

    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::PageContract {
                        stage: JpdbPitchStage::DetailVerification,
                        message: "unexpected source page".into(),
                    },
                },
            )
            .unwrap()
    );
    assert!(
        batch
            .retry("幽霊", "must not repeat a page contract failure".into())
            .is_err()
    );

    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn navigation_retry_classifier_requires_error_or_http_context() {
    let navigation = |message: &str| JpdbPitchFailure::Navigation {
        stage: JpdbPitchStage::SearchNavigation,
        message: message.into(),
    };
    assert!(crate::pitch_batch::is_retryable_failure(&navigation(
        "navigation returned HTTP status 503"
    )));
    assert!(crate::pitch_batch::is_retryable_failure(&navigation(
        "temporary resolver failure: net::ERR_DNS_TIMED_OUT"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "request took 500 ms at https://jpdb.io/vocabulary/500/幽霊/ゆうれい"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "HTTP request took 500 ms"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "HTTP status checked after 500 ms"
    )));
    assert!(crate::pitch_batch::is_retryable_failure(&navigation(
        "HTTP status code 429"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "HTTP status 404"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "DNS_PROBE_FINISHED_NXDOMAIN"
    )));
}

#[test]
fn ambiguity_selection_uses_exact_inventory_id_and_route_then_starts_new_generation() {
    let root = temp_store();
    let mut batch = batch("ambiguity-selection", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let immutable_plan = batch.original_plan.clone();
    let token = batch.item_token("幽霊").unwrap();
    let candidate = JpdbVocabularyCandidate {
        vocabulary_id: 123,
        surface_forms: vec!["幽霊".into()],
        readings: vec!["ゆうれい".into()],
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "ゆうれい".into(),
        }],
        part_of_speech: vec!["noun".into()],
        meanings: vec!["ghost".into()],
        detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
    };
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::AmbiguousVocabulary {
                    surface: "幽霊".into(),
                    reading: Some("ゆうれい".into()),
                    candidates: vec![candidate],
                },
            )
            .unwrap()
    );
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::AmbiguousVocabulary
    );
    let ambiguous_item = batch.item("幽霊").unwrap();
    assert!(ambiguous_item.request.selection.is_none());
    assert!(ambiguous_item.current_candidate_sha256.is_none());
    assert!(batch.item_token("幽霊").is_err());
    assert!(
        batch
            .select_candidate("幽霊", 999, "https://jpdb.io/vocabulary/999/幽霊/ゆうれい",)
            .is_err()
    );
    batch
        .select_candidate("幽霊", 123, "https://jpdb.io/vocabulary/123/幽霊/ゆうれい")
        .unwrap();
    assert_eq!(batch.item("幽霊").unwrap().generation, 1);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_eq!(
        batch.item("幽霊").unwrap().request.selection,
        Some(
            JpdbPitchSelection::new(123, "https://jpdb.io/vocabulary/123/幽霊/ゆうれい",).unwrap()
        )
    );
    assert_eq!(batch.original_plan, immutable_plan);
    let mut tampered_plan_identity = batch.clone();
    tampered_plan_identity.original_plan.requests[0]
        .query
        .reading = None;
    assert!(tampered_plan_identity.validate().is_err());
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn no_pitch_is_a_typed_terminal_outcome_without_canonical_cache() {
    let root = temp_store();
    let mut batch = batch("no-pitch-terminal", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    let evidence = JpdbPitchAbsenceEvidence {
        surface: "幽霊".into(),
        reading: "ゆうれい".into(),
        jpdb_vocabulary_id: 123,
        source_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "ゆうれい".into(),
        }],
        section_inventory: vec!["Meanings".into(), "Forms".into()],
        base_page_contract_valid: true,
        pitch_section_present: false,
        pitch_marker_count: 0,
        browser: browser(),
    };
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::NoPitchAccentOnSource { evidence },
            )
            .unwrap()
    );
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::NoPitchAccentOnSource);
    assert!(batch.is_resolved());
    assert!(item.canonical_sha256.is_none());
    assert!(item.published_sha256.is_none());
    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vocabulary_not_found_is_preserved_as_a_typed_non_acquired_outcome() {
    let root = temp_store();
    let mut batch = batch("vocabulary-not-found", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::VocabularyNotFound {
                    surface: "幽霊".into(),
                    reading: Some("ゆうれい".into()),
                },
            )
            .unwrap()
    );

    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::VocabularyNotFound);
    assert!(!batch.is_resolved());
    assert!(item.request.selection.is_none());
    assert!(item.current_candidate_sha256.is_none());
    assert!(item.canonical_sha256.is_none());
    assert!(matches!(
        item.current_outcome(),
        Some(crate::pitch_batch::PitchBatchOutcome::VocabularyNotFound {
            surface,
            reading: Some(reading),
        }) if surface == "幽霊" && reading == "ゆうれい"
    ));

    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn absence_evidence_must_match_explicit_vocabulary_id_and_detail_route() {
    let selection_url = "https://jpdb.io/vocabulary/123/幽霊/ゆうれい";
    let selection = JpdbPitchSelection::new(123, selection_url).unwrap();
    let request = JpdbPitchRequest::with_selection(
        JpdbPitchQuery::new("幽霊", Some("ゆうれい".into())),
        selection,
    );
    let evidence = |vocabulary_id: u64, source_url: &str| JpdbPitchAbsenceEvidence {
        surface: "幽霊".into(),
        reading: "ゆうれい".into(),
        jpdb_vocabulary_id: vocabulary_id,
        source_url: source_url.into(),
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "ゆうれい".into(),
        }],
        section_inventory: vec!["Word details".into()],
        base_page_contract_valid: true,
        pitch_section_present: false,
        pitch_marker_count: 0,
        browser: browser(),
    };
    let accepted = |batch_id: &str, evidence| {
        let root = temp_store();
        let batch = PitchAccentBatch::new(
            batch_id,
            vec![request.clone()],
            PitchAccentImageValidator::validator_identity(),
        )
        .unwrap();
        let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
        let token = batch.item_token("幽霊").unwrap();
        let result = runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::NoPitchAccentOnSource { evidence },
            )
            .is_ok_and(|recorded| recorded);
        drop(runtime);
        fs::remove_dir_all(root).unwrap();
        result
    };

    assert!(accepted(
        "absence-selection-match",
        evidence(123, selection_url)
    ));
    assert!(!accepted(
        "absence-selection-wrong-id",
        evidence(124, "https://jpdb.io/vocabulary/124/幽霊/ゆうれい")
    ));
    assert!(!accepted(
        "absence-selection-wrong-route",
        evidence(123, "https://jpdb.io/vocabulary/123/幽霊/ゆうれい-variant")
    ));
}

#[test]
fn acquired_png_reopens_verifies_and_rejects_stale_item_token() {
    let root = temp_store();
    let mut batch = batch("durable-candidate", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    let sha = record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    assert!(
        !runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::Capture,
                        diagnostic: None,
                    },
                },
            )
            .unwrap()
    );
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::AcquiredVerified
    );
    drop(runtime);

    let mut reopened = PitchAccentBatchRuntime::open(&root, "durable-candidate").unwrap();
    let batch = reopened.load().unwrap().unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::AcquiredVerified);
    assert!(batch.item_token("幽霊").is_err());
    let candidate = item.candidate(&sha).unwrap();
    let bytes = reopened.read_candidate(&batch, candidate).unwrap();
    assert_eq!(sha256_hex(&bytes), sha);
    assert_eq!(bytes, png(false));
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn publication_reconcile_recovers_crash_after_owner_publish_before_final_state_save() {
    let root = temp_store();
    let mut batch = batch("publication-recovery", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let sha = record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    batch.begin_publication("幽霊", &sha, None).unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);

    // Публикация owner завершилась, затем процесс упал до обновления state.json.
    let owner_record = verified_record("幽霊", false);
    assert_eq!(owner_record.sha256, sha);
    let snapshot = PitchBatchOwnerSnapshot::from_records(vec![owner_record]).unwrap();
    let mut reopened = PitchAccentBatchRuntime::open(&root, "publication-recovery").unwrap();
    let mut recovered = reopened.load().unwrap().unwrap();
    assert_eq!(
        recovered.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::PublicationPending
    );
    recovered.reconcile_owner(&snapshot).unwrap();
    assert_eq!(
        recovered.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    let revision = recovered.revision;
    recovered.reconcile_owner(&snapshot).unwrap();
    assert_eq!(
        recovered.revision, revision,
        "повторный reconcile идемпотентен"
    );
    reopened.save(&recovered).unwrap();
    drop(reopened);

    let mut final_open = PitchAccentBatchRuntime::open(&root, "publication-recovery").unwrap();
    let final_state = final_open.load().unwrap().unwrap();
    let item = final_state.item("幽霊").unwrap();
    assert_eq!(item.published_sha256.as_deref(), Some(sha.as_str()));
    assert_eq!(item.canonical_sha256.as_deref(), Some(sha.as_str()));
    assert!(final_state.is_resolved());
    drop(final_open);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn same_sha_in_new_generation_publishes_metadata_from_exact_current_attempt() {
    let root = temp_store();
    let bytes = png(false);
    let metadata_a = metadata("幽霊", "ゆうれい", 123);
    let metadata_b = metadata("幽霊", "ゆうれい", 456);
    let mut batch = batch("same-sha-attempt-identity", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();

    let sha_a = record_provider_outcome(
        &mut runtime,
        &mut batch,
        "幽霊",
        acquired_with_metadata(bytes.clone(), metadata_a.clone()),
    );
    let owner_a = verified_record_with_metadata("幽霊", bytes.clone(), metadata_a.clone());
    batch.begin_publication("幽霊", &sha_a, None).unwrap();
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![owner_a.clone()]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    runtime.save(&batch).unwrap();

    batch
        .reacquire(
            "幽霊",
            "refresh same bytes with current JPDB evidence".into(),
        )
        .unwrap();
    runtime.save(&batch).unwrap();
    let sha_b = record_provider_outcome(
        &mut runtime,
        &mut batch,
        "幽霊",
        acquired_with_metadata(bytes.clone(), metadata_b.clone()),
    );
    assert_eq!(sha_a, sha_b);
    assert_eq!(
        batch.item("幽霊").unwrap().current_candidate_attempt_index,
        Some(2)
    );

    batch
        .begin_publication("幽霊", &sha_b, Some(sha_a.clone()))
        .unwrap();
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![owner_a]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::PublicationPending,
        "старый metadata A с тем же SHA не должен завершить новую публикацию"
    );
    let publication = batch.item("幽霊").unwrap().publication.as_ref().unwrap();
    let current = batch
        .item("幽霊")
        .unwrap()
        .candidate_at(
            publication.candidate_attempt_index,
            &publication.candidate_sha256,
        )
        .unwrap();
    assert_eq!(current.metadata, metadata_b);

    let owner_b = verified_record_with_metadata("幽霊", bytes, metadata_b.clone());
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![owner_b.clone()]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    let publication = batch.item("幽霊").unwrap().publication.as_ref().unwrap();
    let exact_candidate = batch
        .item("幽霊")
        .unwrap()
        .candidate_at(
            publication.candidate_attempt_index,
            &publication.candidate_sha256,
        )
        .unwrap();
    assert_eq!(exact_candidate.metadata, metadata_b);
    batch.validate().unwrap();

    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn owner_sha_drift_after_refresh_blocks_stale_candidate_publication() {
    let root = temp_store();
    let mut batch = batch("refresh-cas-drift", "幽霊", Some("ゆうれい"));
    let old_owner = rejected_owner(verified_record("幽霊", false));
    let old_sha = old_owner.sha256.clone();
    batch.observe_owner(&old_owner).unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Conflict
    );
    batch
        .reacquire("幽霊", "refresh stale owner".into())
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .refresh_expected_sha256
            .as_deref(),
        Some(old_sha.as_str())
    );

    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let sha = record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", true));
    assert_ne!(sha, old_sha);

    let changed_owner = verified_record("幽霊", true);
    let changed_sha = changed_owner.sha256.clone();
    let snapshot = PitchBatchOwnerSnapshot::from_records(vec![changed_owner]).unwrap();
    batch.reconcile_owner(&snapshot).unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(
        item.owner_current_sha256.as_deref(),
        Some(changed_sha.as_str())
    );
    assert_eq!(
        item.refresh_expected_sha256.as_deref(),
        Some(old_sha.as_str())
    );
    assert_eq!(item.status(), PitchBatchItemStatus::Conflict);
    assert!(
        batch
            .begin_publication("幽霊", &sha, Some(old_sha))
            .is_err()
    );

    let adopted_reason = "review current owner SHA after external replacement";
    batch.reacquire("幽霊", adopted_reason.into()).unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .refresh_expected_sha256
            .as_deref(),
        Some(changed_sha.as_str())
    );
    assert_eq!(
        batch.item("幽霊").unwrap().last_action_reason.as_deref(),
        Some(adopted_reason)
    );
    runtime.save(&batch).unwrap();
    let replacement_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    assert_ne!(replacement_sha, changed_sha);
    batch
        .begin_publication("幽霊", &replacement_sha, Some(changed_sha.clone()))
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .publication
            .as_ref()
            .unwrap()
            .expected_previous_sha256
            .as_deref(),
        Some(changed_sha.as_str())
    );
    let replacement_owner = verified_record("幽霊", false);
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![replacement_owner]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    batch.validate().unwrap();

    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn publication_owner_drift_requires_reasoned_reacquire_with_new_cas_baseline() {
    let root = temp_store();
    let mut batch = batch("publication-cas-drift", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let candidate_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    batch
        .begin_publication("幽霊", &candidate_sha, None)
        .unwrap();

    let external_owner = verified_record("幽霊", true);
    let external_sha = external_owner.sha256.clone();
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![external_owner]).unwrap())
        .unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Conflict);
    assert_eq!(
        item.owner_conflict
            .as_ref()
            .map(|conflict| conflict.code.as_str()),
        Some("refresh_owner_drift")
    );
    assert_eq!(
        item.publication
            .as_ref()
            .map(|publication| publication.status),
        Some(crate::pitch_batch::PitchBatchPublicationStatus::Conflict)
    );

    let reason = "adopt fresh owner SHA after concurrent publication";
    batch.reacquire("幽霊", reason.into()).unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .refresh_expected_sha256
            .as_deref(),
        Some(external_sha.as_str())
    );
    runtime.save(&batch).unwrap();
    let reacquired_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    assert_eq!(reacquired_sha, candidate_sha);
    batch
        .begin_publication("幽霊", &reacquired_sha, Some(external_sha.clone()))
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .publication
            .as_ref()
            .unwrap()
            .candidate_attempt_index,
        2
    );
    let published_owner = verified_record("幽霊", false);
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![published_owner]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    assert_eq!(
        batch.item("幽霊").unwrap().last_action_reason.as_deref(),
        Some(reason)
    );
    batch.validate().unwrap();

    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn published_exact_sha_rejection_quarantines_and_reacquires_with_observed_cas() {
    let root = temp_store();
    let mut batch = batch("reject-published", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(&root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let published_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    batch
        .begin_publication("幽霊", &published_sha, None)
        .unwrap();
    let verified = verified_record("幽霊", false);
    let snapshot = PitchBatchOwnerSnapshot::from_records(vec![verified]).unwrap();
    batch.reconcile_owner(&snapshot).unwrap();
    runtime.save(&batch).unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );

    batch
        .reject_candidate("幽霊", &published_sha, "visual mismatch".into())
        .unwrap();
    let rejected_item = batch.item("幽霊").unwrap();
    assert_eq!(rejected_item.status(), PitchBatchItemStatus::Conflict);
    assert!(rejected_item.publication.is_none());
    assert_eq!(rejected_item.publication_history.len(), 1);
    assert_eq!(
        rejected_item.published_sha256.as_deref(),
        Some(published_sha.as_str())
    );
    batch.validate().unwrap();

    let quarantined = rejected_owner(verified_record("幽霊", false));
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![quarantined]).unwrap())
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .owner_conflict
            .as_ref()
            .unwrap()
            .code,
        "owner_rejected"
    );
    batch
        .reacquire("幽霊", "replace rejected exact SHA".into())
        .unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Pending);
    assert_eq!(
        item.refresh_expected_sha256.as_deref(),
        Some(published_sha.as_str())
    );
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(published_sha.as_str())
    );

    runtime.save(&batch).unwrap();
    let token = batch.item_token("幽霊").unwrap();
    let replacement_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", true));
    assert_ne!(replacement_sha, published_sha);
    batch
        .begin_publication("幽霊", &replacement_sha, Some(published_sha.clone()))
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .publication
            .as_ref()
            .unwrap()
            .expected_previous_sha256,
        Some(published_sha.clone())
    );
    assert_eq!(batch.item("幽霊").unwrap().publication_history.len(), 1);
    assert_eq!(
        batch.item("幽霊").unwrap().published_sha256.as_deref(),
        Some(published_sha.as_str())
    );
    assert_eq!(
        batch.item("幽霊").unwrap().item_revision,
        token.item_revision + 2
    );
    batch.validate().unwrap();

    drop(runtime);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn current_verified_owner_is_reused_without_acquisition() {
    let record = verified_record("幽霊", false);
    let mut batch = batch("fast-reuse", "幽霊", Some("ゆうれい"));
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![record.clone()]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::ExistingVerified
    );
    assert_eq!(
        batch.item("幽霊").unwrap().canonical_sha256.as_deref(),
        Some(record.sha256.as_str())
    );
    assert!(batch.item_token("幽霊").is_err());
}
