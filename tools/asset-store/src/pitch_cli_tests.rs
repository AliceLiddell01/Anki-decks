use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

use super::{
    CorpusCommand, OutputFormat, PitchBatchCommand, PitchCli, PitchCommand, PitchPlanItem,
    StoreSummary, create_batch, execute, load_batch, reject_batch, run_batch,
    temp_workspace_cleanup_error, validate_store_boundary,
};
use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use crate::error::{AssetError, ErrorCode};
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
use crate::temp_workspace::TempWorkspace;

#[test]
fn human_output_does_not_append_a_second_diagnostic_log_path() {
    for (stdout, stderr, exit_code) in [("завершено\n", "", 0), ("", "ошибка\n", 1)]
    {
        let output = super::attach_diagnostic_log(
            super::PitchCliOutput {
                stdout: stdout.into(),
                stderr: stderr.into(),
                exit_code,
            },
            OutputFormat::Human,
            Some("/tmp/diagnostic.jsonl"),
        );

        assert_eq!(output.stdout, stdout);
        assert_eq!(output.stderr, stderr);
    }
}

struct TemporaryRoot {
    workspace: TempWorkspace,
}

impl TemporaryRoot {
    fn new() -> Self {
        Self {
            workspace: TempWorkspace::create("asset-store-pitch-cli-tests").unwrap(),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.workspace.path()
    }
}

#[test]
fn workspace_root_does_not_require_decks_and_supports_git_worktree_file() {
    let workspace = temp_root();
    let root = workspace.path();
    fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(root.join(".git"), "gitdir: /tmp/fixture\n").unwrap();
    let nested = root.join("tools/component");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("Cargo.toml"), "[package]\n").unwrap();
    let canonical = fs::canonicalize(root).unwrap();
    assert_eq!(super::find_workspace_root(&nested), Some(canonical.clone()));
    assert_eq!(
        super::store_path(None, &nested),
        canonical.join(".asset-store/pitch-accent")
    );
    fs::create_dir(root.join("decks")).unwrap();
    assert_eq!(super::find_workspace_root(&nested), Some(canonical));
}

#[test]
fn workspace_cleanup_failure_keeps_a_completed_run_identifiable() {
    let error = temp_workspace_cleanup_error(Ok(()), std::io::Error::other("cleanup denied"));

    assert_eq!(error.code, ErrorCode::IoFailure);
    assert!(error.message.contains("получение завершено"));
    assert_eq!(error.details["run_completed"], true);
    assert_eq!(error.details["original_error"], serde_json::Value::Null);
    assert_eq!(error.details["cleanup_error"], "cleanup denied");
}

#[test]
fn workspace_cleanup_failure_preserves_the_original_error_fields() {
    let original = AssetError::with_details(
        ErrorCode::InvalidIdentity,
        "invalid original identity",
        serde_json::json!({"field":"surface"}),
    );

    let error =
        temp_workspace_cleanup_error::<()>(Err(original), std::io::Error::other("cleanup denied"));

    assert_eq!(error.code, ErrorCode::IoFailure);
    assert_eq!(error.details["run_completed"], false);
    assert_eq!(error.details["original_error"]["code"], "invalid_identity");
    assert_eq!(
        error.details["original_error"]["message"],
        "invalid original identity"
    );
    assert_eq!(
        error.details["original_error"]["details"],
        serde_json::json!({"field":"surface"})
    );
}

fn temp_root() -> TemporaryRoot {
    TemporaryRoot::new()
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
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let (bytes, _, expected_sha) = save_durable_candidate(&store, "candidate-resume", "幽霊");

    // Байты кандидата и состояние сохранены до этого запуска, как если бы
    // предыдущий процесс остановился перед публикацией. Resume должен опубликовать
    // их из runtime blob.
    let (batch, changed) = run_batch(
        &store,
        "candidate-resume",
        OutputFormat::Json,
        "batch_run",
        1,
    )
    .await
    .unwrap();
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
    let (rerun, changed) = run_batch(
        &store,
        "candidate-resume",
        OutputFormat::Json,
        "batch_run",
        1,
    )
    .await
    .unwrap();
    assert!(!changed);
    assert_eq!(rerun.item("幽霊").unwrap().attempts.len(), 1);
    assert_eq!(
        rerun.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );

    drop(store);
}

#[tokio::test]
async fn run_reconciles_a_store_commit_after_restart_without_reacquisition() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let (bytes, metadata, expected_sha) =
        save_durable_candidate(&store, "owner-commit-resume", "幽霊");

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "owner-commit-resume").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .begin_publication("幽霊", &expected_sha, None)
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);

    // Имитируем сбой после фиксации изменения в хранилище и до итогового обновления состояния пакета.
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

    let (batch, _changed) = run_batch(
        &store,
        "owner-commit-resume",
        OutputFormat::Json,
        "batch_run",
        1,
    )
    .await
    .unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Published);
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(expected_sha.as_str())
    );
    assert_eq!(store.verify_integrity().unwrap().len(), 1);

    drop(store);
}

#[test]
fn reject_command_attests_the_exact_current_owner_sha_and_quarantines_it() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
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
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    drop(store);

    let success = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Run {
                batch_id: "missing-run".into(),
                workers: 1,
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
        root.to_path_buf(),
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
}

#[tokio::test]
async fn human_status_explains_technical_failure_ambiguity_and_selection() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        root.to_path_buf(),
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
}

#[tokio::test]
async fn batch_start_noop_and_status_reconcile_report_persistent_changes() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    drop(store);
    let plan_path = root.join("plan.json");
    fs::write(
        &plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい"}]}"#,
    )
    .unwrap();

    let start = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        root.to_path_buf(),
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
}

#[tokio::test]
async fn ensure_reports_each_new_runtime_batch_as_a_change_for_verified_canonical_items() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
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
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        ensure(),
    ))
    .await;
    let first_response: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(first_response["changed"], true);
    assert_eq!(first_response["items"][0]["status"], "existing_verified");

    let second = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Json,
        ensure(),
    ))
    .await;
    let second_response: serde_json::Value = serde_json::from_str(&second.stdout).unwrap();
    assert_eq!(second_response["changed"], true);
    assert_eq!(second_response["items"][0]["status"], "existing_verified");
    assert_ne!(first_response["batch_id"], second_response["batch_id"]);
}

#[tokio::test]
async fn cli_reacquire_publishes_current_metadata_when_png_sha_repeats() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Run {
                batch_id: "same-sha-metadata".into(),
                workers: 1,
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
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        store_root.to_path_buf(),
        root.to_path_buf(),
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
        root.to_path_buf(),
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
}

#[test]
fn pitch_store_boundary_rejects_decks_overlap_and_accepts_external_store() {
    let workspace = temp_root();
    let root = workspace.path();
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
    assert!(validate_store_boundary(&decks.join("hidden-store"), root).is_err());
}

#[cfg(unix)]
#[test]
fn pitch_store_boundary_rejects_symlink_alias_into_decks() {
    use std::os::unix::fs::symlink;

    let workspace = temp_root();

    let root = workspace.path();
    let repository = root.join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(&decks).unwrap();
    let alias = root.join("decks-alias");
    symlink(&decks, &alias).unwrap();

    assert!(validate_store_boundary(&alias.join("pitch-store"), &repository).is_err());
}

// Провайдер и сессия подменяются только на границе CLI; запись пакета,
// token/CAS, проверка кандидата и публикация остаются боевым кодом.
enum ScriptedPitchAction {
    Complete,
    WaitForHeartbeat(futures::channel::oneshot::Receiver<()>),
    InterruptInFlight(futures::channel::oneshot::Sender<()>),
    ReadyWithSignal(futures::channel::oneshot::Sender<()>),
    SessionFailure { after_outcome: bool },
    SetupFailure,
    ItemFailure,
    Reacquire,
}

