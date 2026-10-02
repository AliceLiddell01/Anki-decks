use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use asset_store::{
    AssetIdentity, AssetStore, HumanAttestationRequest, HumanDecision, IngestRequest,
    PitchAccentDomainPolicy, PitchAccentImageValidator, Provenance, SelectionMode,
    SemanticDecision, SemanticStatus, SemanticValidator, StoreOptions, ValidationEvidence,
    ValidatorFailure, ValidatorIdentity, VerifiedIngestRequest,
};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pitch-corpus-trust-{label}-{}-{counter}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("каталог теста создаётся");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
struct FixtureValidator {
    version: &'static str,
    status: SemanticStatus,
}

impl FixtureValidator {
    fn identity(self) -> ValidatorIdentity {
        ValidatorIdentity::new(PitchAccentImageValidator::VALIDATOR_ID, self.version)
            .expect("идентификатор тестового валидатора корректен")
    }
}

impl SemanticValidator for FixtureValidator {
    fn identity(&self) -> ValidatorIdentity {
        (*self).identity()
    }

    fn validate(
        &self,
        asset: &asset_store::AssetRecord,
        _bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let summary = asset
            .domain_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("attempt"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("синтетическое решение валидатора");
        Ok(SemanticDecision::new(
            self.status,
            vec![ValidationEvidence {
                kind: "contract_fixture".into(),
                summary: summary.into(),
                details: None,
            }],
        ))
    }
}

fn pitch_png() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(4, 4, image::Rgba([24, 36, 48, 255]));
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut output, image::ImageFormat::Png)
        .expect("синтетический PNG кодируется");
    output.into_inner()
}

fn identity() -> AssetIdentity {
    AssetIdentity::new("pitch_accent", "幽霊").expect("тестовый идентификатор корректен")
}

fn v4_approved_store(root: &Path) -> (AssetStore, AssetIdentity, Vec<u8>) {
    let store = AssetStore::open_with_policy(StoreOptions::new(root), PitchAccentDomainPolicy)
        .expect("хранилище pitch-accent открывается");
    let identity = identity();
    let bytes = pitch_png();
    let record = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "幽霊.pitch.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixtureValidator {
                version: "4",
                status: SemanticStatus::Verified,
            },
        )
        .expect("тестовый ресурс с проверкой v4 публикуется")
        .asset
        .expect("проверенный тестовый ресурс существует");
    store
        .attest(HumanAttestationRequest {
            identity: identity.clone(),
            expected_sha256: record.sha256,
            decision: HumanDecision::Approve,
            reason: "синтетическое подтверждение для регрессионного теста".into(),
        })
        .expect("одобрение человека фиксируется");
    (store, identity, bytes)
}

fn expected_v5() -> ValidatorIdentity {
    PitchAccentImageValidator::validator_identity()
}

#[test]
fn pitch_corpus_gate_rejects_v4_evidence_even_with_current_sha_human_approval() {
    let temp = TempDir::new("gate-v4-approved");
    let root = temp.path().join(".asset-store/pitch-accent");
    let (_store, _identity, _bytes) = v4_approved_store(&root);

    let output = Command::new(env!("CARGO_BIN_EXE_pitch-corpus-gate"))
        .current_dir(temp.path())
        .output()
        .expect("pitch-corpus-gate запускается");

    assert!(
        !output.status.success(),
        "проверка корпуса отклоняет старые свидетельства"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("не пройдена"),
        "stderr содержит отказ проверки корпуса: {stderr}"
    );
    assert!(
        stderr.contains("ожидаемой версии валидатора"),
        "stderr объясняет причину отказа: {stderr}"
    );
}

#[test]
fn pitch_read_and_same_sha_ingest_require_current_automated_validation() {
    let temp = TempDir::new("read-ingest-v4-approved");
    let root = temp.path().join("pitch-accent");
    let (store, identity, bytes) = v4_approved_store(&root);
    let expected = expected_v5();

    let read_error = AssetStore::read_verified_with_policy(
        &root,
        std::slice::from_ref(&identity),
        &expected,
        &PitchAccentDomainPolicy,
    )
    .expect_err("чтение отвергает одобрение поверх проверки v4");
    assert_eq!(
        read_error.code,
        asset_store::ErrorCode::InvalidValidationEvidence
    );
    assert_eq!(
        store.select(SelectionMode::New, &expected).unwrap().len(),
        1
    );

    let refreshed = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes,
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "幽霊.pitch.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixtureValidator {
                version: "5",
                status: SemanticStatus::Verified,
            },
        )
        .expect("текущий валидатор перепроверяет те же байты");
    let record = refreshed
        .asset
        .expect("текущий проверенный ресурс существует");
    assert!(
        refreshed.changed,
        "обновление свидетельств является изменением"
    );
    assert_eq!(record.validation.unwrap().validator, expected);
    assert!(
        AssetStore::read_verified_with_policy(
            &root,
            &[identity],
            &expected,
            &PitchAccentDomainPolicy,
        )
        .is_ok(),
        "чтение принимает валидатор v5 после перепроверки"
    );
}

