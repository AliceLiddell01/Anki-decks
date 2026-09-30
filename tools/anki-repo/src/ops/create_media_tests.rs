//! Только синтетические exports и изолированные stores. Stub validator задаёт
//! lifecycle evidence fixture; production pixel algorithm тестируется владельцем.
use super::*;
use crate::ops::create::{create_with_options, parse_request_bytes};
use crate::test_support::{MINIMAL_EXPORT, TempDir};
use asset_store::{
    AssetRecord, Provenance, SemanticDecision, SemanticStatus, SemanticValidator, StoreOptions,
    ValidationEvidence, ValidatorFailure, ValidatorIdentity, VerifiedIngestRequest,
};
use serde_json::json;
use std::fs;

struct Fixture {
    temp: TempDir,
    export: PathBuf,
    options: MediaOptions,
}
struct Stub;
impl SemanticValidator for Stub {
    fn identity(&self) -> ValidatorIdentity {
        KanjiImageValidator::validator_identity()
    }
    fn validate(
        &self,
        _: &AssetRecord,
        _: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "synthetic".into(),
                summary: "контролируемые fixture bytes".into(),
                details: None,
            }],
        ))
    }
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new("configured-media");
        let export = temp.path().join("export");
        fs::create_dir(&export).unwrap();
        let value: Value = serde_json::from_str(MINIMAL_EXPORT).unwrap();
        fs::write(
            export.join("deck.json"),
            crate::loader::render_canonical_bytes(&value).unwrap(),
        )
        .unwrap();
        let config = temp.path().join("create.yaml");
        fs::write(&config, policy("model-1", "Заголовок")).unwrap();
        let options = MediaOptions {
            config: Some(config),
            asset_store: Some(temp.path().join("store")),
        };
        Self {
            temp,
            export,
            options,
        }
    }
    fn bytes(&self) -> Vec<u8> {
        fs::read(self.export.join("deck.json")).unwrap()
    }
    fn asset(&self, character: &str, bytes: &[u8], previous: Option<String>) -> String {
        let store =
            AssetStore::open(StoreOptions::new(self.options.asset_store.clone().unwrap())).unwrap();
        let result = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", character).unwrap(),
                    bytes: bytes.to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "synthetic.gif".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: previous,
                },
                &Stub,
            )
            .unwrap();
        result.sha256
    }
    fn request(&self, fields: &[&str]) -> crate::ops::create::CreateRequest {
        let notes = fields.iter().enumerate().map(|(i, field)| json!({"guid": format!("New{i}"), "deck": {"crowdanki_uuid": "deck-uuid-1"}, "model": {"mode": "explicit", "crowdanki_uuid": "model-1"}, "fields": {"Заголовок": field, "Толкование": "значение"}, "tags": ["TEMP"]})).collect::<Vec<_>>();
        parse_request_bytes(
            &serde_json::to_vec(&json!({"schema_version":1,"notes":notes})).unwrap(),
            "synthetic",
        )
        .unwrap()
    }
    fn run(
        &self,
        fields: &[&str],
        apply: bool,
    ) -> Result<crate::ops::create::CreateResult, DomainError> {
        create_with_options(
            &self.export,
            &self.request(fields),
            apply,
            None,
            &self.options,
        )
    }
    fn resolved_request(&self, fields: &[&str]) -> crate::ops::create::CreateRequest {
        let result = self.run(fields, false).unwrap();
        parse_request_bytes(
            &serde_json::to_vec(&result.resolved_request).unwrap(),
            "resolved",
        )
        .unwrap()
    }
}
const GIF: &[u8] = b"GIF89a-synthetic-one";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-synthetic";
fn policy(uuid: &str, field: &str) -> String {
    format!(
        "schema_version: 1\nnote_models:\n  - crowdanki_uuid: '{uuid}'\n    fields:\n      {field}:\n        processors:\n          - type: kanji_assets\n"
    )
}
fn reason(error: &DomainError, expected: &str) {
    assert_eq!(error.details["reason"], expected, "{error:?}");
}

