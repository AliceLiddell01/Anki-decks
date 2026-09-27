//! Контракт CLI команды `qa`: коды правил, границы вывода, exit codes.
//!
//! Свойства findings проверяются на синтетических экспортах, а на канонических
//! колодах — только то, что не зависит от текущего содержимого репозитория:
//! форма ответа, детерминизм и согласованность counts с самим выводом.
//!
//! Ни одно ожидаемое число не зашито в тест: counts по каждому коду
//! пересчитываются из сырого `deck.json` независимым oracle'ом
//! ([`common::raw_qa_counts`]), а адреса каждого показанного finding'а
//! проверяются по сырым заметкам, а не по выводу tool'а.

mod common;

use common::{
    QA_CODES, TempDir, collect_notes, export_with, parse_json, raw_field_position, raw_json,
    raw_models, raw_qa_counts, run_cli, words_deck,
};
use serde_json::{Value, json};

fn qa_json(export: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["--json", "qa", export];
    args.extend_from_slice(extra);
    let (code, stdout, _) = run_cli(&args);
    (code, parse_json(&stdout))
}

/// Экспорт, в котором есть по одному finding каждого правила.
fn rules_export() -> Value {
    export_with(|value| {
        value["notes"][0]["fields"] = json!([" <span style=\"color: #fff\">x</span> ", "значение"]);
        value["notes"][1]["fields"] = json!(["", "значение"]);
        let mut duplicate = value["notes"][1].clone();
        duplicate["guid"] = json!("guid-3");
        value["notes"]
            .as_array_mut()
            .expect("notes")
            .push(duplicate);
    })
}

#[test]
fn rules_registry_is_listed_with_stable_codes() {
    let temp = TempDir::new("qa-rules");
    temp.write_export(&rules_export());
    let (code, parsed) = qa_json(&temp.path().to_string_lossy(), &[]);
    assert_eq!(code, 0);
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["command"], "qa");

    let rules = parsed["result"]["rules"].as_array().expect("rules");
    let codes: Vec<&str> = rules
        .iter()
        .map(|rule| rule["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        vec![
            "empty_field_value",
            "leading_whitespace",
            "trailing_whitespace",
            "forbidden_white_span",
            "duplicate_note_content",
            "duplicate_primary_field"
        ]
    );
    for rule in rules {
        assert!(
            rule["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert!(rule["applicable"].as_bool().is_some());
        assert!(rule["findings"].as_u64().is_some());
    }

    let severities: Vec<&str> = rules
        .iter()
        .map(|rule| rule["severity"].as_str().expect("severity"))
        .collect();
    assert_eq!(
        severities,
        vec!["warning", "warning", "warning", "error", "warning", "info"]
    );
}

#[test]
fn qa_reports_error_severity_findings_without_failing() {
    let temp = TempDir::new("qa-error-severity");
    temp.write_export(&rules_export());
    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "forbidden_white_span"],
    );

    assert_eq!(code, 0, "QA ERROR не делает команду неуспешной");
    assert_eq!(parsed["result"]["findings_total"], 1);
    let finding = &parsed["result"]["findings"][0];
    assert_eq!(finding["severity"], "error");
    assert_eq!(finding["code"], "forbidden_white_span");
    assert_eq!(finding["note_index"], 0);
    assert_eq!(finding["guid"], "guid-1");
    assert_eq!(finding["field"], "Слово");
    assert_eq!(finding["field_ord"], 0);
    assert_eq!(finding["evidence"]["occurrences"], 1);
    assert!(
        finding["evidence"]["first_tag"]
            .as_str()
            .is_some_and(|tag| tag.contains("color: #fff"))
    );
}

