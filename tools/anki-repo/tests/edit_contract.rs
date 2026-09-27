//! Контракт команды `edit`: режимы, предусловия, отчёты и exit codes.
//!
//! Все проверки идут через настоящий CLI-бинарник на временных копиях
//! экспортов, поэтому тесты фиксируют именно тот контракт, который видит
//! агент: код возврата, JSON-схему результата и текст ошибки.

mod common;

use common::{
    TempDir, base_export, edit_request, run_cli_in, run_cli_with_stdin_in, single_line_change,
    write_request,
};
use serde_json::Value;

/// Канонический синтетический экспорт из `base_export`.
fn canonical_fixture(label: &str) -> TempDir {
    let dir = TempDir::new(label);
    dir.write_canonical_deck_json(&base_export());
    dir
}

/// Разбирает stdout как JSON-документ.
fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout).expect("stdout должен быть валидным JSON")
}

/// `result` из успешного JSON-ответа.
fn result_of(exit: i32, stdout: &str) -> Value {
    assert_eq!(exit, 0, "ожидался успех, stdout: {stdout}");
    let document = parse_json(stdout);
    assert_eq!(document["command"], "edit");
    document["result"].clone()
}

/// `error` из неуспешного JSON-ответа.
fn error_of(exit: i32, stdout: &str) -> Value {
    assert_ne!(exit, 0, "ожидалась ошибка, stdout: {stdout}");
    let document = parse_json(stdout);
    assert_eq!(document["command"], "edit");
    assert!(document.get("result").is_none(), "у ошибки нет result");
    document["error"].clone()
}

#[test]
fn dry_run_plans_the_change_and_keeps_the_file() {
    let dir = canonical_fixture("edit-dry-run");
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["applied"], false);
    assert_eq!(result["edits_total"], 1);
    assert_eq!(result["effective_edits"], 1);
    assert_eq!(result["changed_lines"], 1);
    assert_eq!(result["summaries"]["dry_run"], 1);
    assert_eq!(result["outcomes"][0]["status"], "dry_run");
    assert_eq!(result["outcomes"][0]["field_ord"], 1);
    assert_eq!(result["outcomes"][0]["deck_path"], "Test::Deck");
    assert_eq!(result["checks"]["source_canonical"], true);
    assert_eq!(result["checks"]["candidate_reparsed"], true);
    assert_eq!(result["checks"]["semantic_targets_verified"], true);
    assert_eq!(result["checks"]["diff_shape_is_exactly_requested"], true);
    assert_eq!(result["checks"]["byte_delta_matches_token_delta"], true);

    assert_eq!(dir.deck_json_bytes(), before, "dry-run не пишет файл");
}

#[test]
fn apply_writes_exactly_the_requested_line() {
    let dir = canonical_fixture("edit-apply");
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-2",
            "--field",
            "Пример",
            "--expect",
            "",
            "--set",
            "必然の一致",
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["applied"], true);
    assert_eq!(result["dry_run"], false);
    assert_eq!(result["summaries"]["applied"], 1);
    assert_eq!(result["effective_edits"], 1);
    assert_eq!(result["changed_lines"], 1);

    let after = dir.deck_json_bytes();
    assert_ne!(after, before);

    let (old_line, new_line) = single_line_change(&before, &after);
    assert_eq!(old_line.trim(), "\"\"");
    assert_eq!(new_line.trim(), "\"必然の一致\"");
    let indent = old_line.len() - old_line.trim_start().len();
    assert!(indent > 0, "строка значения должна быть с отступом");
    assert_eq!(
        &new_line[..indent],
        &old_line[..indent],
        "отступ строки значения должен сохраниться"
    );

    // Значение действительно изменилось в документе, и только оно.
    assert_eq!(dir.deck_json()["notes"][1]["fields"][2], "必然の一致");
    assert_eq!(dir.deck_json()["notes"][0]["fields"][1], "случайность");
}

