//! Регрессии того, что общий core применяет семантику доверия, объявленную
//! политикой домена, а не распознаёт домен по имени.
//!
//! Синтетические домены ниже намеренно не называются ни `kanji`, ни
//! `pitch_accent`. Если бы `store.rs` ветвился по имени домена, обе политики вели
//! бы себя одинаково и эти проверки не различили бы их.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use asset_store::{
    AssetDomainPolicy, AssetError, AssetIdentity, AssetRecord, AssetStore, CanonicalAssetLocation,
    DetectedFormat, ErrorCode, HumanAttestationRequest, HumanDecision, Provenance, SelectionMode,
    SemanticDecision, SemanticStatus, SemanticValidator, StoreOptions, TrustSemantics,
    ValidationEvidence, ValidatorFailure, ValidatorIdentity, VerifiedIngestRequest,
};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "domain-semantics-{label}-{}-{counter}",
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

/// Синтетический домен с собственным `domain_id` и явно объявленной семантикой.
#[derive(Debug, Clone, Copy)]
struct FixturePolicy {
    domain_id: &'static str,
    namespace: &'static str,
    semantics: TrustSemantics,
}

const AUTOMATED_ONLY: FixturePolicy = FixturePolicy {
    domain_id: "fixture_automated_only",
    namespace: "fixture_auto",
    semantics: TrustSemantics::AUTOMATED_VERIFIED_ONLY,
};

const HUMAN_ATTESTED: FixturePolicy = FixturePolicy {
    domain_id: "fixture_human_attested",
    namespace: "fixture_human",
    semantics: TrustSemantics::HUMAN_ATTESTED,
};

impl AssetDomainPolicy for FixturePolicy {
    fn domain_id(&self) -> &'static str {
        self.domain_id
    }

    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError> {
        if identity.namespace != self.namespace || identity.key.is_empty() {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "идентификатор не принадлежит синтетическому домену",
            ));
        }
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        _sha256: &str,
        _format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/png/{}.png", identity.key),
            consumer_filename: format!("{}.fixture.png", identity.key),
        })
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        Some(8 * 1024 * 1024)
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }

    fn trust_semantics(&self) -> TrustSemantics {
        self.semantics
    }
}

fn expected(version: &'static str) -> ValidatorIdentity {
    ValidatorIdentity::new("fixture-pixel-check", version)
        .expect("идентификатор тестового валидатора корректен")
}

#[derive(Clone, Copy)]
struct FixtureValidator {
    version: &'static str,
}

impl SemanticValidator for FixtureValidator {
    fn identity(&self) -> ValidatorIdentity {
        expected(self.version)
    }

    fn validate(
        &self,
        _asset: &AssetRecord,
        _bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "fixture".into(),
                summary: "синтетическое решение валидатора".into(),
                details: None,
            }],
        ))
    }
}

fn fixture_png() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(4, 4, image::Rgba([18, 24, 30, 255]));
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut output, image::ImageFormat::Png)
        .expect("синтетический PNG кодируется");
    output.into_inner()
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    policy: FixturePolicy,
    identity: AssetIdentity,
    bytes: Vec<u8>,
}

impl Fixture {
    fn new(label: &str, policy: FixturePolicy) -> Self {
        let temp = TempDir::new(label);
        let root = temp.path().join("store");
        let identity =
            AssetIdentity::new(policy.namespace, "見本").expect("тестовый идентификатор корректен");
        Self {
            _temp: temp,
            root,
            policy,
            identity,
            bytes: fixture_png(),
        }
    }

    fn store(&self) -> AssetStore {
        AssetStore::open_with_policy(StoreOptions::new(&self.root), self.policy)
            .expect("синтетическое хранилище открывается")
    }

    fn publish(&self, version: &'static str) -> AssetRecord {
        self.store()
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: self.identity.clone(),
                    bytes: self.bytes.clone(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "candidate-a".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &FixtureValidator { version },
            )
            .expect("первая публикация синтетического ресурса проходит")
            .asset
            .expect("каноническая запись существует")
    }