struct ScriptedPitchDriver {
    store_root: PathBuf,
    batch_id: String,
    actions: std::collections::VecDeque<ScriptedPitchAction>,
    launches: usize,
    fail_launch_on: Option<usize>,
    slow_launch: Option<(usize, futures::channel::oneshot::Receiver<()>)>,
    interrupt_during_launch: Option<futures::channel::oneshot::Sender<()>>,
    active: Option<usize>,
    closed: Vec<usize>,
    seen: Vec<(usize, String)>,
    attempts_at_start: Vec<usize>,
}

impl ScriptedPitchDriver {
    fn new(store: &AssetStore, batch_id: &str, actions: Vec<ScriptedPitchAction>) -> Self {
        Self {
            store_root: store.root().into(),
            batch_id: batch_id.into(),
            actions: actions.into(),
            launches: 0,
            fail_launch_on: None,
            slow_launch: None,
            interrupt_during_launch: None,
            active: None,
            closed: Vec::new(),
            seen: Vec::new(),
            attempts_at_start: Vec::new(),
        }
    }

    fn batch(&self) -> PitchAccentBatch {
        PitchAccentBatchRuntime::open(&self.store_root, &self.batch_id)
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
    }
}

impl super::PitchRunDriver for ScriptedPitchDriver {
    type Session = usize;

    async fn launch(&mut self) -> Result<Self::Session, String> {
        assert!(self.active.is_none(), "сессии не должны пересекаться");
        self.launches += 1;
        if self
            .slow_launch
            .as_ref()
            .is_some_and(|(index, _)| *index == self.launches)
        {
            let (_, permit) = self.slow_launch.take().unwrap();
            permit.await.unwrap();
        }
        if let Some(signal) = self.interrupt_during_launch.take() {
            signal.send(()).unwrap();
            // Дать обработчику увидеть сигнал до создания сессии браузера.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        if self.fail_launch_on == Some(self.launches) {
            return Err("сбой запуска в автономном сценарии".into());
        }
        self.active = Some(self.launches);
        Ok(self.launches)
    }

    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> crate::jpdb::JpdbPitchAcquisitionReport {
        assert_eq!(self.active, Some(*session));
        self.attempts_at_start.push(
            self.batch()
                .items
                .iter()
                .map(|item| item.attempts.len())
                .sum(),
        );
        self.seen.push((*session, request.query.surface.clone()));
        let action = self
            .actions
            .pop_front()
            .expect("запрос вне автономного сценария");
        let mut failure = None;
        match action {
            ScriptedPitchAction::Complete => {}
            ScriptedPitchAction::WaitForHeartbeat(permit) => permit.await.unwrap(),
            ScriptedPitchAction::InterruptInFlight(signal) => {
                signal.send(()).unwrap();
                std::future::pending::<()>().await;
            }
            ScriptedPitchAction::ReadyWithSignal(signal) => signal.send(()).unwrap(),
            ScriptedPitchAction::ItemFailure => {
                return crate::jpdb::JpdbPitchAcquisitionReport {
                    outcomes: vec![JpdbPitchOutcome::Failed {
                        error: JpdbPitchFailure::Timeout {
                            stage: JpdbPitchStage::SearchNavigation,
                            diagnostic: Some("тайм-аут элемента в автономной проверке".into()),
                        },
                    }],
                    session_failure: None,
                };
            }
            ScriptedPitchAction::SetupFailure => {
                return crate::jpdb::JpdbPitchAcquisitionReport {
                    outcomes: Vec::new(),
                    session_failure: Some(JpdbPitchFailure::BrowserConfiguration {
                        stage: JpdbPitchStage::ConfigureBrowser,
                        message: "сбой подготовки страницы в автономной проверке".into(),
                    }),
                };
            }
            ScriptedPitchAction::SessionFailure { after_outcome } => {
                failure = Some(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "остановка телеметрии в автономной проверке".into(),
                });
                if !after_outcome {
                    return crate::jpdb::JpdbPitchAcquisitionReport {
                        outcomes: Vec::new(),
                        session_failure: failure,
                    };
                }
            }
            ScriptedPitchAction::Reacquire => {
                let mut runtime =
                    PitchAccentBatchRuntime::open(&self.store_root, &self.batch_id).unwrap();
                let mut batch = runtime.load().unwrap().unwrap();
                batch
                    .reacquire(&request.query.surface, "новое действие пользователя".into())
                    .unwrap();
                runtime.save(&batch).unwrap();
            }
        }
        crate::jpdb::JpdbPitchAcquisitionReport {
            outcomes: vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: request.query.surface.clone(),
                reading: request.query.reading.clone(),
            }],
            session_failure: failure,
        }
    }

    async fn close(&mut self, session: Self::Session) {
        assert_eq!(self.active.take(), Some(session));
        self.closed.push(session);
    }
}

#[derive(Debug, Clone)]
struct ParallelPitchAttempt {
    worker: u32,
    worker_session: u32,
    session: u32,
    surface: String,
}

#[derive(Default)]
struct ParallelPitchTraceState {
    active: usize,
    max_active: usize,
    next_session: u32,
    starts: Vec<ParallelPitchAttempt>,
    closed: Vec<ParallelPitchAttempt>,
    finished: Vec<u32>,
    workspace_paths: Vec<PathBuf>,
}

#[derive(Clone, Default)]
struct ParallelPitchTrace(Arc<Mutex<ParallelPitchTraceState>>);

impl ParallelPitchTrace {
    fn begin(&self, attempt: ParallelPitchAttempt) -> ActivePitchAcquire {
        let mut state = self.0.lock().unwrap();
        state.active += 1;
        state.max_active = state.max_active.max(state.active);
        state.starts.push(attempt);
        ActivePitchAcquire(self.clone())
    }

    fn launch(&self) -> u32 {
        let mut state = self.0.lock().unwrap();
        state.next_session += 1;
        state.next_session
    }

    fn close(&self, attempt: ParallelPitchAttempt) {
        self.0.lock().unwrap().closed.push(attempt);
    }

    fn finish(&self, worker: u32, workspace: PathBuf) {
        let mut state = self.0.lock().unwrap();
        state.finished.push(worker);
        state.workspace_paths.push(workspace);
    }

    fn snapshot(&self) -> ParallelPitchTraceStateSnapshot {
        let state = self.0.lock().unwrap();
        ParallelPitchTraceStateSnapshot {
            active: state.active,
            max_active: state.max_active,
            starts: state.starts.clone(),
            closed: state.closed.clone(),
            finished: state.finished.clone(),
            workspace_paths: state.workspace_paths.clone(),
        }
    }
}

struct ActivePitchAcquire(ParallelPitchTrace);

impl Drop for ActivePitchAcquire {
    fn drop(&mut self) {
        let mut state = self.0.0.lock().unwrap();
        state.active -= 1;
    }
}

struct ParallelPitchTraceStateSnapshot {
    active: usize,
    max_active: usize,
    starts: Vec<ParallelPitchAttempt>,
    closed: Vec<ParallelPitchAttempt>,
    finished: Vec<u32>,
    workspace_paths: Vec<PathBuf>,
}

enum ParallelPitchAction {
    Complete,
    SessionFailure {
        started: Option<futures::channel::oneshot::Sender<()>>,
    },
    Wait {
        started: Option<futures::channel::oneshot::Sender<()>>,
        release: futures::channel::oneshot::Receiver<()>,
    },
}

struct ParallelPitchSession {
    worker: u32,
    worker_session: u32,
    session: u32,
}

