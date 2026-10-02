use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    CorpusCommand, OutputFormat, PitchBatchCommand, PitchCli, PitchCommand, PitchPlanItem,
    StoreSummary, create_batch, execute, load_batch, reject_batch, run_batch,
    validate_store_boundary,
};
use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAcquired, JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchQuery, JpdbPitchRequest,
    JpdbPitchStage, JpdbVocabularyCandidate,
};
use crate::model::{AssetIdentity, HumanDecision, LifecycleState, Provenance, SemanticStatus};
use crate::pitch_accent::{
    PitchAccentCaptureRect, PitchAccentCoordinateSpace, PitchAccentDarkThemeProof,
    PitchAccentDomainMetadata, PitchAccentDomainPolicy, PitchAccentEvidence,
    PitchAccentGraphEvidence, PitchAccentImageValidator, PitchAccentProvider,
    PitchAccentRenderEvidence, PitchAccentRenderKind, PitchAccentResolvedForm,
};
use crate::pitch_batch::{
    PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchItemStatus, PitchBatchOwnerSnapshot,
};
use crate::store::{AssetStore, StoreOptions, VerifiedIngestRequest};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "asset-store-pitch-cli-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn browser() -> BrowserRuntimeProvenance {
    BrowserRuntimeProvenance {
        product: "Chrome/140".into(),
        protocol_version: "1.3".into(),
        revision: "1234567".into(),
        user_agent: "Mozilla/5.0 Chrome/140".into(),
        js_version: "V8 14.0".into(),
        executable_source: BrowserExecutableSource::PathLookup,
    }
}

fn metadata(surface: &str, reading: &str, vocabulary_id: u64) -> PitchAccentDomainMetadata {
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
            source_url: format!("https://jpdb.io/vocabulary/{vocabulary_id}/{surface}/{reading}"),
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: surface.into(),
                reading: reading.into(),
            }],
            graph_count: 1,
            render: PitchAccentRenderEvidence {
                kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                selector: ".pitch-graph".into(),
                graphs: vec![PitchAccentGraphEvidence {
                    index: 0,
                    selector: ".pitch-graph".into(),
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
                    computed_color_scheme: "normal".into(),
                    background_selector: "body".into(),
                    background_rgb: [24, 36, 48],
                },
                graph_union_rect: rect,
                capture_rect: rect,
            },
            browser: browser(),
        },
    }
}

fn png() -> Vec<u8> {
    let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([24, 36, 48, 255]));
    for x in 10..50 {
        image.put_pixel(x, 15, image::Rgba([235, 235, 235, 255]));
    }
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn store_at(root: &std::path::Path) -> AssetStore {
    AssetStore::open_with_policy(
        StoreOptions::new(root.join("store")),
        PitchAccentDomainPolicy,
    )
    .unwrap()
}

fn save_durable_candidate(
    store: &AssetStore,
    batch_id: &str,
    surface: &str,
) -> (Vec<u8>, PitchAccentDomainMetadata, String) {
    let reading = "ゆうれい";
    let bytes = png();
    let metadata = metadata(surface, reading, 123);
    let mut batch = PitchAccentBatch::new(
        batch_id,
        vec![JpdbPitchRequest::new(JpdbPitchQuery::new(
            surface,
            Some(reading.into()),
        ))],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &batch).unwrap();
    let token = batch.item_token(surface).unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Acquired {
                    asset: Box::new(JpdbPitchAcquired {
                        bytes: bytes.clone(),
                        metadata: metadata.clone(),
                    }),
                },
            )
            .unwrap()
    );
    let sha256 = sha256_hex(&bytes);
    drop(runtime);
    (bytes, metadata, sha256)
}

