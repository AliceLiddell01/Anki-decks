//! Только синтетические экспорты и изолированные хранилища: контракт миграции
//! проверяется без live-колоды и без сети.
//!
//! Ключевой случай — ровно тот, ради которого команда существует: одно и то же
//! имя `飴.png` занято legacy pitch-картинкой, а каноническое имя изображения
//! символа `飴` в kanji-домене совпадает с ним. После миграции pitch-ссылка
//! уходит на `飴.pitch.png`, имя `飴.png` освобождается и kanji-fallback
//! размещается под ним без конфликта.
use super::*;
use crate::ops::create::{CreateResult, create_with_options, parse_request_bytes};
use crate::test_support::TempDir;
use asset_store::kanji_validator::KanjiImageValidator;
use asset_store::pitch_accent::PitchAccentImageValidator;
use asset_store::{
    AssetDomainPolicy, AssetStore, DetectedFormat, KanjiDomainPolicy, PitchAccentDomainPolicy,
    Provenance, SemanticDecision, SemanticStatus, SemanticValidator, StoreOptions,
    ValidationEvidence, ValidatorFailure, ValidatorIdentity, VerifiedIngestRequest,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;

/// Экспорт с двумя полями одной модели: kanji-обработчик на `Заголовок` и
/// pitch-обработчик на `Толкование`. Это фактическая форма целевой модели.
const EXPORT: &str = r#"{
  "__type__": "Deck",
  "name": "Test::Deck",
  "crowdanki_uuid": "deck-uuid-1",
  "deck_config_uuid": "cfg-1",
  "children": [],
  "media_files": ["飴.gif", "飴.png"],
  "note_models": [
    {
      "__type__": "NoteModel",
      "crowdanki_uuid": "model-1",
      "name": "Тестовая модель",
      "css": "",
      "flds": [
        {"name": "Заголовок", "ord": 0},
        {"name": "Толкование", "ord": 1}
      ],
      "tmpls": [
        {"name": "Карточка 1", "ord": 0, "qfmt": "{{Заголовок}}", "afmt": "{{FrontSide}}{{Толкование}}"}
      ]
    }
  ],
  "deck_configurations": [
    {"__type__": "DeckConfig", "crowdanki_uuid": "cfg-1", "name": "По умолчанию"}
  ],
  "notes": [
    {
      "__type__": "Note",
      "guid": "guid-1",
      "note_model_uuid": "model-1",
      "tags": [],
      "fields": ["<img src=\"飴.gif\">", "<img src=\"飴.png\">"]
    }
  ]
}"#;

const KANJI_PNG: &[u8] = b"\x89PNG\r\n\x1a\n-kanji-canonical";
const PITCH_PNG: &[u8] = b"\x89PNG\r\n\x1a\n-pitch-canonical";
const LEGACY_PNG: &[u8] = b"\x89PNG\r\n\x1a\n-legacy-pitch";
const LEGACY_GIF: &[u8] = b"GIF89a-legacy-kanji";

struct Stub;
impl SemanticValidator for Stub {
    fn identity(&self) -> ValidatorIdentity {
        KanjiImageValidator::validator_identity()
    }
    fn validate(
        &self,
        _: &asset_store::AssetRecord,
        _: &mut dyn std::io::Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "synthetic".into(),
                summary: "контролируемые байты тестовой фикстуры".into(),
                details: None,
            }],
        ))
    }
}

struct PitchStub;
impl SemanticValidator for PitchStub {
    fn identity(&self) -> ValidatorIdentity {
        PitchAccentImageValidator::validator_identity()
    }
    fn validate(
        &self,
        _: &asset_store::AssetRecord,
        _: &mut dyn std::io::Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "synthetic".into(),
                summary: "контролируемые байты тестовой фикстуры".into(),
                details: None,
            }],
        ))
    }
}

struct Fixture {
    /// Каталог живёт ровно столько же, сколько фикстура.
    _temp: TempDir,
    export: PathBuf,
    options: MediaOptions,
}

impl Fixture {
    fn new() -> Self {
        Self::with_export(EXPORT)
    }

