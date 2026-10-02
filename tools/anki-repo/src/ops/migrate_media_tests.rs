//! Только синтетические экспорты и изолированные хранилища: контракт миграции
//! проверяется без пользовательской колоды и без сети.
//!
//! Ключевой случай — ровно тот, ради которого команда существует: одно и то же
//! имя `飴.png` занято legacy pitch-картинкой, а каноническое имя изображения
//! символа `飴` в kanji-домене совпадает с ним. После миграции pitch-ссылка
//! уходит на `飴.pitch.png`, имя `飴.png` освобождается и запасное изображение кандзи
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

/// Заполняет состояние ровно как в пользовательской колоде: legacy pitch-картинка занимает
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
    // создание заметки с запасное изображение кандзи обязано отказать, а не перезаписать файл.
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

    // Освобождённое имя принимает канонический запасное изображение кандзи.
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
    reason(&mismatch, REASON_IDENTITY_UNPROVEN);
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
    // памяти: расхождение сделало бы команду бесполезной на данных пользователя.
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

fn set_value(fixture: &Fixture, value: &Value) {
    fixture.set_export(&serde_json::to_string(value).unwrap());
}

fn assert_refusal_preserves_export(fixture: &Fixture, expected_reason: &str) -> DomainError {
    let before = fs::read(fixture.export.join("deck.json")).unwrap();
    let declarations = fixture.document();
    for apply in [false, true] {
        let error = fixture.migrate(apply).unwrap_err();
        reason(&error, expected_reason);
        assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), before);
        assert_eq!(fixture.document(), declarations);
        assert_eq!(fixture.read_media("飴.png").as_deref(), Some(LEGACY_PNG));
        assert_eq!(fixture.read_media("飴.pitch.png"), None);
    }
    fixture.migrate(false).unwrap_err()
}

#[test]
fn identity_binding_refuses_other_identity_or_other_filename_before_store_read() {
    let fixture = colliding_fixture();
    fixture.pitch("幽霊", PITCH_PNG);
    let before = fs::read(fixture.export.join("deck.json")).unwrap();
    for apply in [false, true] {
        for (key, filename) in [("幽霊", "飴.png"), ("飴", "幽霊.png"), ("未登録", "飴.png")]
        {
            let error = fixture
                .migrate_as("pitch_accent", key, filename, apply)
                .unwrap_err();
            reason(&error, REASON_IDENTITY_UNPROVEN);
        }
    }
    assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), before);
    assert_eq!(fixture.read_media("飴.png").as_deref(), Some(LEGACY_PNG));
    assert_eq!(fixture.read_media("飴.pitch.png"), None);
}

#[test]
fn template_and_model_css_consumers_block_release_without_mutations() {
    for surface in ["qfmt", "afmt", "css"] {
        let fixture = colliding_fixture();
        let mut value = fixture.document();
        if surface == "css" {
            value["note_models"][0][surface] = json!(".card { background: url(飴.png) }");
        } else {
            value["note_models"][0]["tmpls"][0][surface] =
                json!("<img src=\"飴.png\">{{Толкование}}");
        }
        set_value(&fixture, &value);
        let error = assert_refusal_preserves_export(&fixture, REASON_UNPROVEN);
        assert_eq!(error.details["references"][0]["surface"], surface);
        assert_eq!(
            error.details["references"][0]["reason"],
            "static_model_reference_not_owned_by_domain"
        );
        assert_eq!(error.details["references"][0]["model_uuid"], "model-1");
        assert_eq!(error.details["references"][0]["children_path"], json!([]));
    }
}

#[test]
fn query_and_percent_encoded_static_consumers_block_release() {
    for reference in ["飴.png?cache=1", "%E9%A3%B4.png"] {
        let fixture = colliding_fixture();
        let mut value = fixture.document();
        value["note_models"][0]["tmpls"][0]["qfmt"] = json!(format!("<img src=\"{reference}\">"));
        set_value(&fixture, &value);
        let error = assert_refusal_preserves_export(&fixture, REASON_UNPROVEN);
        assert_eq!(error.details["references"][0]["surface"], "qfmt");
    }
}