#[tokio::test]
async fn run_resumes_a_durable_candidate_without_reacquisition_and_resolved_rerun_is_noop() {
    let root = temp_root();
    let store = store_at(&root);
    let (bytes, _, expected_sha) = save_durable_candidate(&store, "candidate-resume", "幽霊");

    // Байты кандидата и состояние сохранены до этого запуска, как если бы
    // предыдущий процесс остановился перед публикацией. Resume должен опубликовать
    // их из runtime blob.
    let (batch, changed) = run_batch(&store, "candidate-resume").await.unwrap();
    assert!(changed);
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Published);
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(expected_sha.as_str())
    );

    let assets = store.verify_integrity().unwrap();
    let canonical = assets
        .iter()
        .find(|asset| asset.identity.key == "幽霊")
        .unwrap();
    assert_eq!(canonical.sha256, expected_sha);
    assert_eq!(canonical.storage_path, "assets/png/幽霊.png");
    assert_eq!(canonical.consumer_filename, "幽霊.pitch.png");
    assert_eq!(
        fs::read(store.root().join(&canonical.storage_path)).unwrap(),
        bytes
    );

    // Второй запуск видит уже подтверждённую текущую запись владельца, не открывает браузер
    // и не добавляет новую попытку acquisition.
    let (rerun, changed) = run_batch(&store, "candidate-resume").await.unwrap();
    assert!(!changed);
    assert_eq!(rerun.item("幽霊").unwrap().attempts.len(), 1);
    assert_eq!(
        rerun.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn run_reconciles_a_store_commit_after_restart_without_reacquisition() {
    let root = temp_root();
    let store = store_at(&root);
    let (bytes, metadata, expected_sha) =
        save_durable_candidate(&store, "owner-commit-resume", "幽霊");

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "owner-commit-resume").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .begin_publication("幽霊", &expected_sha, None)
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);

    // Имитируем сбой после commit owner store и до финального обновления batch state.
    let identity = AssetIdentity::new("pitch_accent", "幽霊").unwrap();
    let result = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity,
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_render".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(&metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    assert_eq!(result.status, SemanticStatus::Verified);
    assert_eq!(result.sha256, expected_sha);

    let (batch, _changed) = run_batch(&store, "owner-commit-resume").await.unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Published);
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(expected_sha.as_str())
    );
    assert_eq!(store.verify_integrity().unwrap().len(), 1);

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn reject_command_attests_the_exact_current_owner_sha_and_quarantines_it() {
    let root = temp_root();
    let store = store_at(&root);
    let (bytes, metadata, expected_sha) =
        save_durable_candidate(&store, "reject-owner-sha", "幽霊");

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "reject-owner-sha").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .begin_publication("幽霊", &expected_sha, None)
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes,
                provenance: Provenance {
                    source_kind: "jpdb_browser_render".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "reject-owner-sha").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .reconcile_owner(
            &PitchBatchOwnerSnapshot::from_records(store.verify_integrity().unwrap()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    runtime.save(&batch).unwrap();
    drop(runtime);

    let summary = StoreSummary {
        path: store.root().display().to_string(),
        store_id: store.store_id().to_owned(),
    };
    let response = reject_batch(
        &store,
        "reject-owner-sha",
        "幽霊",
        &expected_sha,
        "visual inspection rejected this exact image".into(),
        summary,
        OutputFormat::Json,
    );
    assert_eq!(response.exit_code, 0);

    let records = store.verify_integrity().unwrap();
    let owner = records
        .iter()
        .find(|record| record.identity.key == "幽霊")
        .unwrap();
    assert_eq!(owner.sha256, expected_sha);
    assert_eq!(owner.lifecycle, LifecycleState::Quarantined);
    assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));

    let batch = load_batch(&store, "reject-owner-sha").unwrap();
    let item = batch.item("幽霊").unwrap();
    assert!(
        item.rejected_candidates
            .iter()
            .any(|rejection| rejection.candidate_sha256 == expected_sha)
    );
    assert_eq!(item.status(), PitchBatchItemStatus::Conflict);
    assert_eq!(item.owner_conflict.as_ref().unwrap().code, "owner_rejected");

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

fn cli(
    store: PathBuf,
    repository_root: PathBuf,
    output: OutputFormat,
    command: PitchCommand,
) -> PitchCli {
    PitchCli {
        store: Some(store),
        repository_root,
        output,
        command,
    }
}