#[test]
fn config_free_and_configured_media_free_have_no_media_side_effects() {
    let f = Fixture::new();
    let r = f.run(&["слово"], false).unwrap();
    assert!(r.media.is_empty());
    assert!(!f.export.join("media").exists());
    assert!(!f.options.asset_store.as_ref().unwrap().exists());
    fs::remove_file(f.options.config.as_ref().unwrap()).unwrap();
    let r = create_with_options(
        &f.export,
        &f.request(&["слово"]),
        true,
        None,
        &MediaOptions::default(),
    )
    .unwrap();
    assert_eq!(r.notes_created, 1);
    assert!(!f.export.join("media").exists());
    let err = create_with_options(
        &f.export,
        &f.request(&["<img src='一.gif'>"]),
        false,
        None,
        &MediaOptions::default(),
    )
    .unwrap_err();
    reason(&err, "config_absent");
}
#[test]
fn batch_deduplicates_exact_assets_and_repairs_media_and_declaration() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    f.asset("二", PNG, None);
    let before = f.bytes();
    let fields = ["<IMG SRC = '一.gif'>", "<img src=一.gif><img src=二.png>"];
    let dry = f.run(&fields, false).unwrap();
    assert_eq!(dry.media.evidence()["mutations_planned"], 2);
    assert_eq!(dry.media.references.len(), 3);
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());
    let applied = f.run(&fields, true).unwrap();
    assert_eq!(applied.media.mutations, 2);
    assert_eq!(applied.notes_created, 2);
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    let value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(
        value["media_files"],
        json!(["a.mp3", "b.png", "一.gif", "二.png"])
    );
    let after = f.bytes();
    let repeat = f.run(&fields, true).unwrap();
    assert!(!repeat.applied);
    assert_eq!(repeat.notes_already_applied, 2);
    assert_eq!(repeat.media.mutations, 0);
    assert_eq!(after, f.bytes());
    fs::remove_file(f.export.join("media/一.gif")).unwrap();
    let repaired = f.run(&fields, true).unwrap();
    assert_eq!(repaired.media.mutations, 1);
    assert!(!repaired.applied);
    assert_eq!(after, f.bytes());
    let mut damaged = value.clone();
    damaged["media_files"] = json!(["a.mp3", "b.png", "二.png"]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&damaged).unwrap(),
    )
    .unwrap();
    let repaired = f.run(&fields, true).unwrap();
    assert!(repaired.applied);
    assert_eq!(repaired.notes_created, 0);
    assert_eq!(repaired.media.mutations, 0);
    assert_eq!(repaired.media.declarations_added, vec!["一.gif"]);
    let current = f.bytes();
    assert!(!f.run(&fields, true).unwrap().applied);
    assert_eq!(current, f.bytes());
}
#[test]
fn config_routing_depends_on_identity_and_exact_field_only() {
    let f = Fixture::new();
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("other-model", "Заголовок"),
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], false).unwrap_err(),
        "processor_not_enabled",
    );
    f.run(&["обычный текст"], false).unwrap(); // неизвестная foreign модель не блокирует.
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("model-1", "Толкование"),
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], false).unwrap_err(),
        "processor_not_enabled",
    );
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("model-1", "УдалённоеПоле"),
    )
    .unwrap();
    reason(
        &f.run(&["обычный текст"], false).unwrap_err(),
        "config_stale_field",
    );
}
#[test]
fn invalid_yaml_fails_closed_including_duplicate_mapping_keys() {
    let f = Fixture::new();
    let good = policy("model-1", "Заголовок");
    let cases = vec![
        good.replace("schema_version: 1", "schema_version: 2"),
        good.replace("kanji_assets", "unknown"),
        good.replace("schema_version: 1", "schema_version: 1\nschema_version: 1"),
        good.replace(
            "crowdanki_uuid: 'model-1'",
            "crowdanki_uuid: 'model-1'\n    crowdanki_uuid: 'model-1'",
        ),
        good.replace(
            "          - type: kanji_assets",
            "          - type: kanji_assets\n          - type: kanji_assets",
        ),
        format!("{good}      Заголовок:\n        processors:\n          - type: kanji_assets\n"),
        format!("{good}{}", good.split("note_models:\n").nth(1).unwrap()),
        good.replace(
            "type: kanji_assets",
            "type: kanji_assets\n            unexpected: true",
        ),
        good.replace(
            "type: kanji_assets",
            "type: kanji_assets\n            type: kanji_assets",
        ),
        "schema_version: 1\nnote_models: [".into(),
        format!("{good}---\n{good}"),
    ];
    for case in cases {
        fs::write(f.options.config.as_ref().unwrap(), &case).unwrap();
        let before = f.bytes();
        let e = match f.run(&["обычный текст"], true) {
            Err(e) => e,
            Ok(_) => panic!("не отклонён YAML: {case}"),
        };
        assert!(
            matches!(
                e.details["reason"].as_str(),
                Some("config_invalid" | "config_unsupported")
            ),
            "{case}: {e:?}"
        );
        assert_eq!(before, f.bytes());
    }
}
#[test]
fn configured_field_claims_only_complete_kanji_img_src() {
    let f = Fixture::new();
    for value in [
        "<img src=x.png>",
        "<audio src=一.gif>",
        "[sound:一.gif]",
        "<img srcset='一.gif 1x'>",
        "<div style='background:url(一.gif)'>",
        "<img src=../一.gif>",
        "<img src=/一.gif>",
        "<img src=https://host/一.gif>",
        "<img src=一.gif?q>",
        "<img src=一.gif#x>",
        "<img src='一.gif'",
        "<img src=一.gif><audio src=一.gif>",
        "<img src=一.gif><style>x{background:url(一.gif)}</style>",
    ] {
        let e = f.run(&[value], true).unwrap_err();
        reason(&e, "media_reference_unclaimed");
        assert!(!f.export.join("media").exists());
    }
}
#[test]
fn missing_integrity_nonverified_and_filename_mismatch_write_nothing() {
    let f = Fixture::new();
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
    f.asset("一", PNG, None);
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "canonical_filename_mismatch",
    );
    let path = f
        .options
        .asset_store
        .as_ref()
        .unwrap()
        .join("assets/一.png");
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"corrupt").unwrap();
    reason(
        &f.run(&["<img src=一.png>"], true).unwrap_err(),
        "asset_integrity_invalid",
    );
    assert_eq!(f.bytes(), before);
    assert!(!f.export.join("media").exists());
}
#[test]
fn pending_and_quarantine_are_never_used_as_canonical() {
    use asset_store::IngestRequest;
    let f = Fixture::new();
    let source = f.temp.path().join("pending.gif");
    fs::write(&source, GIF).unwrap();
    let store =
        AssetStore::open(StoreOptions::new(f.options.asset_store.clone().unwrap())).unwrap();
    store
        .ingest(IngestRequest {
            identity: AssetIdentity::new("kanji", "一").unwrap(),
            source_path: source,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
    let manifest_path = store.root().join(".runtime/manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["assets"][0]["lifecycle"] = json!("quarantined");
    fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
}
#[test]
fn resolved_artifact_pins_hash_and_rejects_changed_corpus() {
    let f = Fixture::new();
    let old = f.asset("一", GIF, None);
    let result = f.run(&["<img src=一.gif>"], false).unwrap();
    assert_eq!(result.resolved_request["media_assets"][0]["sha256"], old);
    let request = parse_request_bytes(
        &serde_json::to_vec(&result.resolved_request).unwrap(),
        "resolved",
    )
    .unwrap();
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    f.asset("一", b"GIF89a-synthetic-two", Some(old));
    let before = f.bytes();
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    assert_eq!(before, f.bytes());
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    fs::write(f.export.join("media/一.gif"), b"GIF89a-synthetic-two").unwrap();
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "destination_media_conflict",
    );
    fs::remove_file(f.export.join("media/一.gif")).unwrap();
    f.run(&["<img src=一.gif>"], true).unwrap();
}

#[test]
fn pinned_missing_asset_and_filename_drift_are_stale_before_destination_checks() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let request = f.resolved_request(&["<img src=一.gif>"]);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), b"foreign destination").unwrap();
    fs::remove_file(
        f.options
            .asset_store
            .as_ref()
            .unwrap()
            .join("assets/一.gif"),
    )
    .unwrap();
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );

    let f = Fixture::new();
    let old = f.asset("一", GIF, None);
    let request = f.resolved_request(&["<img src=一.gif>"]);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    f.asset("一", PNG, Some(old));
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
}