#[test]
fn escaped_css_and_imports_use_the_shared_scanner_for_inventory() {
    for css in [
        r".card { background: u\72l(\98f4.png) }",
        "@import '飴.png';",
        "@import url(飴.png);",
    ] {
        let fixture = colliding_fixture();
        let mut value = fixture.document();
        value["note_models"][0]["css"] = json!(css);
        set_value(&fixture, &value);
        assert_refusal_preserves_export(&fixture, REASON_UNPROVEN);
    }
    let fixture = colliding_fixture();
    let mut value = fixture.document();
    value["notes"][0]["fields"][1] = json!(r"<div style='background:url(\98f4.png)'></div>");
    set_value(&fixture, &value);
    assert_refusal_preserves_export(&fixture, REASON_UNPROVEN);
}

#[test]
fn static_consumers_in_child_models_block_even_with_repeated_model_uuid() {
    let fixture = colliding_fixture();
    let mut value = fixture.document();
    let mut model = value["note_models"][0].clone();
    model["css"] = json!(".card { background:url(飴.png) }");
    value["children"] = json!([{
        "__type__":"Deck", "name":"Test::Child", "crowdanki_uuid":"child-1", "deck_config_uuid":"cfg-1",
        "children":[], "notes":[], "media_files":[], "note_models":[model]
    }]);
    set_value(&fixture, &value);
    let error = assert_refusal_preserves_export(&fixture, REASON_UNPROVEN);
    assert_eq!(error.details["references"][0]["children_path"], json!([0]));
}

#[test]
fn repeated_unicode_and_ascii_links_are_rebuilt_from_the_original_text() {
    for surface in ["飴", "candy"] {
        let fixture = Fixture::new();
        fixture.pitch(surface, PITCH_PNG);
        let legacy = format!("{surface}.png");
        let canonical = format!("{surface}.pitch.png");
        let original = format!(
            "начало <img src=\"{legacy}\"> промежуток 漢字 <img SRC='{legacy}'> / <IMG src={legacy}> конец"
        );
        let mut value = fixture.document();
        value["notes"][0]["fields"][1] = json!(original);
        value["media_files"] = json!(["unrelated-before.png", legacy, "unrelated-after.gif"]);
        set_value(&fixture, &value);
        fixture.write_media(&legacy, LEGACY_PNG);
        let before = fs::read(fixture.export.join("deck.json")).unwrap();
        let dry = fixture
            .migrate_as("pitch_accent", surface, &legacy, false)
            .unwrap();
        assert_eq!(dry.references_total, 3);
        assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), before);
        let applied = fixture
            .migrate_as("pitch_accent", surface, &legacy, true)
            .unwrap();
        assert_eq!(applied.references_total, 3);
        assert_eq!(
            fixture.field("guid-1", 1),
            original.replace(&legacy, &canonical)
        );
        assert_eq!(
            fixture.media_files(),
            vec!["unrelated-before.png", &canonical, "unrelated-after.gif"]
        );
        let after = fs::read(fixture.export.join("deck.json")).unwrap();
        let repeat = fixture
            .migrate_as("pitch_accent", surface, &legacy, true)
            .unwrap();
        assert_eq!(repeat.references_total, 0);
        assert!(!repeat.changed);
        assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), after);
    }
}

fn child_migration_value(fixture: &Fixture) -> Value {
    let mut value = fixture.document();
    let notes = value["notes"].take();
    value["notes"] = json!([]);
    value["media_files"] = json!(["root-unrelated.png"]);
    value["children"] = json!([{
        "__type__":"Deck", "name":"Test::Child", "crowdanki_uuid":"child-1", "deck_config_uuid":"cfg-1",
        "children":[], "notes":notes, "media_files":["before.png", "飴.png", "after.gif"], "note_models":[]
    }, {
        "__type__":"Deck", "name":"Test::Other", "crowdanki_uuid":"child-2", "deck_config_uuid":"cfg-1",
        "children":[], "notes":[], "media_files":["other.png", "other.gif"], "note_models":[]
    }]);
    value
}