struct ParallelPitchDriver {
    worker: u32,
    worker_session_count: u32,
    active_session: Option<u32>,
    trace: ParallelPitchTrace,
    actions: std::collections::BTreeMap<String, ParallelPitchAction>,
    workspace: Option<TempWorkspace>,
    fail_finish: bool,
}

impl ParallelPitchDriver {
    fn new(
        worker: u32,
        trace: ParallelPitchTrace,
        actions: impl IntoIterator<Item = (String, ParallelPitchAction)>,
    ) -> Self {
        let workspace = TempWorkspace::create("pitch-parallel-synthetic-worker").unwrap();
        Self {
            worker,
            worker_session_count: 0,
            active_session: None,
            trace,
            actions: actions.into_iter().collect(),
            workspace: Some(workspace),
            fail_finish: false,
        }
    }

    fn complete_actions(surfaces: &[&str]) -> Vec<(String, ParallelPitchAction)> {
        surfaces
            .iter()
            .map(|surface| ((*surface).into(), ParallelPitchAction::Complete))
            .collect()
    }
}

impl super::PitchRunDriver for ParallelPitchDriver {
    type Session = ParallelPitchSession;

    async fn launch(&mut self) -> Result<Self::Session, String> {
        assert!(
            self.active_session.is_none(),
            "у worker не должно быть двух сессий"
        );
        self.worker_session_count += 1;
        let worker_session = self.worker_session_count;
        let session = self.trace.launch();
        self.active_session = Some(worker_session);
        Ok(ParallelPitchSession {
            worker: self.worker,
            worker_session,
            session,
        })
    }

    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> crate::jpdb::JpdbPitchAcquisitionReport {
        assert_eq!(self.worker, session.worker);
        assert_eq!(self.active_session, Some(session.worker_session));
        let surface = request.query.surface.clone();
        let _active = self.trace.begin(ParallelPitchAttempt {
            worker: self.worker,
            worker_session: session.worker_session,
            session: session.session,
            surface: surface.clone(),
        });
        let action = self
            .actions
            .remove(&surface)
            .unwrap_or(ParallelPitchAction::Complete);
        let session_failure = match action {
            ParallelPitchAction::Complete => None,
            ParallelPitchAction::SessionFailure { started } => {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                Some(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "synthetic worker session failure".into(),
                })
            }
            ParallelPitchAction::Wait { started, release } => {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                release.await.expect("тест должен освободить запрос worker");
                None
            }
        };
        let outcomes = if session_failure.is_some() {
            Vec::new()
        } else {
            vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: request.query.surface.clone(),
                reading: request.query.reading.clone(),
            }]
        };
        crate::jpdb::JpdbPitchAcquisitionReport {
            outcomes,
            session_failure,
        }
    }

    async fn close(&mut self, session: Self::Session) {
        assert_eq!(self.worker, session.worker);
        assert_eq!(self.active_session.take(), Some(session.worker_session));
        self.trace.close(ParallelPitchAttempt {
            worker: session.worker,
            worker_session: session.worker_session,
            session: session.session,
            surface: String::new(),
        });
    }

    fn finish(&mut self) -> Result<(), String> {
        if self.active_session.is_some() {
            return Err(format!(
                "worker {} завершился с активной сессией",
                self.worker
            ));
        }
        let workspace = self
            .workspace
            .take()
            .expect("synthetic worker workspace закрывается один раз");
        let workspace_path = workspace.path().to_path_buf();
        workspace
            .close()
            .map_err(|error| format!("synthetic worker workspace cleanup: {error}"))?;
        self.trace.finish(self.worker, workspace_path);
        if self.fail_finish {
            Err(format!(
                "synthetic cleanup failure for worker {}",
                self.worker
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct CapturedPitchProgress {
    events: Vec<super::PitchProgressEvent>,
    jsonl: Vec<u8>,
    interrupt_on_checkpoint: Option<futures::channel::oneshot::Sender<()>>,
    checkpoint_signals: std::collections::BTreeMap<String, futures::channel::oneshot::Sender<()>>,
    session_end_signals: std::collections::BTreeMap<u32, futures::channel::oneshot::Sender<()>>,
    release_on_heartbeat: Option<futures::channel::oneshot::Sender<()>>,
    release_on_session_heartbeat: Option<(u32, futures::channel::oneshot::Sender<()>)>,
    state_path: Option<PathBuf>,
    heartbeat_states: Vec<Vec<u8>>,
}

impl super::PitchProgressSink for CapturedPitchProgress {
    fn emit(&mut self, event: super::PitchProgressEvent) -> Result<(), crate::error::AssetError> {
        if event.event == "heartbeat" {
            if let Some(path) = &self.state_path {
                self.heartbeat_states.push(fs::read(path).unwrap());
            }
            if let Some(permit) = self.release_on_heartbeat.take() {
                permit.send(()).unwrap();
            }
            if self
                .release_on_session_heartbeat
                .as_ref()
                .is_some_and(|(session, _)| event.session == Some(*session))
            {
                let (_, permit) = self.release_on_session_heartbeat.take().unwrap();
                permit.send(()).unwrap();
            }
        }
        if event.event == "item_checkpointed"
            && let Some(signal) = self.interrupt_on_checkpoint.take()
        {
            signal.send(()).unwrap();
        }
        if event.event == "item_checkpointed"
            && let Some(identity) = &event.identity
            && let Some(signal) = self.checkpoint_signals.remove(&identity.key)
        {
            let _ = signal.send(());
        }
        if event.event == "browser_session_ended"
            && let Some(worker) = event.worker
            && let Some(signal) = self.session_end_signals.remove(&worker)
        {
            let _ = signal.send(());
        }
        super::write_pitch_progress(&event, OutputFormat::Json, &mut self.jsonl)?;
        self.events.push(event);
        Ok(())
    }
}

fn offline_pitch_batch(store: &AssetStore, batch_id: &str, surfaces: &[&str]) {
    let items = surfaces
        .iter()
        .map(|surface| PitchPlanItem {
            surface: (*surface).into(),
            reading: None,
            selection: None,
        })
        .collect::<Vec<_>>();
    create_batch(store, batch_id, &items, None).unwrap();
}

fn offline_pitch_policy(max_items: usize) -> super::PitchRunPolicy {
    super::PitchRunPolicy {
        max_items,
        max_age: std::time::Duration::from_secs(1200),
        heartbeat: std::time::Duration::from_millis(1),
    }
}

fn no_pitch_interruption() -> impl std::future::Future<Output = Result<(), String>> {
    std::future::pending()
}

async fn pitch_signal(receiver: futures::channel::oneshot::Receiver<()>) -> Result<(), String> {
    receiver.await.map_err(|error| error.to_string())
}

fn assert_untouched_pitch_tail(batch: &PitchAccentBatch, start: usize) {
    for item in &batch.items[start..] {
        assert_eq!(item.status(), PitchBatchItemStatus::Pending);
        assert!(item.attempts.is_empty());
        assert!(item.current_candidate_sha256.is_none());
    }
}

async fn wait_for_worker_signal<T: std::fmt::Debug, F: std::future::Future<Output = T>>(
    run: &mut std::pin::Pin<Box<F>>,
    signal: &mut futures::channel::oneshot::Receiver<()>,
) {
    tokio::select! {
        biased;
        result = run.as_mut() => panic!("run завершился до ожидаемого worker-события: {result:?}"),
        result = signal => result.expect("synthetic barrier должен быть освобождён"),
    }
}

fn parallel_pitch_policy(max_items: usize) -> super::PitchRunPolicy {
    super::PitchRunPolicy {
        max_items,
        max_age: std::time::Duration::from_secs(1200),
        heartbeat: std::time::Duration::from_secs(60),
    }
}

fn assert_parallel_workspaces_removed(trace: &ParallelPitchTrace, expected_workers: usize) {
    let snapshot = trace.snapshot();
    assert_eq!(snapshot.finished.len(), expected_workers);
    assert_eq!(
        snapshot
            .finished
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        (1..=expected_workers as u32).collect()
    );
    assert_eq!(snapshot.workspace_paths.len(), expected_workers);
    assert_eq!(
        snapshot
            .workspace_paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected_workers
    );
    assert!(snapshot.workspace_paths.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn parallel_pitch_worker_count_bounds_unique_dispatch_and_json_progress() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-one-worker", &["一", "二", "三", "四"]);

    let trace = ParallelPitchTrace::default();
    let driver = ParallelPitchDriver::new(
        1,
        trace.clone(),
        ParallelPitchDriver::complete_actions(&["一", "二", "三", "四"]),
    );
    let mut drivers = [driver];
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_drivers(
        &store,
        "parallel-one-worker",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    let snapshot = trace.snapshot();
    assert_eq!(snapshot.max_active, 1);
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.starts.len(), 4);
    assert_eq!(snapshot.closed.len(), 1);
    assert!(snapshot.starts.iter().all(|attempt| attempt.worker == 1));
    let dispatched = snapshot
        .starts
        .iter()
        .map(|attempt| attempt.surface.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(dispatched, ["一", "二", "三", "四"].into_iter().collect());
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    assert_parallel_workspaces_removed(&trace, 1);

    let jsonl = String::from_utf8(progress.jsonl).unwrap();
    let events = jsonl
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(event["workers"], 1);
        assert!(event["in_flight"].as_u64().unwrap() <= 1);
    }
    for event in events
        .iter()
        .filter(|event| event["event"] == "item_started" || event["event"] == "item_checkpointed")
    {
        assert_eq!(event["worker"], 1);
        assert_eq!(event["worker_session"], 1);
        assert!(event["session"].as_u64().is_some());
    }
    let session_started = events
        .iter()
        .find(|event| event["event"] == "browser_session_started")
        .unwrap();
    assert_eq!(session_started["in_flight"], 1);
    assert_eq!(session_started["identity"]["key"], "一");
}

#[tokio::test]
async fn parallel_pitch_rejects_invalid_worker_counts_before_creating_actors() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-invalid-workers", &["一"]);

    let mut no_drivers: Vec<ParallelPitchDriver> = Vec::new();
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-invalid-workers",
        "batch_run",
        &mut no_drivers,
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidIdentity);

    let too_many_trace = ParallelPitchTrace::default();
    let mut too_many = (1..=5)
        .map(|worker| ParallelPitchDriver::new(worker, too_many_trace.clone(), []))
        .collect::<Vec<_>>();
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-invalid-workers",
        "batch_run",
        &mut too_many,
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidIdentity);
    let snapshot = too_many_trace.snapshot();
    assert!(snapshot.starts.is_empty());
    assert!(snapshot.closed.is_empty());
    assert!(snapshot.finished.is_empty());
    drop(too_many);
}

#[test]
fn pitch_batch_workers_default_to_four_and_cli_rejects_values_outside_supported_range() {
    let parsed = <PitchCli as clap::Parser>::try_parse_from([
        "pitch-assets",
        "batch",
        "run",
        "--batch-id",
        "worker-cli-default",
    ])
    .unwrap();
    match parsed.command {
        PitchCommand::Batch {
            command: PitchBatchCommand::Run { workers, .. },
        } => assert_eq!(workers, 4),
        _ => panic!("ожидалась команда batch run"),
    }

    for workers in ["0", "5"] {
        assert!(
            <PitchCli as clap::Parser>::try_parse_from([
                "pitch-assets",
                "batch",
                "run",
                "--batch-id",
                "worker-cli-range",
                "--workers",
                workers,
            ])
            .is_err()
        );
    }
}

#[tokio::test]
async fn parallel_pitch_checkpoints_out_of_order_reports_by_identity_and_cas_token() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-out-of-order", &["一", "二", "三"]);
    let trace = ParallelPitchTrace::default();

    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let (start_c_tx, mut start_c_rx) = futures::channel::oneshot::channel();
    let (release_c_tx, release_c_rx) = futures::channel::oneshot::channel();
    let (checkpoint_b_tx, mut checkpoint_b_rx) = futures::channel::oneshot::channel();
    let (checkpoint_c_tx, mut checkpoint_c_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_a_tx),
                    release: release_a_rx,
                },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                (
                    "二".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_b_tx),
                        release: release_b_rx,
                    },
                ),
                (
                    "三".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_c_tx),
                        release: release_c_rx,
                    },
                ),
            ],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress
        .checkpoint_signals
        .insert("二".into(), checkpoint_b_tx);
    progress
        .checkpoint_signals
        .insert("三".into(), checkpoint_c_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-out-of-order",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));

    wait_for_worker_signal(&mut run, &mut start_a_rx).await;
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    release_b_tx.send(()).unwrap();
    wait_for_worker_signal(&mut run, &mut checkpoint_b_rx).await;
    wait_for_worker_signal(&mut run, &mut start_c_rx).await;
    assert_eq!(
        load_batch(&store, "parallel-out-of-order")
            .unwrap()
            .item("二")
            .unwrap()
            .attempts
            .len(),
        1
    );
    assert!(
        load_batch(&store, "parallel-out-of-order")
            .unwrap()
            .item("一")
            .unwrap()
            .attempts
            .is_empty()
    );
    release_c_tx.send(()).unwrap();
    wait_for_worker_signal(&mut run, &mut checkpoint_c_rx).await;
    release_a_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();

    let snapshot = trace.snapshot();
    assert_eq!(snapshot.max_active, 2);
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.starts.len(), 3);
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "一")
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "二")
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "三")
            .count(),
        1
    );
    let checkpoints = progress
        .events
        .iter()
        .filter(|event| event.event == "item_checkpointed")
        .map(|event| {
            (
                event.identity.as_ref().unwrap().key.as_str(),
                event.run_completed,
                event.worker,
                event.worker_session,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        checkpoints,
        vec![
            ("二", 1, Some(2), Some(1)),
            ("三", 2, Some(2), Some(1)),
            ("一", 3, Some(1), Some(1))
        ]
    );
    let jsonl = String::from_utf8(progress.jsonl).unwrap();
    let json_events = jsonl
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        json_events
            .iter()
            .all(|event| event["workers"] == 2 && event["in_flight"].as_u64().unwrap() <= 2)
    );
    assert_eq!(
        json_events
            .iter()
            .map(|event| event["in_flight"].as_u64().unwrap())
            .max(),
        Some(2)
    );
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_discards_stale_cas_result_without_counting_it_as_checkpoint() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-stale-cas", &["一", "二"]);
    let trace = ParallelPitchTrace::default();
    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_a_tx),
                    release: release_a_rx,
                },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            ParallelPitchDriver::complete_actions(&["二"]),
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-stale-cas",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));
    wait_for_worker_signal(&mut run, &mut start_a_rx).await;

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "parallel-stale-cas").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .reacquire("一", "параллельное изменение перед CAS".into())
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    release_a_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();

    assert_eq!(
        batch.item("一").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert!(batch.item("一").unwrap().attempts.is_empty());
    assert_eq!(batch.item("二").unwrap().attempts.len(), 1);
    let discarded = progress
        .events
        .iter()
        .find(|event| event.event == "item_discarded_stale")
        .unwrap();
    assert_eq!(discarded.identity.as_ref().unwrap().key, "一");
    assert_eq!(discarded.run_completed, 1);
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_session_failure_stops_tail_but_joins_a_live_neighbor() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(
        &store,
        "parallel-neighbor-failure",
        &["一", "二", "三", "四"],
    );
    let trace = ParallelPitchTrace::default();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let (worker_one_end_tx, mut worker_one_end_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::SessionFailure { started: None },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [(
                "二".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_b_tx),
                    release: release_b_rx,
                },
            )],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress.session_end_signals.insert(1, worker_one_end_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-neighbor-failure",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    wait_for_worker_signal(&mut run, &mut worker_one_end_rx).await;
    release_b_tx.send(()).unwrap();
    let error = run.await.unwrap_err();

    assert_eq!(error.details["run_stop_reason"], "session_failure");
    let batch = load_batch(&store, "parallel-neighbor-failure").unwrap();
    assert!(batch.item("一").unwrap().attempts.is_empty());
    assert_eq!(batch.item("二").unwrap().attempts.len(), 1);
    for surface in ["三", "四"] {
        assert!(batch.item(surface).unwrap().attempts.is_empty());
        assert_eq!(
            batch.item(surface).unwrap().status(),
            PitchBatchItemStatus::Pending
        );
    }
    let snapshot = trace.snapshot();
    assert_eq!(snapshot.starts.len(), 2);
    assert!(
        snapshot
            .starts
            .iter()
            .any(|attempt| attempt.surface == "一")
    );
    assert!(
        snapshot
            .starts
            .iter()
            .any(|attempt| attempt.surface == "二")
    );
    assert_eq!(snapshot.closed.len(), 2);
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_ctrl_c_keeps_completed_set_and_resumes_without_reacquiring_it() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-interrupt-resume", &["一", "二", "三"]);
    let trace = ParallelPitchTrace::default();
    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (_release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_a_tx),
                    release: release_a_rx,
                },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [(
                "二".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_b_tx),
                    release: release_b_rx,
                },
            )],
        ),
    ];
    let (interrupt_tx, interrupt_rx) = futures::channel::oneshot::channel();
    let mut progress = CapturedPitchProgress {
        interrupt_on_checkpoint: Some(interrupt_tx),
        ..Default::default()
    };
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-interrupt-resume",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        pitch_signal(interrupt_rx),
    ));
    wait_for_worker_signal(&mut run, &mut start_a_rx).await;
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    release_b_tx.send(()).unwrap();
    let error = run.await.unwrap_err();

    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let batch = load_batch(&store, "parallel-interrupt-resume").unwrap();
    assert!(batch.item("一").unwrap().attempts.is_empty());
    assert_eq!(batch.item("二").unwrap().attempts.len(), 1);
    assert!(batch.item("三").unwrap().attempts.is_empty());
    let snapshot = trace.snapshot();
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.starts.len(), 2);
    assert_eq!(snapshot.closed.len(), 2);
    assert_parallel_workspaces_removed(&trace, 2);
    assert_eq!(progress.events.last().unwrap().run_completed, 1);

    let resume_trace = ParallelPitchTrace::default();
    let mut resume_drivers = [ParallelPitchDriver::new(
        1,
        resume_trace.clone(),
        ParallelPitchDriver::complete_actions(&["一", "三"]),
    )];
    let mut resume_progress = CapturedPitchProgress::default();
    let (resumed, _) = super::run_batch_with_drivers(
        &store,
        "parallel-interrupt-resume",
        "batch_resume",
        &mut resume_drivers,
        &mut resume_progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    let resume_snapshot = resume_trace.snapshot();
    assert_eq!(resume_snapshot.starts.len(), 2);
    assert!(
        resume_snapshot
            .starts
            .iter()
            .all(|attempt| attempt.surface != "二")
    );
    assert!(resumed.items.iter().all(|item| item.attempts.len() == 1));
    assert_eq!(resume_progress.events[0].run_total, 2);
    assert_parallel_workspaces_removed(&resume_trace, 1);
}

