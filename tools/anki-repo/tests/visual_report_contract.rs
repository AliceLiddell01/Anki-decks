//! Контракт команды `visual-report`: границы каталога отчёта, классификация
//! изменений, офлайн-пригодность HTML и детерминированность.
//!
//! Отчёт — это артефакт, который человек открывает в браузере, поэтому тесты
//! проверяют не только JSON-схему, но и фактическое содержимое файлов: куда
//! команда имеет право писать, какие ссылки попадают в HTML и что происходит с
//! media, на которые ссылаются значения полей.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::{TempDir, base_export, canonical_export, export_with, run_cli_in};
use serde_json::{Value, json};

/// Полное имя колоды базового экспорта.
const DECK_PATH: &str = "Test::Deck";

/// Канонический экспорт из базового fixture.
fn canonical_base(label: &str) -> TempDir {
    canonical_export(label, &base_export())
}

/// Все относительные пути файлов внутри каталога.
fn walk(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).expect("каталог должен читаться");
        for entry in entries {
            let entry = entry.expect("запись каталога");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("путь внутри каталога")
                    .to_str()
                    .expect("utf-8")
                    .replace('\\', "/");
                found.push(relative);
            }
        }
    }
    found.sort();
    found
}

/// Разбирает stdout как JSON-документ.
fn parse_json(stdout: &str) -> Value {
    serde_json::from_str(stdout).expect("stdout должен быть валидным JSON")
}

/// Запускает `visual-report` и возвращает пару «exit code, документ».
fn report(before: &Path, after: &Path, out: &Path, extra: &[&str]) -> (i32, Value) {
    let mut args: Vec<String> = vec![
        "visual-report".to_string(),
        "--json".to_string(),
        "--before".to_string(),
        before.to_str().expect("путь").to_string(),
        "--after".to_string(),
        after.to_str().expect("путь").to_string(),
        "--out".to_string(),
        out.to_str().expect("путь").to_string(),
    ];
    args.extend(extra.iter().map(|item| (*item).to_string()));
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let (exit, stdout, _) = run_cli_in(None, &borrowed);
    (exit, parse_json(&stdout))
}

/// `result` успешного ответа.
fn result_of(exit: i32, document: &Value) -> Value {
    assert_eq!(exit, 0, "ожидался успех: {document}");
    document["result"].clone()
}

/// Меняет поле заметки прямо в `deck.json` временного каталога.
fn set_field(dir: &TempDir, guid: &str, ord: usize, value: &str) {
    let mut export = dir.deck_json();
    let notes = export["notes"].as_array_mut().expect("заметки");
    let note = notes
        .iter_mut()
        .find(|note| note["guid"] == guid)
        .expect("заметка с guid");
    note["fields"][ord] = json!(value);
    dir.write_canonical_deck_json(&export);
}

/// Добавляет заметку в `deck.json` временного каталога.
fn append_note(dir: &TempDir, guid: &str, fields: Value) {
    let mut export = dir.deck_json();
    export["notes"]
        .as_array_mut()
        .expect("заметки")
        .push(json!({
            "__type__": "Note",
            "guid": guid,
            "note_model_uuid": "model-1",
            "tags": [],
            "fields": fields,
        }));
    dir.write_canonical_deck_json(&export);
}