    fn with_export(document: &str) -> Self {
        let temp = TempDir::new("migrate-media");
        let export = temp.path().join("export");
        fs::create_dir(&export).unwrap();
        fs::create_dir(export.join("media")).unwrap();
        let config = temp.path().join("create.yaml");
        fs::write(
            &config,
            "schema_version: 1\nnote_models:\n  - crowdanki_uuid: 'model-1'\n    fields:\n      Заголовок:\n        processors:\n          - type: kanji_assets\n      Толкование:\n        processors:\n          - type: pitch_accent\n",
        )
        .unwrap();
        let fixture = Self {
            options: MediaOptions {
                config: Some(config),
                asset_store: Some(temp.path().join("kanji-store")),
                pitch_asset_store: Some(temp.path().join("pitch-store")),
            },
            _temp: temp,
            export,
        };
        fixture.set_export(document);
        fixture
    }

    fn set_export(&self, document: &str) {
        let value: Value = serde_json::from_str(document).unwrap();
        fs::write(
            self.export.join("deck.json"),
            crate::loader::render_canonical_bytes(&value).unwrap(),
        )
        .unwrap();
    }

    fn config_path(&self) -> &Path {
        self.options.config.as_deref().unwrap()
    }

    fn kanji(&self, character: &str, bytes: &[u8]) -> String {
        let store =
            AssetStore::open_kanji(StoreOptions::new(self.options.asset_store.clone().unwrap()))
                .unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", character).unwrap(),
                    bytes: bytes.to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "synthetic.png".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &Stub,
            )
            .unwrap()
            .sha256
    }

    fn pitch(&self, surface: &str, bytes: &[u8]) -> String {
        let store = AssetStore::open_with_policy(
            StoreOptions::new(self.options.pitch_asset_store.clone().unwrap()),
            PitchAccentDomainPolicy,
        )
        .unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("pitch_accent", surface).unwrap(),
                    bytes: bytes.to_vec(),
                    provenance: Provenance {
                        source_kind: "jpdb_browser_capture".into(),
                        source_name: "synthetic.pitch.png".into(),
                    },
                    domain_metadata: Some(json!({
                        "surface": surface,
                        "reading": "あめ",
                        "vocabulary_id": 1,
                    })),
                    replace_expected_sha256: None,
                },
                &PitchStub,
            )
            .unwrap()
            .sha256
    }

    fn write_media(&self, name: &str, bytes: &[u8]) {
        fs::write(self.export.join("media").join(name), bytes).unwrap();
    }

    fn read_media(&self, name: &str) -> Option<Vec<u8>> {
        fs::read(self.export.join("media").join(name)).ok()
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&fs::read(self.export.join("deck.json")).unwrap()).unwrap()
    }

    fn media_files(&self) -> Vec<String> {
        self.document()["media_files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry.as_str().unwrap().to_owned())
            .collect()
    }

    fn field(&self, guid: &str, ord: usize) -> String {
        self.document()["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|note| note["guid"] == guid)
            .unwrap()["fields"][ord]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn migrate(&self, apply: bool) -> Result<MigrationResult, DomainError> {
        self.migrate_as("pitch_accent", "飴", "飴.png", apply)
    }

    fn migrate_as(
        &self,
        namespace: &str,
        key: &str,
        from: &str,
        apply: bool,
    ) -> Result<MigrationResult, DomainError> {
        migrate(
            &self.export,
            &MigrationRequest {
                namespace: namespace.into(),
                key: key.into(),
                legacy_filename: from.into(),
            },
            &self.options,
            apply,
        )
    }

    fn create(&self, values: &[(&str, &str)]) -> Result<CreateResult, DomainError> {
        let fields = values
            .iter()
            .map(|(name, value)| ((*name).to_string(), json!(value)))
            .collect::<serde_json::Map<_, _>>();
        let request = parse_request_bytes(
            &serde_json::to_vec(&json!({
                "schema_version": 1,
                "notes": [{
                    "guid": "New0",
                    "deck": {"crowdanki_uuid": "deck-uuid-1"},
                    "model": {"mode": "explicit", "crowdanki_uuid": "model-1"},
                    "fields": fields,
                    "tags": ["TEMP"],
                }],
            }))
            .unwrap(),
            "synthetic",
        )
        .unwrap();
        create_with_options(&self.export, &request, true, None, &self.options)
    }
}

fn reason(error: &DomainError, expected: &str) {
    assert_eq!(error.details["reason"], expected, "{error:?}");
}

/// Заполняет состояние ровно как в live-колоде: legacy pitch-картинка занимает
/// `media/飴.png`, а kanji-домен держит для символа `飴` канонические байты под
/// именем `飴.png`.
fn colliding_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.pitch("飴", PITCH_PNG);
    fixture.kanji("飴", KANJI_PNG);
    fixture.write_media("飴.gif", LEGACY_GIF);
    fixture.write_media("飴.png", LEGACY_PNG);
    fixture
}

#[test]
fn legacy_pitch_reference_migrates_and_frees_the_kanji_fallback_name() {
    let fixture = colliding_fixture();

    // До миграции каноническое имя кандзи занято legacy pitch-байтами, поэтому
    // создание заметки с kanji-fallback обязано отказать, а не перезаписать файл.
    let blocked = fixture
        .create(&[
            ("Заголовок", "<img src=\"飴.png\">"),
            ("Толкование", "<img src=\"飴.pitch.png\">"),
        ])
        .unwrap_err();
    reason(&blocked, "destination_media_conflict");

    let dry = fixture.migrate(false).unwrap();
    assert!(dry.dry_run && !dry.applied && dry.changed);
    assert_eq!(dry.legacy_filename, "飴.png");
    assert_eq!(dry.canonical_filename, "飴.pitch.png");
    assert_eq!(dry.canonical_action, "copy");
    assert_eq!(
        dry.canonical_sha256,
        format!("{:x}", Sha256::digest(PITCH_PNG))
    );
    assert_eq!(dry.references_total, 1);
    assert_eq!(dry.references[0].guid.as_deref(), Some("guid-1"));
    assert_eq!(dry.references[0].field, "Толкование");
    assert_eq!(dry.references[0].field_ord, 1);
    assert_eq!(dry.references[0].model_uuid, "model-1");
    assert_eq!(dry.media_files_added, vec!["飴.pitch.png".to_string()]);
    assert_eq!(dry.media_files_removed, vec!["飴.png".to_string()]);
    assert!(dry.legacy_declared);
    let digest = dry.legacy_media.as_ref().expect("legacy-файл на месте");
    assert_eq!(digest.byte_length, LEGACY_PNG.len());
    assert_eq!(digest.sha256, format!("{:x}", Sha256::digest(LEGACY_PNG)));
    // Dry-run ничего не записал.
    assert_eq!(fixture.read_media("飴.png").as_deref(), Some(LEGACY_PNG));
    assert_eq!(fixture.read_media("飴.pitch.png"), None);
    assert_eq!(fixture.field("guid-1", 1), "<img src=\"飴.png\">");

    let applied = fixture.migrate(true).unwrap();
    assert!(applied.applied && !applied.dry_run && applied.changed);
    assert_eq!(fixture.field("guid-1", 1), "<img src=\"飴.pitch.png\">");
    assert_eq!(fixture.media_files(), vec!["飴.gif", "飴.pitch.png"]);
    assert_eq!(fixture.read_media("飴.png"), None, "legacy-имя освобождено");
    assert_eq!(
        fixture.read_media("飴.pitch.png").as_deref(),
        Some(PITCH_PNG),
        "под каноническим именем лежат проверенные байты домена, а не переименованные legacy"
    );

    // Повторный прогон ничего не меняет: контракт идемпотентен.
    let before = fs::read(fixture.export.join("deck.json")).unwrap();
    let repeat = fixture.migrate(true).unwrap();
    assert!(!repeat.changed && !repeat.applied && repeat.references_total == 0);
    assert_eq!(fixture.media_files(), vec!["飴.gif", "飴.pitch.png"]);
    assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), before);

    // Освобождённое имя принимает канонический kanji-fallback.
    let created = fixture
        .create(&[
            ("Заголовок", "<img src=\"飴.png\">"),
            ("Толкование", "<img src=\"飴.pitch.png\">"),
        ])
        .unwrap();
    assert_eq!(created.notes_created, 1);
    assert_eq!(fixture.read_media("飴.png").as_deref(), Some(KANJI_PNG));
    assert_eq!(
        fixture.read_media("飴.pitch.png").as_deref(),
        Some(PITCH_PNG)
    );
}

