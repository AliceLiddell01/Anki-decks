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

/// Читает файл отчёта как текст.
fn card_text(out: &TempDir, path: &str) -> String {
    std::fs::read_to_string(out.path().join(path)).unwrap_or_else(|error| panic!("{path}: {error}"))
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
    // Изменённая заметка показывается двумя кадрами — «до» и «после», — потому
    // что ревьюеру нужно именно то, что было, а не diff поверх нового значения.
    assert_eq!(result["counts"]["previews"], 3);
    assert_eq!(
        result["card_files"],
        json!([
            "cards/card-0001.html",
            "cards/card-0002.html",
            "cards/card-0003.html"
        ])
    );

    // Каждый файл превью называет состояние, из которого построен: без этого
    // потребителю `--json` пришлось бы угадывать, где «до», а где «после».
    let states_of = |guid: &str| -> Vec<String> {
        result["preview_files"]
            .as_array()
            .expect("файлы превью")
            .iter()
            .filter(|fact| fact["guid"] == guid)
            .map(|fact| fact["state"].as_str().expect("состояние").to_string())
            .collect()
    };
    assert_eq!(
        states_of("guid-3"),
        vec!["after"],
        "созданная: только «после»"
    );
    assert_eq!(
        states_of("guid-2"),
        vec!["before", "after"],
        "изменённая: обе стороны, «до» первым"
    );
    assert!(
        states_of("guid-1").is_empty(),
        "выведенной из обращения превью нет"
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
    assert_eq!(
        walk(out.path()),
        vec!["cards/card-0001.html", "cards/card-0002.html", "index.html"]
    );

    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(index.contains("<!doctype html>"));
    assert!(index.contains("до:"));
    assert!(index.contains(DECK_PATH));
    assert!(
        index.contains("cards/card-0001.html"),
        "index ссылается на превью"
    );
    assert!(index.contains("Ограничения этого отчёта"));

    // Отчёт открывается офлайн: внешних ссылок нет ни в одном записанном файле.
    for path in walk(out.path()) {
        let text = std::fs::read_to_string(out.path().join(&path)).expect("файл отчёта");
        // Упоминание схемы в тексте ограничений допустимо, а ссылка — нет.
        for marker in [
            "src=\"http",
            "src='http",
            "src=\"//",
            "src='//",
            "href=\"http",
            "href='http",
            "href=\"//",
            "href='//",
            "url(http",
            "@import",
        ] {
            assert!(
                !text.contains(marker),
                "{path} не должен ссылаться в сеть: {marker}"
            );
        }
        assert_report_scripts_are_own(&text, &path);
    }

    // Интерактивность отчёта — собственный код, записанный в сами файлы. Он
    // обязан быть на месте, иначе высота кадра и ночная тема не работают.
    assert!(index.contains("data-report-runtime=\"index\""));
    assert!(index.contains("report:hello"));
    assert!(index.contains("report:height"));
    assert!(index.contains("report:theme"));
    assert!(
        index.contains("data-report-theme-value=\"night\""),
        "у отчёта есть переключатель темы"
    );
    assert!(
        index.contains("nightMode"),
        "отчёт объясняет, что ночная тема доходит до карточек"
    );

    // Превью обязано быть готовым к обоим состояниям и к измерению высоты.
    for (path, state) in [
        ("cards/card-0001.html", "before"),
        ("cards/card-0002.html", "after"),
    ] {
        let card = std::fs::read_to_string(out.path().join(path)).expect("превью");
        assert!(card.contains("data-report-runtime=\"card\""));
        assert!(card.contains("nightMode"), "runtime ставит класс Anki");
        assert!(card.contains("report:height"));
        assert!(
            index.contains(&format!("data-report-state=\"{state}\"")),
            "index называет состояние кадра {state}"
        );
    }
}

/// Каждый `<script>` в файле отчёта обязан быть собственным runtime отчёта.
///
/// Это и есть граница доверия в проверяемом виде: код шаблона Anki не
/// исполняется, а внешних скриптов нет вовсе — ни `src`, ни `type`.
fn assert_report_scripts_are_own(text: &str, path: &str) {
    let mut rest = text;
    let mut found = 0usize;
    while let Some(position) = rest.find("<script") {
        let tail = &rest[position..];
        let end = tail.find('>').expect("открывающий тег скрипта");
        let tag = &tail[..end];
        assert!(
            tag.contains("data-report-runtime=\""),
            "{path}: посторонний скрипт запрещён: {tag}"
        );
        assert!(
            !tag.contains("src="),
            "{path}: внешний скрипт запрещён: {tag}"
        );
        assert!(
            !tag.contains("type="),
            "{path}: у runtime отчёта нет отдельного типа: {tag}"
        );
        found += 1;
        rest = &tail[end..];
    }
    assert!(found > 0, "{path}: runtime отчёта обязан быть в файле");
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
        vec![
            "cards/card-0001.html",
            "cards/card-0002.html",
            "index.html",
            "media/after/b.png"
        ]
    );
    // Media состояния лежат в своём подкаталоге: одинаковое имя в «до» и
    // «после» — два разных файла, и общий путь молча перепутал бы их.
    let states = result["media"]["states"].as_array().expect("состояния");
    let media_of = |state: &str| -> Value {
        states
            .iter()
            .find(|item| item["state"] == state)
            .expect("состояние")
            .clone()
    };
    let after_media = media_of("after");
    let before_media = media_of("before");
    assert_eq!(after_media["copied"], json!(["b.png"]));
    assert_eq!(after_media["missing"], json!(["нет.png"]));
    // У состояния «до» нет ни одной media-ссылки: поле «Пример» там пусто. Это
    // и есть доказательство, что «до» не подхватывает файлы из «после».
    assert_eq!(before_media["copied"], json!([]));
    assert_eq!(before_media["missing"], json!([]));

    // Кадры «до» и «после» — разные файлы: «до» пусто в поле «Пример», поэтому
    // картинок в нём нет вовсе, а ссылка на них не выдумывается.
    let before_card =
        std::fs::read_to_string(out.path().join("cards/card-0001.html")).expect("превью «до»");
    let after_card =
        std::fs::read_to_string(out.path().join("cards/card-0002.html")).expect("превью «после»");
    assert!(
        !before_card.contains("b.png"),
        "превью «до» не берёт содержимое из «после»: {before_card}"
    );
    assert!(
        after_card.contains("src=\"../media/after/b.png\""),
        "путь переписан: {after_card}"
    );
    assert!(
        after_card.contains("https://пример/в.png"),
        "внешняя ссылка остаётся как есть, а не подменяется"
    );
    assert!(
        !after_card.contains("\"media/a.mp3\""),
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
    // Детерминированность проверяется и на media, и на звуке: скопированные
    // файлы и подставленные ссылки обязаны совпадать побайтово.
    for dir in [&before, &after] {
        set_field(dir, "guid-2", 0, "[sound:a.mp3]必然<img src=\"a.png\">");
    }
    before.write_media_bytes("a.png", b"before-png");
    after.write_media_bytes("a.png", b"after-png");
    set_field(&after, "guid-2", 1, "значение для сравнения");

    let first = TempDir::new("report-determinism-first");
    let second = TempDir::new("report-determinism-second");
    let (exit, _) = report(before.path(), after.path(), first.path(), &[]);
    assert_eq!(exit, 0);
    let (exit, _) = report(before.path(), after.path(), second.path(), &[]);
    assert_eq!(exit, 0);

    let files = walk(first.path());
    assert_eq!(files, walk(second.path()), "состав файлов обязан совпадать");
    assert!(
        files.iter().any(|path| path.ends_with("a.png")),
        "media попадает в артефакт: {files:?}"
    );
    for path in files {
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
    // Для неоднозначного `guid` превью не строится: отчёт не выбирает заметку
    // наугад. `guid-2` исчез из «после», и для него честно показано состояние
    // «до» — единственное, которое про него известно.
    let files: Vec<(&str, &str)> = result["preview_files"]
        .as_array()
        .expect("файлы превью")
        .iter()
        .map(|fact| {
            (
                fact["guid"].as_str().expect("guid"),
                fact["state"].as_str().expect("состояние"),
            )
        })
        .collect();
    assert_eq!(files, vec![("guid-2", "before")]);
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
    // Предел ограничивает число подробно показанных заметок, а не кадров: одна
    // изменённая заметка — это законные «до» и «после», и терять вторую сторону
    // из-за предела нельзя.
    assert_eq!(result["counts"]["previews"], 2);
    assert_eq!(
        result["card_files"],
        json!(["cards/card-0001.html", "cards/card-0002.html"])
    );
    // `card_files_total` — число фактически записанных файлов, а не длина
    // обрезанного списка: по этим двум значениям потребитель видит и предел
    // превью, и то, что было бы записано без него.
    assert_eq!(result["card_files_total"], 2);
    assert_eq!(result["outcomes"].as_array().expect("заметки").len(), 2);
    assert_eq!(
        walk(out.path()),
        vec!["cards/card-0001.html", "cards/card-0002.html", "index.html"]
    );

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
    assert_eq!(result["counts"]["previews"], 4);
    assert_eq!(result["card_files_total"], 4);
    assert_eq!(
        result["card_files"],
        json!([
            "cards/card-0001.html",
            "cards/card-0002.html",
            "cards/card-0003.html",
            "cards/card-0004.html"
        ])
    );
    // Состояние каждого записанного файла названо, и список превью не обрезан.
    assert_eq!(
        result["preview_files"]
            .as_array()
            .expect("файлы превью")
            .iter()
            .map(|fact| fact["state"].as_str().expect("состояние"))
            .collect::<Vec<_>>(),
        vec!["before", "after", "before", "after"]
    );
    assert_eq!(result["preview_files_truncated"], false);
    assert_eq!(
        walk(out.path()),
        vec![
            "cards/card-0001.html",
            "cards/card-0002.html",
            "cards/card-0003.html",
            "cards/card-0004.html",
            "index.html"
        ]
    );
}

#[test]
fn report_leaves_both_exports_untouched_and_keeps_cards_diff_free() {
    let before = canonical_base("report-immutable-before");
    let after = canonical_base("report-immutable-after");
    // Маркер в CSS модели: он обязан попасть в изолированное превью и не
    // попасть в страницу отчёта, иначе CSS колоды смог бы переоформить UI.
    for dir in [&before, &after] {
        let mut export = dir.deck_json();
        export["note_models"][0]["css"] = json!("#модель { color: red }");
        dir.write_canonical_deck_json(&export);
    }
    set_field(&after, "guid-1", 1, "изменённое толкование");
    let before_bytes = before.deck_json_bytes();
    let after_bytes = after.deck_json_bytes();
    let before_files = walk(before.path());
    let after_files = walk(after.path());

    let out = TempDir::new("report-immutable-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_eq!(result["counts"]["changed"], 1);

    // Генератор отчёта ничего не пишет в сравниваемые состояния: ни в deck.json,
    // ни в новые файлы рядом.
    assert_eq!(
        before.deck_json_bytes(),
        before_bytes,
        "состояние «до» не тронуто"
    );
    assert_eq!(
        after.deck_json_bytes(),
        after_bytes,
        "состояние «после» не тронуто"
    );
    assert_eq!(walk(before.path()), before_files);
    assert_eq!(walk(after.path()), after_files);

    // Превью обязано показывать карточку, а не diff: подсветка правки живёт
    // отдельно, в описании изменения на странице отчёта.
    for path in ["cards/card-0001.html", "cards/card-0002.html"] {
        let card = std::fs::read_to_string(out.path().join(path)).expect("карточка");
        assert!(
            !card.contains("report-token-del") && !card.contains("report-token-ins"),
            "в превью нет подсветки diff: {path}"
        );
        assert!(
            !card.contains("report-counts") && !card.contains("report-diagnostics"),
            "в превью нет элементов страницы отчёта: {path}"
        );
        // Превью несёт фактический CSS модели — и в «до», и в «после».
        assert!(
            card.contains("#модель { color: red }"),
            "превью несёт фактический CSS модели: {path}"
        );
    }

    // И наоборот: страница отчёта не встраивает CSS модели, а только ссылается
    // на изолированные карточки.
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(
        index.contains("cards/card-0001.html") && index.contains("cards/card-0002.html"),
        "отчёт ссылается на оба состояния: {index:.200}"
    );
    assert!(
        !index.contains("#модель { color: red }"),
        "страница отчёта не встраивает CSS модели и не может им переоформляться"
    );
}

#[test]
fn report_renders_every_template_of_a_touched_model() {
    let before = canonical_base("report-templates-before");
    let after = canonical_base("report-templates-after");
    // Вторая карточка той же модели: Anki показывает по карточке на каждый
    // шаблон, и отчёт обязан показать их все, а не только первую.
    let mut export = after.deck_json();
    export["note_models"][0]["css"] = json!(".report-title { color: red }");
    export["note_models"][0]["tmpls"]
        .as_array_mut()
        .expect("шаблоны")
        .push(json!({
            "__type__": "CardTemplate",
            "name": "Карточка 2",
            "ord": 1,
            "qfmt": "{{Толкование}}",
            "afmt": "{{FrontSide}}",
        }));
    after.write_canonical_deck_json(&export);
    set_field(&after, "guid-1", 1, "изменённое толкование");

    let out = TempDir::new("report-templates-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    // Второй шаблон добавлен только в «после», поэтому у «до» одна карточка, а у
    // «после» — две: превью строится по модели своего состояния, а не по общей.
    assert_eq!(
        result["card_files_total"], 3,
        "шаблоны берутся у модели своего состояния"
    );
    let read = |path: &str| -> String {
        std::fs::read_to_string(out.path().join(path)).expect("превью")
    };
    let before_first = read("cards/card-0001.html");
    let after_first = read("cards/card-0002.html");
    let after_second = read("cards/card-0003.html");
    assert!(before_first.contains("card card1"), "ord 0 → card1");
    assert!(after_first.contains("card card1"), "ord 0 → card1");
    assert!(after_second.contains("card card2"), "ord 1 → card2");
    assert!(
        !before_first.contains("card card2"),
        "шаблона «Карточка 2» в состоянии «до» не было"
    );
    for (name, card) in [("card-0002", &after_first), ("card-0003", &after_second)] {
        assert!(
            card.contains(".report-title { color: red }"),
            "{name} обязан нести фактический CSS модели"
        );
    }
    assert!(
        !before_first.contains(".report-title"),
        "превью «до» несёт CSS модели своего состояния, а не из «после»"
    );
}

#[test]
fn report_renders_each_state_from_its_own_note_value() {
    let before = canonical_base("report-state-before");
    let after = canonical_base("report-state-after");
    // Изменяется поле «Заголовок» — оно попадает в шаблон, поэтому значения
    // обоих состояний видно в самой карточке, а не только в field diff.
    set_field(&before, "guid-2", 0, "было-слово");
    set_field(&after, "guid-2", 0, "стало-слово");

    let out = TempDir::new("report-state-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_eq!(result["counts"]["changed"], 1);

    let before_card = card_text(&out, "cards/card-0001.html");
    let after_card = card_text(&out, "cards/card-0002.html");
    assert!(
        before_card.contains("было-слово"),
        "кадр «до»: {before_card:.400}"
    );
    assert!(
        !before_card.contains("стало-слово"),
        "кадр «до» не должен показывать новое значение"
    );
    assert!(
        after_card.contains("стало-слово"),
        "кадр «после» показывает фактическое состояние «после»"
    );
    assert!(
        !after_card.contains("было-слово"),
        "кадр «после» не должен показывать старое значение"
    );

    // Обе стороны лежат в одной раскладке сравнения и подписаны.
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");
    assert!(index.contains("class=\"report-compare\""));
    assert!(index.contains(">До<"));
    assert!(index.contains(">После<"));
}

#[test]
fn report_copies_the_same_basename_separately_for_each_state() {
    let before = canonical_base("report-shared-before");
    let after = canonical_base("report-shared-after");
    for dir in [&before, &after] {
        set_field(dir, "guid-2", 2, "<img src=\"shared.png\">");
    }
    // Одинаковое имя, разное содержимое: состояние обязано показывать свой файл.
    before.write_media_bytes("shared.png", b"before-bytes");
    after.write_media_bytes("shared.png", b"after-bytes");
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let out = TempDir::new("report-shared-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_eq!(result["media"]["copied"], 2);

    assert_eq!(
        std::fs::read(out.path().join("media/before/shared.png")).expect("файл «до»"),
        b"before-bytes".to_vec()
    );
    assert_eq!(
        std::fs::read(out.path().join("media/after/shared.png")).expect("файл «после»"),
        b"after-bytes".to_vec()
    );
    assert!(
        card_text(&out, "cards/card-0001.html").contains("src=\"../media/before/shared.png\""),
        "кадр «до» ссылается на файл своего состояния"
    );
    assert!(
        card_text(&out, "cards/card-0002.html").contains("src=\"../media/after/shared.png\""),
        "кадр «после» ссылается на файл своего состояния"
    );
}

#[test]
fn media_missing_only_in_before_is_not_taken_from_after() {
    let before = canonical_base("report-lost-before");
    let after = canonical_base("report-lost-after");
    // Файл есть только в «после»: в «до» он обязан остаться отсутствующим.
    set_field(&before, "guid-2", 2, "<img src=\"only-before.png\">");
    set_field(&after, "guid-2", 2, "<img src=\"only-after.png\">");
    set_field(&after, "guid-2", 1, "изменение ради превью");
    after.write_media(&["only-before.png", "only-after.png"]);

    let out = TempDir::new("report-lost-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    assert!(
        !out.path().join("media/before/only-before.png").exists(),
        "отсутствующий файл не подменяется одноимённым из «после»"
    );
    assert!(
        !out.path().join("media/after/only-before.png").exists(),
        "файл копируется только для того состояния, которое на него ссылается"
    );
    assert!(
        card_text(&out, "cards/card-0001.html").contains("src=\"only-before.png\""),
        "в кадре «до» ссылка остаётся сырой: файла там нет"
    );
    assert!(
        card_text(&out, "cards/card-0002.html").contains("src=\"../media/after/only-after.png\"")
    );

    let missing: Vec<&str> = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .filter(|item| item["code"] == "missing_media")
        .map(|item| item["subject"].as_str().expect("subject"))
        .collect();
    assert_eq!(missing, vec!["before:only-before.png"], "состояние названо");
}

#[test]
fn sound_media_becomes_a_local_player_and_a_missing_sound_stays_marked() {
    let before = canonical_base("report-sound-before");
    let after = canonical_base("report-sound-after");
    for dir in [&before, &after] {
        set_field(dir, "guid-2", 0, "[sound:a.mp3][sound:нет-звука.mp3]必然");
    }
    before.write_media(&["a.mp3"]);
    after.write_media(&["a.mp3"]);
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let out = TempDir::new("report-sound-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    assert_eq!(result["media"]["copied"], 2, "файл есть в обоих состояниях");
    assert_eq!(result["media"]["missing"], json!(["нет-звука.mp3"]));

    for (path, state) in [
        ("cards/card-0001.html", "before"),
        ("cards/card-0002.html", "after"),
    ] {
        let card = card_text(&out, path);
        assert!(
            card.contains("class=\"replay-button\""),
            "{path}: звук показан кнопкой повтора, а не текстом"
        );
        assert!(
            card.contains("class=\"report-audio\""),
            "{path}: у звука есть локальный проигрыватель"
        );
        assert!(
            card.contains(&format!("src=\"../media/{state}/a.mp3\"")),
            "{path}: проигрыватель ссылается на файл своего состояния"
        );
        assert!(
            !card.contains("[sound:a.mp3]"),
            "{path}: существующий звук не остаётся сырым текстом"
        );
        // Отсутствующий звук остаётся видимым и помеченным: подстановки нет.
        assert!(card.contains("report-audio-missing"), "{path}");
        assert!(card.contains("[sound:нет-звука.mp3]"), "{path}");
        assert!(
            !card.contains("media/before/нет-звука.mp3")
                && !card.contains(&format!("media/{state}/нет-звука.mp3")),
            "{path}: файла нет и он не выдуман"
        );
    }

    let missing_sounds: Vec<&str> = result["preview_files"]
        .as_array()
        .expect("файлы превью")
        .iter()
        .flat_map(|fact| {
            fact["missing_sounds"]
                .as_array()
                .expect("отсутствующие звуки")
                .iter()
                .map(|name| name.as_str().expect("имя"))
        })
        .collect();
    assert_eq!(missing_sounds, vec!["нет-звука.mp3", "нет-звука.mp3"]);
}

/// Извлекает тело runtime отчёта из записанного файла.
fn runtime_source(html: &str, marker: &str) -> String {
    let opening = format!("<script data-report-runtime=\"{marker}\">");
    let start = html
        .find(&opening)
        .unwrap_or_else(|| panic!("в файле нет runtime {marker}"));
    let from = start + opening.len();
    let end = html[from..]
        .find("</script>")
        .unwrap_or_else(|| panic!("runtime {marker} не закрыт"))
        + from;
    html[from..end].to_string()
}

/// Скрипт-стенд: исполняет записанный runtime в Node с минимальным DOM.
///
/// Браузера в этом test stack нет, поэтому проверяется не картинка, а контракт,
/// от которого картинка зависит: ночной класс доходит до корня документа и до
/// самой карточки (тогда Anki-селекторы `.card.nightMode` и `.nightMode .…`
/// применимы), а высота кадра приходит из содержимого, а не из константы.
const RUNTIME_HARNESS: &str = r#"
'use strict';
const fs = require('fs');
const assert = require('assert');
const vm = require('vm');

function source(file, marker) {
  const text = fs.readFileSync(file, 'utf8');
  const opening = '<script data-report-runtime="' + marker + '">';
  const start = text.indexOf(opening);
  assert.ok(start >= 0, 'runtime ' + marker + ' отсутствует в ' + file);
  const from = start + opening.length;
  const end = text.indexOf('</script>', from);
  assert.ok(end > from, 'runtime ' + marker + ' не закрыт');
  return text.slice(from, end);
}

function element(tag, height) {
  const classes = new Set();
  const attrs = {};
  return {
    tagName: tag,
    scrollHeight: height,
    offsetHeight: 0,
    classList: {
      add: (name) => classes.add(name),
      remove: (name) => classes.delete(name),
      contains: (name) => classes.has(name),
    },
    setAttribute: (key, value) => { attrs[key] = value; },
    getAttribute: (key) => (key in attrs ? attrs[key] : null),
    attrs: attrs,
    has: (name) => classes.has(name),
  };
}

function cardRun(src) {
  const posted = [];
  const listeners = {};
  const root = element('html', 0);
  const body = element('body', 480);
  const card = element('div', 480);
  const document = {
    documentElement: root,
    body: body,
    querySelectorAll: (selector) => (selector === '.card' ? [card] : []),
    addEventListener: (type, fn) => { (listeners[type] = listeners[type] || []).push(fn); },
  };
  const parent = { postMessage: (message) => posted.push(message) };
  const window = {
    parent: parent,
    addEventListener: (type, fn) => { (listeners[type] = listeners[type] || []).push(fn); },
  };
  const sandbox = { document: document, window: window, Number, Math, String, isFinite, JSON, console };
  vm.createContext(sandbox);
  vm.runInContext(src, sandbox);

  const hello = posted.find((message) => message.type === 'report:hello');
  assert.ok(hello, 'карточка обязана сообщить о готовности');
  assert.strictEqual(hello.height, 480, 'высота берётся из содержимого');

  const message = listeners.message[0];
  message({ data: { type: 'report:theme', theme: 'night' } });
  assert.ok(root.has('nightMode'), 'ночной класс обязан дойти до корня документа');
  assert.ok(body.has('nightMode'), 'ночной класс обязан дойти до body');
  assert.ok(card.has('nightMode'), '.card.nightMode обязан стать применимым');
  message({ data: { type: 'report:theme', theme: 'light' } });
  assert.ok(!card.has('nightMode'), 'светлая тема снимает ночной класс');

  body.scrollHeight = 900;
  const load = listeners.load.find((fn) => fn);
  load({ target: { tagName: 'IMG' } });
  const height = posted.filter((item) => item.type === 'report:height').pop();
  assert.strictEqual(height.height, 900, 'высота обновляется после загрузки media');
}

function indexRun(src) {
  const frames = [];
  const buttons = [];
  const inbox = [];
  for (let i = 0; i < 2; i += 1) {
    const frame = {
      messages: [],
      style: {},
      attrs: {},
      setAttribute: function (key, value) { this.attrs[key] = value; },
    };
    frame.contentWindow = { postMessage: (message) => frame.messages.push(message) };
    frames.push(frame);
  }
  for (const value of ['light', 'night']) {
    const button = {
      attrs: {},
      handlers: {},
      getAttribute: (key) => (key === 'data-report-theme-value' ? value : button.attrs[key] || null),
      setAttribute: (key, val) => { button.attrs[key] = val; },
      addEventListener: (type, fn) => { button.handlers[type] = fn; },
    };
    buttons.push(button);
  }
  const listeners = {};
  const root = element('html', 0);
  const body = element('body', 0);
  const document = {
    documentElement: root,
    body: body,
    querySelectorAll: (selector) => {
      if (selector === 'iframe.report-preview') { return frames; }
      if (selector === '[data-report-theme-value]') { return buttons; }
      return [];
    },
    addEventListener: () => {},
  };
  const window = {
    parent: null,
    addEventListener: (type, fn) => { (listeners[type] = listeners[type] || []).push(fn); },
  };
  window.parent = window;
  const sandbox = { document: document, window: window, Number, Math, String, isFinite, JSON, console };
  vm.createContext(sandbox);
  vm.runInContext(src, sandbox);

  assert.strictEqual(frames[0].messages.length, 1, 'кадры получают тему при загрузке отчёта');
  assert.strictEqual(frames[1].messages[0].theme, 'light', 'начальная тема — светлая');
  inbox.push(frames[0].messages.length);

  const night = buttons[1];
  night.handlers.click({ currentTarget: night });
  assert.strictEqual(root.getAttribute('data-report-theme'), 'night');
  assert.ok(body.has('report-night'), 'оболочка отчёта переходит в ночную тему');
  assert.strictEqual(night.attrs['aria-pressed'], 'true');
  assert.strictEqual(frames[0].messages[1].theme, 'night', 'первый кадр переключился');
  assert.strictEqual(frames[1].messages[1].theme, 'night', 'второй кадр переключился');

  // Кадр, загрузившийся позже переключения, обязан получить текущую тему, а не
  // остаться светлым из-за гонки.
  const message = listeners.message[0];
  message({ source: frames[1].contentWindow, data: { type: 'report:hello', height: 700 } });
  const reply = frames[1].messages[frames[1].messages.length - 1];
  assert.strictEqual(reply.theme, 'night', 'поздний кадр получает текущую тему');
  assert.strictEqual(frames[1].style.height, '700px', 'высота кадра берётся из сообщения');

  message({ source: frames[0].contentWindow, data: { type: 'report:height', height: 512 } });
  assert.strictEqual(frames[0].style.height, '512px', 'высота обновляется повторно');
}

const indexFile = process.argv[2];
const cardFile = process.argv[3];
cardRun(source(cardFile, 'card'));
indexRun(source(indexFile, 'index'));
console.log('runtime contract ok');
"#;

#[test]
fn report_runtime_satisfies_the_theme_and_height_contract() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        // Node не является зависимостью проекта: без него проверка браузерного
        // контракта пропускается, а не падает.
        eprintln!("node недоступен: проверка runtime пропущена");
        return;
    }

    let before = canonical_base("report-runtime-before");
    let after = canonical_base("report-runtime-after");
    set_field(&after, "guid-2", 1, "изменение ради двух кадров");

    let out = TempDir::new("report-runtime-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    result_of(exit, &document);

    // Высота кадра в HTML — запасная: её перекрывает измеренная высота, и это
    // единственный размер, который видит файл до выполнения runtime.
    let index = card_text(&out, "index.html");
    assert!(
        !index.contains("height=\"420\""),
        "фиксированная высота кадра не возвращается"
    );
    assert!(index.contains("height=\"720\""), "запасная высота названа");

    let card = card_text(&out, "cards/card-0001.html");
    assert!(!runtime_source(&index, "index").is_empty());
    assert!(!runtime_source(&card, "card").is_empty());

    let harness = TempDir::new("report-runtime-harness");
    std::fs::write(harness.path().join("harness.js"), RUNTIME_HARNESS).expect("стенд");
    let output = std::process::Command::new("node")
        .arg(harness.path().join("harness.js"))
        .arg(out.path().join("index.html"))
        .arg(out.path().join("cards/card-0001.html"))
        .output()
        .expect("node должен запускаться");
    assert!(
        output.status.success(),
        "стенд не прошёл: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn report_does_not_run_template_javascript() {
    let before = canonical_base("report-template-js-before");
    let after = canonical_base("report-template-js-after");
    // `<script>` в шаблоне остаётся неподдержанной конструкцией: runtime отчёта
    // не подхватывает его и не превращает в поддержку.
    let mut export = after.deck_json();
    export["note_models"][0]["tmpls"][0]["qfmt"] =
        json!("<script>window.__injected = 1;</script>{{Заголовок}}");
    after.write_canonical_deck_json(&export);
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let out = TempDir::new("report-template-js-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    // `<script>` — не конструкция шаблона, а отказ от отрисовки стороны: он
    // обязан быть виден в диагностике, а не только в HTML карточки.
    let messages: String = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .map(|item| {
            format!(
                "{}: {}",
                item["code"].as_str().expect("код"),
                item["message"].as_str().expect("сообщение")
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        messages.contains("preview_incomplete"),
        "неполное превью названо: {messages}"
    );
    assert!(
        messages.contains("<script"),
        "причина названа явно: {messages}"
    );
    assert!(
        messages.contains("(До)") || messages.contains("(После)"),
        "диагностика называет состояние превью: {messages}"
    );

    // В записанных файлах есть только runtime отчёта: шаблонный код остаётся
    // видимым текстом и не превращается в исполняемый тег.
    let mut shown_as_text = 0usize;
    for path in walk(out.path()) {
        let text = card_text(&out, &path);
        assert_report_scripts_are_own(&text, &path);
        if text.contains("&lt;script>window.__injected") {
            shown_as_text += 1;
        }
    }
    assert_eq!(
        shown_as_text, 1,
        "шаблонный script показан текстом ровно в кадре «после»"
    );
}