#[tokio::test]
async fn parallel_pitch_worker_sessions_rotate_independently() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(
        &store,
        "parallel-independent-rotation",
        &["一", "二", "三", "四"],
    );
    let trace = ParallelPitchTrace::default();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let (start_c_tx, mut start_c_rx) = futures::channel::oneshot::channel();
    let (release_c_tx, release_c_rx) = futures::channel::oneshot::channel();
    let (worker_one_rotation_tx, mut worker_one_rotation_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [
                ("一".into(), ParallelPitchAction::Complete),
                (
                    "三".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_c_tx),
                        release: release_c_rx,
                    },
                ),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [(
                "二".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_b_tx),
                    release: release_b_rx,
                },
            )],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress
        .session_end_signals
        .insert(1, worker_one_rotation_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-independent-rotation",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(1),
        no_pitch_interruption(),
    ));
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    wait_for_worker_signal(&mut run, &mut worker_one_rotation_rx).await;
    wait_for_worker_signal(&mut run, &mut start_c_rx).await;

    let snapshot = trace.snapshot();
    assert!(snapshot.starts.iter().any(|attempt| {
        attempt.worker == 1 && attempt.surface == "一" && attempt.worker_session == 1
    }));
    assert!(snapshot.starts.iter().any(|attempt| {
        attempt.worker == 2 && attempt.surface == "二" && attempt.worker_session == 1
    }));
    assert!(snapshot.starts.iter().any(|attempt| {
        attempt.worker == 1 && attempt.surface == "三" && attempt.worker_session == 2
    }));
    assert_eq!(
        snapshot.active, 2,
        "worker 2 должен оставаться в своей первой сессии"
    );
    release_c_tx.send(()).unwrap();
    release_b_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    let rotation = progress
        .events
        .iter()
        .find(|event| event.event == "browser_session_rotated" && event.worker == Some(1))
        .unwrap();
    assert_eq!(rotation.worker_session, Some(1));
    let worker_two_item = progress
        .events
        .iter()
        .find(|event| {
            event.event == "item_started"
                && event.worker == Some(2)
                && event
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.key == "二")
        })
        .unwrap();
    assert_eq!(worker_two_item.worker_session, Some(1));
    let worker_one_rotated_item = progress
        .events
        .iter()
        .find(|event| {
            event.event == "item_started"
                && event.worker == Some(1)
                && event
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.key == "三")
        })
        .unwrap();
    assert_eq!(worker_one_rotated_item.worker_session, Some(2));
    assert_eq!(rotation.next_session, worker_one_rotated_item.session);
    let snapshot = trace.snapshot();
    let sessions = snapshot
        .starts
        .iter()
        .map(|attempt| attempt.session)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(sessions.len(), snapshot.starts.len());
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_reports_worker_workspace_cleanup_failure_after_checkpoint() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-cleanup-error", &["一"]);
    let trace = ParallelPitchTrace::default();
    let mut drivers = [ParallelPitchDriver::new(
        1,
        trace.clone(),
        ParallelPitchDriver::complete_actions(&["一"]),
    )];
    drivers[0].fail_finish = true;
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-cleanup-error",
        "batch_run",
        &mut drivers,
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .message
            .contains("synthetic cleanup failure for worker 1")
    );
    assert_eq!(
        load_batch(&store, "parallel-cleanup-error")
            .unwrap()
            .item("一")
            .unwrap()
            .attempts
            .len(),
        1
    );
    assert_eq!(trace.snapshot().closed.len(), 1);
    assert_parallel_workspaces_removed(&trace, 1);
}