#[test]
fn evidence_stays_bounded_and_never_contains_full_field_value() {
    let long = "я".repeat(2000);
    let temp = TempDir::new("qa-bounded-evidence");
    temp.write_export(&export_with(|value| {
        value["notes"][0]["fields"][0] = json!(format!("{long} "));
    }));
    let (_, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "trailing_whitespace"],
    );

    let finding = &parsed["result"]["findings"][0];
    let sample = finding["evidence"]["sample"].as_str().expect("sample");
    assert!(
        sample.chars().count() <= 121,
        "выборка должна быть ограничена, длина {}",
        sample.chars().count()
    );
    assert!(sample.ends_with('…'));
    assert_eq!(finding["evidence"]["value_chars"], 2001);
    assert_eq!(finding["evidence"]["boundary"]["chars"], 1);
}

#[test]
fn max_per_code_bounds_output_but_not_counts() {
    let temp = TempDir::new("qa-max-per-code");
    temp.write_export(&export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..30 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("bulk-{index}"));
            copy["fields"] = json!(["слово", ""]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    }));

    let (_, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "empty_field_value", "--max-per-code", "5"],
    );
    assert_eq!(parsed["result"]["findings_total"], 31);
    assert_eq!(parsed["result"]["findings_returned"], 5);
    assert_eq!(parsed["result"]["truncated"], true);
    assert_eq!(parsed["result"]["max_per_code"], 5);
    assert_eq!(parsed["result"]["by_code"][0]["count"], 31);
    assert_eq!(
        parsed["result"]["findings"]
            .as_array()
            .expect("findings")
            .len(),
        5
    );

    let (_, complete) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "empty_field_value", "--max-per-code", "200"],
    );
    assert_eq!(complete["result"]["findings_returned"], 31);
    assert_eq!(complete["result"]["truncated"], false);
}

#[test]
fn output_is_deterministic_across_runs() {
    let temp = TempDir::new("qa-determinism");
    temp.write_export(&rules_export());
    let (_, first) = qa_json(&temp.path().to_string_lossy(), &[]);
    let (_, second) = qa_json(&temp.path().to_string_lossy(), &[]);
    assert_eq!(first["result"], second["result"]);
}

#[test]
fn unknown_code_is_a_usage_class_error_with_available_codes() {
    let temp = TempDir::new("qa-unknown-code");
    temp.write_export(&rules_export());
    let (code, parsed) = qa_json(&temp.path().to_string_lossy(), &["--code", "нет_такого"]);

    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_qa_code");
    let available = parsed["error"]["details"]["available_codes"]
        .as_array()
        .expect("available_codes");
    assert!(available.iter().any(|code| code == "empty_field_value"));
    assert_eq!(parsed["error"]["details"]["unknown_codes"][0], "нет_такого");
}

