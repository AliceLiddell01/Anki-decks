//! Контракт CLI команды `review-check` и сквозной путь
//! `qa/review → предложения → review-check → edit`.
//!
//! Ключевые свойства: `review-check` ничего не пишет, отдаёт готовый запрос
//! Stage 2 только для полностью валидного документа и различает stale,
//! invalid и conflict.

mod common;

use std::fs;
use std::process::Stdio;

use common::{
    TempDir, edit_request, parse_json, run_cli, run_cli_with_stdin_in, single_line_change,
    write_request,
};
use serde_json::{Value, json};

/// Экспорт с двумя заметками, у которых пустое «Пример».
fn proposals_export() -> Value {
    common::export_with(|value| {
        value["notes"][0]["fields"] = json!(["слово-1", "значение", ""]);
        value["notes"][1]["fields"] = json!(["слово-2", "значение", ""]);
    })
}

fn check_json(export: &str, request: &Value, name: &str) -> (i32, Value) {
    let temp = TempDir::new("review-check-request");
    let path = write_request(&temp, name, request);
    let (code, stdout, _) = run_cli(&[
        "--json",
        "review-check",
        export,
        "--proposals",
        &path.to_string_lossy(),
    ]);
    (code, parse_json(&stdout))
}

#[test]
fn valid_proposals_produce_a_stage_two_request() {
    let temp = TempDir::new("review-check-valid");
    temp.write_export(&proposals_export());
    let request = edit_request(&[
        ("guid-1", "Пример", "", "пример-1"),
        ("guid-2", "Пример", "", "пример-2"),
    ]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 0);
    assert_eq!(parsed["command"], "review-check");
    let result = &parsed["result"];
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["proposals_total"], 2);
    assert_eq!(result["counts"]["valid"], 2);
    assert_eq!(result["effective_proposals"], 2);
    assert_eq!(result["proposals_truncated"], false);

    let proposal = &result["proposals"][0];
    assert_eq!(proposal["proposal_index"], 0);
    assert_eq!(proposal["proposal_id"], "e0");
    assert_eq!(proposal["status"], "valid");
    assert_eq!(proposal["guid"], "guid-1");
    assert_eq!(proposal["field"], "Пример");
    assert_eq!(proposal["note_index"], 0);
    assert_eq!(proposal["deck_path"], "Test::Deck");
    assert_eq!(proposal["field_ord"], 2);
    assert_eq!(proposal["current_len"], 0);
    assert_eq!(proposal["expected_len"], 0);
    assert_eq!(proposal["replacement_len"], 8);

    // Готовый запрос принимается `edit` без переупаковки.
    let emitted = &result["edit_request"];
    assert_eq!(emitted["schema_version"], 1);
    assert_eq!(emitted["edits"].as_array().expect("edits").len(), 2);
    assert_eq!(emitted["edits"][0]["guid"], "guid-1");
    assert_eq!(emitted["edits"][0]["field"], "Пример");
    assert_eq!(emitted["edits"][0]["expected"], "");
    assert_eq!(emitted["edits"][0]["replacement"], "пример-1");
}

#[test]
fn review_check_never_touches_the_export() {
    let temp = TempDir::new("review-check-readonly");
    temp.write_export(&proposals_export());
    let deck = temp.path().join("deck.json");
    let before = fs::read(&deck).expect("deck.json");

    let request = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let (code, _) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 0);
    let after = fs::read(&deck).expect("deck.json");
    assert_eq!(before, after, "review-check не пишет в deck.json");
    let entries: Vec<String> = fs::read_dir(temp.path())
        .expect("каталог")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    assert_eq!(entries, vec!["deck.json"], "review-check не создаёт файлов");
}