#[tokio::test]
async fn pitch_launch_failure_leaves_entire_frontier_pending() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "launch-failure", &["一", "二", "三"]);
    let before = load_batch(&store, "launch-failure").unwrap();
    let mut driver = ScriptedPitchDriver::new(&store, "launch-failure", vec![]);
    driver.fail_launch_on = Some(1);
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "launch-failure",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "session_failure");
    assert_eq!(load_batch(&store, "launch-failure").unwrap(), before);
    assert!(driver.seen.is_empty());
    assert!(driver.closed.is_empty());
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    assert!(progress.events.iter().all(|event| event.run_completed == 0));
}

#[tokio::test]
async fn pitch_configuration_and_telemetry_failures_do_not_fabricate_tail_attempts() {
    for (after_outcome, action) in [
        (false, ScriptedPitchAction::SetupFailure),
        (
            false,
            ScriptedPitchAction::SessionFailure {
                after_outcome: false,
            },
        ),
        (
            true,
            ScriptedPitchAction::SessionFailure {
                after_outcome: true,
            },
        ),
    ] {
        let workspace = temp_root();
        let root = workspace.path();
        let store = store_at(root);
        offline_pitch_batch(&store, "session-failure", &["一", "二", "三"]);
        let mut driver = ScriptedPitchDriver::new(&store, "session-failure", vec![action]);
        let mut progress = CapturedPitchProgress::default();
        let error = super::run_batch_with_driver(
            &store,
            "session-failure",
            "batch_run",
            &mut driver,
            &mut progress,
            offline_pitch_policy(2),
            no_pitch_interruption(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.details["run_stop_reason"], "session_failure");
        let batch = load_batch(&store, "session-failure").unwrap();
        let completed = usize::from(after_outcome);
        assert_eq!(batch.items[0].attempts.len(), completed);
        assert_untouched_pitch_tail(&batch, completed);
        assert_eq!(driver.seen.len(), 1);
        assert_eq!(driver.closed, vec![1]);
        assert_eq!(progress.events.last().unwrap().run_completed, completed);
        assert_eq!(
            progress.events[progress.events.len() - 2].event,
            "browser_session_ended"
        );
    }
}

#[tokio::test]
async fn pitch_interruption_between_items_preserves_durable_prefix() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "interrupt-between", &["一", "二", "三"]);
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "interrupt-between",
        vec![ScriptedPitchAction::Complete],
    );
    let mut progress = CapturedPitchProgress {
        interrupt_on_checkpoint: Some(signal),
        ..Default::default()
    };
    let error = super::run_batch_with_driver(
        &store,
        "interrupt-between",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let batch = load_batch(&store, "interrupt-between").unwrap();
    assert_eq!(batch.items[0].attempts.len(), 1);
    assert_untouched_pitch_tail(&batch, 1);
    assert_eq!(driver.seen, vec![(1, "一".into())]);
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
}

