//! Контракт CLI команды `review`: отбор, страницы, компактность batch.

mod common;

use common::{TempDir, export_with, parse_json, run_cli, words_deck};
use serde_json::{Value, json};

fn review_json(export: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["--json", "review", export];
    args.extend_from_slice(extra);
    let (code, stdout, _) = run_cli(&args);
    (code, parse_json(&stdout))
}

/// Экспорт с пятью заметками: три подходят под `--field Значение = значение».
fn selection_export() -> Value {
    export_with(|value| {
        value["notes"][0]["fields"] = json!(["слово-1", "значение", ""]);
        value["notes"][1]["fields"] = json!(["слово-2", "значение", ""]);
        for index in 0..3 {
            value["notes"].as_array_mut().expect("notes").push(json!({
                "__type__": "Note",
                "guid": format!("bulk-{index}"),
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": [format!("слово-{}", index + 3), "значение", ""],
            }));
        }
    })
}

/// Экспорт с вложенными колодами: Root, Root::Child, Root::Child::Leaf.
///
/// Имена узлов включают путь, как в реальных CrowdAnki-экспортах, поэтому
/// `--deck` можно проверять и по короткому, и по полному имени.
fn nested_export() -> Value {
    export_with(|value| {
        value["name"] = json!("Root");
        value["notes"] = json!([
            {"guid": "guid-root", "note_model_uuid": "model-1", "tags": [],
             "fields": ["根", "корень", ""]}
        ]);
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Root::Child",
                "crowdanki_uuid": "deck-child",
                "deck_config_uuid": "cfg-1",
                "children": [
                    {
                        "__type__": "Deck",
                        "name": "Root::Child::Leaf",
                        "crowdanki_uuid": "deck-leaf",
                        "deck_config_uuid": "cfg-1",
                        "children": [],
                        "notes": [
                            {"guid": "guid-leaf", "note_model_uuid": "model-1", "tags": [],
                             "fields": ["深い", "глубокий", ""]}
                        ],
                        "media_files": [],
                        "note_models": [],
                        "deck_configurations": []
                    }
                ],
                "notes": [
                    {"guid": "guid-child", "note_model_uuid": "model-1", "tags": [],
                     "fields": ["子", "ребёнок", ""]}
                ],
                "media_files": [],
                "note_models": [],
                "deck_configurations": []
            }
        ]);
    })
}

#[test]
fn review_defaults_to_one_bounded_page() {
    let temp = TempDir::new("review-page");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--field", "Значение", "--value", "значение", "--limit", "2"],
    );

    assert_eq!(code, 0);
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["command"], "review");
    let result = &parsed["result"];
    assert_eq!(result["notes_total"], 5);
    assert_eq!(result["total_selected"], 5);
    assert_eq!(result["returned"], 2);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["next_offset"], 2);
    assert_eq!(result["offset"], 0);
    assert_eq!(result["limit"], 2);
    assert_eq!(result["items"].as_array().expect("items").len(), 2);
}

#[test]
fn pagination_covers_every_selected_note_exactly_once() {
    let temp = TempDir::new("review-pagination");
    temp.write_export(&selection_export());
    let export = temp.path().to_string_lossy().to_string();

    let mut guids = Vec::new();
    let mut offset = 0;
    loop {
        let (code, parsed) = review_json(
            &export,
            &[
                "--field",
                "Значение",
                "--value",
                "значение",
                "--limit",
                "2",
                "--offset",
                &offset.to_string(),
            ],
        );
        assert_eq!(code, 0);
        let result = &parsed["result"];
        for item in result["items"].as_array().expect("items") {
            guids.push(item["guid"].as_str().expect("guid").to_string());
        }
        match result["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }

    guids.sort();
    assert_eq!(
        guids,
        vec!["bulk-0", "bulk-1", "bulk-2", "guid-1", "guid-2"]
    );
}

#[test]
fn offset_past_the_end_is_a_valid_empty_page() {
    let temp = TempDir::new("review-offset-past-end");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--all", "--limit", "2", "--offset", "1000"],
    );

    assert_eq!(code, 0, "страница за концом — не ошибка, а конец пагинации");
    let result = &parsed["result"];
    assert_eq!(result["returned"], 0);
    assert_eq!(result["truncated"], false);
    assert_eq!(result["next_offset"], Value::Null);
    assert_eq!(result["total_selected"], 5);
}

#[test]
fn review_batch_stays_small_and_carries_qa_context() {
    let temp = TempDir::new("review-compact");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "empty_field_value"],
    );

    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(result["selection"]["kind"], "qa_code");
    assert_eq!(result["selection"]["qa_code"], "empty_field_value");
    assert_eq!(result["total_selected"], 5);
    assert_eq!(
        result["returned"], 5,
        "batch по умолчанию меньше выбранного"
    );

    let item = &result["items"][0];
    assert_eq!(item["note_index"], 0);
    assert_eq!(item["qa_findings"].as_array().expect("findings").len(), 1);
    assert_eq!(item["qa_findings"][0]["code"], "empty_field_value");
    assert_eq!(item["qa_findings"][0]["field"], "Пример");
    assert_eq!(item["qa_findings_truncated"], false);
}