#[test]
fn mismatched_expected_value_is_a_conflict_and_blocks_the_request() {
    let temp = TempDir::new("review-check-conflict");
    temp.write_export(&proposals_export());
    let request = edit_request(&[("guid-1", "Пример", "устаревшее", "новое")]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 7, "stale — это source_changed/conflict класс");
    let result = &parsed["result"];
    assert_eq!(result["outcome"], "stale");
    assert_eq!(result["counts"]["conflict"], 1);
    assert_eq!(result["effective_proposals"], 0);
    assert_eq!(result["edit_request"], Value::Null);
    assert_eq!(result["proposals"][0]["status"], "conflict");
}

#[test]
fn no_request_is_emitted_when_nothing_is_effective() {
    let temp = TempDir::new("review-check-noop");
    temp.write_export(&proposals_export());
    let request = edit_request(&[
        ("guid-1", "Пример", "", ""),                   // значение не меняется
        ("guid-2", "Значение", "значение", "значение"), // тоже без изменения
        ("guid-2", "Пример", "не то", "новое"),         // конфликт
    ]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 7);
    let result = &parsed["result"];
    assert_eq!(result["counts"]["already_correct"], 2);
    assert_eq!(result["counts"]["conflict"], 1);
    assert_eq!(result["edit_request"], Value::Null);
}

#[test]
fn duplicate_targets_are_rejected_by_the_shared_request_validator() {
    let temp = TempDir::new("review-check-duplicate");
    temp.write_export(&proposals_export());
    let request = edit_request(&[
        ("guid-1", "Пример", "", "пример-1"),
        ("guid-1", "Пример", "", "пример-2"),
    ]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(
        code, 3,
        "проблема уровня документа, а не отдельного предложения"
    );
    assert_eq!(parsed["error"]["code"], "duplicate_edit_target");
}

#[test]
fn unknown_guid_is_reported_per_proposal_while_valid_ones_survive() {
    let temp = TempDir::new("review-check-invalid");
    temp.write_export(&proposals_export());
    let request = edit_request(&[
        ("guid-1", "Пример", "", "пример-1"),
        ("нет-такого", "Пример", "", "пример-2"),
        ("guid-2", "НетТакогоПоля", "", "значение"),
    ]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 4, "есть note_not_found, но нет ambiguous");
    let result = &parsed["result"];
    assert_eq!(result["outcome"], "invalid");
    assert_eq!(result["counts"]["valid"], 1);
    assert_eq!(result["counts"]["invalid"], 2);
    assert_eq!(result["effective_proposals"], 1);

    let proposals = result["proposals"].as_array().expect("proposals");
    assert_eq!(proposals[1]["status"], "invalid");
    assert_eq!(proposals[1]["problem"], "note_not_found");
    assert!(
        proposals[1]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
    assert_eq!(proposals[2]["problem"], "unknown_field");

    // Валидная часть не попадает в запрос: документ в целом не ok.
    assert_eq!(result["edit_request"], Value::Null);
}

#[test]
fn ambiguous_guid_outranks_the_other_invalid_kinds() {
    let temp = TempDir::new("review-check-ambiguous");
    temp.write_export(&common::export_with(|value| {
        let mut duplicate = value["notes"][0].clone();
        duplicate["fields"] = json!(["слово", "значение", ""]);
        value["notes"][0]["fields"] = json!(["слово", "значение", ""]);
        value["notes"]
            .as_array_mut()
            .expect("notes")
            .push(duplicate);
    }));
    let request = edit_request(&[
        ("guid-1", "Пример", "", "пример-1"),
        ("нет-такого", "Пример", "", "пример-2"),
    ]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 5, "ambiguous важнее note_not_found");
    assert_eq!(parsed["result"]["counts"]["invalid"], 2);
}

#[test]
fn already_applied_proposals_are_reported_without_a_request() {
    let temp = TempDir::new("review-check-applied");
    temp.write_canonical_deck_json(&proposals_export());
    let export = temp.path().to_string_lossy().to_string();

    let apply = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let path = write_request(&temp, "apply.json", &apply);
    let (code, _, _) = run_cli(&[
        "edit",
        &export,
        "--request",
        &path.to_string_lossy(),
        "--apply",
    ]);
    assert_eq!(code, 0);

    let (code, parsed) = check_json(&export, &apply, "again.json");
    assert_eq!(code, 0, "уже применённое — не ошибка");
    assert_eq!(parsed["result"]["outcome"], "ok");
    assert_eq!(parsed["result"]["counts"]["already_applied"], 1);
    assert_eq!(parsed["result"]["effective_proposals"], 0);
    assert_eq!(parsed["result"]["edit_request"], Value::Null);
}

#[test]
fn report_is_bounded_while_counts_stay_complete() {
    let temp = TempDir::new("review-check-bounded");
    temp.write_export(&common::export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..60 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("bulk-{index}"));
            copy["fields"] = json!(["слово", "значение", ""]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    }));

    let request = json!({
        "schema_version": 1,
        "edits": (0..60)
            .map(|index| json!({
                "edit_id": format!("e{index}"),
                "guid": format!("bulk-{index}"),
                "field": "Пример",
                "expected": "",
                "replacement": format!("пример-{index}"),
            }))
            .collect::<Vec<_>>(),
    });
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    assert_eq!(code, 0);
    let result = &parsed["result"];
    assert_eq!(result["proposals_total"], 60);
    assert_eq!(result["counts"]["valid"], 60);
    assert_eq!(result["effective_proposals"], 60);
    assert_eq!(result["proposals_truncated"], true);
    assert_eq!(
        result["proposals"].as_array().expect("proposals").len(),
        50,
        "отчёт ограничен, а счётчики и запрос — нет"
    );
    assert_eq!(
        result["edit_request"]["edits"]
            .as_array()
            .expect("edits")
            .len(),
        60
    );
}