#[test]
fn missing_store_is_stale_for_pinned_requests_but_missing_for_unpinned_requests() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let pinned = f.resolved_request(&["<img src=一.gif>"]);
    fs::remove_dir_all(f.options.asset_store.as_ref().unwrap()).unwrap();

    reason(
        &create_with_options(&f.export, &pinned, false, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    reason(
        &f.run(&["<img src=一.gif>"], false).unwrap_err(),
        "kanji_asset_missing",
    );
}

#[test]
fn resolved_empty_pins_are_distinct_from_legacy_unpinned_requests() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let legacy = f.request(&["обычный текст"]);
    assert_eq!(legacy.media_assets, None);

    let mut resolved = f.resolved_request(&["обычный текст"]);
    assert_eq!(resolved.media_assets, Some(Vec::new()));
    let mut null_pins = f.run(&["обычный текст"], false).unwrap().resolved_request;
    null_pins["media_assets"] = serde_json::Value::Null;
    assert!(parse_request_bytes(&serde_json::to_vec(&null_pins).unwrap(), "null").is_err());
    resolved.notes[0]
        .fields
        .insert("Заголовок".into(), "<img src=一.gif>".into());
    reason(
        &create_with_options(&f.export, &resolved, false, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    let no_store = MediaOptions {
        config: f.options.config.clone(),
        asset_store: None,
    };
    reason(
        &create_with_options(&f.export, &resolved, false, None, &no_store).unwrap_err(),
        "stale_pinned_asset",
    );
}
#[test]
fn destination_conflict_and_symlinks_never_overwrite() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    fs::create_dir(f.export.join("media")).unwrap();
    let destination = f.export.join("media/一.gif");
    fs::write(&destination, b"foreign").unwrap();
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "destination_media_conflict",
    );
    assert_eq!(fs::read(&destination).unwrap(), b"foreign");
    assert_eq!(before, f.bytes());
    fs::remove_file(&destination).unwrap();
    std::os::unix::fs::symlink(
        f.options
            .asset_store
            .as_ref()
            .unwrap()
            .join("assets/一.gif"),
        &destination,
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "destination_media_conflict",
    );
}
#[test]
fn concurrent_destination_appearance_same_bytes_reuses_different_bytes_conflicts() {
    for bytes in [GIF, b"foreign".as_slice()] {
        let f = Fixture::new();
        f.asset("一", GIF, None);
        let before = f.bytes();
        let destination = f.export.join("media/一.gif");
        let owned = bytes.to_vec();
        TEST_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |phase| {
                if phase == "before_link" {
                    fs::write(&destination, &owned).unwrap();
                }
                Ok(())
            }))
        });
        let result = f.run(&["<img src=一.gif>"], true);
        TEST_HOOK.with(|h| *h.borrow_mut() = None);
        if bytes == GIF {
            let r = result.unwrap();
            assert_eq!(r.media.mutations, 0);
            assert_eq!(r.notes_created, 1);
        } else {
            reason(&result.unwrap_err(), "destination_media_conflict");
            assert_eq!(before, f.bytes());
        }
    }
}
#[test]
fn partial_media_failure_does_not_publish_json_and_retry_converges() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    f.asset("二", PNG, None);
    let before = f.bytes();
    TEST_HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(|phase| {
            if phase == "asset_copied" {
                Err(blocker(
                    ErrorCode::WriteFailed,
                    "injected_io_failure",
                    json!({}),
                ))
            } else {
                Ok(())
            }
        }))
    });
    let result = f.run(&["<img src=一.gif><img src=二.png>"], true);
    TEST_HOOK.with(|h| *h.borrow_mut() = None);
    reason(&result.unwrap_err(), "injected_io_failure");
    assert_eq!(before, f.bytes());
    assert!(f.export.join("media/一.gif").exists());
    assert!(!f.export.join("media/二.png").exists());
    let repaired = f.run(&["<img src=一.gif><img src=二.png>"], true).unwrap();
    assert_eq!(repaired.media.mutations, 1);
    assert!(
        !f.run(&["<img src=一.gif><img src=二.png>"], true)
            .unwrap()
            .applied
    );
}
#[test]
fn deck_publication_failure_leaves_safe_orphan_and_retry_converges() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let before = f.bytes();
    TEST_HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(|phase| {
            if phase == "before_deck_publish" {
                Err(blocker(
                    ErrorCode::WriteFailed,
                    "injected_publish_failure",
                    json!({}),
                ))
            } else {
                Ok(())
            }
        }))
    });
    let result = f.run(&["<img src=一.gif>"], true);
    TEST_HOOK.with(|h| *h.borrow_mut() = None);
    reason(&result.unwrap_err(), "injected_publish_failure");
    assert_eq!(before, f.bytes());
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    let r = f.run(&["<img src=一.gif>"], true).unwrap();
    assert_eq!(r.media.mutations, 0);
    assert_eq!(r.notes_created, 1);
}
#[test]
fn source_changed_after_media_copy_refuses_publication() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let source = f.export.join("deck.json");
    let mut external = f.bytes();
    external.push(b'\n');
    let edited = external.clone();
    TEST_HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(move |phase| {
            if phase == "before_deck_publish" {
                fs::write(&source, &external).unwrap();
            }
            Ok(())
        }))
    });
    let result = f.run(&["<img src=一.gif>"], true);
    TEST_HOOK.with(|h| *h.borrow_mut() = None);
    assert_eq!(result.unwrap_err().code, ErrorCode::SourceChanged);
    assert_eq!(f.bytes(), edited);
    assert!(f.export.join("media/一.gif").exists());
}
#[test]
fn emit_resolved_cannot_replace_policy_or_corpus_or_media() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    for path in [
        f.options.config.clone().unwrap(),
        f.options
            .asset_store
            .as_ref()
            .unwrap()
            .join("manifest.json"),
        f.export.join("media/一.gif"),
    ] {
        reason(
            &create_with_options(
                &f.export,
                &f.request(&["<img src=一.gif>"]),
                true,
                Some(&path),
                &f.options,
            )
            .unwrap_err(),
            "emit_resolved_protected_path",
        );
    }
}