#[test]
fn canonical_name_that_already_holds_domain_bytes_is_reused() {
    let fixture = Fixture::new();
    fixture.pitch("飴", PITCH_PNG);
    fixture.write_media("飴.gif", LEGACY_GIF);
    fixture.write_media("飴.png", LEGACY_PNG);
    // Каноническое имя уже занято ровно теми байтами, что лежат в хранилище.
    fixture.write_media("飴.pitch.png", PITCH_PNG);

    let result = fixture.migrate(true).unwrap();
    assert_eq!(result.canonical_action, "reuse");
    assert!(result.applied && result.changed);
    assert_eq!(result.media_files_removed, vec!["飴.png".to_string()]);
    assert_eq!(fixture.read_media("飴.png"), None);
    assert_eq!(
        fixture.read_media("飴.pitch.png").as_deref(),
        Some(PITCH_PNG)
    );
    assert_eq!(fixture.field("guid-1", 1), "<img src=\"飴.pitch.png\">");
}

#[test]
fn reference_outside_the_domain_owned_field_fails_closed() {
    let fixture = Fixture::new();
    fixture.pitch("飴", PITCH_PNG);
    fixture.write_media("飴.png", LEGACY_PNG);
    // Ссылка лежит в поле kanji-обработчика: доказать pitch-семантику нельзя.
    fixture.set_export(&EXPORT.replace(
        "[\"<img src=\\\"飴.gif\\\">\", \"<img src=\\\"飴.png\\\">\"]",
        "[\"<img src=\\\"飴.png\\\">\", \"текст\"]",
    ));
    let before = fs::read(fixture.export.join("deck.json")).unwrap();

    let error = fixture.migrate(true).unwrap_err();
    reason(&error, REASON_UNPROVEN);
    assert_eq!(error.details["references"][0]["field"], "Заголовок");
    assert_eq!(
        error.details["references"][0]["reason"],
        "field_not_owned_by_domain"
    );
    assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), before);
    assert_eq!(fixture.read_media("飴.png").as_deref(), Some(LEGACY_PNG));
    assert_eq!(fixture.read_media("飴.pitch.png"), None);
}