#[test]
fn human_mode_lists_every_rule_and_keeps_stdout_only() {
    let temp = TempDir::new("qa-human");
    temp.write_export(&rules_export());
    let (code, stdout, stderr) = run_cli(&["qa", &temp.path().to_string_lossy()]);

    assert_eq!(code, 0);
    assert!(stderr.is_empty());
    for needle in [
        "По кодам:",
        "Findings:",
        "Правила (6):",
        "empty_field_value",
        "duplicate_primary_field",
        "усечено",
    ] {
        assert!(
            stdout.contains(needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

#[test]
fn canonical_decks_report_independently_recomputed_counts() {
    for level in [1u8, 2, 3, 4, 5] {
        let export = words_deck(level);
        let raw = raw_json(level);
        let expected = raw_qa_counts(&raw);

        let mut notes = Vec::new();
        collect_notes(&raw, &mut notes);

        let (code, parsed) = qa_json(&export.to_string_lossy(), &["--max-per-code", "200"]);
        assert_eq!(code, 0, "N{level}");
        assert_eq!(
            parsed["result"]["notes_total"],
            notes.len(),
            "N{level}: число заметок"
        );

        let by_code = parsed["result"]["by_code"].as_array().expect("by_code");
        assert_eq!(by_code.len(), QA_CODES.len(), "N{level}: все коды в выводе");
        for code in QA_CODES {
            let count = by_code
                .iter()
                .find(|entry| entry["code"] == code)
                .map_or(0, |entry| entry["count"].as_u64().expect("count"));
            assert_eq!(
                count as usize, expected[code],
                "N{level}: {code} должен совпадать с независимым пересчётом"
            );
        }

        let total: u64 = by_code
            .iter()
            .map(|entry| entry["count"].as_u64().expect("count"))
            .sum();
        assert_eq!(
            total as usize, parsed["result"]["findings_total"],
            "N{level}: сумма по кодам"
        );

        // Реестр правил не зависит от содержимого колоды.
        assert_eq!(
            parsed["result"]["rules"].as_array().expect("rules").len(),
            QA_CODES.len(),
            "N{level}"
        );

        // Показанные findings не обрезаны, поэтому каждый адрес проверяется
        // прямо по сырым заметкам.
        assert_eq!(parsed["result"]["truncated"], false, "N{level}");
        assert_eq!(
            parsed["result"]["findings_returned"], parsed["result"]["findings_total"],
            "N{level}"
        );
        for finding in parsed["result"]["findings"].as_array().expect("findings") {
            assert_finding_matches_raw(&raw, finding, level);
        }
    }
}

/// Проверяет один finding по сырому `deck.json`.
///
/// Проверка идёт от адреса (`note_index` → заметка в сыром порядке обхода) и от
/// свойства правила, а не от сообщения tool'а: тест не повторяет формулировки,
/// а пересчитывает факт.
fn assert_finding_matches_raw(raw: &Value, finding: &Value, level: u8) {
    let code = finding["code"].as_str().expect("code");
    let label = format!("N{level}: {code} #{}", finding["note_index"]);

    let mut notes = Vec::new();
    collect_notes(raw, &mut notes);
    let position = finding["note_index"].as_u64().expect("note_index") as usize;
    let note = notes
        .get(position)
        .unwrap_or_else(|| panic!("{label}: нет заметки"));
    let models = raw_models(raw);

    if code.starts_with("duplicate_") {
        assert!(
            finding["addressable"].as_bool().expect("addressable"),
            "{label}: владелец группы обязан быть адресуемым"
        );
        let related = finding["related_note_indices"]
            .as_array()
            .expect("related_note_indices");
        assert!(
            related.len() >= 2,
            "{label}: группа не может состоять из одной заметки"
        );
        assert_eq!(
            finding["group_size"],
            finding["related_note_indices"]
                .as_array()
                .expect("related_note_indices")
                .len(),
            "{label}: группа не обрезана, размер совпадает со списком"
        );
        let guids = finding["related_guids"].as_array().expect("related_guids");
        assert_eq!(
            guids.len(),
            related.len(),
            "{label}: guid на каждого участника"
        );

        let members: Vec<&Value> = related
            .iter()
            .map(|position| {
                *notes
                    .get(position.as_u64().expect("note_index") as usize)
                    .unwrap_or_else(|| panic!("{label}: нет участника группы"))
            })
            .collect();
        match code {
            "duplicate_note_content" => {
                let first = serde_json::to_string(&members[0]["fields"]).expect("fields");
                for member in &members {
                    assert_eq!(
                        serde_json::to_string(&member["fields"]).expect("fields"),
                        first,
                        "{label}: участники обязаны иметь одинаковые fields"
                    );
                }
            }
            _ => {
                for member in &members {
                    let ord = raw_field_position(&models, member, "Слово");
                    assert_eq!(
                        member["fields"][ord],
                        members[0]["fields"][raw_field_position(&models, members[0], "Слово")],
                        "{label}: участники обязаны иметь одинаковое головное поле"
                    );
                }
            }
        }
        return;
    }

    let ord = raw_field_position(&models, note, finding["field"].as_str().expect("field"));
    let value = note["fields"][ord]
        .as_str()
        .unwrap_or_else(|| panic!("{label}: значение поля должно быть строкой"));

    match code {
        "empty_field_value" => {
            assert!(value.is_empty(), "{label}: значение должно быть пустым");
            assert_eq!(finding["evidence"]["value_chars"], 0, "{label}");
        }
        "leading_whitespace" => {
            assert!(
                value.starts_with(char::is_whitespace),
                "{label}: значение должно начинаться с whitespace: {value:?}"
            );
        }
        "trailing_whitespace" => {
            assert!(
                value.ends_with(char::is_whitespace),
                "{label}: значение должно заканчиваться whitespace: {value:?}"
            );
        }
        "forbidden_white_span" => {
            assert!(
                finding["evidence"]["occurrences"]
                    .as_u64()
                    .expect("occurrences")
                    >= 1
                    && value.contains("<span"),
                "{label}: значение должно содержать <span>-обёртку"
            );
        }
        other => panic!("{label}: неизвестный код {other}"),
    }
}

#[test]
fn empty_field_findings_point_at_a_real_empty_value() {
    let export = words_deck(1);
    let (code, parsed) = qa_json(&export.to_string_lossy(), &["--code", "empty_field_value"]);
    assert_eq!(code, 0);

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert!(!findings.is_empty(), "в N1 есть пустые «Ударение»");

    // Проверяем адрес каждого показанного finding прямо по сырому deck.json.
    let text = std::fs::read_to_string(export.join("deck.json")).expect("deck.json");
    let raw: Value = serde_json::from_str(&text).expect("JSON");
    let notes = raw["notes"].as_array().expect("notes");
    for finding in findings {
        let index = finding["note_index"].as_u64().expect("note_index") as usize;
        let ord = finding["field_ord"].as_u64().expect("field_ord") as usize;
        assert_eq!(finding["field"], "Ударение");
        assert_eq!(finding["severity"], "warning");
        assert_eq!(
            notes[index]["fields"][ord], "",
            "finding должен указывать на пустое значение"
        );
        assert_eq!(finding["evidence"]["value_chars"], 0);
    }
}

#[test]
fn findings_on_unaddressable_notes_are_marked_and_counted() {
    let temp = TempDir::new("qa-unaddressable");
    let export = export_with(|value| {
        let notes = value["notes"].as_array_mut().expect("notes");
        let mut no_guid = notes[0].clone();
        no_guid["guid"] = json!(null);
        no_guid["fields"] = json!(["", "значение", ""]);
        notes.push(no_guid);
        let mut duplicate_guid = notes[0].clone();
        duplicate_guid["fields"] = json!(["", "значение", ""]);
        notes.push(duplicate_guid);
    });
    temp.write_export(&export);

    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "empty_field_value", "--max-per-code", "200"],
    );
    assert_eq!(code, 0, "неадресуемость — не отказ команды");

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert_eq!(
        findings.len(),
        5,
        "findings остаются диагностикой содержимого, даже если их нельзя исполнить"
    );
    assert_eq!(parsed["result"]["unaddressable_findings"], 4);

    // Ожидание считается по самому fixture: заметка неадресуема, если её guid
    // отсутствует или встречается больше одного раза.
    let notes = export["notes"].as_array().expect("notes");
    let guids: Vec<Option<&str>> = notes.iter().map(|note| note["guid"].as_str()).collect();
    let duplicated: Vec<&str> = guids
        .iter()
        .flatten()
        .filter(|guid| guids.iter().filter(|other| **other == Some(**guid)).count() > 1)
        .copied()
        .collect();

    let mut marked = 0;
    let mut addressable = 0;
    for finding in findings {
        let index = finding["note_index"].as_u64().expect("note_index") as usize;
        let guid = guids[index];
        let expected = guid.is_none_or(|guid| duplicated.contains(&guid));
        assert_eq!(
            finding["addressable"], !expected,
            "адресуемость обязана совпасть с сырым guid: {finding}"
        );
        assert_eq!(
            finding["guid"], notes[index]["guid"],
            "guid заметки не подменяется"
        );
        marked += usize::from(expected);
        addressable += usize::from(!expected);
    }
    assert_eq!(marked, 4);
    assert_eq!(
        addressable, 1,
        "правило не помечает неадресуемым всё подряд: заметка с уникальным guid остаётся целью"
    );
}

#[test]
fn qa_summarizes_the_full_group_for_grouped_findings() {
    let temp = TempDir::new("qa-group-context");
    temp.write_export(&export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 3..6 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("guid-{index}"));
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    }));

    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "duplicate_note_content"],
    );
    assert_eq!(code, 0);
    let finding = &parsed["result"]["findings"][0];
    assert_eq!(finding["group_size"], 4);
    assert_eq!(finding["related_note_indices"], json!([0, 2, 3, 4]));
    assert_eq!(
        finding["related_guids"],
        json!(["guid-1", "guid-3", "guid-4", "guid-5"]),
        "участники названы так, чтобы предложение можно было исполнить"
    );
    assert_eq!(finding["related_truncated"], false);
    assert_eq!(
        finding["evidence"]["fields_total"], 3,
        "evidence хранит только то, что относится к правилу"
    );
}