#[test]
fn report_classifies_every_change_by_guid() {
    let before = canonical_base("report-before");
    let after = canonical_base("report-after");
    // Добавленная заметка.
    append_note(&after, "guid-3", json!(["новое", "значение", ""]));
    // Изменённое значение поля.
    set_field(&after, "guid-2", 1, "другое значение");
    // Выведенная из обращения заметка.
    let mut export = after.deck_json();
    export["notes"][0]["tags"] = json!(["тэг", "архив"]);
    after.write_canonical_deck_json(&export);

    let out = TempDir::new("report-out");
    let (exit, document) = report(
        before.path(),
        after.path(),
        out.path(),
        &["--retire-tag", "архив"],
    );
    let result = result_of(exit, &document);

    assert_eq!(result["counts"]["created"], 1);
    assert_eq!(result["counts"]["changed"], 1);
    assert_eq!(result["counts"]["retired"], 1);
    assert_eq!(result["counts"]["removed"], 0);
    assert_eq!(result["counts"]["unchanged"], 0);
    assert_eq!(result["counts"]["ambiguous"], 0);
    assert_eq!(result["counts"]["notes_before"], 2);
    assert_eq!(result["counts"]["notes_after"], 3);

    // Превью строятся только для созданных и изменённых заметок: заметка без
    // изменений не нуждается в проверке, а вывод из обращения не меняет содержимое.
    assert_eq!(result["counts"]["previews"], 2);
    assert_eq!(
        result["card_files"],
        json!(["cards/card-0001.html", "cards/card-0002.html"])
    );

    for (name, value) in result["checks"].as_object().expect("проверки") {
        assert_eq!(value, &json!(true), "проверка {name} должна пройти");
    }

    // JSON-список outcomes описывает каждую заметку ровно один раз.
    let guids: Vec<&str> = result["outcomes"]
        .as_array()
        .expect("список")
        .iter()
        .map(|outcome| outcome["guid"].as_str().expect("guid"))
        .collect();
    let unique: BTreeSet<&str> = guids.iter().copied().collect();
    assert_eq!(unique.len(), guids.len(), "guid не дублируются: {guids:?}");
}

#[test]
fn report_writes_a_self_contained_offline_document() {
    let before = canonical_base("report-offline-before");
    let after = canonical_base("report-offline-after");
    set_field(&after, "guid-2", 1, "переписанное толкование");

    let out = TempDir::new("report-offline-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    assert!(
        result["index_html"]
            .as_str()
            .expect("путь")
            .ends_with("index.html")
    );
    assert_eq!(result["out_dir"], out.path().to_str().expect("путь"));
    assert_eq!(walk(out.path()), vec!["cards/card-0001.html", "index.html"]);

    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(index.contains("<!doctype html>"));
    assert!(index.contains("до:"));
    assert!(index.contains(DECK_PATH));
    assert!(
        index.contains("cards/card-0001.html"),
        "index ссылается на превью"
    );
    assert!(index.contains("Ограничения этого отчёта"));

    // Отчёт открывается офлайн: ни внешних ссылок, ни исполняемого кода.
    for marker in [
        "src=\"http",
        "src='http",
        "src=\"//",
        "src='//",
        "href=\"http",
        "href='http",
        "href=\"//",
        "href='//",
        "@import",
        "url(http",
        "<script",
    ] {
        assert!(
            !index.contains(marker),
            "index.html не должен содержать {marker}"
        );
    }
    for path in walk(out.path()) {
        let text = std::fs::read_to_string(out.path().join(&path)).expect("файл отчёта");
        assert!(
            !text.contains("<script"),
            "{path} не должен исполнять JavaScript"
        );
        // Упоминание схемы в тексте ограничений допустимо, а ссылка — нет.
        for marker in [
            "src=\"http",
            "src='http",
            "src=\"//",
            "href=\"http",
            "href='http",
            "href=\"//",
            "url(http",
            "@import",
        ] {
            assert!(
                !text.contains(marker),
                "{path} не должен ссылаться в сеть: {marker}"
            );
        }
    }
}

