//! Контракт жизненного цикла заметки: `models`, `create` и `retire`.
//!
//! Эти три команды — один маршрут: сначала агент узнаёт фактическую схему полей
//! модели, потом создаёт заметку по этой схеме, а когда заметка устаревает —
//! помечает её тегом вместо физического удаления. Тесты держат именно этот
//! маршрут: все проверки идут через настоящий CLI-бинарник на временных
//! канонических копиях синтетических экспортов и фиксируют exit code, JSON-схему
//! результата и фактическое состояние `deck.json` после записи.

mod common;

use common::{
    TempDir, base_export, canonical_base_export, canonical_export, export_with, mixed_export,
    run_cli, run_cli_in, run_cli_with_stdin_in, write_request,
};
use serde_json::{Value, json};

/// Полное имя колоды базового экспорта.
const DECK_PATH: &str = "Test::Deck";
/// Идентичность колоды базового экспорта.
const DECK_UUID: &str = "deck-uuid-1";
/// Идентичность модели базового экспорта.
const MODEL_UUID: &str = "model-1";

/// Документ запроса `create`.
fn create_request(notes: &[Value]) -> Value {
    json!({"schema_version": 1, "notes": notes})
}

/// Документ запроса `retire`.
fn retire_request(tag: &str, guids: &[&str]) -> Value {
    json!({
        "schema_version": 1,
        "tag": tag,
        "notes": guids
            .iter()
            .enumerate()
            .map(|(position, guid)| json!({"note_id": format!("n{position}"), "guid": guid}))
            .collect::<Vec<_>>(),
    })
}

/// Значения всех полей базовой модели.
fn fields(headword: &str, meaning: &str, example: &str) -> Value {
    json!({"Заголовок": headword, "Толкование": meaning, "Пример": example})
}

/// Description одной новой заметки в запросе `create`.
fn note_spec(fields: Value) -> Value {
    json!({
        "deck": {"path": DECK_PATH, "crowdanki_uuid": DECK_UUID},
        "model": {"mode": "auto"},
        "fields": fields,
        "tags": [],
    })
}

/// Разбирает stdout как JSON-документ.
fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout).expect("stdout должен быть валидным JSON")
}

/// `result` из успешного JSON-ответа.
fn result_of(exit: i32, stdout: &str) -> Value {
    assert_eq!(exit, 0, "ожидался успех, stdout: {stdout}");
    let document = parse_json(stdout);
    document["result"].clone()
}

/// `error` из неуспешного JSON-ответа.
fn error_of(exit: i32, stdout: &str) -> Value {
    assert_ne!(exit, 0, "ожидалась ошибка, stdout: {stdout}");
    let document = parse_json(stdout);
    assert!(document.get("result").is_none(), "у ошибки нет result");
    document["error"].clone()
}

/// Запускает команду в режиме `--json`.
///
/// `--json` — глобальный флаг, который clap не принимает дважды, поэтому он
/// добавляется здесь ровно один раз, а не передаётся вместе с аргументами.
fn json_run(args: &[&str]) -> (i32, Value) {
    let mut full: Vec<&str> = vec![args[0], "--json"];
    full.extend_from_slice(&args[1..]);
    let (exit, stdout, _) = run_cli(&full);
    (exit, parse_json(&stdout))
}

// --- models ---------------------------------------------------------------

#[test]
fn models_reports_field_schema_and_value_evidence() {
    let dir = canonical_base_export("models-evidence");

    let (exit, document) = json_run(&["models", dir.path().to_str().expect("путь")]);
    assert_eq!(exit, 0, "модели читаются: {document}");
    assert_eq!(document["command"], "models");
    let result = &document["result"];

    assert_eq!(result["deck"]["path"], DECK_PATH);
    assert_eq!(result["deck"]["crowdanki_uuid"], DECK_UUID);
    assert_eq!(result["deck"]["notes_in_deck"], 2);
    assert_eq!(result["models"].as_array().expect("список").len(), 1);

    let model = &result["models"][0];
    assert_eq!(model["crowdanki_uuid"], MODEL_UUID);
    assert_eq!(model["model_kind"], "standard");
    assert_eq!(model["notes_in_deck"], 2);
    assert!(
        model["schema_problems"]
            .as_array()
            .expect("список")
            .is_empty()
    );

    // Порядок полей — это порядок `fields` заметки, а не порядок объявления.
    let names: Vec<&str> = model["fields"]
        .as_array()
        .expect("поля")
        .iter()
        .map(|field| field["name"].as_str().expect("имя"))
        .collect();
    assert_eq!(names, vec!["Заголовок", "Толкование", "Пример"]);
    let ords: Vec<u64> = model["fields"]
        .as_array()
        .expect("поля")
        .iter()
        .map(|field| field["ord"].as_u64().expect("ord"))
        .collect();
    assert_eq!(ords, vec![0, 1, 2]);

    // Свидетельство о семантике поля — фактическое значение из экспорта.
    let samples = model["fields"][0]["samples"].as_array().expect("примеры");
    assert_eq!(samples[0]["guid"], "guid-1");
    assert_eq!(samples[0]["value"], "[sound:a.mp3]偶然");
    assert_eq!(model["fields"][2]["empty_in_deck"], 1);

    let template = &model["templates"][0];
    assert_eq!(template["ord"], 0);
    assert_eq!(template["name"], "Карточка 1");
    assert_eq!(template["specials"], json!(["FrontSide"]));
    let used: Vec<&str> = template["fields"]
        .as_array()
        .expect("поля шаблона")
        .iter()
        .map(|field| field.as_str().expect("имя"))
        .collect();
    assert!(used.contains(&"Заголовок"));
    assert!(used.contains(&"Пример"));
}