#[tokio::test]
async fn pitch_cli_routes_human_errors_to_stderr_and_json_errors_to_stdout() {
    let root = temp_root();
    let store = store_at(&root);
    let store_root = store.root().to_path_buf();
    drop(store);

    let success = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Human,
        PitchCommand::Corpus {
            command: CorpusCommand::List,
        },
    ))
    .await;
    assert_eq!(success.exit_code, 0);
    assert!(success.stdout.contains("Операция: список корпуса"));
    assert!(success.stderr.is_empty());

    let human_status = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "missing-status".into(),
            },
        },
    ))
    .await;
    assert_ne!(human_status.exit_code, 0);
    assert!(human_status.stdout.is_empty());
    assert!(
        human_status
            .stderr
            .contains("сохранённое состояние batch не найдено")
    );

    let human_run = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Run {
                batch_id: "missing-run".into(),
            },
        },
    ))
    .await;
    assert_ne!(human_run.exit_code, 0);
    assert!(human_run.stdout.is_empty());
    assert!(
        human_run
            .stderr
            .contains("сохранённое состояние batch не найдено")
    );

    let json_error = execute(cli(
        store_root,
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "missing-json".into(),
            },
        },
    ))
    .await;
    assert_eq!(json_error.exit_code, human_status.exit_code);
    assert!(json_error.stderr.is_empty());
    let response: serde_json::Value = serde_json::from_str(&json_error.stdout).unwrap();
    assert_eq!(response["outcome"], "failed");
    assert_eq!(
        response["error"]["message"],
        "сохранённое состояние batch не найдено"
    );

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn human_status_explains_technical_failure_ambiguity_and_selection() {
    let root = temp_root();
    let store = store_at(&root);
    let store_root = store.root().to_path_buf();
    let validator = PitchAccentImageValidator::validator_identity();

    let request = JpdbPitchRequest::new(JpdbPitchQuery::new("幽霊", Some("ゆうれい".into())));
    let mut failed_batch =
        PitchAccentBatch::new("human-failure", vec![request.clone()], validator.clone()).unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &failed_batch).unwrap();
    let token = failed_batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut failed_batch,
            &token,
            JpdbPitchOutcome::Failed {
                error: JpdbPitchFailure::Navigation {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "net::ERR_NAME_RESOLUTION_FAILED".into(),
                },
            },
        )
        .unwrap();
    drop(runtime);

    let failure_output = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "human-failure".into(),
            },
        },
    ))
    .await;
    assert!(failure_output.stdout.contains("техническая ошибка"));
    assert!(failure_output.stdout.contains("переход к поиску"));
    assert!(failure_output.stdout.contains("ERR_NAME_RESOLUTION_FAILED"));
    assert!(failure_output.stdout.contains("повтор допустим"));

    let candidates = vec![
        JpdbVocabularyCandidate {
            vocabulary_id: 123,
            surface_forms: vec!["幽霊".into()],
            readings: vec!["ゆうれい".into()],
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            part_of_speech: vec!["Noun".into()],
            meanings: vec!["ghost".into()],
            detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        },
        JpdbVocabularyCandidate {
            vocabulary_id: 124,
            surface_forms: vec!["幽霊".into()],
            readings: vec!["ゆうれい".into()],
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            part_of_speech: vec!["Noun".into()],
            meanings: vec!["phantom".into()],
            detail_url: "https://jpdb.io/vocabulary/124/幽霊/ゆうれい".into(),
        },
    ];
    let mut ambiguous_batch =
        PitchAccentBatch::new("human-ambiguity", vec![request], validator).unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &ambiguous_batch).unwrap();
    let token = ambiguous_batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut ambiguous_batch,
            &token,
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                candidates,
            },
        )
        .unwrap();
    drop(runtime);

    let ambiguity_output = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "human-ambiguity".into(),
            },
        },
    ))
    .await;
    assert!(
        ambiguity_output
            .stdout
            .contains("Требуется выбрать точную запись JPDB")
    );
    assert!(ambiguity_output.stdout.contains("ID 123"));
    assert!(ambiguity_output.stdout.contains("ID 124"));
    assert!(ambiguity_output.stdout.contains("ghost"));

    let selection_output = execute(cli(
        store_root,
        root.clone(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Select {
                batch_id: "human-ambiguity".into(),
                surface: "幽霊".into(),
                vocabulary_id: 123,
                detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
            },
        },
    ))
    .await;
    assert!(
        selection_output
            .stdout
            .contains("Выбранная запись JPDB: ID 123")
    );
    assert!(
        selection_output
            .stdout
            .contains("https://jpdb.io/vocabulary/123/幽霊/ゆうれい")
    );

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn batch_start_noop_and_status_reconcile_report_persistent_changes() {
    let root = temp_root();
    let store = store_at(&root);
    let store_root = store.root().to_path_buf();
    drop(store);
    let plan_path = root.join("plan.json");
    fs::write(
        &plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい"}]}"#,
    )
    .unwrap();

    let start = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("changed-contract".into()),
                plan: plan_path.clone(),
            },
        },
    ))
    .await;
    let start_response: serde_json::Value = serde_json::from_str(&start.stdout).unwrap();
    assert_eq!(start_response["changed"], true);

    let repeated_start = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("changed-contract".into()),
                plan: plan_path,
            },
        },
    ))
    .await;
    let repeated_response: serde_json::Value =
        serde_json::from_str(&repeated_start.stdout).unwrap();
    assert_eq!(repeated_response["changed"], false);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let bytes = png();
    let metadata = metadata("幽霊", "ゆうれい", 123);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes,
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    drop(store);

    let status = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "changed-contract".into(),
            },
        },
    ))
    .await;
    let status_response: serde_json::Value = serde_json::from_str(&status.stdout).unwrap();
    assert_eq!(status_response["changed"], true);
    assert_eq!(status_response["items"][0]["status"], "existing_verified");
    assert_eq!(
        status_response["batch"]["items"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let repeated_status = execute(cli(
        store_root,
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "changed-contract".into(),
            },
        },
    ))
    .await;
    let repeated_status_response: serde_json::Value =
        serde_json::from_str(&repeated_status.stdout).unwrap();
    assert_eq!(repeated_status_response["changed"], false);

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ensure_reports_each_new_runtime_batch_as_a_change_for_verified_canonical_items() {
    let root = temp_root();
    let store = store_at(&root);
    let store_root = store.root().to_path_buf();
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: png(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(
                    serde_json::to_value(metadata("幽霊", "ゆうれい", 123)).unwrap(),
                ),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    drop(store);

    let ensure = || PitchCommand::Ensure {
        plan: None,
        surface: Some("幽霊".into()),
        reading: Some("ゆうれい".into()),
        vocabulary_id: None,
        detail_url: None,
        refresh: false,
    };
    let first = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        ensure(),
    ))
    .await;
    let first_response: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(first_response["changed"], true);
    assert_eq!(first_response["items"][0]["status"], "existing_verified");

    let second = execute(cli(store_root, root.clone(), OutputFormat::Json, ensure())).await;
    let second_response: serde_json::Value = serde_json::from_str(&second.stdout).unwrap();
    assert_eq!(second_response["changed"], true);
    assert_eq!(second_response["items"][0]["status"], "existing_verified");
    assert_ne!(first_response["batch_id"], second_response["batch_id"]);

    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cli_reacquire_publishes_current_metadata_when_png_sha_repeats() {
    let root = temp_root();
    let store = store_at(&root);
    let store_root = store.root().to_path_buf();
    let bytes = png();
    let expected_sha = sha256_hex(&bytes);
    let original_metadata = metadata("幽霊", "ゆうれい", 123);
    let current_metadata = metadata("幽霊", "ゆうれい", 456);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(&original_metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    let item = PitchPlanItem {
        surface: "幽霊".into(),
        reading: Some("ゆうれい".into()),
        selection: None,
    };
    let (batch, created) = create_batch(&store, "same-sha-metadata", &[item], None).unwrap();
    assert!(created);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::ExistingVerified
    );
    drop(store);

    let reacquire = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Reacquire {
                batch_id: "same-sha-metadata".into(),
                surface: "幽霊".into(),
                reason: "повторная проверка источника".into(),
            },
        },
    ))
    .await;
    assert_eq!(reacquire.exit_code, 0);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "same-sha-metadata").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Acquired {
                    asset: Box::new(JpdbPitchAcquired {
                        bytes: bytes.clone(),
                        metadata: current_metadata.clone(),
                    }),
                },
            )
            .unwrap()
    );
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .current_candidate_sha256
            .as_deref(),
        Some(expected_sha.as_str())
    );
    drop(runtime);
    drop(store);

    let run = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Run {
                batch_id: "same-sha-metadata".into(),
            },
        },
    ))
    .await;
    assert_eq!(run.exit_code, 0, "{}", run.stdout);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let canonical = store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity.key == "幽霊")
        .unwrap();
    assert_eq!(canonical.sha256, expected_sha);
    assert_eq!(
        canonical.domain_metadata,
        Some(serde_json::to_value(&current_metadata).unwrap())
    );

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

