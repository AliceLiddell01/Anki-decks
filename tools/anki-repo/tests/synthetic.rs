//! Синтетические fixtures: все ветки domain-ошибок и validation contracts.
//!
//! Fixtures строятся программно, поэтому тесты не зависят от содержимого
//! канонических колод.

mod common;

use std::path::Path;

use common::{TempDir, base_export, export_with};
use serde_json::json;

use anki_repo::error::ErrorCode;
use anki_repo::loader::load_export;
use anki_repo::ops::validate::{Severity, ValidateResult, validate};

fn run_validate(dir: &TempDir) -> ValidateResult {
    validate(dir.path()).expect("validate должен вернуть результат, а не доменную ошибку")
}

fn has_issue(result: &ValidateResult, code: &str) -> bool {
    result.issues.iter().any(|issue| issue.code == code)
}

fn severity_of(result: &ValidateResult, code: &str) -> Option<Severity> {
    result
        .issues
        .iter()
        .find(|issue| issue.code == code)
        .map(|issue| issue.severity)
}

#[test]
fn valid_export_has_no_errors() {
    let dir = TempDir::new("valid");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(result.valid, "issues: {:?}", result.issues);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.warnings, 0);
    assert!(has_issue(&result, "export_summary"));
}

#[test]
fn invalid_json_becomes_a_single_error_issue() {
    let dir = TempDir::new("invalid-json");
    dir.write_raw_deck_json("{ это не JSON");

    let result = run_validate(&dir);
    assert!(!result.valid);
    assert_eq!(result.summary.errors, 1);
    assert!(has_issue(&result, "invalid_json"));
}

#[test]
fn root_with_wrong_type_is_reported() {
    let dir = TempDir::new("root-type");
    dir.write_export(&json!({"__type__": "Note", "name": "Не колода"}));

    let result = run_validate(&dir);
    assert!(!result.valid);
    assert_eq!(severity_of(&result, "root_not_deck"), Some(Severity::Error));
    let issue = result
        .issues
        .iter()
        .find(|issue| issue.code == "root_not_deck")
        .expect("issue root_not_deck");
    assert_eq!(issue.details["reason"], json!("type_mismatch"));
}

#[test]
fn root_without_type_is_reported() {
    let dir = TempDir::new("root-missing-type");
    dir.write_export(&json!({"name": "Без типа"}));

    let result = run_validate(&dir);
    assert_eq!(severity_of(&result, "root_not_deck"), Some(Severity::Error));
    let issue = result
        .issues
        .iter()
        .find(|issue| issue.code == "root_not_deck")
        .expect("issue root_not_deck");
    assert_eq!(issue.details["reason"], json!("type_missing"));
}