#[tokio::test]
async fn pitch_interrupt_in_flight_keeps_current_identity_unchanged() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "interrupt-active", &["一", "二"]);
    let before = load_batch(&store, "interrupt-active").unwrap();
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "interrupt-active",
        vec![ScriptedPitchAction::InterruptInFlight(signal)],
    );
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "interrupt-active",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    assert_eq!(load_batch(&store, "interrupt-active").unwrap(), before);
    assert_eq!(driver.closed, vec![1]);
    assert!(progress.events.iter().all(|event| event.run_completed == 0));
    assert!(
        !progress
            .events
            .iter()
            .any(|event| event.event == "item_checkpointed")
    );
}

#[tokio::test]
async fn pitch_ready_outcome_wins_signal_and_is_checkpointed_before_stop() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "ready-signal", &["一", "二"]);
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "ready-signal",
        vec![ScriptedPitchAction::ReadyWithSignal(signal)],
    );
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "ready-signal",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let batch = load_batch(&store, "ready-signal").unwrap();
    assert_eq!(batch.items[0].attempts.len(), 1);
    assert_untouched_pitch_tail(&batch, 1);
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .count(),
        1
    );
}

#[tokio::test]
async fn pitch_interruption_during_launch_waits_for_owner_then_closes_session() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "launch-interrupted", &["一", "二"]);
    let before = load_batch(&store, "launch-interrupted").unwrap();
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(&store, "launch-interrupted", vec![]);
    driver.interrupt_during_launch = Some(signal);
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "launch-interrupted",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    assert_eq!(load_batch(&store, "launch-interrupted").unwrap(), before);
    assert!(driver.seen.is_empty());
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
}

#[tokio::test]
async fn pitch_progress_jsonl_has_live_heartbeat_and_only_durable_completed_count() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "json-progress", &["一"]);
    let state_path = store
        .root()
        .join(".runtime/batches/json-progress/state.json");
    let before = fs::read(&state_path).unwrap();
    let (permit, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "json-progress",
        vec![ScriptedPitchAction::WaitForHeartbeat(receiver)],
    );
    let mut progress = CapturedPitchProgress {
        release_on_heartbeat: Some(permit),
        state_path: Some(state_path),
        ..Default::default()
    };
    let (batch, changed) = super::run_batch_with_driver(
        &store,
        "json-progress",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert!(changed);
    assert!(!progress.heartbeat_states.is_empty());
    assert!(
        progress
            .heartbeat_states
            .iter()
            .all(|state| state == &before)
    );
    let heartbeat = progress
        .events
        .iter()
        .find(|event| event.event == "heartbeat")
        .unwrap();
    assert_eq!(heartbeat.identity.as_ref().unwrap().key, "一");
    assert_eq!(heartbeat.run_completed, 0);
    assert_eq!(heartbeat.run_total, 1);
    assert_eq!(heartbeat.attempt, Some(1));
    let checkpoint = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(checkpoint.run_completed, 1);
    assert_eq!(checkpoint.attempt, Some(1));
    assert_eq!(checkpoint.outcome.as_deref(), Some("vocabulary_not_found"));
    let response = super::render_response(
        super::batch_response(
            "batch_run",
            "needs_review",
            changed,
            Some(StoreSummary {
                path: store.root().display().to_string(),
                store_id: store.store_id().into(),
            }),
            &batch,
            Vec::new(),
            None,
        ),
        OutputFormat::Json,
        3,
    );
    let stdout: serde_json::Value = serde_json::from_str(&response.stdout).unwrap();
    assert_eq!(stdout["operation"], "batch_run");
    assert!(response.stderr.is_empty());
    let jsonl = String::from_utf8(progress.jsonl).unwrap();
    for line in jsonl.lines() {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["batch_id"], "json-progress");
        assert!(event.get("round").is_none());
        assert!(event.get("round_limit").is_none());
        assert!(event.get("event").is_some());
    }
    assert!(serde_json::from_str::<serde_json::Value>(&jsonl).is_err());
}