    fn republish_same_bytes(
        &self,
        version: &'static str,
        replace_expected_sha256: Option<&str>,
    ) -> Result<AssetRecord, AssetError> {
        self.store()
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: self.identity.clone(),
                    bytes: self.bytes.clone(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "candidate-b".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: replace_expected_sha256.map(str::to_owned),
                },
                &FixtureValidator { version },
            )
            .map(|outcome| {
                outcome
                    .asset
                    .expect("каноническая запись остаётся доступной")
            })
    }

    fn approve(&self, record: &AssetRecord) {
        self.store()
            .attest(HumanAttestationRequest {
                identity: self.identity.clone(),
                expected_sha256: record.sha256.clone(),
                decision: HumanDecision::Approve,
                reason: "синтетическое подтверждение человека".into(),
            })
            .expect("подтверждение человека фиксируется");
    }

    fn read_with(&self, version: &'static str) -> Result<usize, AssetError> {
        AssetStore::read_verified_with_policy(
            &self.root,
            std::slice::from_ref(&self.identity),
            &expected(version),
            &self.policy,
        )
        .map(|assets| assets.len())
    }

    fn select(&self, version: &'static str) -> usize {
        self.store()
            .select(SelectionMode::New, &expected(version))
            .expect("выборка не требующих проверки ресурсов проходит")
            .len()
    }
}

/// Доверие потребителя определяется объявленной семантикой, а не именем домена:
/// при `AUTOMATED_VERIFIED_ONLY` подтверждение человека не заменяет решение
/// ожидаемого валидатора, при `HUMAN_ATTESTED` — заменяет.
#[test]
fn declared_semantics_decide_trust_not_the_domain_name() {
    let strict = Fixture::new("trust-automated-only", AUTOMATED_ONLY);
    let strict_record = strict.publish("1");
    strict.approve(&strict_record);
    let error = strict
        .read_with("2")
        .expect_err("подтверждение человека не заменяет решение другого валидатора");
    assert_eq!(error.code, ErrorCode::InvalidValidationEvidence);

    let lenient = Fixture::new("trust-human-attested", HUMAN_ATTESTED);
    let lenient_record = lenient.publish("1");
    lenient.approve(&lenient_record);
    assert_eq!(
        lenient
            .read_with("2")
            .expect("объявленное доверие к человеку разрешает чтение"),
        1
    );
}

/// `SelectionMode::New` следует объявленной семантике и для домена, которого
/// общий core не знает по имени.
#[test]
fn new_selection_follows_declared_semantics_for_an_unknown_domain() {
    let strict = Fixture::new("select-automated-only", AUTOMATED_ONLY);
    let strict_record = strict.publish("1");
    assert_eq!(
        strict.select("1"),
        0,
        "актуальное решение ожидаемого валидатора уже есть"
    );
    strict.approve(&strict_record);
    assert_eq!(
        strict.select("2"),
        1,
        "человек не заменяет решение другого валидатора в строгом домене"
    );

    let lenient = Fixture::new("select-human-attested", HUMAN_ATTESTED);
    let lenient_record = lenient.publish("1");
    lenient.approve(&lenient_record);
    assert_eq!(
        lenient.select("2"),
        0,
        "подтверждение человека закрывает выборку в домене с его доверием"
    );
}

/// Домен, объявивший метаданные частью семантики ресурса, получает CAS на
/// повторную публикацию тех же bytes, даже если core не знает его имени.
#[test]
fn declared_metadata_cas_applies_without_recognising_the_domain() {
    let strict = Fixture::new("cas-automated-only", AUTOMATED_ONLY);
    let strict_record = strict.publish("1");
    let error = strict
        .republish_same_bytes("1", None)
        .expect_err("изменение метаданных при том же SHA требует явного CAS");
    assert_eq!(error.code, ErrorCode::IdentityConflict);
    let refreshed = strict
        .republish_same_bytes("1", Some(&strict_record.sha256))
        .expect("точный CAS разрешает обновление метаданных");
    assert_eq!(refreshed.sha256, strict_record.sha256);
    assert_eq!(refreshed.provenance.source_name, "candidate-b");

    let lenient = Fixture::new("cas-human-attested", HUMAN_ATTESTED);
    let lenient_record = lenient.publish("1");
    let unchanged = lenient
        .republish_same_bytes("1", None)
        .expect("домен без семантических метаданных не требует CAS");
    assert_eq!(unchanged.sha256, lenient_record.sha256);
    assert_eq!(
        unchanged.provenance.source_name, "candidate-a",
        "неизменённые байты сохраняют прежние сведения об источнике"
    );
}