#[test]
fn concurrent_creates_share_one_export_lock_and_do_not_lose_updates() {
    use std::sync::{Arc, Barrier};
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for guid in ["ThreadA", "ThreadB"] {
        let export = f.export.clone();
        let options = f.options.clone();
        let barrier = barrier.clone();
        let mut request = f.request(&["<img src=一.gif>"]);
        request.notes[0].guid = Some(guid.into());
        handles.push(std::thread::spawn(move || {
            TEST_HOOK.with(|h| {
                *h.borrow_mut() = Some(Box::new(move |phase| {
                    if phase == "before_export_lock" {
                        barrier.wait();
                    }
                    Ok(())
                }))
            });
            create_with_options(&export, &request, true, None, &options)
        }));
    }
    let outcomes = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes.into_iter().find_map(Result::err).unwrap().code,
        ErrorCode::SourceChanged
    );
    let value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(value["notes"].as_array().unwrap().len(), 3);
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    assert_eq!(value["media_files"], json!(["a.mp3", "b.png", "一.gif"]));
}

#[test]
fn media_declared_in_child_is_not_duplicated_in_root() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let mut value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    value["children"] = json!([{"__type__": "Deck", "name":"Child", "crowdanki_uuid":"child", "deck_config_uuid":"cfg-1", "notes":[], "children":[], "media_files":["一.gif"]}]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&value).unwrap(),
    )
    .unwrap();
    let r = f.run(&["<img src=一.gif>"], true).unwrap();
    assert!(r.media.declarations_added.is_empty());
    assert_eq!(r.media.mutations, 1);
    let after: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(after["media_files"], value["media_files"]);
    assert_eq!(after["children"], value["children"]);
}