#[test]
fn same_sha_metadata_refresh_requires_exact_cas_and_commits_current_metadata() {
    let temp = TempDir::new("same-sha-metadata-refresh");
    let root = temp.path().join("pitch-accent");
    let store = AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy)
        .expect("хранилище pitch-accent открывается");
    let identity = identity();
    let bytes = pitch_png();
    let validator = FixtureValidator {
        version: "5",
        status: SemanticStatus::Verified,
    };
    let first = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "candidate-a".into(),
                },
                domain_metadata: Some(serde_json::json!({ "attempt": "A" })),
                replace_expected_sha256: None,
            },
            &validator,
        )
        .expect("первая публикация кандидата проходит");
    let first_record = first.asset.expect("первая каноническая запись существует");

    let without_cas = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "candidate-b".into(),
                },
                domain_metadata: Some(serde_json::json!({ "attempt": "B" })),
                replace_expected_sha256: None,
            },
            &validator,
        )
        .expect_err("одинаковый SHA не обходит CAS при изменении метаданных");
    assert_eq!(without_cas.code, asset_store::ErrorCode::IdentityConflict);

    let stale_cas = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "candidate-b".into(),
                },
                domain_metadata: Some(serde_json::json!({ "attempt": "B" })),
                replace_expected_sha256: Some("f".repeat(64)),
            },
            &validator,
        )
        .expect_err("same SHA не обходит неверный explicit CAS token");
    assert_eq!(stale_cas.code, asset_store::ErrorCode::IdentityConflict);

    let refreshed = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity,
                bytes,
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "candidate-b".into(),
                },
                domain_metadata: Some(serde_json::json!({ "attempt": "B" })),
                replace_expected_sha256: Some(first_record.sha256.clone()),
            },
            &validator,
        )
        .expect("точный CAS разрешает обновление канонических метаданных");
    let refreshed_record = refreshed
        .asset
        .expect("каноническая запись остаётся доступной");
    assert!(refreshed.changed);
    assert_eq!(refreshed_record.sha256, first_record.sha256);
    assert_eq!(
        refreshed_record.domain_metadata,
        Some(serde_json::json!({ "attempt": "B" }))
    );
    assert_eq!(
        refreshed_record.validation.as_ref().unwrap().evidence[0].summary,
        "B"
    );
    assert_eq!(refreshed_record.provenance.source_name, "candidate-b");
    assert_eq!(store.verify_integrity().unwrap(), [refreshed_record]);
}

#[test]
fn pitch_corpus_gate_rejects_human_approval_of_current_non_verified_decision() {
    let temp = TempDir::new("gate-rejected-approved");
    let root = temp.path().join(".asset-store/pitch-accent");
    let store = AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy)
        .expect("хранилище pitch-accent открывается");
    let identity = identity();
    let bytes = pitch_png();
    let candidate_path = temp.path().join("candidate.png");
    fs::write(&candidate_path, &bytes).expect("fixture PNG записывается");
    store
        .ingest(IngestRequest {
            identity: identity.clone(),
            source_path: candidate_path,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .expect("кандидат сохраняется во временном состоянии");
    store
        .validate(
            SelectionMode::Full,
            &FixtureValidator {
                version: "5",
                status: SemanticStatus::Rejected,
            },
        )
        .expect("отрицательное решение сохраняется");
    let record = store
        .verify_integrity()
        .expect("отрицательная запись целостна")
        .into_iter()
        .find(|record| record.identity == identity)
        .expect("кандидат найден");
    store
        .attest(HumanAttestationRequest {
            identity,
            expected_sha256: record.sha256,
            decision: HumanDecision::Approve,
            reason: "синтетическое подтверждение для проверки границы trust".into(),
        })
        .expect("модель lifecycle сохраняет явно заданное решение");
    drop(store);

    let output = Command::new(env!("CARGO_BIN_EXE_pitch-corpus-gate"))
        .current_dir(temp.path())
        .output()
        .expect("pitch-corpus-gate запускается");

    assert!(
        !output.status.success(),
        "проверка корпуса требует решения verified от валидатора"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("доверенного решения `verified`"),
        "stderr указывает на отсутствие доверенного автоматического решения verified"
    );
}