#[test]
fn apply_is_idempotent_and_reports_already_applied() {
    let dir = canonical_fixture("edit-idempotent");
    let args = |apply: &str| {
        vec![
            "edit".to_string(),
            dir.path().to_str().expect("путь").to_string(),
            "--json".to_string(),
            "--guid".to_string(),
            "guid-1".to_string(),
            "--field".to_string(),
            "Значение".to_string(),
            "--expect".to_string(),
            "случайность".to_string(),
            "--set".to_string(),
            "случайность!".to_string(),
            apply.to_string(),
        ]
    };

    let first = args("--apply");
    let (exit, stdout, _) = run_cli_in(None, &first.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(result_of(exit, &stdout)["applied"], true);
    let after_first = dir.deck_json_bytes();

    let second = args("--apply");
    let (exit, stdout, _) =
        run_cli_in(None, &second.iter().map(String::as_str).collect::<Vec<_>>());

    let result = result_of(exit, &stdout);
    assert_eq!(result["applied"], false, "повторная запись не нужна");
    assert_eq!(result["effective_edits"], 0);
    assert_eq!(result["changed_lines"], 0);
    assert_eq!(result["first_changed_line"], Value::Null);
    assert_eq!(result["summaries"]["already_applied"], 1);
    assert_eq!(result["outcomes"][0]["status"], "already_applied");

    assert_eq!(dir.deck_json_bytes(), after_first, "файл не переписывался");
}

#[test]
fn identical_expected_and_replacement_change_nothing() {
    let dir = canonical_fixture("edit-noop");
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность",
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["applied"], false);
    assert_eq!(result["summaries"]["noop_identical"], 1);
    assert_eq!(result["outcomes"][0]["status"], "noop_identical");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn conflict_reports_the_actual_value_and_exits_with_seven() {
    let dir = canonical_fixture("edit-conflict");
    let before = dir.deck_json_bytes();

    let (exit, stdout, stderr) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "другое",
            "--set",
            "новое",
            "--apply",
        ],
    );

    assert_eq!(exit, 7, "конфликт предусловия — отдельный exit code");
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "expected_mismatch");
    assert_eq!(error["details"]["edits_total"], 1);
    assert_eq!(error["details"]["conflicts_total"], 1);
    assert_eq!(error["details"]["conflicts"][0]["guid"], "guid-1");
    assert_eq!(error["details"]["conflicts"][0]["field"], "Значение");
    assert_eq!(
        error["details"]["conflicts"][0]["expected_sample"],
        "другое"
    );
    assert_eq!(
        error["details"]["conflicts"][0]["current_sample"],
        "случайность"
    );

    assert!(stderr.is_empty(), "в режиме --json stderr пуст: {stderr}");

    let (exit, _, stderr) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "другое",
            "--set",
            "новое",
        ],
    );
    assert_eq!(exit, 7);
    assert!(
        stderr.contains("случайность"),
        "человеческая диагностика уходит в stderr: {stderr}"
    );
    assert_eq!(dir.deck_json_bytes(), before, "конфликт не пишет файл");
}

#[test]
fn in_memory_error_also_uses_the_conflict_exit_code() {
    let dir = canonical_fixture("edit-in-memory-conflict");

    // Заметка с некорректным числом полей: типизированное дерево сообщает
    // ERROR, поэтому правка отклоняется раньше конфликта.
    let mut export = base_export();
    export["notes"][0]["fields"] = serde_json::json!(["только одно"]);
    dir.write_canonical_deck_json(&export);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-2",
            "--field",
            "Значение",
            "--expect",
            "неизбежность",
            "--set",
            "иное",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "export_invalid");
    assert_eq!(exit, 6);
}

#[test]
fn unknown_guid_exits_with_four() {
    let dir = canonical_fixture("edit-unknown-guid");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "нет-такого",
            "--field",
            "Значение",
            "--expect",
            "x",
            "--set",
            "y",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "note_not_found");
    assert_eq!(exit, 4);
    assert_eq!(error["details"]["problems_total"], 1);
    assert_eq!(error["details"]["problems"][0]["code"], "note_not_found");
}

#[test]
fn unknown_field_exits_with_three_and_lists_available_names() {
    let dir = canonical_fixture("edit-unknown-field");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "НетТакогоПоля",
            "--expect",
            "x",
            "--set",
            "y",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "unknown_field");
    assert_eq!(exit, 3);
    assert!(
        error["message"].as_str().expect("текст").contains("Слово"),
        "диагностика должна перечислять доступные поля: {}",
        error["message"]
    );
}