#[test]
fn models_orders_fields_by_ord_not_by_declaration() {
    let dir = canonical_export("models-ord", &mixed_export());

    let (exit, document) = json_run(&[
        "models",
        dir.path().to_str().expect("путь"),
        "--deck",
        "Группа::Первая",
    ]);
    assert_eq!(exit, 0, "модели читаются: {document}");
    let result = &document["result"];

    let model = result["models"]
        .as_array()
        .expect("список")
        .iter()
        .find(|model| model["crowdanki_uuid"] == "model-out-of-order")
        .expect("модель с непорядковыми ord");
    let ordered: Vec<(&str, u64)> = model["fields"]
        .as_array()
        .expect("поля")
        .iter()
        .map(|field| {
            (
                field["name"].as_str().expect("имя"),
                field["ord"].as_u64().expect("ord"),
            )
        })
        .collect();
    assert_eq!(ordered, vec![("Альфа", 0), ("Бета", 1), ("Гамма", 2)]);
}

#[test]
fn models_refuses_unknown_and_ambiguous_deck() {
    let dir = canonical_export("models-unknown-deck", &mixed_export());

    let (exit, document) = json_run(&[
        "models",
        dir.path().to_str().expect("путь"),
        "--deck",
        "Нет такой",
    ]);
    assert_eq!(exit, 3);
    assert_eq!(
        error_of(exit, &document.to_string())["code"],
        "unknown_deck"
    );

    let (exit, document) = json_run(&[
        "models",
        dir.path().to_str().expect("путь"),
        "--deck-uuid",
        "нет-такого",
    ]);
    assert_eq!(exit, 3);
    assert_eq!(
        error_of(exit, &document.to_string())["code"],
        "unknown_deck"
    );

    // `--deck` и `--deck-preorder` указывают на разные узлы: догадка запрещена.
    let (exit, document) = json_run(&[
        "models",
        dir.path().to_str().expect("путь"),
        "--deck",
        "Группа",
        "--deck-preorder",
        "1",
    ]);
    assert_eq!(exit, 3);
    assert_eq!(
        error_of(exit, &document.to_string())["code"],
        "deck_identity_mismatch"
    );
}

#[test]
fn models_defaults_to_the_root_deck() {
    let dir = canonical_base_export("models-root-default");

    let (exit, document) = json_run(&["models", dir.path().to_str().expect("путь")]);
    assert_eq!(exit, 0, "чтение без селектора: {document}");
    assert_eq!(document["result"]["deck"]["path"], DECK_PATH);
}

// --- create ---------------------------------------------------------------