#[test]
fn proposals_can_be_read_from_stdin() {
    let temp = TempDir::new("review-check-stdin");
    temp.write_export(&proposals_export());
    let request = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let raw = serde_json::to_vec(&request).expect("сериализация");

    let (code, stdout, _) = run_cli_with_stdin_in(
        None,
        &[
            "--json",
            "review-check",
            &temp.path().to_string_lossy(),
            "--proposals",
            "-",
        ],
        &raw,
    );

    assert_eq!(code, 0);
    let parsed = parse_json(&stdout);
    assert_eq!(parsed["result"]["counts"]["valid"], 1);
}

#[test]
fn non_string_field_value_is_invalid_not_an_error() {
    let temp = TempDir::new("review-check-non-string");
    temp.write_export(&common::export_with(|value| {
        value["notes"][0]["fields"][2] = json!(7);
    }));
    let request = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let (code, parsed) = check_json(&temp.path().to_string_lossy(), &request, "p.json");

    // Значение поля, которое нельзя править как строку, — неисполнимое
    // предложение, а не внутренняя ошибка инструмента.
    assert_eq!(code, 3);
    let proposal = &parsed["result"]["proposals"][0];
    assert_eq!(proposal["status"], "invalid");
    assert_eq!(proposal["problem"], "field_not_resolvable");
    assert!(
        proposal["message"]
            .as_str()
            .is_some_and(|message| message.contains("строкового значения")),
        "{proposal}"
    );
    assert_eq!(parsed["result"]["edit_request"], Value::Null);
}