#[test]
fn human_truncation_message_names_the_real_way_to_see_the_rest() {
    let temp = TempDir::new("qa-human-truncated");
    temp.write_export(&export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..30 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("bulk-{index}"));
            copy["fields"] = json!(["слово", ""]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    }));

    let (code, stdout, _) = run_cli(&[
        "qa",
        &temp.path().to_string_lossy(),
        "--code",
        "empty_field_value",
        "--max-per-code",
        "5",
    ]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("--max-per-code"),
        "сообщение обязано называть реальный способ увидеть остаток: {stdout}"
    );
    assert!(
        !stdout.contains("полный список доступен"),
        "обещание полного списка неверно: JSON сериализует тот же vector\n{stdout}"
    );
    assert!(stdout.contains("остальные не выводятся"), "{stdout}");
}

/// Шкала QA не связана со шкалой `validate`: `error` у правила — это серьёзный
/// дефект содержимого, а не структурная ошибка экспорта.
#[test]
fn qa_error_severity_neither_invalidates_the_export_nor_blocks_edit() {
    let temp = TempDir::new("qa-error-independence");
    temp.write_canonical_deck_json(&export_with(|value| {
        value["notes"][0]["fields"][0] = json!("<span style=\"color: #fff\">偶然</span>");
    }));
    let export = temp.path().to_string_lossy().to_string();

    let (code, parsed) = qa_json(&export, &["--code", "forbidden_white_span"]);
    assert_eq!(code, 0);
    assert_eq!(parsed["result"]["findings"][0]["severity"], "error");

    // Экспорт с QA `error` остаётся структурно валидным: это другая ось.
    let (code, stdout, _) = run_cli(&["--json", "validate", &export]);
    assert_eq!(code, 0, "QA error не делает export невалидным");
    let validated = parse_json(&stdout);
    assert_eq!(validated["result"]["valid"], true);
    assert_eq!(validated["result"]["summary"]["errors"], 0);

    // И не мешает правке значения поля.
    let request = common::edit_request(&[(
        "guid-1",
        "Слово",
        "<span style=\"color: #fff\">偶然</span>",
        "偶然",
    )]);
    let path = common::write_request(&temp, "edit.json", &request);
    let (code, stdout, _) = run_cli(&[
        "--json",
        "edit",
        &export,
        "--request",
        &path.to_string_lossy(),
    ]);
    assert_eq!(code, 0, "QA findings не блокируют edit");
    let planned = parse_json(&stdout);
    assert_eq!(planned["result"]["effective_edits"], 1);
    assert_eq!(planned["result"]["applied"], false);
}