#[test]
fn a_field_of_another_model_is_rejected_with_the_models_own_names() {
    let dir = TempDir::new("edit-foreign-field");
    let mut export = base_export();
    let first_model = export["note_models"][0].clone();
    let mut second = first_model.clone();
    second["crowdanki_uuid"] = Value::String("model-2".to_string());
    second["flds"] = serde_json::json!([
        {"name": "Слово", "ord": 0},
        {"name": "Значение-2", "ord": 1},
        {"name": "Пример", "ord": 2}
    ]);
    second["tmpls"][0]["afmt"] = Value::String("{{Слово}}{{Пример}}".to_string());
    export["note_models"] = serde_json::json!([first_model, second]);
    export["notes"][0]["note_model_uuid"] = Value::String("model-2".to_string());
    export["notes"][0]["fields"] = serde_json::json!(["[sound:a.mp3]偶然", "случайность-2", ""]);
    dir.write_canonical_deck_json(&export);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "x",
            "--set",
            "y",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "unknown_field");
    assert_eq!(exit, 3);
    let message = error["message"].as_str().expect("текст");
    assert!(message.contains("модели заметки"), "{message}");
    assert!(message.contains("Значение-2"), "{message}");
}

#[test]
fn batch_reports_every_resolution_problem_at_once() {
    let dir = canonical_fixture("edit-batch-problems");
    let request = edit_request(&[
        ("guid-1", "Значение", "случайность", "случайность!"),
        ("нет-такого", "Значение", "x", "y"),
        ("guid-2", "НетТакогоПоля", "x", "y"),
    ]);
    let path = write_request(&dir, "request.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(
        error["code"], "note_not_found",
        "старшинство кода: not_found важнее unknown_field"
    );
    assert_eq!(error["details"]["problems_total"], 2);
    assert_eq!(error["details"]["problems"][0]["edit_index"], 1);
    assert_eq!(error["details"]["problems"][1]["edit_index"], 2);
    assert_eq!(error["details"]["problems_total"], 2);
}

#[test]
fn duplicate_target_is_rejected_without_touching_the_export() {
    let dir = canonical_fixture("edit-duplicate");
    let before = dir.deck_json_bytes();
    let request = edit_request(&[
        ("guid-1", "Значение", "случайность", "а"),
        ("guid-1", "Значение", "а", "б"),
    ]);
    let path = write_request(&dir, "request.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "duplicate_edit_target");
    assert_eq!(exit, 3);
    assert_eq!(error["details"]["first_edit_index"], 0);
    assert_eq!(error["details"]["duplicate_edit_index"], 1);
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn empty_request_is_a_usage_level_error() {
    let dir = canonical_fixture("edit-empty-request");
    let path = write_request(
        &dir,
        "request.json",
        &serde_json::json!({"schema_version": 1, "edits": []}),
    );

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "invalid_request");
    assert_eq!(error["details"]["reason"], "empty_request");
    assert_eq!(exit, 3);
}

#[test]
fn unsupported_schema_version_is_rejected() {
    let dir = canonical_fixture("edit-schema-version");
    let path = write_request(
        &dir,
        "request.json",
        &serde_json::json!({"schema_version": 99, "edits": [
            {"guid": "guid-1", "field": "Значение", "expected": "a", "replacement": "b"}
        ]}),
    );

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["details"]["reason"], "unsupported_schema_version");
    assert_eq!(error["details"]["expected"], 1);
}