#[test]
fn matching_existing_destination_dry_run_has_no_mutation_and_store_is_unchanged() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    let store = f.options.asset_store.as_ref().unwrap();
    let manifest = fs::read(store.join("manifest.json")).unwrap();
    let file_time = fs::metadata(f.export.join("media/一.gif"))
        .unwrap()
        .modified()
        .unwrap();
    let before = f.bytes();
    let r = f.run(&["<img src=一.gif>"], false).unwrap();
    assert_eq!(r.media.evidence()["assets"][0]["action"], "reuse");
    assert_eq!(r.media.mutations, 0);
    assert_eq!(before, f.bytes());
    assert_eq!(manifest, fs::read(store.join("manifest.json")).unwrap());
    assert_eq!(
        file_time,
        fs::metadata(f.export.join("media/一.gif"))
            .unwrap()
            .modified()
            .unwrap()
    );
}

#[test]
fn repository_context_auto_policy_is_export_relative() {
    let f = Fixture::new();
    fs::create_dir(f.temp.path().join(".git")).unwrap();
    fs::create_dir(f.temp.path().join(".anki-repo")).unwrap();
    fs::write(
        f.temp.path().join(CONFIG_PATH),
        policy("model-1", "Заголовок"),
    )
    .unwrap();
    let store = f.temp.path().join(".asset-store/kanji");
    fs::create_dir(f.temp.path().join(".asset-store")).unwrap();
    let mut options = f.options.clone();
    options.asset_store = Some(store.clone());
    let fixture = Fixture {
        temp: f.temp,
        export: f.export,
        options,
    };
    fixture.asset("一", GIF, None);
    let r = create_with_options(
        &fixture.export,
        &fixture.request(&["<img src=一.gif>"]),
        false,
        None,
        &MediaOptions::default(),
    )
    .unwrap();
    assert_eq!(r.media.pins().len(), 1);
    assert_eq!(r.outcomes[0].processor_fields, vec!["Заголовок"]);
}