#[test]
fn reference_shape_outside_the_claimable_form_fails_closed() {
    let fixture = Fixture::new();
    fixture.pitch("飴", PITCH_PNG);
    fixture.write_media("飴.png", LEGACY_PNG);
    // Поле принадлежит pitch-обработчику, но форма ссылки — CSS, а не `<img src>`.
    fixture.set_export(&EXPORT.replace(
        "\"<img src=\\\"飴.png\\\">\"",
        "\"<div style=\\\"background:url(飴.png)\\\"></div>\"",
    ));

    let error = fixture.migrate(false).unwrap_err();
    reason(&error, REASON_UNPROVEN);
    assert_eq!(
        error.details["references"][0]["reason"],
        "reference_shape_not_claimable"
    );
    assert_eq!(error.details["references"][0]["occurrences"], 1);
    assert_eq!(error.details["references"][0]["claimable"], 0);
}

#[test]
fn kanji_legacy_extension_migrates_to_the_canonical_consumer_name() {
    // Второй домен проходит тот же путь: legacy-имя `飴.gif` из прежней схемы
    // хранения уступает каноническому `飴.png` текущей.
    let fixture = Fixture::new();
    fixture.kanji("飴", KANJI_PNG);
    fixture.write_media("飴.gif", LEGACY_GIF);
    fixture.set_export(
        &EXPORT
            .replace(
                "\"media_files\": [\"飴.gif\", \"飴.png\"]",
                "\"media_files\": [\"飴.gif\"]",
            )
            .replace("\"<img src=\\\"飴.png\\\">\"", "\"текст\""),
    );

    let result = fixture.migrate_as("kanji", "飴", "飴.gif", true).unwrap();
    assert_eq!(result.identity.namespace, "kanji");
    assert_eq!(result.canonical_filename, "飴.png");
    assert_eq!(result.references_total, 1);
    assert_eq!(result.references[0].field, "Заголовок");
    assert_eq!(fixture.field("guid-1", 0), "<img src=\"飴.png\">");
    assert_eq!(fixture.media_files(), vec!["飴.png"]);
    assert_eq!(fixture.read_media("飴.gif"), None);
    assert_eq!(fixture.read_media("飴.png").as_deref(), Some(KANJI_PNG));
}