#[tokio::test]
async fn pitch_rotation_then_interruption_resumes_tail_without_prefix_overlap() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "rotate-resume", &["一", "二", "三"]);
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "rotate-resume",
        vec![
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::InterruptInFlight(signal),
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    super::run_batch_with_driver(
        &store,
        "rotate-resume",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(1),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(driver.seen, vec![(1, "一".into()), (2, "二".into())]);
    assert_eq!(driver.closed, vec![1, 2]);
    assert_eq!(driver.attempts_at_start, vec![0, 1]);
    assert_untouched_pitch_tail(&load_batch(&store, "rotate-resume").unwrap(), 1);
    let mut resumed = ScriptedPitchDriver::new(
        &store,
        "rotate-resume",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    let mut resumed_progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "rotate-resume",
        "batch_resume",
        &mut resumed,
        &mut resumed_progress,
        offline_pitch_policy(1),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.seen, vec![(1, "二".into()), (2, "三".into())]);
    assert_eq!(resumed.closed, vec![1, 2]);
    assert_eq!(resumed.attempts_at_start, vec![1, 2]);
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    assert!(
        resumed_progress
            .events
            .iter()
            .any(|event| event.event == "browser_session_rotated")
    );
    assert_eq!(resumed_progress.events.first().unwrap().run_total, 2);
    assert!(
        resumed_progress
            .events
            .iter()
            .all(|event| event.operation == "batch_run")
    );
    assert_eq!(resumed_progress.events.last().unwrap().run_completed, 2);
}

#[tokio::test]
async fn pitch_rotation_launch_failure_stops_once_and_resume_keeps_checkpoint() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "rotation-failure", &["一", "二", "三"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "rotation-failure",
        vec![ScriptedPitchAction::Complete],
    );
    driver.fail_launch_on = Some(2);
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "rotation-failure",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(1),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "session_failure");
    assert_eq!(driver.launches, 2);
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(driver.seen, vec![(1, "一".into())]);
    assert_untouched_pitch_tail(&load_batch(&store, "rotation-failure").unwrap(), 1);
    let mut resumed = ScriptedPitchDriver::new(
        &store,
        "rotation-failure",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "rotation-failure",
        "batch_resume",
        &mut resumed,
        &mut CapturedPitchProgress::default(),
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.seen, vec![(1, "二".into()), (1, "三".into())]);
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
}

#[tokio::test]
async fn pitch_stale_cas_is_reported_without_incrementing_checkpoint_counter() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "stale-progress", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "stale-progress",
        vec![
            ScriptedPitchAction::Reacquire,
            ScriptedPitchAction::Complete,
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "stale-progress",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(batch.items[0].status(), PitchBatchItemStatus::Pending);
    assert!(batch.items[0].attempts.is_empty());
    assert_eq!(batch.items[1].attempts.len(), 1);
    let discarded = progress
        .events
        .iter()
        .find(|event| event.event == "item_discarded_stale")
        .unwrap();
    assert_eq!(discarded.run_completed, 0);
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
}

#[tokio::test]
async fn pitch_session_age_rotates_between_items_and_production_limits_are_explicit() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "age-rotation", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "age-rotation",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    let mut progress = CapturedPitchProgress::default();
    let mut policy = offline_pitch_policy(128);
    policy.max_age = std::time::Duration::from_nanos(1);
    super::run_batch_with_driver(
        &store,
        "age-rotation",
        "batch_run",
        &mut driver,
        &mut progress,
        policy,
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(driver.closed, vec![1, 2]);
    assert_eq!(driver.seen, vec![(1, "一".into()), (2, "二".into())]);
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "browser_session_rotated"
                && event.reason.as_deref() == Some("age_limit"))
    );
    assert_eq!(super::PitchRunPolicy::default().max_items, 64);
    assert_eq!(
        super::PitchRunPolicy::default().max_age,
        std::time::Duration::from_secs(20 * 60)
    );
}

#[tokio::test]
async fn pitch_manual_retry_reports_actual_attempt_and_pending_denominator() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "retry-progress", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "retry-progress",
        vec![
            ScriptedPitchAction::ItemFailure,
            ScriptedPitchAction::Complete,
        ],
    );
    super::run_batch_with_driver(
        &store,
        "retry-progress",
        "batch_run",
        &mut driver,
        &mut CapturedPitchProgress::default(),
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "retry-progress").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    assert_eq!(
        batch.items[0].status(),
        PitchBatchItemStatus::TechnicalFailure
    );
    batch
        .retry("一", "явный повтор после сетевой ошибки".into())
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    let mut resumed = ScriptedPitchDriver::new(
        &store,
        "retry-progress",
        vec![ScriptedPitchAction::Complete],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "retry-progress",
        "batch_resume",
        &mut resumed,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(batch.items[0].attempts.len(), 2);
    assert_eq!(batch.items[1].attempts.len(), 1);
    assert_eq!(progress.events[0].run_total, 1);
    assert_eq!(progress.events[0].batch_total, 2);
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.operation == "batch_run")
    );
    let retry = progress
        .events
        .iter()
        .find(|event| event.event == "retry_started")
        .unwrap();
    assert_eq!(retry.attempt, Some(2));
    let checkpoint = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(checkpoint.attempt, Some(2));
    assert_eq!(checkpoint.run_completed, 1);
}

#[tokio::test]
async fn pitch_rotation_and_launch_heartbeat_have_explicit_session_context() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "rotation-context", &["一", "二"]);
    let (permit, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "rotation-context",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    driver.slow_launch = Some((2, receiver));
    let mut progress = CapturedPitchProgress {
        release_on_session_heartbeat: Some((2, permit)),
        ..CapturedPitchProgress::default()
    };
    super::run_batch_with_driver(
        &store,
        "rotation-context",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(1),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    let checkpoint = progress
        .events
        .iter()
        .position(|event| event.event == "item_checkpointed")
        .unwrap();
    let rotation = progress
        .events
        .iter()
        .position(|event| event.event == "browser_session_rotated")
        .unwrap();
    let heartbeat = progress
        .events
        .iter()
        .position(|event| event.event == "heartbeat" && event.session == Some(2))
        .unwrap();
    let next_start = progress
        .events
        .iter()
        .rposition(|event| event.event == "item_started")
        .unwrap();
    assert!(checkpoint < rotation && rotation < heartbeat && heartbeat < next_start);
    assert_eq!(progress.events[rotation].session, Some(1));
    assert_eq!(progress.events[rotation].next_session, Some(2));
    assert_eq!(progress.events[heartbeat].session, Some(2));
    for event in &progress.events {
        if let Some(identity) = event.identity.as_ref() {
            if event.session.is_some() {
                assert_eq!(
                    event.session,
                    Some(if identity.key == "一" { 1 } else { 2 }),
                    "event={} identity={} context={event:?}",
                    event.event,
                    identity.key
                );
                assert!(event.worker_session.is_some());
            } else {
                assert_eq!(event.event, "heartbeat");
                assert!(event.worker_session.is_none());
            }
            assert_eq!(event.attempt, Some(1));
            assert!(event.next_session.is_none());
        } else {
            assert!(event.attempt.is_none(), "{} inherited attempt", event.event);
        }
    }
}

#[test]
fn pitch_human_progress_localizes_protocol_values_without_changing_jsonl() {
    let cases = [
        (
            "browser_session_rotated",
            None,
            Some("item_limit"),
            "смена сессии браузера",
            "достигнут лимит записей сессии",
        ),
        (
            "run_stopped",
            None,
            Some("session_failure"),
            "получение остановлено",
            "ошибка сессии браузера",
        ),
        (
            "item_checkpointed",
            Some("technical_failure"),
            None,
            "результат записи сохранён",
            "техническая ошибка",
        ),
        (
            "item_checkpointed",
            Some("vocabulary_not_found"),
            None,
            "результат записи сохранён",
            "запись JPDB не найдена",
        ),
        (
            "item_discarded_stale",
            None,
            Some("item_token_changed"),
            "устаревший результат отброшен",
            "запись изменена другим действием",
        ),
    ];
    for (event, outcome, reason, label, detail) in cases {
        let progress = super::PitchProgressEvent {
            schema_version: 1,
            operation: "batch_run",
            event,
            batch_id: "human-progress".into(),
            elapsed_ms: 25,
            run_completed: 1,
            run_total: 2,
            batch_total: 2,
            workers: Some(2),
            in_flight: Some(0),
            identity: None,
            worker: None,
            worker_session: None,
            session: Some(1),
            next_session: None,
            attempt: None,
            outcome: outcome.map(str::to_owned),
            reason: reason.map(str::to_owned),
        };
        let mut human = Vec::new();
        super::write_pitch_progress(&progress, OutputFormat::Human, &mut human).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains(label));
        assert!(human.contains(detail));
        assert!(!human.contains(event));
        if let Some(value) = outcome.or(reason) {
            assert!(!human.contains(value));
        }
        let mut machine = Vec::new();
        super::write_pitch_progress(&progress, OutputFormat::Json, &mut machine).unwrap();
        let machine: serde_json::Value = serde_json::from_slice(&machine).unwrap();
        assert_eq!(machine["event"], event);
        assert_eq!(machine["outcome"].as_str(), outcome);
        assert_eq!(machine["reason"].as_str(), reason);
    }
}