#[test]
fn create_dry_run_plans_the_note_and_keeps_the_file() {
    let dir = canonical_base_export("create-dry-run");
    let before = dir.deck_json_bytes();
    let request = create_request(&[note_spec(fields("新語", "новое слово", ""))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    let result = result_of(exit, &stdout);

    assert_eq!(result["dry_run"], true);
    assert_eq!(result["applied"], false);
    assert_eq!(result["notes_created"], 1);
    assert_eq!(result["outcomes"][0]["status"], "dry_run");
    assert_eq!(result["outcomes"][0]["guid_generated"], true);
    assert_eq!(result["outcomes"][0]["model_mode"], "auto");
    assert_eq!(result["outcomes"][0]["model_uuid"], MODEL_UUID);
    assert_eq!(
        result["outcomes"][0]["field_names"],
        json!(["Заголовок", "Толкование", "Пример"])
    );
    assert!(result["outcomes"][0]["guid"].as_str().expect("guid").len() <= 10);
    assert_eq!(result["decks_touched"][0]["notes_before"], 2);
    assert_eq!(result["decks_touched"][0]["notes_added"], 1);
    assert!(result["byte_delta"].as_i64().expect("байты") > 0);
    assert!(
        result["validation"]["new_error_codes"]
            .as_array()
            .expect("коды")
            .is_empty()
    );

    for (name, value) in result["checks"]
        .as_object()
        .expect("проверки")
        .iter()
        .map(|(name, value)| (name.as_str(), value))
    {
        assert_eq!(value, &json!(true), "проверка {name} должна пройти");
    }

    assert_eq!(dir.deck_json_bytes(), before, "dry-run не пишет файл");
}

#[test]
fn create_apply_appends_one_note_and_keeps_the_export_valid() {
    let dir = canonical_base_export("create-apply");
    let request = create_request(&[note_spec(fields("新語", "новое слово", "例"))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["applied"], true);
    assert_eq!(result["dry_run"], false);
    assert_eq!(result["outcomes"][0]["status"], "created");

    let guid = result["outcomes"][0]["guid"]
        .as_str()
        .expect("guid")
        .to_string();
    let export = dir.deck_json();
    let notes = export["notes"].as_array().expect("заметки");
    assert_eq!(notes.len(), 3);

    let added = notes.last().expect("добавленная заметка");
    assert_eq!(added["__type__"], "Note");
    assert_eq!(added["guid"], guid.as_str());
    assert_eq!(added["note_model_uuid"], MODEL_UUID);
    assert_eq!(added["tags"], json!([]));
    assert_eq!(
        added["fields"],
        json!(["新語", "новое слово", "例"]),
        "поля обязаны стоять в порядке ord модели"
    );

    // Записанный файл остаётся каноническим, а экспорт — структурно валидным.
    assert_eq!(
        anki_repo::loader::render_canonical_bytes(&export).expect("канонизация"),
        dir.deck_json_bytes(),
        "после записи deck.json обязан быть каноническим"
    );

    let (exit, stdout, _) = run_cli_in(
        None,
        &["validate", dir.path().to_str().expect("путь"), "--json"],
    );
    assert_eq!(exit, 0, "экспорт остаётся валидным: {stdout}");
}

#[test]
fn create_is_idempotent_through_the_emitted_resolved_request() {
    let dir = canonical_base_export("create-idempotent");
    let request = create_request(&[note_spec(fields("新語", "новое слово", ""))]);
    let path = write_request(&dir, "create.json", &request);
    let resolved = dir.path().join("resolved.json");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
            "--emit-resolved",
            resolved.to_str().expect("путь"),
        ],
    );
    let first = result_of(exit, &stdout);
    let after_first = dir.deck_json_bytes();

    let emitted: Value =
        serde_json::from_slice(&std::fs::read(&resolved).expect("файл")).expect("JSON");
    assert_eq!(emitted["schema_version"], 1);
    assert_eq!(emitted["notes"][0]["guid"], first["outcomes"][0]["guid"]);
    assert_eq!(emitted["notes"][0]["deck"]["crowdanki_uuid"], DECK_UUID);
    assert_eq!(emitted["notes"][0]["model"]["mode"], "explicit");
    assert_eq!(emitted["notes"][0]["model"]["crowdanki_uuid"], MODEL_UUID);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            resolved.to_str().expect("путь"),
            "--apply",
        ],
    );
    let second = result_of(exit, &stdout);
    assert_eq!(second["notes_created"], 0);
    assert_eq!(second["notes_already_applied"], 1);
    assert_eq!(second["outcomes"][0]["status"], "already_applied");
    assert_eq!(dir.deck_json_bytes(), after_first, "повтор не пишет файл");
}