#[test]
fn non_canonical_source_is_rejected_before_any_write() {
    let dir = TempDir::new("edit-non-canonical");
    dir.write_export(&base_export());
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
            "--apply",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "source_not_canonical");
    assert_eq!(exit, 3);
    assert_eq!(error["details"]["reason"], "canonical_round_trip_mismatch");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn export_with_errors_is_not_mutable() {
    let dir = TempDir::new("edit-export-invalid");
    let mut export = base_export();
    export["notes"][0]["guid"] = Value::String(String::new());
    export["notes"][1]["guid"] = Value::String(String::new());
    dir.write_canonical_deck_json(&export);
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-2",
            "--field",
            "Значение",
            "--expect",
            "неизбежность",
            "--set",
            "другое",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "export_invalid");
    assert_eq!(exit, 6);
    assert_eq!(error["details"]["phase"], "source");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn conflicting_model_definitions_make_the_export_not_mutable() {
    let dir = TempDir::new("edit-model-conflict");
    let mut export = base_export();
    let mut variant = export["note_models"][0].clone();
    variant["flds"] = serde_json::json!([
        {"name": "Другое", "ord": 0},
        {"name": "Значение", "ord": 1},
        {"name": "Пример", "ord": 2}
    ]);
    export["children"] = serde_json::json!([{
        "__type__": "Deck",
        "name": "Test::Deck::Child",
        "crowdanki_uuid": "deck-uuid-2",
        "deck_config_uuid": "cfg-1",
        "children": [],
        "media_files": [],
        "note_models": [variant],
        "deck_configurations": [],
        "notes": []
    }]);
    dir.write_canonical_deck_json(&export);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "export_not_mutable");
    assert_eq!(exit, 6);
    assert_eq!(
        error["details"]["code"],
        "conflicting_note_model_definition"
    );
}

#[test]
fn missing_export_directory_exits_with_three() {
    let dir = TempDir::new("edit-missing");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "x",
            "--set",
            "y",
        ],
    );

    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "deck_json_missing");
    assert_eq!(exit, 3);
}

#[test]
fn request_file_and_stdin_are_equivalent() {
    let dir = canonical_fixture("edit-stdin");
    let request = edit_request(&[
        ("guid-1", "Значение", "случайность", "случайность!"),
        ("guid-2", "Слово", "必然", "必然!"),
    ]);
    let path = write_request(&dir, "request.json", &request);
    let raw = serde_json::to_vec(&request).expect("запрос");

    let from_file = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    let from_stdin = run_cli_with_stdin_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            "-",
        ],
        &raw,
    );

    assert_eq!(from_file.0, 0);
    assert_eq!(from_stdin.0, 0);
    assert_eq!(
        from_file.1, from_stdin.1,
        "один и тот же запрос — один ответ"
    );
}

#[test]
fn batch_mixes_statuses_in_one_invocation() {
    let dir = canonical_fixture("edit-mixed");
    // Сначала применяем одну правку, чтобы вторая стала `already_applied`.
    let first = edit_request(&[("guid-1", "Значение", "случайность", "случайность!")]);
    let path = write_request(&dir, "first.json", &first);
    let (exit, _, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    assert_eq!(exit, 0);

    let mixed = edit_request(&[
        ("guid-1", "Значение", "случайность", "случайность!"),
        ("guid-2", "Значение", "неизбежность", "неизбежность"),
        ("guid-2", "Пример", "", "必然の一致"),
    ]);
    let path = write_request(&dir, "mixed.json", &mixed);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["edits_total"], 3);
    assert_eq!(result["effective_edits"], 1);
    assert_eq!(result["changed_lines"], 1);
    assert_eq!(result["summaries"]["already_applied"], 1);
    assert_eq!(result["summaries"]["noop_identical"], 1);
    assert_eq!(result["summaries"]["applied"], 1);
    assert_eq!(result["outcomes"][0]["status"], "already_applied");
    assert_eq!(result["outcomes"][1]["status"], "noop_identical");
    assert_eq!(result["outcomes"][2]["status"], "applied");
    assert_eq!(dir.deck_json()["notes"][1]["fields"][2], "必然の一致");
}