#[test]
fn broken_document_is_a_usage_class_error_before_any_report() {
    let temp = TempDir::new("review-check-broken");
    temp.write_export(&proposals_export());
    let export = temp.path().to_string_lossy().to_string();

    // Все проблемы уровня всего документа — это `invalid_request`/exit 3:
    // ни одно предложение не проверяется, пока документ не разобран.
    let cases: [(Value, &str); 4] = [
        (
            json!({"schema_version": 9, "edits": [{"guid": "guid-1", "field": "Пример",
                "expected": "", "replacement": "x"}]}),
            "unsupported_schema_version",
        ),
        (json!({"schema_version": 1, "edits": []}), "empty_request"),
        (json!({"schema_version": 1}), "malformed_request"),
        (
            json!({"schema_version": 1, "edits": [{"guid": "guid-1", "field": "Пример",
                "expected": "", "replacement": "x"}], "extra": true}),
            "malformed_request",
        ),
    ];

    for (index, (document, reason)) in cases.iter().enumerate() {
        let (code, parsed) = check_json(&export, document, &format!("bad-{index}.json"));
        assert_eq!(code, 3, "{reason}");
        assert_eq!(parsed["error"]["code"], "invalid_request", "{reason}");
        assert_eq!(parsed["error"]["details"]["reason"], *reason);
        assert!(parsed["result"].is_null(), "отчёта быть не должно");
    }
}

#[test]
fn missing_proposals_file_is_an_input_error() {
    let temp = TempDir::new("review-check-missing");
    temp.write_export(&proposals_export());
    let missing = temp.path().join("нет.json");
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "review-check",
        &temp.path().to_string_lossy(),
        "--proposals",
        &missing.to_string_lossy(),
    ]);

    assert_eq!(code, 3);
    assert!(stderr.is_empty(), "--json уносит ошибку в stdout: {stderr}");
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        "input_unreadable",
        "отсутствующий документ — ошибка ввода, а не домена"
    );
}