#[test]
fn create_refuses_media_references_in_new_values() {
    let dir = canonical_base_export("create-media");
    let before = dir.deck_json_bytes();
    let request = create_request(&[note_spec(fields("[sound:a.mp3]新語", "слово", ""))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "media_forbidden");
    assert_eq!(exit, 3);
    assert_eq!(dir.deck_json_bytes(), before);

    let request = create_request(&[note_spec(fields("<img src=\"b.png\">", "слово", ""))]);
    let path = write_request(&dir, "create-media-src.json", &request);
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    assert_eq!(error_of(exit, &stdout)["code"], "media_forbidden");
}

#[test]
fn create_refuses_field_set_that_no_model_matches() {
    let dir = canonical_base_export("create-unknown-field");
    let request = create_request(&[note_spec(json!({
        "Заголовок": "x",
        "Опечатка": "y",
        "Пример": "z",
    }))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "unknown_model");
    // Сообщение обязано называть фактическую причину, а не только код.
    let message = error["message"].as_str().expect("сообщение");
    assert!(
        message.contains("Опечатка"),
        "сообщение называет набор полей запроса: {message}"
    );
}

#[test]
fn create_resolves_the_model_explicitly() {
    let dir = canonical_export("create-explicit-model", &mixed_export());
    let spec = json!({
        "deck": {"crowdanki_uuid": "deck-first"},
        "model": {"mode": "explicit", "crowdanki_uuid": "model-single-field"},
        "fields": {"Единственное поле": "значение"},
        "tags": ["новая"],
    });
    let request = create_request(&[spec]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["outcomes"][0]["model_mode"], "explicit");
    assert_eq!(result["outcomes"][0]["model_uuid"], "model-single-field");
    assert_eq!(result["outcomes"][0]["status"], "created");

    // Выбор по имени разрешается так же явно, как по uuid.
    let spec = json!({
        "deck": {"crowdanki_uuid": "deck-second"},
        "model": {"mode": "explicit", "name": "Модель с непорядковыми ord"},
        "fields": {"Альфа": "а", "Бета": "б", "Гамма": "в"},
    });
    let request = create_request(&[spec]);
    let path = write_request(&dir, "create-by-name.json", &request);
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["outcomes"][0]["model_uuid"], "model-out-of-order");
}

#[test]
fn create_refuses_unknown_and_ambiguous_explicit_model() {
    let dir = canonical_base_export("create-model-errors");
    let request = create_request(&[json!({
        "deck": {"crowdanki_uuid": DECK_UUID},
        "model": {"mode": "explicit", "crowdanki_uuid": "нет-такой"},
        "fields": fields("x", "y", "z"),
    })]);
    let path = write_request(&dir, "unknown.json", &request);
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    assert_eq!(error_of(exit, &stdout)["code"], "unknown_model");

    // Две модели с одним именем: выбор по имени обязан отказать, а не угадать.
    let export = export_with(|value| {
        let models = value["note_models"].as_array_mut().expect("модели");
        let mut clone = models[0].clone();
        clone["crowdanki_uuid"] = json!("model-1-clone");
        models.push(clone);
    });
    let dir = canonical_export("create-model-ambiguous", &export);
    let request = create_request(&[json!({
        "deck": {"crowdanki_uuid": DECK_UUID},
        "model": {"mode": "explicit", "name": "Тестовая модель"},
        "fields": fields("x", "y", "z"),
    })]);
    let path = write_request(&dir, "ambiguous.json", &request);
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    assert_eq!(exit, 5, "неоднозначность модели — exit 5");
    assert_eq!(error_of(exit, &stdout)["code"], "ambiguous_model");
}

#[test]
fn create_apply_requires_declared_deck_identity() {
    // Колода без `crowdanki_uuid`: адресовать цель после записи нечем.
    let export = export_with(|value| {
        value
            .as_object_mut()
            .expect("объект")
            .remove("crowdanki_uuid");
    });
    let dir = canonical_export("create-needs-uuid", &export);
    let before = dir.deck_json_bytes();
    let request = create_request(&[json!({
        "deck": {"preorder": 0},
        "model": {"mode": "auto"},
        "fields": fields("新語", "новое слово", ""),
    })]);
    let path = write_request(&dir, "create.json", &request);

    // Dry-run довольствуется позицией в дереве: он ничего не пишет.
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    assert_eq!(result_of(exit, &stdout)["outcomes"][0]["status"], "dry_run");

    // Запись требует идентичности, которая не зависит от имени.
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "unresolved_deck_identity");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn create_reports_guid_conflict_instead_of_overwriting() {
    let dir = canonical_base_export("create-guid-conflict");
    let before = dir.deck_json_bytes();
    let request = create_request(&[json!({
        "guid": "guid-1",
        "deck": {"crowdanki_uuid": DECK_UUID},
        "model": {"mode": "auto"},
        "fields": fields("другое", "другое", ""),
    })]);
    let path = write_request(&dir, "conflict.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    assert_eq!(error_of(exit, &stdout)["code"], "guid_conflict");
    assert_eq!(dir.deck_json_bytes(), before);

    // Тот же guid и то же содержимое — это уже применённый запрос, а не конфликт.
    let request = create_request(&[json!({
        "guid": "guid-2",
        "deck": {"crowdanki_uuid": DECK_UUID},
        "model": {"mode": "auto"},
        "fields": fields("必然", "неизбежность", ""),
        "tags": [],
    })]);
    let path = write_request(&dir, "already.json", &request);
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["outcomes"][0]["status"], "already_applied");
    assert_eq!(result["applied"], false);
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn create_refuses_a_source_that_is_not_canonical() {
    let dir = TempDir::new("create-non-canonical");
    dir.write_export(&base_export());
    let before = dir.deck_json_bytes();
    let request = create_request(&[note_spec(fields("新語", "", ""))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    assert_eq!(exit, 3, "граница записи отвергает неканонический источник");
    assert_eq!(error_of(exit, &stdout)["code"], "source_not_canonical");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn create_refuses_a_deck_that_cannot_take_notes() {
    // Колода без массива `notes` — структурный отказ, а не ошибка запроса.
    let export = export_with(|value| {
        value.as_object_mut().expect("объект").remove("notes");
    });
    let dir = canonical_export("create-immutable", &export);
    let before = dir.deck_json_bytes();
    let request = create_request(&[note_spec(fields("新語", "", ""))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(exit, 6);
    assert_eq!(error["code"], "export_not_mutable");
    assert_eq!(error["details"]["reason"], "deck_without_notes_array");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn create_reads_the_request_from_stdin() {
    let dir = canonical_base_export("create-stdin");
    let request = create_request(&[note_spec(fields("新語", "новое слово", ""))]);

    let (exit, stdout, _) = run_cli_with_stdin_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            "-",
        ],
        serde_json::to_vec(&request).expect("запрос").as_slice(),
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["notes_created"], 1);
}

// --- retire ---------------------------------------------------------------

#[test]
fn retire_appends_the_tag_and_keeps_the_note() {
    let dir = canonical_base_export("retire-apply");
    let before = dir.deck_json().clone();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--tag",
            "archived::auto",
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["applied"], true);
    assert_eq!(result["notes_retired"], 1);
    assert_eq!(result["tag"], "archived::auto");
    assert_eq!(result["outcomes"][0]["status"], "retired");
    assert_eq!(result["outcomes"][0]["previous_tags"], json!(["тэг"]));
    assert_eq!(
        result["outcomes"][0]["tags"],
        json!(["тэг", "archived::auto"])
    );

    let after = dir.deck_json();
    let notes = after["notes"].as_array().expect("заметки");
    assert_eq!(notes.len(), 2, "вывод из обращения ничего не удаляет");

    // Единственное изменение — дописанный тег: содержимое заметки не тронуто.
    let retired = &notes[0];
    assert_eq!(retired["fields"], before["notes"][0]["fields"]);
    assert_eq!(retired["guid"], "guid-1");
    assert_eq!(retired["note_model_uuid"], MODEL_UUID);
    assert_eq!(retired["tags"], json!(["тэг", "archived::auto"]));
    assert_eq!(notes[1], before["notes"][1]);

    assert_eq!(
        anki_repo::loader::render_canonical_bytes(&after).expect("канонизация"),
        dir.deck_json_bytes()
    );

    // Заметка остаётся адресуемой: тег — не удаление.
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "find",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["matched_total"], 1);
    let tags: Vec<&str> = result["notes"][0]["tags"]
        .as_array()
        .expect("теги")
        .iter()
        .map(|tag| tag.as_str().expect("тег"))
        .collect();
    assert!(tags.contains(&"archived::auto"));
}

#[test]
fn retire_is_idempotent_and_never_duplicates_the_tag() {
    let dir = canonical_base_export("retire-idempotent");
    let args = [
        "retire",
        dir.path().to_str().expect("путь"),
        "--json",
        "--guid",
        "guid-1",
        "--tag",
        "archived::auto",
        "--apply",
    ];

    let (exit, stdout, _) = run_cli_in(None, &args);
    assert_eq!(result_of(exit, &stdout)["outcomes"][0]["status"], "retired");
    let after_first = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(None, &args);
    let result = result_of(exit, &stdout);
    assert_eq!(result["outcomes"][0]["status"], "already_retired");
    assert_eq!(result["notes_retired"], 0);
    assert_eq!(result["notes_already_retired"], 1);
    assert_eq!(result["applied"], false);
    assert_eq!(dir.deck_json_bytes(), after_first);

    assert_eq!(
        dir.deck_json()["notes"][0]["tags"],
        json!(["тэг", "archived::auto"])
    );
}

#[test]
fn retire_by_repeated_guid_marks_every_requested_note() {
    let dir = canonical_base_export("retire-many");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--guid",
            "guid-2",
            "--tag",
            "archived::auto",
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["notes_total"], 2);
    assert_eq!(result["notes_retired"], 2);
    assert_eq!(
        dir.deck_json()["notes"][1]["tags"],
        json!(["archived::auto"]),
        "тег дописывается и к заметке без тегов"
    );
}

#[test]
fn retire_reads_a_batch_request_from_a_file() {
    let dir = canonical_base_export("retire-batch");
    let request = retire_request("archived::auto", &["guid-1", "guid-2"]);
    let path = write_request(&dir, "retire.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["notes_retired"], 2);
    assert_eq!(result["outcomes"][0]["note_id"], "n0");
    assert_eq!(result["outcomes"][1]["note_id"], "n1");
}

#[test]
fn retire_refuses_bad_tags_and_unknown_guid() {
    let dir = canonical_base_export("retire-errors");
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--tag",
            "плохой тег",
        ],
    );
    assert_eq!(error_of(exit, &stdout)["code"], "invalid_request");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--tag",
            "архив",
        ],
    );
    assert_eq!(exit, 0, "одиночный тег допустим: {stdout}");

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "нет-такой",
            "--tag",
            "архив",
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "unresolved_guid");
    assert_eq!(error["details"]["guid"], "нет-такой");
    assert_eq!(dir.deck_json_bytes(), before, "отказ ничего не пишет");
}