#[tokio::test]
async fn pitch_saved_typed_failure_is_available_in_status_and_error_json_summary() {
    for failure in [
        JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "row contract: has_forms=false; 日本語\nточный diagnostic".into(),
        },
        JpdbPitchFailure::Telemetry {
            stage: JpdbPitchStage::SearchReadiness,
            message: "monitor_failed=true; relevant_pending=2; безопасная подробность".into(),
        },
    ] {
        let workspace = temp_root();
        let root = workspace.path();
        let store = store_at(root);
        offline_pitch_batch(&store, "typed-summary", &["一"]);
        let token = load_batch(&store, "typed-summary")
            .unwrap()
            .item_token("一")
            .unwrap();
        assert!(
            super::record_one_outcome(
                &store,
                "typed-summary",
                &token,
                JpdbPitchOutcome::Failed {
                    error: failure.clone()
                }
            )
            .unwrap()
        );
        let saved = load_batch(&store, "typed-summary").unwrap();
        let expected = serde_json::to_value(&failure).unwrap();
        let snapshot = serde_json::to_value(&saved).unwrap();
        assert_eq!(
            snapshot["items"][0]["attempts"][0]["outcome"]["error"],
            expected
        );
        let status = execute(cli(
            store.root().to_path_buf(),
            root.to_path_buf(),
            OutputFormat::Json,
            PitchCommand::Batch {
                command: PitchBatchCommand::Status {
                    batch_id: "typed-summary".into(),
                },
            },
        ))
        .await;
        assert_eq!(status.exit_code, 0);
        let status: serde_json::Value = serde_json::from_str(&status.stdout).unwrap();
        assert_eq!(status["items"][0]["failure"], expected);
        assert_eq!(status["items"][0]["last_outcome"]["error"], expected);
        let stopped = super::render_batch_error(
            &store,
            StoreSummary {
                path: store.root().display().to_string(),
                store_id: store.store_id().into(),
            },
            "typed-summary",
            "batch_run",
            super::pitch_session_failure(failure),
            OutputFormat::Json,
            true,
        );
        let stopped: serde_json::Value = serde_json::from_str(&stopped.stdout).unwrap();
        assert_eq!(stopped["items"][0]["failure"], expected);
        assert_eq!(stopped["error"]["details"]["session_failure"], expected);
    }
}

#[tokio::test]
async fn json_batch_response_exposes_flushed_log_on_success_and_failure() {
    for succeeds in [true, false] {
        let workspace = temp_root();
        let root = workspace.path();
        let store = store_at(root);
        let batch_id = if succeeds {
            save_durable_candidate(&store, "diagnostic-success", "幽霊");
            "diagnostic-success"
        } else {
            "diagnostic-missing-batch"
        };
        let summary = StoreSummary {
            path: store.root().display().to_string(),
            store_id: store.store_id().to_owned(),
        };
        let output = super::run_batch_output(
            &store,
            batch_id,
            "batch_run",
            summary,
            OutputFormat::Json,
            false,
            1,
        )
        .await;
        assert_eq!(output.stderr, "");
        let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
        let log_path = PathBuf::from(response["diagnostic_log"].as_str().unwrap());
        assert!(log_path.exists());
        assert!(log_path.to_string_lossy().contains("/logs/"));
        let events = fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| event["fields"]["event"] == "run_started")
        );
        let terminal_event = if succeeds {
            "run_finished"
        } else {
            "run_stopped"
        };
        let terminal = events
            .iter()
            .find(|event| event["fields"]["event"] == terminal_event)
            .unwrap();
        assert!(terminal["spans"].as_array().unwrap().iter().any(|span| {
            span["name"] == "pitch_batch_run"
                && span["run_id"] == terminal["fields"]["run_id"]
                && span["diagnostic_log"] == response["diagnostic_log"]
        }));
    }
}

#[tokio::test]
async fn session_failure_log_records_real_prefix_and_leaves_tail_unstarted() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "diagnostic-session-failure", &["一", "二", "三"]);
    let run_log =
        crate::batch_runtime::SafeBatchRuntime::open(store.root(), "diagnostic-session-failure")
            .unwrap()
            .create_run_log()
            .unwrap();
    let run_id = run_log.run_id;
    let log_path = run_log.path.display().to_string();
    let guard =
        crate::diagnostics::RunLogGuard::new(run_log.file, crate::diagnostics::OutputMode::Json);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "diagnostic-session-failure",
        vec![ScriptedPitchAction::SessionFailure {
            after_outcome: true,
        }],
    );
    let mut progress = CapturedPitchProgress::default();
    let batch_id = "diagnostic-session-failure";
    let operation = "batch_run";
    let span_run_id = run_id.clone();
    let span_log_path = log_path.clone();
    let future = async {
        let span = tracing::info_span!(
            "pitch_batch_run",
            operation,
            batch_id,
            run_id = span_run_id,
            diagnostic_log = span_log_path,
        );
        async {
            tracing::info!(event = "run_started", operation, batch_id, run_id);
            let result = super::run_batch_with_driver(
                &store,
                batch_id,
                operation,
                &mut driver,
                &mut progress,
                offline_pitch_policy(4),
                no_pitch_interruption(),
            )
            .await;
            if let Err(error) = &result {
                tracing::error!(
                    event = "run_stopped",
                    operation,
                    batch_id,
                    run_id,
                    code = error.code.as_str(),
                    message = %crate::diagnostics::safe_message(&error.message),
                );
            }
            result
        }
        .instrument(span)
        .await
    }
    .with_subscriber(guard.dispatch());
    let error = future.await.unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "session_failure");
    guard.finish().unwrap();

    let batch = load_batch(&store, batch_id).unwrap();
    assert_eq!(batch.items[0].attempts.len(), 1);
    assert!(batch.items[1..].iter().all(|item| item.attempts.is_empty()));
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_started")
            .map(|event| event.identity.as_ref().unwrap().key.as_str())
            .collect::<Vec<_>>(),
        ["一"]
    );

    let contents = fs::read_to_string(log_path).unwrap();
    assert!(!contents.contains('\u{1b}'));
    let events = contents
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let failure = events
        .iter()
        .find(|event| event["fields"]["event"] == "pitch_failure")
        .unwrap();
    assert_eq!(failure["fields"]["code"], "session_failure");
    assert_eq!(failure["fields"]["identity"], "一");
    assert_eq!(failure["fields"]["worker"], 1);
    assert_eq!(failure["fields"]["worker_session"], 1);
    assert_eq!(failure["fields"]["session"], 1);
    assert_eq!(failure["fields"]["attempt"], 1);
    assert!(
        failure["fields"]["message"]
            .as_str()
            .unwrap()
            .contains("автономной проверке")
    );
    assert!(events.iter().any(|event| {
        event["fields"]["event"] == "run_stopped"
            && event["spans"]
                .as_array()
                .unwrap()
                .iter()
                .any(|span| span["name"] == "pitch_batch_run" && span["run_id"] == run_id)
    }));
    assert!(!events.iter().any(|event| {
        event["fields"]["code"] == "item_started" && event["fields"]["identity"] != "一"
    }));
}