#[test]
fn json_result_contract_has_stable_keys() {
    let dir = canonical_fixture("edit-json-contract");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
        ],
    );

    let document = parse_json(&stdout);
    assert_eq!(exit, 0);
    assert_eq!(document["schema_version"], 1);

    let result = document["result"].as_object().expect("result — объект");
    let keys: Vec<&str> = result.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "applied",
            "byte_delta",
            "candidate_bytes",
            "changed_lines",
            "checks",
            "deck_json",
            "dry_run",
            "edits_total",
            "effective_edits",
            "export_dir",
            "first_changed_line",
            "outcomes",
            "outcomes_truncated",
            "source_bytes",
            "summaries",
            "validation",
        ],
        "набор ключей result должен быть стабильным и отсортированным"
    );

    let outcome = document["result"]["outcomes"][0]
        .as_object()
        .expect("outcome — объект");
    let outcome_keys: Vec<&str> = outcome.keys().map(String::as_str).collect();
    assert_eq!(
        outcome_keys,
        [
            "deck_path",
            "edit_id",
            "edit_index",
            "field",
            "field_ord",
            "guid",
            "new_len",
            "new_sample",
            "note_index",
            "old_len",
            "old_sample",
            "status",
        ]
    );

    let checks = document["result"]["checks"]
        .as_object()
        .expect("checks — объект");
    assert_eq!(checks.len(), 5);
    assert_eq!(document["result"]["validation"]["before"]["errors"], 0);
    assert_eq!(
        document["result"]["validation"]["new_error_codes"],
        serde_json::json!([])
    );
}

#[test]
fn human_output_names_modes_checks_and_values() {
    let dir = canonical_fixture("edit-human");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
        ],
    );

    assert_eq!(exit, 0);
    assert!(stdout.contains("dry-run, deck.json не изменён"), "{stdout}");
    assert!(stdout.contains("Эффективных правок: 1"), "{stdout}");
    assert!(stdout.contains("было: случайность"), "{stdout}");
    assert!(stdout.contains("стало: случайность!"), "{stdout}");
    assert!(stdout.contains("source_canonical: да"), "{stdout}");
    assert!(
        stdout.contains("diff_shape_is_exactly_requested: да"),
        "{stdout}"
    );
    assert!(stdout.contains("Новые ERROR: нет"), "{stdout}");
}

#[test]
fn usage_errors_exit_with_two() {
    let dir = canonical_fixture("edit-usage");

    // Одновременно файл запроса и одиночная правка.
    let (exit, _, stderr) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--request",
            "r.json",
            "--guid",
            "guid-1",
            "--field",
            "f",
            "--expect",
            "a",
            "--set",
            "b",
        ],
    );
    assert_eq!(exit, 2, "clap должен отвергнуть конфликт аргументов");
    assert!(!stderr.is_empty());

    // Одиночная правка без обязательного --expect.
    let (exit, _, stderr) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "f",
            "--set",
            "b",
        ],
    );
    assert_eq!(exit, 2);
    assert!(!stderr.is_empty());

    // Ни --request, ни --guid.
    let (exit, _, _) = run_cli_in(None, &["edit", dir.path().to_str().expect("путь")]);
    assert_eq!(exit, 2);
}

#[test]
fn media_directory_is_never_touched() {
    let dir = canonical_fixture("edit-media");
    dir.write_media(&["a.mp3"]);

    let before = std::fs::read(dir.path().join("media/a.mp3")).expect("media");

    let (exit, _, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "Значение",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
            "--apply",
        ],
    );

    assert_eq!(exit, 0);
    assert_eq!(
        std::fs::read(dir.path().join("media/a.mp3")).expect("media"),
        before,
        "media/ не участвует в правке"
    );
}

#[test]
fn validation_results_are_reported_before_and_after() {
    let dir = TempDir::new("edit-validation-delta");
    dir.write_canonical_deck_json(&base_export());

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--field",
            "Слово",
            "--expect",
            "[sound:a.mp3]偶然",
            "--set",
            "[sound:missing.mp3]偶然",
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["validation"]["before"]["errors"], 0);
    assert_eq!(result["validation"]["after"]["errors"], 0);

    let after = dir.deck_json_bytes();
    let before = {
        let mut export = base_export();
        export["notes"][0]["fields"][0] = Value::String("[sound:a.mp3]偶然".to_string());
        anki_repo::loader::render_canonical_bytes(&export).expect("каноническая форма")
    };
    let (old_line, new_line) = single_line_change(&before, &after);
    assert!(old_line.contains("sound:a.mp3"));
    assert!(new_line.contains("sound:missing.mp3"));
}