#[test]
fn retire_refuses_a_source_that_is_not_canonical() {
    let dir = TempDir::new("retire-non-canonical");
    dir.write_export(&base_export());
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--tag",
            "архив",
            "--apply",
        ],
    );
    assert_eq!(exit, 3);
    assert_eq!(error_of(exit, &stdout)["code"], "source_not_canonical");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn retire_refuses_a_note_that_cannot_take_tags() {
    // Заметка без массива `tags`: тег дописать некуда, и это отказ, а не догадка.
    let export = export_with(|value| {
        value["notes"][0]
            .as_object_mut()
            .expect("заметка")
            .remove("tags");
    });
    let dir = canonical_export("retire-immutable", &export);
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "guid-1",
            "--tag",
            "архив",
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(exit, 6);
    assert_eq!(error["code"], "export_not_mutable");
    assert_eq!(error["details"]["reason"], "note_without_tags_array");
    assert_eq!(dir.deck_json_bytes(), before);
}

#[test]
fn retire_reads_the_request_from_stdin() {
    let dir = canonical_base_export("retire-stdin");
    let request = retire_request("архив", &["guid-1"]);

    let (exit, stdout, _) = run_cli_with_stdin_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            "-",
            "--apply",
        ],
        serde_json::to_vec(&request).expect("запрос").as_slice(),
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["notes_retired"], 1);
    assert_eq!(dir.deck_json()["notes"][0]["tags"], json!(["тэг", "архив"]));
}