#[test]
fn child_declaration_is_replaced_in_place_and_repeat_is_a_noop() {
    let fixture = colliding_fixture();
    let value = child_migration_value(&fixture);
    set_value(&fixture, &value);
    let result = fixture.migrate(true).unwrap();
    assert!(result.legacy_declared && !result.legacy_declared_after);
    assert_eq!(result.media_files_removed, vec!["飴.png"]);
    let after = fixture.document();
    assert_eq!(after["media_files"], value["media_files"]);
    assert_eq!(after["children"][1], value["children"][1]);
    assert_eq!(
        after["children"][0]["media_files"],
        json!(["before.png", "飴.pitch.png", "after.gif"])
    );
    assert_eq!(
        after["children"][0]["notes"][0]["fields"][1],
        "<img src=\"飴.pitch.png\">"
    );
    assert_eq!(fixture.read_media("飴.png"), None);
    let bytes = fs::read(fixture.export.join("deck.json")).unwrap();
    let repeat = fixture.migrate(true).unwrap();
    assert!(!repeat.changed && !repeat.legacy_declared && !repeat.legacy_released);
    assert_eq!(fs::read(fixture.export.join("deck.json")).unwrap(), bytes);
}

#[test]
fn canonical_declaration_in_another_node_keeps_its_locality() {
    for legacy_in_child in [true, false] {
        let fixture = colliding_fixture();
        let mut value = child_migration_value(&fixture);
        if legacy_in_child {
            value["media_files"] = json!(["root-before.png", "飴.pitch.png", "root-after.png"]);
        } else {
            value["media_files"] = json!(["root-before.png", "飴.png", "root-after.png"]);
            value["children"][0]["media_files"] =
                json!(["before.png", "飴.pitch.png", "after.gif"]);
        }
        set_value(&fixture, &value);
        let result = fixture.migrate(true).unwrap();
        assert!(result.media_files_added.is_empty());
        let after = fixture.document();
        if legacy_in_child {
            assert_eq!(after["media_files"], value["media_files"]);
            assert_eq!(
                after["children"][0]["media_files"],
                json!(["before.png", "after.gif"])
            );
        } else {
            assert_eq!(
                after["media_files"],
                json!(["root-before.png", "root-after.png"])
            );
            assert_eq!(
                after["children"][0]["media_files"],
                value["children"][0]["media_files"]
            );
        }
        assert_eq!(after["children"][1], value["children"][1]);
    }
}

#[test]
fn duplicate_legacy_or_canonical_declarations_across_nodes_refuse_without_mutations() {
    for filename in ["飴.png", "飴.pitch.png"] {
        let fixture = colliding_fixture();
        let mut value = child_migration_value(&fixture);
        if filename == "飴.pitch.png" {
            value["children"][0]["media_files"] = json!(["飴.png", filename]);
        }
        value["children"][1]["media_files"] = json!(["other.png", filename]);
        set_value(&fixture, &value);
        let error = assert_refusal_preserves_export(&fixture, "media_declaration_conflict");
        assert_eq!(error.details["evidence"]["filename"], filename);
        assert_eq!(
            error.details["evidence"]["children_paths"],
            json!([[0], [1]])
        );
    }
}

#[test]
fn undeclared_legacy_media_is_released_after_all_consumers_are_proven() {
    let fixture = colliding_fixture();
    let mut value = child_migration_value(&fixture);
    value["children"][0]["media_files"] = json!(["before.png", "after.gif"]);
    set_value(&fixture, &value);
    let result = fixture.migrate(true).unwrap();
    assert!(!result.legacy_declared);
    assert!(result.legacy_released);
    assert_eq!(fixture.read_media("飴.png"), None);
    assert_eq!(
        fixture.document()["children"][0]["media_files"],
        json!(["before.png", "after.gif", "飴.pitch.png"])
    );
}

