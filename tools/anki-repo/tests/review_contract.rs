//! Контракт CLI команды `review`: отбор, страницы, компактность batch.

mod common;

use common::{
    TempDir, collect_notes, export_with, parse_json, raw_json, raw_qa_counts, run_cli, words_deck,
};
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
    // Ожидаемое число заметок с пустым полем пересчитывается из сырого
    // deck.json, а не берётся из текущего содержимого репозитория.
    let raw = raw_json(3);
    let mut notes = Vec::new();
    collect_notes(&raw, &mut notes);
    let expected_selected = raw_qa_counts(&raw)["empty_field_value"];

    let (code, parsed) = review_json(
        &export.to_string_lossy(),
        &["--qa-code", "empty_field_value", "--limit", "1"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(result["notes_total"], notes.len());
    assert_eq!(result["total_selected"], expected_selected);
    assert_eq!(result["excluded_unaddressable"], 0);
    assert_eq!(result["returned"], 1);
    assert_eq!(result["truncated"], true, "страница ограничена одним item");

    let item = &result["items"][0];
    let fields = item["fields"].as_object().expect("fields");
    assert_eq!(
        fields.len(),
        6,
        "batch несёт только именованные поля заметки"
    );
    assert!(
        item["qa_findings"]
            .as_array()
            .expect("qa_findings")
            .iter()
            .any(|finding| finding["code"] == "empty_field_value"),
        "item обязан нести finding, по которому выбран"
    );
    assert_eq!(
        fields["Ударение"], "",
        "выбранная заметка действительно содержит пустое поле"
    );
}

/// Экспорт с одной группой из трёх одинаковых заметок и одной уникальной.
fn content_group_export() -> Value {
    export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 3..6 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("guid-{index}"));
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    })
}

/// Экспорт с одной группой из трёх заметок с одинаковым головным полем.
fn primary_group_export() -> Value {
    export_with(|value| {
        for index in 3..6 {
            value["notes"].as_array_mut().expect("notes").push(json!({
                "__type__": "Note",
                "guid": format!("guid-{index}"),
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": ["[sound:a.mp3]偶然", format!("значение-{index}"), ""],
            }));
        }
    })
}

#[test]
fn grouped_qa_code_selection_brings_every_participant() {
    let temp = TempDir::new("review-group-content");
    temp.write_export(&content_group_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "duplicate_note_content", "--limit", "10"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(
        result["total_selected"], 4,
        "в batch попадают все участники группы, а не только владелец finding'а"
    );

    let owner = &result["items"][0];
    let finding = owner["qa_findings"]
        .as_array()
        .expect("qa_findings")
        .iter()
        .find(|finding| finding["code"] == "duplicate_note_content")
        .expect("finding группы у владельца");
    assert_eq!(finding["group_size"], 4);
    assert_eq!(finding["related_truncated"], false);

    // Владелец называет остальных участников с их guid.
    let related: Vec<&Value> = finding["related"]
        .as_array()
        .expect("related")
        .iter()
        .map(|related| &related["guid"])
        .collect();
    assert_eq!(
        related,
        vec![&json!("guid-3"), &json!("guid-4"), &json!("guid-5")],
        "все участники группы, кроме самого владельца"
    );

    // Остальные участники понимают, почему они в batch'е: каждая группа, в
    // которой заметка — участник, названа вместе со своим владельцем.
    for item in &result["items"].as_array().expect("items")[1..] {
        let membership = item["group_membership"]
            .as_array()
            .expect("group_membership");
        assert!(!membership.is_empty(), "участник обязан знать свою группу");
        assert!(
            membership
                .iter()
                .any(|entry| entry["code"] == "duplicate_note_content"),
            "участник выбран по этому коду"
        );
        for entry in membership {
            assert_eq!(entry["owner_note_index"], 0);
            assert_eq!(entry["owner_guid"], "guid-1");
            assert_eq!(entry["group_size"], 4);
        }
    }
}

#[test]
fn grouped_primary_field_selection_brings_every_participant() {
    let temp = TempDir::new("review-group-primary");
    temp.write_export(&primary_group_export());
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--qa-code", "duplicate_primary_field", "--limit", "10"],
    );
    assert_eq!(code, 0);

    let result = &parsed["result"];
    assert_eq!(result["total_selected"], 4);
    assert_eq!(
        result["items"].as_array().expect("items").len(),
        4,
        "каждая заметка группы — отдельный item с полями"
    );
    let owner = &result["items"][0];
    let finding = owner["qa_findings"]
        .as_array()
        .expect("qa_findings")
        .iter()
        .find(|finding| finding["code"] == "duplicate_primary_field")
        .expect("finding группы");
    assert_eq!(finding["group_size"], 4);
    assert_eq!(finding["related"].as_array().expect("related").len(), 3);
}

#[test]
fn unaddressable_notes_never_reach_the_batch() {
    let temp = TempDir::new("review-unaddressable");
    temp.write_export(&export_with(|value| {
        let notes = value["notes"].as_array_mut().expect("notes");
        // Заметка без guid: её нельзя назвать предложением.
        let mut no_guid = notes[0].clone();
        no_guid["guid"] = json!(null);
        no_guid["fields"] = json!(["дубль", "значение", ""]);
        notes.push(no_guid);
        // Заметка с повторяющимся guid: адресация неоднозначна.
        let mut duplicate_guid = notes[0].clone();
        duplicate_guid["fields"] = json!(["ещё один", "значение", ""]);
        notes.push(duplicate_guid);
    }));

    let (code, parsed) = review_json(&temp.path().to_string_lossy(), &["--all", "--limit", "10"]);
    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(
        result["notes_total"], 4,
        "неадресуемые заметки остаются частью экспорта"
    );
    assert_eq!(
        result["excluded_unaddressable"], 3,
        "исключены заметка без guid и оба вхождения повторённого guid: \
         адрес неоднозначен у каждого из них"
    );
    assert_eq!(result["total_selected"], 1);
    assert_eq!(
        result["items"][0]["guid"], "guid-2",
        "в batch остаётся единственная однозначно адресуемая заметка"
    );

    // То же для критерия по полю: исключение не зависит от вида критерия.
    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &[
            "--field",
            "Значение",
            "--value",
            "значение",
            "--limit",
            "10",
        ],
    );
    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(result["excluded_unaddressable"], 2);
    assert_eq!(
        result["total_selected"], 0,
        "обе подходящие заметки неадресуемы, и это видно снаружи"
    );
}

#[test]
fn guid_lookup_of_an_unresolvable_note_reports_the_exclusion() {
    let temp = TempDir::new("review-guid-unresolvable");
    temp.write_export(&export_with(|value| {
        value["notes"][0]["note_model_uuid"] = json!("нет-такой-модели");
    }));

    let (code, parsed) = review_json(
        &temp.path().to_string_lossy(),
        &["--guid", "guid-1", "--limit", "10"],
    );
    assert_eq!(
        code, 0,
        "identity lookup нашёл заметку, отказом это не является"
    );
    let result = &parsed["result"];
    assert_eq!(result["total_selected"], 0);
    assert_eq!(result["returned"], 0);
    assert_eq!(
        result["excluded_unaddressable"], 1,
        "пустая страница объяснена, а не молчалива"
    );
}