#[test]
fn human_output_names_the_mode_and_the_statuses() {
    let dir = canonical_base_export("lifecycle-human");

    let (exit, stdout, _) = run_cli(&[
        "retire",
        dir.path().to_str().expect("путь"),
        "--guid",
        "guid-1",
        "--tag",
        "архив",
    ]);
    assert_eq!(exit, 0);
    assert!(stdout.contains("dry-run"), "режим назван: {stdout}");
    assert!(stdout.contains("dry_run"));
    assert!(stdout.contains("guid-1"));
    assert!(stdout.contains("тэг"));
    assert!(!stdout.contains("deck.json заменён"));
}

#[test]
fn create_apply_changes_nothing_but_the_appended_note() {
    let dir = canonical_base_export("create-only-appended");
    let before = dir.deck_json();
    let before_bytes = dir.deck_json_bytes();
    let request = create_request(&[note_spec(fields("新語", "новое слово", "例"))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    let guid = result["outcomes"][0]["guid"].as_str().expect("guid");
    let after = dir.deck_json();

    // Точное доказательство «изменилось только это»: канонический рендер
    // исходника с дописанной заметкой обязан совпасть с записанным файлом байт
    // в байт. Любая посторонняя правка — переупорядочивание, нормализация,
    // переписанный ключ — сломала бы это равенство.
    let mut expected = before.clone();
    expected["notes"]
        .as_array_mut()
        .expect("заметки")
        .push(json!({
            "__type__": "Note",
            "guid": guid,
            "note_model_uuid": MODEL_UUID,
            "tags": [],
            "fields": ["新語", "новое слово", "例"],
        }));
    assert_eq!(
        dir.deck_json_bytes(),
        anki_repo::loader::render_canonical_bytes(&expected).expect("канонизация"),
        "изменилась ровно дописанная заметка"
    );

    // То же наблюдение по частям: идентичности и модели не тронуты.
    assert_eq!(after["crowdanki_uuid"], before["crowdanki_uuid"]);
    assert_eq!(after["deck_config_uuid"], before["deck_config_uuid"]);
    assert_eq!(
        after["note_models"], before["note_models"],
        "модель не изменилась"
    );
    assert_eq!(
        after["deck_configurations"], before["deck_configurations"],
        "конфигурации не изменились"
    );
    assert_eq!(
        after["media_files"], before["media_files"],
        "media_files не изменился"
    );
    assert_eq!(
        after["children"], before["children"],
        "дерево колод не изменилось"
    );
    let before_notes = before["notes"].as_array().expect("заметки").clone();
    let after_notes = after["notes"].as_array().expect("заметки");
    assert_eq!(
        &after_notes[..before_notes.len()],
        before_notes.as_slice(),
        "существующие заметки не переупорядочены и не переписаны"
    );
    assert_eq!(after_notes.len(), before_notes.len() + 1);
    assert!(
        dir.deck_json_bytes().len() > before_bytes.len(),
        "файл вырос только на новую заметку"
    );
    // Каталог media в этой фикстуре отсутствует, и создание заметки его не создаёт.
    assert!(
        !dir.path().join("media").exists(),
        "создание заметки не создаёт media/"
    );
}

#[test]
fn create_batch_failure_writes_nothing() {
    let dir = canonical_base_export("create-batch-failure");
    let before = dir.deck_json_bytes();
    // Первая заметка корректна, вторая ссылается на media: запрос отвергается
    // целиком, а не частично — иначе батч был бы не атомарным.
    let request = create_request(&[
        note_spec(fields("新語", "новое слово", "例")),
        note_spec(fields("第二", "[sound:x.mp3]", "例")),
    ]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "media_forbidden");
    assert_eq!(dir.deck_json_bytes(), before, "ни одна заметка не записана");
    assert_eq!(
        dir.deck_json()["notes"].as_array().expect("заметки").len(),
        2
    );
}

#[test]
fn retire_batch_failure_writes_nothing() {
    let dir = canonical_base_export("retire-batch-failure");
    let before = dir.deck_json_bytes();
    let request = retire_request("smoke::retired", &["guid-1", "guid-нет"]);
    let path = write_request(&dir, "retire.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "retire",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "unresolved_guid");
    assert_eq!(
        dir.deck_json_bytes(),
        before,
        "первая заметка не помечена, потому что вторая не разрешилась"
    );
    assert_eq!(
        dir.deck_json()["notes"][0]["tags"],
        json!(["тэг"]),
        "теги первой заметки не тронуты"
    );
}

#[test]
fn create_auto_rejects_two_equally_compatible_models() {
    // Две модели с одинаковым набором имён полей, и обе реально используются
    // заметками целевой колоды: `auto` обязан отказать, а не выбрать первую.
    let export = export_with(|value| {
        let models = value["note_models"].as_array_mut().expect("модели");
        let mut clone = models[0].clone();
        clone["crowdanki_uuid"] = json!("model-clone");
        clone["name"] = json!("Тестовая модель (клон)");
        models.push(clone);
        // Вторая заметка колоды переходит на клон, поэтому кандидатов два.
        value["notes"][1]["note_model_uuid"] = json!("model-clone");
    });
    let dir = canonical_export("create-auto-ambiguous", &export);
    let request = create_request(&[note_spec(fields("x", "y", "z"))]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    assert_eq!(exit, 5, "неоднозначность auto — exit 5");
    assert_eq!(error_of(exit, &stdout)["code"], "ambiguous_model");
    assert_eq!(
        dir.deck_json()["notes"].as_array().expect("заметки").len(),
        2,
        "при неоднозначности ничего не записано"
    );
}

#[test]
fn create_auto_picks_the_only_compatible_model_of_the_deck() {
    // В целевой колоде две модели с несовместимыми наборами полей; набор
    // запроса совпадает ровно с одной — выигрывает она, без догадок.
    let dir = canonical_export("create-auto-unique", &mixed_export());
    let request = create_request(&[json!({
        "deck": {"crowdanki_uuid": "deck-first"},
        "model": {"mode": "auto"},
        "fields": {"Альфа": "а", "Бета": "б", "Гамма": "в"},
        "tags": [],
    })]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    let result = result_of(exit, &stdout);
    let outcome = &result["outcomes"][0];
    assert_eq!(outcome["status"], "dry_run");
    assert_eq!(outcome["model_mode"], "auto");
    assert_eq!(outcome["model_uuid"], "model-out-of-order");
    assert_eq!(
        outcome["field_names"],
        json!(["Альфа", "Бета", "Гамма"]),
        "имена полей перечислены в порядке ord модели"
    );
}

#[test]
fn create_auto_falls_back_to_the_only_compatible_model_of_the_export() {
    // Пустая колода: заметок, которые «использовали» бы модель, в ней нет.
    // Явное правило — взять единственную совместимую модель экспорта и назвать
    // это в evidence; догадка по соседней колоде без правила была бы здесь
    // недопустима, поэтому свидетельство обязано быть видимым.
    let export = export_with(|value| {
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Test::Deck::Пустая",
                "crowdanki_uuid": "deck-empty",
                "deck_config_uuid": "cfg-1",
                "children": [],
                "media_files": [],
                "note_models": [],
                "deck_configurations": [],
                "notes": [],
            }
        ]);
    });
    let dir = canonical_export("create-auto-empty-deck", &export);
    let request = create_request(&[json!({
        "deck": {"crowdanki_uuid": "deck-empty"},
        "model": {"mode": "auto"},
        "fields": fields("x", "y", "z"),
        "tags": [],
    })]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    let result = result_of(exit, &stdout);
    let outcome = &result["outcomes"][0];
    assert_eq!(outcome["model_mode"], "auto");
    assert_eq!(outcome["model_uuid"], MODEL_UUID);
    let evidence = outcome["model_evidence"].as_str().expect("свидетельство");
    assert!(
        evidence.contains("нет заметок"),
        "свидетельство обязано назвать, что заметок модели в колоде нет: {evidence}"
    );

    // Заметка попадёт именно в целевую колоду, а не в ту, чьи заметки дали
    // свидетельство.
    let request = create_request(&[json!({
        "deck": {"crowdanki_uuid": "deck-empty"},
        "model": {"mode": "auto"},
        "fields": fields("x", "y", "z"),
        "tags": [],
    })]);
    let path = write_request(&dir, "apply.json", &request);
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
            "--apply",
        ],
    );
    let result = result_of(exit, &stdout);
    assert_eq!(result["outcomes"][0]["status"], "created");
    assert_eq!(result["outcomes"][0]["deck_uuid"], "deck-empty");
    let export = dir.deck_json();
    assert_eq!(
        export["children"][0]["notes"]
            .as_array()
            .expect("заметки")
            .len(),
        1,
        "заметка попала именно в целевую колоду"
    );
    assert_eq!(
        export["notes"].as_array().expect("заметки").len(),
        2,
        "заметки корневой колоды не тронуты"
    );
}

#[test]
fn create_auto_refuses_an_empty_deck_with_several_compatible_models() {
    // Та же пустая колода, но совместимых моделей в экспорте две: правило
    // «единственная совместимая» не выполняется, и auto обязан отказать.
    let export = export_with(|value| {
        let models = value["note_models"].as_array_mut().expect("модели");
        let mut clone = models[0].clone();
        clone["crowdanki_uuid"] = json!("model-clone");
        clone["name"] = json!("Тестовая модель (клон)");
        models.push(clone);
        value["children"] = json!([
            {
                "__type__": "Deck",
                "name": "Test::Deck::Пустая",
                "crowdanki_uuid": "deck-empty",
                "deck_config_uuid": "cfg-1",
                "children": [],
                "media_files": [],
                "note_models": [],
                "deck_configurations": [],
                "notes": [],
            }
        ]);
    });
    let dir = canonical_export("create-auto-empty-ambiguous", &export);
    let request = create_request(&[json!({
        "deck": {"crowdanki_uuid": "deck-empty"},
        "model": {"mode": "auto"},
        "fields": fields("x", "y", "z"),
        "tags": [],
    })]);
    let path = write_request(&dir, "create.json", &request);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "create",
            dir.path().to_str().expect("путь"),
            "--json",
            "--request",
            path.to_str().expect("путь"),
        ],
    );
    assert_eq!(exit, 5, "две совместимые модели — exit 5");
    let error = error_of(exit, &stdout);
    assert_eq!(error["code"], "ambiguous_model");
    assert_eq!(
        error["details"]["candidates"]
            .as_array()
            .expect("кандидаты")
            .len(),
        2,
        "отказ обязан перечислить кандидатов"
    );
}