fn ambiguity_candidates() -> Vec<JpdbVocabularyCandidate> {
    [(123, "ghost"), (124, "phantom")]
        .into_iter()
        .map(|(vocabulary_id, meaning)| JpdbVocabularyCandidate {
            vocabulary_id,
            surface_forms: vec!["幽霊".into()],
            readings: vec!["ゆうれい".into()],
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            part_of_speech: vec!["Noun".into()],
            meanings: vec![meaning.into()],
            detail_url: format!("https://jpdb.io/vocabulary/{vocabulary_id}/幽霊/ゆうれい"),
        })
        .collect()
}

#[tokio::test]
async fn batch_start_uses_immutable_original_plan_identity_after_selection() {
    let root = temp_root();
    let store = store_at(&root);
    let store_root = store.root().to_path_buf();
    drop(store);
    let original_plan_path = root.join("original-plan.json");
    let history_only_plan_path = root.join("history-only-plan.json");
    fs::write(
        &original_plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい"}]}"#,
    )
    .unwrap();
    fs::write(
        &history_only_plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい","selection":{"vocabulary_id":123,"detail_url":"https://jpdb.io/vocabulary/123/幽霊/ゆうれい"}}]}"#,
    )
    .unwrap();

    let start = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("immutable-plan".into()),
                plan: original_plan_path.clone(),
            },
        },
    ))
    .await;
    assert_eq!(start.exit_code, 0, "{}", start.stdout);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "immutable-plan").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut batch,
            &token,
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                candidates: ambiguity_candidates(),
            },
        )
        .unwrap();
    drop(runtime);
    drop(store);

    let first_selection = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Select {
                batch_id: "immutable-plan".into(),
                surface: "幽霊".into(),
                vocabulary_id: 123,
                detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
            },
        },
    ))
    .await;
    assert_eq!(first_selection.exit_code, 0, "{}", first_selection.stdout);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "immutable-plan").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut batch,
            &token,
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                candidates: ambiguity_candidates(),
            },
        )
        .unwrap();
    drop(runtime);
    drop(store);

    let second_selection = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Select {
                batch_id: "immutable-plan".into(),
                surface: "幽霊".into(),
                vocabulary_id: 124,
                detail_url: "https://jpdb.io/vocabulary/124/幽霊/ゆうれい".into(),
            },
        },
    ))
    .await;
    assert_eq!(second_selection.exit_code, 0, "{}", second_selection.stdout);

    let repeated_original = execute(cli(
        store_root.clone(),
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("immutable-plan".into()),
                plan: original_plan_path,
            },
        },
    ))
    .await;
    assert_eq!(
        repeated_original.exit_code, 0,
        "{}",
        repeated_original.stdout
    );

    let history_only = execute(cli(
        store_root,
        root.clone(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("immutable-plan".into()),
                plan: history_only_plan_path,
            },
        },
    ))
    .await;
    assert_ne!(history_only.exit_code, 0);
    let response: serde_json::Value = serde_json::from_str(&history_only.stdout).unwrap();
    assert_eq!(response["error"]["code"], "identity_conflict");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pitch_store_boundary_rejects_decks_overlap_and_accepts_external_store() {
    let root = temp_root();
    let repository = root.join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(&decks).unwrap();

    assert!(validate_store_boundary(&decks.join("pitch-store"), &repository).is_err());
    assert!(validate_store_boundary(&repository, &repository).is_err());
    assert!(validate_store_boundary(&root.join("outside-store"), &repository).is_ok());

    let hostile_repository_root = decks.join("nested-checkout");
    fs::create_dir_all(&hostile_repository_root).unwrap();
    assert!(
        validate_store_boundary(&root.join("safe-looking-store"), &hostile_repository_root)
            .is_err()
    );
    assert!(
        validate_store_boundary(&root.join("outside-store"), &root.join("missing-checkout"))
            .is_err()
    );
    assert!(validate_store_boundary(&decks.join("hidden-store"), &root).is_err());

    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn pitch_store_boundary_rejects_symlink_alias_into_decks() {
    use std::os::unix::fs::symlink;

    let root = temp_root();
    let repository = root.join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(&decks).unwrap();
    let alias = root.join("decks-alias");
    symlink(&decks, &alias).unwrap();

    assert!(validate_store_boundary(&alias.join("pitch-store"), &repository).is_err());

    fs::remove_dir_all(root).unwrap();
}