#[test]
fn migration_render_preserves_pre_state_and_reports_actual_post_state() {
    let fixture = colliding_fixture();
    for (apply, expect_released, exists_after) in [
        (false, false, true),
        (true, true, false),
        (true, false, false),
    ] {
        let result = fixture.migrate(apply).unwrap();
        let json: Value =
            serde_json::from_str(&crate::render::json::migrate_media_json(&result)).unwrap();
        let data = &json["result"];
        assert_eq!(data["legacy_released"], expect_released);
        assert_eq!(data["legacy_media_exists_after"], exists_after);
        assert_eq!(data["legacy_declared_after"], exists_after);
        if result.changed {
            assert_eq!(
                data["legacy_media"]["sha256"],
                format!("{:x}", Sha256::digest(LEGACY_PNG))
            );
        } else {
            assert!(data["legacy_media"].is_null());
        }
        let human = crate::render::human::migrate_media(&result);
        assert!(human.contains("Прежний файл до миграции:"));
        assert_eq!(
            human.contains("После применения legacy-имя освобождено"),
            expect_released
        );
        assert!(!human.contains("Освободившееся имя занято файлом"));
        if !result.changed {
            assert!(human.contains("Мигрировать нечего"));
        }
    }
}

#[test]
fn applying_migration_reports_when_legacy_file_was_already_absent() {
    let fixture = colliding_fixture();
    fs::remove_file(fixture.export.join("media").join("飴.png")).unwrap();

    let result = fixture.migrate(true).unwrap();

    assert!(result.changed);
    assert!(!result.legacy_released);
    assert!(!result.legacy_media_exists_after);
    assert!(result.legacy_media.is_none());
    let human = crate::render::human::migrate_media(&result);
    assert!(human.contains("Прежний файл до миграции: media/飴.png физически отсутствовал"));
    assert!(human.contains("После завершения команды legacy-файл отсутствует"));
    assert!(!human.contains("После применения legacy-имя освобождено"));
}

#[test]
fn canonical_declaration_in_sibling_is_promoted_to_cover_migrated_consumers() {
    let fixture = colliding_fixture();
    let mut value = child_migration_value(&fixture);
    value["children"][1]["media_files"] =
        json!(["other-before.png", "飴.pitch.png", "other-after.gif"]);
    set_value(&fixture, &value);
    let result = fixture.migrate(true).unwrap();
    assert_eq!(result.media_files_added, vec!["飴.pitch.png"]);
    assert_eq!(result.media_files_removed, vec!["飴.png", "飴.pitch.png"]);
    let after = fixture.document();
    assert_eq!(
        after["media_files"],
        json!(["root-unrelated.png", "飴.pitch.png"])
    );
    assert_eq!(
        after["children"][0]["media_files"],
        json!(["before.png", "after.gif"])
    );
    assert_eq!(
        after["children"][1]["media_files"],
        json!(["other-before.png", "other-after.gif"])
    );
    assert_eq!(fixture.read_media("飴.png"), None);
}

#[test]
fn one_declaration_covers_migrated_notes_in_multiple_subtrees() {
    let fixture = colliding_fixture();
    let mut value = child_migration_value(&fixture);
    let mut second = value["children"][0]["notes"][0].clone();
    second["guid"] = json!("guid-2");
    value["children"][1]["notes"] = json!([second]);
    set_value(&fixture, &value);
    let result = fixture.migrate(true).unwrap();
    assert_eq!(result.references_total, 2);
    let after = fixture.document();
    assert_eq!(
        after["media_files"],
        json!(["root-unrelated.png", "飴.pitch.png"])
    );
    assert_eq!(
        after["children"][0]["media_files"],
        json!(["before.png", "after.gif"])
    );
    assert_eq!(
        after["children"][1]["media_files"],
        value["children"][1]["media_files"]
    );
    assert_eq!(
        after["children"][1]["notes"][0]["fields"][1],
        "<img src=\"飴.pitch.png\">"
    );
}

#[test]
fn entity_encoded_static_or_unclaimable_field_reference_blocks_release() {
    for surface in ["qfmt", "note_field"] {
        let fixture = colliding_fixture();
        let mut value = fixture.document();
        let reference = json!("<img src='&#39156;.png'>");
        if surface == "qfmt" {
            value["note_models"][0]["tmpls"][0]["qfmt"] = reference;
        } else {
            value["notes"][0]["fields"][1] = reference;
        }
        set_value(&fixture, &value);
        let error = assert_refusal_preserves_export(&fixture, REASON_UNPROVEN);
        assert_eq!(error.details["references"][0]["surface"], surface);
    }
}
