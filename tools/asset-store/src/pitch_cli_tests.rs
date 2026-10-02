use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{OutputFormat, StoreSummary, load_batch, reject_batch, run_batch};
use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use crate::hashing::sha256_hex;
use crate::jpdb::{JpdbPitchAcquired, JpdbPitchOutcome, JpdbPitchQuery, JpdbPitchRequest};
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

    // Candidate bytes и состояние сохранены до этого запуска, как если бы
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

    // Второй запуск видит уже разрешённый current owner, не открывает browser
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