#[test]
fn qa_findings_in_an_item_are_bounded() {
    let temp = TempDir::new("review-findings-bound");
    temp.write_export(&export_with(|value| {
        // Значение из одних пробелов даёт и leading, и trailing findings.
        value["notes"][0]["fields"] = json!(["слово", "  ", ""]);
        value["notes"][1]["fields"] = json!(["слово", "  ", ""]);
    }));
    let (code, parsed) = review_json(&temp.path().to_string_lossy(), &["--word", "слово"]);
    assert_eq!(code, 0);

    let item = &parsed["result"]["items"][0];
    // Порядок findings — порядок реестра правил, а не порядок обнаружения.
    let codes: Vec<&str> = item["qa_findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| finding["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        vec![
            "empty_field_value",
            "leading_whitespace",
            "trailing_whitespace",
            "duplicate_note_content",
            "duplicate_primary_field"
        ]
    );
    assert_eq!(item["qa_findings_truncated"], false);
}

#[test]
fn guid_lookup_mirrors_find_error_contract() {
    let temp = TempDir::new("review-guid");
    temp.write_export(&selection_export());
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = review_json(&export, &["--guid", "guid-2"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["returned"], 1);
    assert_eq!(parsed["result"]["items"][0]["guid"], "guid-2");
    assert_eq!(parsed["result"]["next_offset"], Value::Null);

    // Код ошибки совпадает с `find`: generic `not_found`, exit 4.
    // `edit` для той же ситуации использует `note_not_found` с тем же exit.
    let (code, parsed) = review_json(&export, &["--guid", "нет-такого"]);
    assert_eq!(code, 4);
    assert_eq!(parsed["error"]["code"], "not_found");
}

#[test]
fn deck_scope_limits_selection_and_unknown_deck_is_rejected() {
    let temp = TempDir::new("review-scope");
    temp.write_export(&nested_export());
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = review_json(&export, &["--all", "--deck", "Root::Child"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["total_selected"], 2, "Child и его Leaf");

    let (code, parsed) = review_json(&export, &["--all", "--deck", "Root::нет"]);
    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_deck");
}

#[test]
fn empty_selection_is_not_an_error() {
    let temp = TempDir::new("review-empty");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--word", "нет-такого-слова"],
    );

    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["total_selected"], 0);
    assert_eq!(parsed["result"]["returned"], 0);
    assert!(
        parsed["result"]["items"]
            .as_array()
            .expect("items")
            .is_empty()
    );
}

#[test]
fn unknown_qa_code_is_rejected() {
    let temp = TempDir::new("review-unknown-code");
    temp.write_export(&selection_export());
    let (code, parsed) = review_json(&temp.path().to_string_lossy(), &["--qa-code", "нет_такого"]);

    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_qa_code");
}

#[test]
fn human_mode_prints_selected_and_next_page() {
    let temp = TempDir::new("review-human");
    temp.write_export(&selection_export());
    let (code, stdout, stderr) = run_cli(&[
        "review",
        &temp.path().to_string_lossy(),
        "--all",
        "--limit",
        "1",
    ]);

    assert_eq!(code, 0);
    assert!(stderr.is_empty());
    for needle in [
        "Критерий:",
        "Следующая страница: --offset 1",
        "[#0]",
        "QA (1):",
    ] {
        assert!(
            stdout.contains(needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

#[test]
fn canonical_deck_review_is_bounded_per_note() {
    let export = words_deck(3);
    let (code, parsed) = review_json(
        &export.to_string_lossy(),
        &["--qa-code", "empty_field_value", "--limit", "1"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(result["notes_total"], 1507);
    assert_eq!(result["total_selected"], 72);
    assert_eq!(result["returned"], 1);

    let item = &result["items"][0];
    let fields = item["fields"].as_object().expect("fields");
    assert_eq!(
        fields.len(),
        6,
        "batch несёт только именованные поля заметки"
    );
    assert_eq!(fields["Ударение"], "");
}