#[test]
fn human_mode_explains_statuses_and_the_missing_request() {
    let temp = TempDir::new("review-check-human");
    temp.write_export(&proposals_export());
    let request = edit_request(&[
        ("guid-1", "Пример", "", "пример-1"),
        ("guid-2", "Пример", "не то", "пример-2"),
    ]);
    let path = write_request(&temp, "p.json", &request);
    let (code, stdout, stderr) = run_cli(&[
        "review-check",
        &temp.path().to_string_lossy(),
        "--proposals",
        &path.to_string_lossy(),
    ]);

    assert_eq!(code, 7);
    assert!(stderr.is_empty(), "отчёт уходит в stdout: {stderr}");
    for needle in [
        "Итог: stale",
        "valid 1",
        "conflict 1",
        "Готовый запрос для Stage 2: нет: отчёт не ok",
        "#0 valid",
        "#1 conflict",
        "ожидалось: не то",
    ] {
        assert!(
            stdout.contains(needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

#[test]
fn review_check_accepts_the_request_alias_and_reports_unknown_arguments() {
    let temp = TempDir::new("review-check-alias");
    temp.write_export(&proposals_export());
    let request = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let path = write_request(&temp, "p.json", &request);

    let (code, stdout, _) = run_cli(&[
        "--json",
        "review-check",
        &temp.path().to_string_lossy(),
        "--request",
        &path.to_string_lossy(),
    ]);
    assert_eq!(code, 0);
    assert_eq!(parse_json(&stdout)["result"]["counts"]["valid"], 1);

    let (code, _, stderr) = run_cli(&["review-check", &temp.path().to_string_lossy()]);
    assert_eq!(code, 2, "без документа команда бесполезна");
    assert!(stderr.contains("--proposals"));
}

/// Сквозной путь: `review` → предложения → `review-check` → `edit` → `validate`.
#[test]
fn full_pipeline_applies_exactly_one_reviewed_change() {
    let export_dir = TempDir::new("pipeline-export");
    export_dir.write_canonical_deck_json(&common::export_with(|value| {
        value["notes"][0]["fields"] = json!(["偶然", "[sound:случайность.mp3]случайность", ""]);
    }));
    let export = export_dir.path().to_string_lossy().to_string();

    // 1. Агент получает компактный batch по конкретному QA-коду.
    let (code, stdout, _) = run_cli(&[
        "--json",
        "review",
        &export,
        "--qa-code",
        "empty_field_value",
        "--limit",
        "1",
    ]);
    assert_eq!(code, 0);
    let batch = parse_json(&stdout);
    let item = batch["result"]["items"][0].clone();
    assert_eq!(item["guid"], "guid-1");
    assert_eq!(item["fields"]["Пример"], "");

    // 2. Внешний агент возвращает предложение в форме Stage 2.
    let proposals = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let proposals_path = write_request(&export_dir, "proposals.json", &proposals);

    // 3. Toolkit проверяет предложения против текущего состояния экспорта.
    let (code, stdout, _) = run_cli(&[
        "--json",
        "review-check",
        &export,
        "--proposals",
        &proposals_path.to_string_lossy(),
    ]);
    assert_eq!(code, 0);
    let check = parse_json(&stdout);
    assert_eq!(check["result"]["outcome"], "ok");
    let emitted = check["result"]["edit_request"].clone();

    // 4. Готовый запрос идёт в `edit` без переупаковки: сначала dry-run.
    let request_path = write_request(&export_dir, "request.json", &emitted);
    let before = fs::read(export_dir.path().join("deck.json")).expect("deck.json");
    let (code, stdout, _) = run_cli(&[
        "edit",
        &export,
        "--request",
        &request_path.to_string_lossy(),
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("dry-run"));
    assert_eq!(
        before,
        fs::read(export_dir.path().join("deck.json")).expect("deck.json"),
        "dry-run не пишет"
    );

    // 5. Только явный --apply меняет deck.json, и ровно одну строку.
    let (code, stdout, _) = run_cli(&[
        "edit",
        &export,
        "--request",
        &request_path.to_string_lossy(),
        "--apply",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("заменён атомарно"));
    let after = fs::read(export_dir.path().join("deck.json")).expect("deck.json");
    // Канонический JSON печатает значение поля отдельной строкой.
    let (before_line, after_line) = single_line_change(&before, &after);
    assert_eq!(before_line.trim(), "\"\"", "изменилась не та строка");
    assert_eq!(
        after_line.trim(),
        "\"пример-1\"",
        "замена не попала в строку"
    );

    // 6. Экспорт остаётся валидным, а QA-finding по правленой заметке исчезает.
    let (code, stdout, _) = run_cli(&["--json", "validate", &export]);
    assert_eq!(code, 0, "валидность сохраняется: {stdout}");
    assert_eq!(parse_json(&stdout)["result"]["valid"], true);

    let (code, stdout, _) = run_cli(&["--json", "qa", &export, "--code", "empty_field_value"]);
    assert_eq!(code, 0);
    let qa = parse_json(&stdout);
    assert_eq!(
        qa["result"]["findings_total"], 1,
        "остаётся только вторая заметка fixture"
    );
    assert_eq!(
        qa["result"]["findings"][0]["guid"], "guid-2",
        "правленая заметка больше не нарушает правило"
    );

    // 7. Повторная проверка тех же предложений видна как already_applied.
    let (code, stdout, _) = run_cli(&[
        "--json",
        "review-check",
        &export,
        "--proposals",
        &proposals_path.to_string_lossy(),
    ]);
    assert_eq!(code, 0);
    assert_eq!(
        parse_json(&stdout)["result"]["counts"]["already_applied"],
        1
    );
}

#[test]
fn pipeline_stdout_can_be_piped_through_shell_redirection() {
    let temp = TempDir::new("pipeline-pipe");
    temp.write_export(&proposals_export());
    let export = temp.path().to_string_lossy().to_string();
    let request = edit_request(&[("guid-1", "Пример", "", "пример-1")]);
    let path = write_request(&temp, "p.json", &request);

    // Проверяем, что JSON-ответ — один документ, который парсится целиком.
    let output = std::process::Command::new(common::cli_binary())
        .args([
            "--json",
            "review-check",
            &export,
            "--proposals",
            &path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("anki-repo должен запускаться");

    assert!(output.status.success());
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("stdout — один JSON");
    assert_eq!(parsed["result"]["outcome"], "ok");
}