#[test]
fn export_without_legacy_references_is_a_noop() {
    let fixture = Fixture::new();
    fixture.pitch("飴", PITCH_PNG);
    fixture.write_media("飴.gif", LEGACY_GIF);
    fixture.set_export(&EXPORT.replace("\"<img src=\\\"飴.png\\\">\"", "\"текст\""));
    let before = fs::read(fixture.export.join("deck.json")).unwrap();

    let result = fixture.migrate(true).unwrap();
    assert!(!result.changed && !result.applied && !result.dry_run);
    assert_eq!(result.references_total, 0);
    assert!(result.media_files_added.is_empty());
    assert!(result.media_files_removed.is_empty());
    assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), before);
}

#[test]
fn occupied_canonical_name_with_other_bytes_fails_closed() {
    let fixture = colliding_fixture();
    fixture.write_media("飴.pitch.png", b"\x89PNG\r\n\x1a\n-someone-else");

    let error = fixture.migrate(false).unwrap_err();
    reason(&error, "destination_media_conflict");
    assert_eq!(fixture.field("guid-1", 1), "<img src=\"飴.png\">");
}

#[test]
fn duplicate_legacy_declaration_fails_closed() {
    let fixture = colliding_fixture();
    fixture.set_export(&EXPORT.replace(
        "\"media_files\": [\"飴.gif\", \"飴.png\"],",
        "\"media_files\": [\"飴.gif\", \"飴.png\", \"飴.png\"],",
    ));

    let error = fixture.migrate(false).unwrap_err();
    reason(&error, "media_declaration_conflict");
}

#[test]
fn unknown_namespace_and_unsafe_filename_are_rejected() {
    let fixture = colliding_fixture();

    let unknown = fixture
        .migrate_as("unknown_domain", "飴", "飴.png", false)
        .unwrap_err();
    reason(&unknown, "media_domain_unknown");

    for unsafe_name in ["../飴.png", "media/飴.png", "", "."] {
        let error = fixture
            .migrate_as("pitch_accent", "飴", unsafe_name, false)
            .unwrap_err();
        reason(&error, REASON_INVALID);
    }

    let mismatch = fixture
        .migrate_as("pitch_accent", "語", "飴.png", false)
        .unwrap_err();
    reason(&mismatch, "pitch_asset_missing");
}

#[test]
fn configured_policy_must_name_the_legacy_field() {
    let fixture = colliding_fixture();
    // Тот же экспорт, но обработчик на поле не включён.
    fs::write(
        fixture.config_path(),
        "schema_version: 1\nnote_models:\n  - crowdanki_uuid: 'model-1'\n    fields:\n      Заголовок:\n        processors:\n          - type: kanji_assets\n",
    )
    .unwrap();

    let error = fixture.migrate(false).unwrap_err();
    reason(&error, REASON_UNPROVEN);
    assert_eq!(
        error.details["references"][0]["reason"],
        "field_not_owned_by_domain"
    );
}

#[test]
fn legacy_name_equal_to_the_canonical_one_is_rejected() {
    let fixture = colliding_fixture();

    // `飴.png` — уже каноническое имя kanji-домена: мигрировать его некуда.
    let error = fixture
        .migrate_as("kanji", "飴", "飴.png", false)
        .unwrap_err();
    reason(&error, REASON_INVALID);
    assert_eq!(error.details["domain"], "kanji");
}

#[test]
fn kanji_domain_policy_is_the_owner_of_the_consumer_name() {
    // Имена, на которые опирается миграция, берутся у владельца домена, а не из
    // памяти: расхождение сделало бы команду бесполезной на live-данных.
    let pitch = PitchAccentDomainPolicy
        .canonical_location(
            &AssetIdentity::new("pitch_accent", "飴").unwrap(),
            &format!("{:x}", Sha256::digest(PITCH_PNG)),
            DetectedFormat::Png,
        )
        .unwrap();
    assert_eq!(pitch.consumer_filename, "飴.pitch.png");
    let kanji = KanjiDomainPolicy
        .canonical_location(
            &AssetIdentity::new("kanji", "飴").unwrap(),
            &format!("{:x}", Sha256::digest(KANJI_PNG)),
            DetectedFormat::Png,
        )
        .unwrap();
    assert_eq!(kanji.consumer_filename, "飴.png");
}