#[test]
fn canonical_lifecycle_decision_validator_and_hash_must_all_match() {
    for mutation in [
        "pending",
        "semantic",
        "decision_hash",
        "validator",
        "format",
        "missing_file",
    ] {
        let f = Fixture::new();
        f.asset("一", GIF, None);
        let store = f.options.asset_store.as_ref().unwrap();
        let path = store.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match mutation {
            "pending" => manifest["assets"][0]["lifecycle"] = json!("pending"),
            "semantic" => manifest["assets"][0]["validation"]["status"] = json!("uncertain"),
            "decision_hash" => {
                manifest["assets"][0]["validation"]["content_sha256"] = json!("0".repeat(64))
            }
            "validator" => {
                manifest["assets"][0]["validation"]["validator"]["version"] = json!("old")
            }
            "format" => manifest["assets"][0]["format"] = json!("png"),
            "missing_file" => fs::remove_file(store.join("assets/一.gif")).unwrap(),
            _ => unreachable!(),
        }
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let before = f.bytes();
        reason(
            &f.run(&["<img src=一.gif>"], true).unwrap_err(),
            "asset_integrity_invalid",
        );
        assert_eq!(before, f.bytes());
        assert!(!f.export.join("media").exists());
    }
}

#[test]
fn whole_batch_preflight_finishes_before_first_media_mutation() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>", "<img src=二.gif>"], true)
            .unwrap_err(),
        "kanji_asset_missing",
    );
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());
    f.asset("二", PNG, None);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/二.png"), b"foreign").unwrap();
    reason(
        &f.run(&["<img src=一.gif>", "<img src=二.png>"], true)
            .unwrap_err(),
        "destination_media_conflict",
    );
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media/一.gif").exists());
}

#[test]
fn required_duplicate_declaration_is_a_blocker_and_unrelated_duplicates_are_preserved() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let mut value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    value["media_files"] = json!(["a.mp3", "a.mp3", "一.gif", "一.gif"]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&value).unwrap(),
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "media_declaration_conflict",
    );
    assert!(!f.export.join("media").exists());
    value["media_files"] = json!(["a.mp3", "a.mp3"]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&value).unwrap(),
    )
    .unwrap();
    f.run(&["<img src=一.gif>"], true).unwrap();
    let after: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(after["media_files"], json!(["a.mp3", "a.mp3", "一.gif"]));
}