#[test]
fn unresolved_note_model_uuid_is_an_error() {
    let dir = TempDir::new("model-unresolved");
    dir.write_export(&export_with(|value| {
        value["notes"][0]["note_model_uuid"] = json!("нет-такой");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(!result.valid);
    assert_eq!(
        severity_of(&result, "note_model_unresolved"),
        Some(Severity::Error)
    );
}

#[test]
fn missing_note_model_uuid_is_an_error() {
    let dir = TempDir::new("model-missing");
    dir.write_export(&export_with(|value| {
        value["notes"][0]
            .as_object_mut()
            .expect("note object")
            .remove("note_model_uuid");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "note_model_uuid_missing"),
        Some(Severity::Error)
    );
}

#[test]
fn note_with_wrong_field_count_is_an_error() {
    let dir = TempDir::new("fields-count");
    dir.write_export(&export_with(|value| {
        value["notes"][0]["fields"] = json!(["только", "два"]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(!result.valid);
    assert_eq!(
        severity_of(&result, "note_fields_count_mismatch"),
        Some(Severity::Error)
    );
}

#[test]
fn non_string_field_value_is_an_error() {
    let dir = TempDir::new("field-not-string");
    dir.write_export(&export_with(|value| {
        value["notes"][0]["fields"] = json!([42, "ок", "ок"]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "note_field_value_not_string"),
        Some(Severity::Error)
    );
}

#[test]
fn duplicate_and_missing_guids_are_errors() {
    let dir = TempDir::new("guid");
    dir.write_export(&export_with(|value| {
        value["notes"][1]["guid"] = json!("guid-1");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "duplicate_note_guid"),
        Some(Severity::Error)
    );

    let dir = TempDir::new("guid-missing");
    dir.write_export(&export_with(|value| {
        value["notes"][0]
            .as_object_mut()
            .expect("note object")
            .remove("guid");
    }));
    dir.write_media(&["a.mp3"]);
    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "note_guid_missing"),
        Some(Severity::Error)
    );
}

#[test]
fn duplicate_identity_in_one_declaration_is_an_error() {
    let dir = TempDir::new("duplicate-model");
    dir.write_export(&export_with(|value| {
        let copy = value["note_models"][0].clone();
        value["note_models"]
            .as_array_mut()
            .expect("note_models")
            .push(copy);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "duplicate_note_model_uuid"),
        Some(Severity::Error)
    );

    let dir = TempDir::new("duplicate-config");
    dir.write_export(&export_with(|value| {
        let copy = value["deck_configurations"][0].clone();
        value["deck_configurations"]
            .as_array_mut()
            .expect("deck_configurations")
            .push(copy);
    }));
    dir.write_media(&["a.mp3"]);
    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "duplicate_deck_config_uuid"),
        Some(Severity::Error)
    );
}

#[test]
fn conflicting_model_definition_across_nodes_is_a_warning() {
    let dir = TempDir::new("conflicting-model");
    dir.write_export(&export_with(|value| {
        let child_model = {
            let mut model = value["note_models"][0].clone();
            model["name"] = json!("Слова (другое определение)");
            model
        };
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Test::Deck::Child",
                "crowdanki_uuid": "deck-uuid-2",
                "deck_config_uuid": "cfg-1",
                "children": [],
                "media_files": [],
                "notes": [],
                "deck_configurations": [],
                "note_models": [child_model]
            }
        ]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "conflicting_note_model_definition"),
        Some(Severity::Warning)
    );
    assert!(
        result.valid,
        "конфликт определений не делает экспорт невалидным"
    );
}

#[test]
fn repeated_model_declaration_with_same_definition_is_not_a_warning() {
    let dir = TempDir::new("repeated-model");
    dir.write_export(&export_with(|value| {
        let model = value["note_models"][0].clone();
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Test::Deck::Child",
                "crowdanki_uuid": "deck-uuid-2",
                "deck_config_uuid": "cfg-1",
                "children": [],
                "media_files": [],
                "notes": [],
                "deck_configurations": [],
                "note_models": [model]
            }
        ]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(result.summary.warnings, 0, "issues: {:?}", result.issues);
    assert!(result.valid);
}

#[test]
fn ord_matrix_is_covered_by_distinct_error_codes() {
    let cases = [
        (
            "invalid",
            json!([{"name": "A", "ord": "x"}, {"name": "B", "ord": 1}, {"name": "C", "ord": 2}]),
            "field_ord_invalid",
        ),
        (
            "negative",
            json!([{"name": "A", "ord": -1}, {"name": "B", "ord": 1}, {"name": "C", "ord": 2}]),
            "field_ord_negative",
        ),
        (
            "duplicate",
            json!([{"name": "A", "ord": 0}, {"name": "B", "ord": 0}, {"name": "C", "ord": 2}]),
            "field_ord_duplicate",
        ),
        (
            "out-of-range",
            json!([{"name": "A", "ord": 0}, {"name": "B", "ord": 1}, {"name": "C", "ord": 5}]),
            "field_ord_out_of_range",
        ),
        (
            "gap",
            json!([{"name": "A", "ord": 0}, {"name": "B", "ord": 2}, {"name": "C", "ord": 3}]),
            "field_ord_gap",
        ),
    ];

    for (label, flds, expected_code) in cases {
        let dir = TempDir::new(&format!("ord-{label}"));
        dir.write_export(&export_with(|value| {
            value["note_models"][0]["flds"] = flds.clone();
        }));
        dir.write_media(&["a.mp3"]);

        let result = run_validate(&dir);
        assert_eq!(
            severity_of(&result, expected_code),
            Some(Severity::Error),
            "случай {label}: issues {:?}",
            result.issues
        );
        assert!(
            !result.valid,
            "случай {label} должен делать экспорт невалидным"
        );
    }
}

#[test]
fn duplicate_field_name_is_an_error() {
    let dir = TempDir::new("field-name");
    dir.write_export(&export_with(|value| {
        value["note_models"][0]["flds"] = json!([
            {"name": "Слово", "ord": 0},
            {"name": "Слово", "ord": 1},
            {"name": "Пример", "ord": 2}
        ]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "field_name_duplicate"),
        Some(Severity::Error)
    );
}

#[test]
fn unresolved_deck_config_uuid_is_an_error() {
    let dir = TempDir::new("config-unresolved");
    dir.write_export(&export_with(|value| {
        value["deck_config_uuid"] = json!("нет-такой");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "deck_config_unresolved"),
        Some(Severity::Error)
    );
}

#[test]
fn template_reference_to_unknown_field_is_an_error() {
    let dir = TempDir::new("template-field");
    dir.write_export(&export_with(|value| {
        value["note_models"][0]["tmpls"][0]["afmt"] = json!("{{Пропавшее}}");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "template_field_unresolved"),
        Some(Severity::Error)
    );
}

#[test]
fn complex_template_construct_is_only_a_warning() {
    let dir = TempDir::new("template-unchecked");
    dir.write_export(&export_with(|value| {
        value["note_models"][0]["tmpls"][0]["afmt"] = json!("{{tts ja_JP:Пропавшее}}");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "template_construct_unchecked"),
        Some(Severity::Warning)
    );
    assert!(!has_issue(&result, "template_field_unresolved"));
}

#[test]
fn unterminated_template_construct_is_an_unresolved_error() {
    let dir = TempDir::new("template-unterminated");
    dir.write_export(&export_with(|value| {
        value["note_models"][0]["tmpls"][0]["afmt"] = json!("{{НетТакого}");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "template_field_unresolved"),
        Some(Severity::Error)
    );
}

#[test]
fn missing_physical_media_is_a_warning_and_keeps_export_valid() {
    let dir = TempDir::new("media-missing");
    dir.write_export(&export_with(|value| {
        value["media_files"] = json!(["a.mp3", "b.mp3"]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(result.valid);
    assert_eq!(
        severity_of(&result, "media_physical_missing"),
        Some(Severity::Warning)
    );
    let issue = result
        .issues
        .iter()
        .find(|issue| issue.code == "media_physical_missing")
        .expect("issue");
    assert_eq!(issue.details["name"], json!("b.mp3"));
}

#[test]
fn missing_media_directory_is_a_warning() {
    let dir = TempDir::new("media-dir-missing");
    dir.write_export(&base_export());

    let result = run_validate(&dir);
    assert!(result.valid);
    assert_eq!(
        severity_of(&result, "media_dir_missing"),
        Some(Severity::Warning)
    );
}

#[test]
fn undeclared_physical_media_is_only_info() {
    let dir = TempDir::new("media-unused");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3", "лишний.mp3"]);

    let result = run_validate(&dir);
    assert!(result.valid);
    assert_eq!(
        severity_of(&result, "media_physical_unused"),
        Some(Severity::Info)
    );
    assert_eq!(result.summary.warnings, 0);
}

#[test]
fn undeclared_media_reference_is_a_warning() {
    let dir = TempDir::new("media-reference");
    dir.write_export(&export_with(|value| {
        value["notes"][0]["fields"][0] = json!("[sound:никто.mp3]偶然");
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(result.valid);
    assert_eq!(
        severity_of(&result, "media_reference_undeclared"),
        Some(Severity::Warning)
    );
}

#[test]
fn media_path_traversal_names_do_not_reach_the_filesystem() {
    let dir = TempDir::new("media-traversal");
    dir.write_export(&export_with(|value| {
        value["media_files"] = json!(["../secret.mp3", "a.mp3"]);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(result.valid);
    assert_eq!(
        severity_of(&result, "media_name_not_basename"),
        Some(Severity::Warning)
    );
}

#[test]
fn unknown_extra_json_keys_are_tolerated_everywhere() {
    let dir = TempDir::new("extra-keys");
    dir.write_export(&export_with(|value| {
        value["x_future_root"] = json!({"keep": true});
        value["note_models"][0]["x_future_model"] = json!([1, 2, 3]);
        value["notes"][0]["x_future_note"] = json!("строка");
        value["deck_configurations"][0]["x_future_config"] = json!(true);
    }));
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert!(result.valid);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.warnings, 0);
}

#[test]
fn empty_field_values_are_reported_as_info() {
    let dir = TempDir::new("empty-fields");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);

    let result = run_validate(&dir);
    assert_eq!(
        severity_of(&result, "empty_field_values"),
        Some(Severity::Info)
    );
    assert!(result.valid);
}

#[test]
fn issues_are_deterministically_ordered() {
    let dir = TempDir::new("ordering");
    dir.write_export(&export_with(|value| {
        value["media_files"] = json!(["a.mp3", "b.mp3", "c.mp3"]);
    }));
    dir.write_media(&["a.mp3", "z.mp3"]);

    let first = run_validate(&dir);
    let second = run_validate(&dir);
    let key = |result: &ValidateResult| -> Vec<(String, String, String)> {
        result
            .issues
            .iter()
            .map(|issue| {
                (
                    issue.severity.as_str().to_string(),
                    issue.code.to_string(),
                    issue.location.clone(),
                )
            })
            .collect()
    };
    assert_eq!(
        key(&first),
        key(&second),
        "порядок issues должен быть стабильным"
    );

    let severities: Vec<Severity> = first.issues.iter().map(|issue| issue.severity).collect();
    let mut sorted = severities.clone();
    sorted.sort();
    assert_eq!(
        severities, sorted,
        "issues должны идти по возрастанию severity"
    );
}

#[test]
fn missing_deck_json_is_a_domain_error() {
    let dir = TempDir::new("no-deck-json");

    let error = validate(dir.path()).expect_err("нет deck.json");
    assert_eq!(error.code, ErrorCode::DeckJsonMissing);
    assert_eq!(error.exit_code(), 3);

    let error = load_export(dir.path()).expect_err("нет deck.json");
    assert_eq!(error.code, ErrorCode::DeckJsonMissing);
}

#[test]
fn missing_export_directory_is_a_domain_error() {
    let dir = TempDir::new("no-directory");
    let missing = dir.path().join("нет-такого");

    let error = validate(&missing).expect_err("нет каталога");
    assert_eq!(error.code, ErrorCode::InputUnreadable);
    assert_eq!(error.exit_code(), 3);
}

#[test]
fn passing_deck_json_path_instead_of_directory_is_rejected() {
    let dir = TempDir::new("deck-json-path");
    dir.write_export(&base_export());

    let error = validate(&dir.path().join("deck.json")).expect_err("нужен каталог, а не файл");
    assert_eq!(error.code, ErrorCode::InputUnreadable);
    assert_eq!(error.exit_code(), 3);
}

#[test]
fn invalid_json_in_loader_is_a_domain_error() {
    let dir = TempDir::new("loader-invalid-json");
    dir.write_raw_deck_json("[1, 2, 3]");
    assert_eq!(
        load_export(dir.path())
            .expect_err("массив — не колода")
            .code,
        ErrorCode::RootNotDeck
    );

    dir.write_raw_deck_json("{");
    assert_eq!(
        load_export(dir.path()).expect_err("сломанный JSON").code,
        ErrorCode::InvalidJson
    );
}

#[test]
fn loader_keeps_unknown_root_keys() {
    let dir = TempDir::new("loader-extra");
    dir.write_export(&export_with(|value| {
        value["x_future_root"] = json!({"keep": true});
    }));

    let loaded = load_export(dir.path()).expect("экспорт должен загрузиться");
    assert_eq!(loaded.root.extra["x_future_root"], json!({"keep": true}));
    assert_eq!(
        loaded.deck_json.file_name().and_then(|name| name.to_str()),
        Some("deck.json")
    );
}

#[test]
fn nested_nodes_are_indexed_with_their_own_deck_paths() {
    let dir = TempDir::new("nested-index");
    dir.write_export(&export_with(|value| {
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Test::Deck::Child",
                "crowdanki_uuid": "deck-uuid-2",
                "deck_config_uuid": "cfg-1",
                "children": [],
                "media_files": [],
                "note_models": [],
                "deck_configurations": [],
                "notes": [
                    {
                        "guid": "guid-child",
                        "note_model_uuid": "model-1",
                        "tags": [],
                        "fields": ["子", "ребёнок", ""]
                    }
                ]
            }
        ]);
    }));
    dir.write_media(&["a.mp3"]);

    let loaded = load_export(dir.path()).expect("загрузка");
    let index = anki_repo::index::ExportIndex::build(&loaded.root);
    let paths: Vec<&str> = index.nodes.iter().map(|node| node.path).collect();
    assert_eq!(paths, vec!["Test::Deck", "Test::Deck::Child"]);
    assert_eq!(index.notes.len(), 3);

    let result = run_validate(&dir);
    assert!(result.valid, "issues: {:?}", result.issues);
}

#[test]
fn validate_is_read_only() {
    let dir = TempDir::new("read-only");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let before = std::fs::read(dir.path().join("deck.json")).expect("deck.json");
    let media_before = std::fs::read_dir(dir.path().join("media"))
        .expect("media")
        .count();

    let _ = run_validate(&dir);

    let after = std::fs::read(dir.path().join("deck.json")).expect("deck.json");
    assert_eq!(before, after, "validate не должен изменять deck.json");
    assert_eq!(
        media_before,
        std::fs::read_dir(dir.path().join("media"))
            .expect("media")
            .count(),
        "validate не должен менять состав media/"
    );
}

#[test]
fn export_dir_reported_as_given_not_canonicalized() {
    let dir = TempDir::new("path-reporting");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);

    let loaded = load_export(dir.path()).expect("загрузка");
    assert_eq!(loaded.export_dir, *dir.path());

    let relative = Path::new(".");
    let error = load_export(relative).expect_err("в cwd нет deck.json");
    assert_eq!(error.code, ErrorCode::DeckJsonMissing);
    assert_eq!(
        error.details["path"],
        json!(relative.join("deck.json").to_string_lossy())
    );
}