#[test]
fn report_copies_media_and_rewrites_only_resolved_references() {
    let before = canonical_base("report-media-before");
    let after = canonical_base("report-media-after");
    // Base-фикстура ссылается на `a.mp3`; добавляем существующий и отсутствующий файл.
    set_field(
        &after,
        "guid-2",
        2,
        "<img src=\"b.png\"><img src=\"нет.png\"><img src=\"https://пример/в.png\">",
    );
    after.write_media(&["a.mp3", "b.png"]);

    let out = TempDir::new("report-media-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    // Копируются только файлы, на которые ссылаются фактические превью:
    // `a.mp3` лежит в поле неизменённой заметки и в отчёт не попадает.
    assert_eq!(result["media"]["copied"], 1);
    assert_eq!(result["media"]["missing"], json!(["нет.png"]));
    assert_eq!(result["media"]["remote"], json!(["https://пример/в.png"]));
    assert_eq!(
        walk(out.path()),
        vec!["cards/card-0001.html", "index.html", "media/b.png"]
    );

    let card = std::fs::read_to_string(out.path().join("cards/card-0001.html")).expect("превью");
    assert!(
        card.contains("src=\"../media/b.png\""),
        "путь переписан: {card}"
    );
    assert!(
        card.contains("https://пример/в.png"),
        "внешняя ссылка остаётся как есть, а не подменяется"
    );
    assert!(
        !card.contains("\"media/a.mp3\""),
        "неотрендеренная ссылка не переписывается"
    );

    // Диагностика называет отсутствующий файл и не выдумывает его.
    let codes: Vec<&str> = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .map(|item| item["code"].as_str().expect("код"))
        .collect();
    assert!(codes.contains(&"missing_media"));
}

#[test]
fn report_is_deterministic_for_equal_inputs() {
    let before = canonical_base("report-determinism-before");
    let after = canonical_base("report-determinism-after");
    set_field(&after, "guid-2", 1, "значение для сравнения");

    let first = TempDir::new("report-determinism-first");
    let second = TempDir::new("report-determinism-second");
    let (exit, _) = report(before.path(), after.path(), first.path(), &[]);
    assert_eq!(exit, 0);
    let (exit, _) = report(before.path(), after.path(), second.path(), &[]);
    assert_eq!(exit, 0);

    for path in walk(first.path()) {
        let left = std::fs::read(first.path().join(&path)).expect("файл");
        let right = std::fs::read(second.path().join(&path)).expect("файл");
        assert_eq!(left, right, "файл {path} обязан совпадать побайтово");
    }
}

#[test]
fn report_refuses_an_out_dir_inside_decks() {
    let before = canonical_base("report-guard-before");
    let after = canonical_base("report-guard-after");
    set_field(&after, "guid-2", 1, "изменение");

    // Каталог отчёта не имеет права попасть внутрь экспорта: это скрытая правка
    // `decks/**`.
    let out = before.path().join("decks").join("отчёт");
    std::fs::create_dir_all(&out).expect("каталог");
    let (exit, document) = report(before.path(), after.path(), &out, &[]);
    assert_ne!(exit, 0);
    let error = &document["error"];
    assert_eq!(error["code"], "invalid_request");
    assert_eq!(error["details"]["reason"], "out_dir_inside_decks");
    assert!(
        !out.join("index.html").exists(),
        "отчёт не записан внутрь экспорта"
    );
    assert_eq!(walk(before.path()), vec!["deck.json"], "экспорт не тронут");
}

#[test]
fn report_refuses_a_non_empty_out_dir_without_an_index() {
    let before = canonical_base("report-occupied-before");
    let after = canonical_base("report-occupied-after");
    let out = TempDir::new("report-occupied-out");
    std::fs::write(out.path().join("чужой.txt"), b"occupied").expect("файл");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    assert_ne!(exit, 0);
    assert_eq!(document["error"]["code"], "invalid_request");
    assert_eq!(document["error"]["details"]["reason"], "out_dir_not_empty");

    // Каталог с уже готовым отчётом перезаписывается: это повторный запуск.
    let (exit, _) = report(before.path(), after.path(), out.path(), &[]);
    assert_ne!(exit, 0, "чужой файл всё ещё на месте");
}

#[test]
fn report_refuses_identical_states_and_is_usable_for_deck_wide_identity() {
    let before = canonical_base("report-identical");
    let out = TempDir::new("report-identical-out");

    let (exit, document) = report(before.path(), before.path(), out.path(), &[]);
    assert_ne!(exit, 0);
    assert_eq!(document["error"]["code"], "invalid_request");
    assert_eq!(
        document["error"]["details"]["reason"],
        "before_equals_after"
    );
}

#[test]
fn report_reports_ambiguous_guid_separately() {
    // Две заметки с одним guid в состоянии «после»: догадка запрещена.
    let before = canonical_base("report-ambiguous-before");
    let export = export_with(|value| {
        let notes = value["notes"].as_array_mut().expect("заметки");
        notes[1]["guid"] = json!("guid-1");
        notes[1]["fields"] = json!(["другое", "содержимое", ""]);
    });
    let after = canonical_export("report-ambiguous-after", &export);

    let out = TempDir::new("report-ambiguous-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    assert_eq!(result["counts"]["ambiguous"], 1);
    assert_eq!(result["counts"]["created"], 0);
    assert_eq!(result["counts"]["changed"], 0);
    let codes: Vec<&str> = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .map(|item| item["code"].as_str().expect("код"))
        .collect();
    assert!(
        codes.contains(&"ambiguous_guid"),
        "неоднозначность названа: {codes:?}"
    );
    assert_eq!(result["counts"]["previews"], 0, "угаданных превью нет");
}

#[test]
fn report_surfaces_unsupported_template_constructs() {
    let before = canonical_base("report-template-before");
    let export = export_with(|value| {
        value["note_models"][0]["tmpls"][0]["qfmt"] =
            json!("{{Заголовок}}{{cloze:Толкование}}<script>alert(1)</script>");
    });
    let after = canonical_export("report-template-after", &export);
    set_field(&after, "guid-2", 1, "значение");

    let out = TempDir::new("report-template-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    let constructs = result["unsupported_constructs"]
        .as_array()
        .expect("конструкции");
    assert!(!constructs.is_empty(), "неподдержанные конструкции найдены");
    let kinds: Vec<&str> = constructs
        .iter()
        .map(|item| item["construct"].as_str().expect("конструкция"))
        .collect();
    assert!(
        kinds.iter().any(|kind| kind.contains("cloze")),
        "фильтр cloze не вычисляется статически: {kinds:?}"
    );

    // `<script>` — не конструкция шаблона, а отказ от отрисовки стороны: он
    // обязан быть виден и в диагностике, а не только в HTML карточки.
    let codes: Vec<&str> = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .map(|item| item["code"].as_str().expect("код"))
        .collect();
    assert!(
        codes.contains(&"preview_incomplete"),
        "неполное превью названо диагностикой: {codes:?}"
    );
    let reasons: String = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .map(|item| item["message"].as_str().expect("сообщение"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(reasons.contains("<script"), "причина названа: {reasons}");

    // То же сказано и в самом отчёте, а не только в JSON.
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(index.contains("Неподдержанные") || index.contains("неподдержан"));
    assert!(index.contains("preview_incomplete") || index.contains("неполное"));
}

#[test]
fn report_preview_limit_bounds_the_number_of_cards() {
    let before = canonical_base("report-limit-before");
    let after = canonical_base("report-limit-after");
    set_field(&after, "guid-1", 1, "первое изменение");
    set_field(&after, "guid-2", 1, "второе изменение");

    let out = TempDir::new("report-limit-out");
    let (exit, document) = report(
        before.path(),
        after.path(),
        out.path(),
        &["--preview-limit", "1"],
    );
    let result = result_of(exit, &document);

    assert_eq!(result["counts"]["changed"], 2);
    assert_eq!(result["counts"]["previews"], 1);
    assert_eq!(result["card_files"], json!(["cards/card-0001.html"]));
    // `card_files_total` — число фактически записанных файлов, а не длина
    // обрезанного списка: по этим двум значениям потребитель видит и предел
    // превью, и то, что было бы записано без него.
    assert_eq!(result["card_files_total"], 1);
    assert_eq!(result["outcomes"].as_array().expect("заметки").len(), 2);
    assert_eq!(walk(out.path()), vec!["cards/card-0001.html", "index.html"]);

    // Усечение превью обязано быть видно в самом отчёте, а не только в списке
    // файлов: иначе читатель примет один кадр за полный отчёт.
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(
        index.contains("остальные перечислены только в JSON-результате"),
        "отчёт называет усечение превью"
    );
}

#[test]
fn report_keeps_full_card_file_list_separate_from_the_written_files() {
    // Предел превью ограничивает запись, а `--json` обязан назвать и то, что
    // записано, и то, что в список не поместилось.
    let before = canonical_base("report-total-before");
    let after = canonical_base("report-total-after");
    set_field(&after, "guid-1", 1, "первое изменение");
    set_field(&after, "guid-2", 1, "второе изменение");
    set_field(&after, "guid-2", 2, "и пример тоже");

    let out = TempDir::new("report-total-out");
    let (exit, document) = report(
        before.path(),
        after.path(),
        out.path(),
        &["--preview-limit", "3"],
    );
    let result = result_of(exit, &document);

    assert_eq!(result["counts"]["changed"], 2);
    assert_eq!(result["counts"]["previews"], 2);
    assert_eq!(result["card_files_total"], 2);
    assert_eq!(
        result["card_files"],
        json!(["cards/card-0001.html", "cards/card-0002.html"])
    );
    assert_eq!(
        walk(out.path()),
        vec!["cards/card-0001.html", "cards/card-0002.html", "index.html"]
    );
}
