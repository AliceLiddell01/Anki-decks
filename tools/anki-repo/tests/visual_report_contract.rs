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
/// Файлы самого отчёта: без доказательства владения, лежащего рядом с ними.
fn artifact_files(root: &Path) -> Vec<String> {
    walk(root)
        .into_iter()
        .filter(|relative| relative != anki_repo::report::manifest::MANIFEST_FILE)
        .collect()
}

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
        artifact_files(out.path()),
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

    // Отчёт открывается офлайн: каждый записанный файл проходит общую проверку.
    assert_every_page_is_offline(&out);

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
    // Тема — одно состояние, значит и переключатель один на весь документ: две
    // группы кнопок разошлись бы в состоянии при первой же правке.
    assert_eq!(
        index.matches("class=\"report-theme\"").count(),
        1,
        "переключатель темы в документе ровно один"
    );
    assert_eq!(
        index.matches("data-report-theme-value=").count(),
        2,
        "в переключателе две кнопки"
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
/// Проверка структурная: тег и его тело разбираются сканером HTML, поэтому
/// «запрещённой подстроки нет» здесь недостаточно — сравнивается само тело
/// скрипта с runtime отчёта, и регистр тега, кавычек и пробелов ничего не меняет.
fn assert_report_scripts_are_own(text: &str, path: &str) {
    let mut found = 0usize;
    for tag in anki_repo::htmlscan::scan_tags(text) {
        let anki_repo::htmlscan::Tag::RawText {
            name,
            body,
            close_end,
            ..
        } = tag
        else {
            continue;
        };
        if !name.eq_ignore_ascii_case("script") {
            continue;
        }
        assert!(close_end.is_some(), "{path}: скрипт обязан быть закрыт");
        let script = &text[body];
        assert!(
            script.trim() == anki_repo::report::runtime::CARD_RUNTIME_JS.trim()
                || script.trim() == anki_repo::report::runtime::INDEX_RUNTIME_JS.trim(),
            "{path}: исполняться может только runtime отчёта"
        );
        found += 1;
    }
    assert!(found > 0, "{path}: runtime отчёта обязан быть в файле");
}

/// Класс страницы по её месту в отчёте.
///
/// Не-HTML-файлы (скопированная media) страницами не являются и проверку
/// разметки не проходят — их проверяет отдельный контракт media.
fn page_of(relative: &str) -> Option<anki_repo::report::sanitize::Page> {
    use anki_repo::report::sanitize::Page;
    if relative == "index.html" {
        return Some(Page::Index);
    }
    if relative.starts_with("cards/") {
        return Some(Page::Card);
    }
    assert!(
        !relative.ends_with(".html"),
        "неизвестная страница отчёта: {relative}"
    );
    None
}

/// Значения адресных атрибутов документа.
///
/// Разбор тот же, что у отчёта: таблица «что здесь адрес» — не второй список в
/// тесте, а та же самая.
fn address_values(text: &str) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    for tag in anki_repo::htmlscan::scan_tags(text) {
        let anki_repo::htmlscan::Tag::Element(element) = tag else {
            continue;
        };
        for attribute in &element.attributes {
            if anki_repo::media::attribute_is_address(element.name, attribute.name) {
                found.push((
                    attribute.name.to_ascii_lowercase(),
                    attribute.value.to_string(),
                ));
            }
        }
    }
    found
}

/// Имена обработчиков событий, оставшихся атрибутами документа.
///
/// Упоминание `onerror` в тексте превью — это объяснение, а атрибут — это код,
/// поэтому проверяется разбор, а не подстрока.
fn event_handlers(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for tag in anki_repo::htmlscan::scan_tags(text) {
        let anki_repo::htmlscan::Tag::Element(element) = tag else {
            continue;
        };
        for attribute in &element.attributes {
            if attribute.name.to_ascii_lowercase().starts_with("on") {
                found.push(attribute.name.to_string());
            }
        }
    }
    found
}

/// Тела всех `<script>` документа.
fn script_bodies(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for tag in anki_repo::htmlscan::scan_tags(text) {
        let anki_repo::htmlscan::Tag::RawText { name, body, .. } = tag else {
            continue;
        };
        if name.eq_ignore_ascii_case("script") {
            found.push(text[body].to_string());
        }
    }
    found
}

/// Содержимое всех `<style>` документа.
fn style_texts(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for tag in anki_repo::htmlscan::scan_tags(text) {
        let anki_repo::htmlscan::Tag::RawText { name, body, .. } = tag else {
            continue;
        };
        if name.eq_ignore_ascii_case("style") {
            found.push(text[body].to_string());
        }
    }
    found
}

/// Проверяет, что каждый записанный файл отчёта описывается как его страница и
/// проходит общую границу доверия.
///
/// Это та же проверка, которой пользуется сам отчёт перед записью: у теста нет
/// отдельного списка запрещённых подстрок, который неизбежно отстал бы от
/// контракта.
fn assert_every_page_is_offline(out: &TempDir) {
    for path in artifact_files(out.path()) {
        let Some(page) = page_of(&path) else {
            continue;
        };
        let text = std::fs::read_to_string(out.path().join(&path)).expect("файл отчёта");
        let violations = anki_repo::report::sanitize::inspect(&text, page);
        assert!(
            violations.is_empty(),
            "{path} не проходит границу доверия: {violations:?}"
        );
        assert_report_scripts_are_own(&text, &path);
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
        artifact_files(out.path()),
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

    // Сравнивается весь каталог, включая манифест владения: детерминированность
    // относится к артефакту целиком, а не только к его страницам.
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
fn report_refuses_a_non_empty_out_dir_without_ownership_proof() {
    let before = canonical_base("report-occupied-before");
    let after = canonical_base("report-occupied-after");
    let out = TempDir::new("report-occupied-out");
    std::fs::write(out.path().join("чужой.txt"), "occupied").expect("файл");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    assert_ne!(exit, 0);
    assert_eq!(document["error"]["code"], "invalid_request");
    assert_eq!(
        document["error"]["details"]["reason"], "out_dir_not_owned",
        "чужой каталог отвергается: {document}"
    );

    // Отказ ничего не меняет и не оставляет следов: чужой файл на месте, а
    // каталог не превратился в отчёт.
    let (exit, _) = report(before.path(), after.path(), out.path(), &[]);
    assert_ne!(exit, 0, "чужой файл всё ещё на месте");
    assert_eq!(artifact_files(out.path()), vec!["чужой.txt".to_string()]);
}

/// Чужой каталог с обычным `index.html` — не доказательство владения: раньше
/// достаточно было этого имени, и отчёт перезаписывал чужой файл.
#[test]
fn a_foreign_index_html_is_not_a_proof_of_ownership() {
    let before = canonical_base("report-foreign-index-before");
    let after = canonical_base("report-foreign-index-after");
    let out = TempDir::new("report-foreign-index-out");
    std::fs::create_dir_all(out.path().join("cards")).expect("подкаталог");
    std::fs::write(
        out.path().join("index.html"),
        "<!doctype html><title>чужой отчёт</title>",
    )
    .expect("файл");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    assert_ne!(exit, 0);
    assert_eq!(document["error"]["details"]["reason"], "out_dir_not_owned");
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("чужой index.html");
    assert!(
        index.contains("чужой отчёт"),
        "чужой index.html не перезаписан: {index}"
    );
    assert!(
        !out.path().join("report-manifest.json").exists(),
        "отчёт не оставил следов владения в чужом каталоге"
    );
}

/// Испорченный или чужой манифест — отказ до любых изменений: доверять
/// половине доказательства владения нельзя.
#[test]
fn a_corrupt_manifest_is_refused_without_changes() {
    let before = canonical_base("report-corrupt-before");
    let after = canonical_base("report-corrupt-after");
    let out = TempDir::new("report-corrupt-out");
    std::fs::write(out.path().join("index.html"), "прежний index").expect("файл");
    std::fs::write(out.path().join("report-manifest.json"), "{ это не JSON").expect("манифест");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    assert_ne!(exit, 0);
    assert_eq!(
        document["error"]["details"]["reason"],
        "out_dir_manifest_invalid"
    );
    assert_eq!(
        std::fs::read_to_string(out.path().join("index.html")).expect("файл"),
        "прежний index"
    );
}

/// Повторная генерация оставляет ровно файлы текущего отчёта: устаревшие превью
/// прежнего прогона удаляются, а не остаются висеть рядом с новыми.
#[test]
fn repeated_generation_leaves_exactly_the_current_files() {
    let before = canonical_base("report-repeat-before");
    let after = canonical_base("report-repeat-after");
    let out = TempDir::new("report-repeat-out");

    // Первый прогон: меняются обе заметки, превью две.
    set_field(&after, "guid-1", 1, "первое изменение");
    set_field(&after, "guid-2", 1, "второе изменение");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    result_of(exit, &document);
    assert_eq!(
        artifact_files(out.path()),
        vec![
            "cards/card-0001.html",
            "cards/card-0002.html",
            "cards/card-0003.html",
            "cards/card-0004.html",
            "index.html"
        ],
        "первый прогон пишет превью обеих изменённых заметок"
    );
    let first_index = std::fs::read(out.path().join("index.html")).expect("index");

    // Второй прогон в тот же каталог: изменена одна заметка, превью одно.
    let second_before = canonical_base("report-repeat-before-2");
    let second_after = canonical_base("report-repeat-after-2");
    set_field(&second_after, "guid-2", 1, "единственное изменение");
    let (exit, document) = report(second_before.path(), second_after.path(), out.path(), &[]);
    let second = result_of(exit, &document);

    assert_eq!(
        artifact_files(out.path()),
        vec!["cards/card-0001.html", "cards/card-0002.html", "index.html"],
        "в каталоге остаётся ровно текущий набор файлов: превью прежней заметки убраны"
    );
    assert!(
        std::fs::read(out.path().join("index.html")).expect("index") != first_index,
        "index.html заменён, а не дополнен"
    );

    let manifest = read_manifest(out.path());
    assert_eq!(
        manifest["files"],
        json!(["cards/card-0001.html", "cards/card-0002.html", "index.html"]),
        "манифест перечисляет ровно файлы текущего отчёта: {manifest}"
    );

    // Удалённое названо: читатель обязан видеть, что прежние превью убраны, а не
    // что отчёт «внезапно стал короче».
    let removed_diagnostics: Vec<&str> = stale_diagnostics(&second);
    assert_eq!(
        removed_diagnostics.len(),
        1,
        "одно сообщение об уборке: {second}"
    );
    assert!(
        removed_diagnostics[0].contains("card-0003.html")
            && removed_diagnostics[0].contains("card-0004.html"),
        "названы именно устаревшие файлы: {removed_diagnostics:?}"
    );

    // Два независимых прогона с одним входом дают побайтово одинаковый артефакт.
    let third = TempDir::new("report-repeat-third");
    let fourth = TempDir::new("report-repeat-fourth");
    for dir in [&third, &fourth] {
        let (exit, _) = report(second_before.path(), second_after.path(), dir.path(), &[]);
        assert_eq!(exit, 0);
    }
    for relative in walk(third.path()) {
        let left = std::fs::read(third.path().join(&relative)).expect("файл");
        let right = std::fs::read(fourth.path().join(&relative)).expect("файл");
        assert!(left == right, "{relative} обязан совпадать побайтово");
    }
}

/// Чужой файл внутри каталога отчёта не удаляется и не перезаписывается: он
/// назван в диагностике, потому что «файл исчез» и «файл не наш» — разные вещи.
#[test]
fn foreign_files_inside_the_report_directory_are_reported_and_kept() {
    let before = canonical_base("report-foreign-file-before");
    let after = canonical_base("report-foreign-file-after");
    let out = TempDir::new("report-foreign-file-out");
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    result_of(exit, &document);
    std::fs::write(out.path().join("заметка.txt"), "чужое").expect("чужой файл");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_eq!(
        std::fs::read_to_string(out.path().join("заметка.txt")).expect("файл"),
        "чужое",
        "чужой файл остаётся на месте"
    );
    assert!(
        result["diagnostics"]
            .as_array()
            .expect("диагностика")
            .iter()
            .any(|item| item["code"] == "out_dir_foreign_files"
                && item["message"]
                    .as_str()
                    .expect("сообщение")
                    .contains("заметка.txt")),
        "чужой файл назван в диагностике: {result}"
    );
    let manifest = read_manifest(out.path());
    assert!(
        !manifest["files"]
            .as_array()
            .expect("файлы")
            .iter()
            .any(|file| file == "заметка.txt"),
        "чужой файл не попадает в доказательство владения: {manifest}"
    );
}

/// Отказ генерации не уничтожает прежний отчёт: сборка идёт рядом с каталогом
/// отчёта, и до переноса каталог не меняется вообще.
#[test]
fn a_failed_generation_keeps_the_previous_report() {
    let before = canonical_base("report-keep-before");
    let after = canonical_base("report-keep-after");
    let out = TempDir::new("report-keep-out");
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    result_of(exit, &document);
    let index_before = std::fs::read(out.path().join("index.html")).expect("index");
    let manifest_before = std::fs::read(out.path().join("report-manifest.json")).expect("манифест");

    // Новое состояние ссылается на media, который существует, но не читается:
    // копирование падает уже во время сборки отчёта.
    let broken_before = canonical_base("report-keep-broken-before");
    let broken_after = canonical_base("report-keep-broken-after");
    set_field(&broken_after, "guid-2", 0, "<img src=\"нечитаемый.png\">");
    broken_after.write_media(&["нечитаемый.png"]);
    make_unreadable(&broken_after.path().join("media/нечитаемый.png"));
    set_field(&broken_after, "guid-2", 1, "изменение ради превью");

    // Если окружение позволяет прочитать файл (например, запуск от root), проверка
    // теряет смысл: тогда она пропускается явно, а не проходит молча.
    if std::fs::read(broken_after.path().join("media/нечитаемый.png")).is_ok() {
        return;
    }

    let (exit, document) = report(broken_before.path(), broken_after.path(), out.path(), &[]);
    assert_ne!(exit, 0, "копирование media обязано упасть: {document}");
    assert_eq!(
        std::fs::read(out.path().join("index.html")).expect("index"),
        index_before,
        "прежний index.html не тронут"
    );
    assert_eq!(
        std::fs::read(out.path().join("report-manifest.json")).expect("манифест"),
        manifest_before,
        "прежнее доказательство владения не тронуто"
    );

    // Каталог сборки убран: после отказа он не остаётся мусором рядом с отчётом.
    let leftovers: Vec<String> = std::fs::read_dir(out.path().parent().expect("родитель"))
        .expect("каталог")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("report-keep-out") && name.contains("staging"))
        .collect();
    assert!(leftovers.is_empty(), "каталог сборки убран: {leftovers:?}");
}

/// Сообщения об уборке устаревших файлов прежнего отчёта.
fn stale_diagnostics(document: &serde_json::Value) -> Vec<&str> {
    document["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .filter(|item| item["code"] == "out_dir_stale_removed")
        .filter_map(|item| item["message"].as_str())
        .collect()
}

/// Манифест отчёта: доказательство владения, а не служебная мелочь.
fn read_manifest(root: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(root.join("report-manifest.json")).expect("манифест");
    serde_json::from_str(&text).expect("манифест — JSON")
}

/// Делает файл нечитаемым для текущего пользователя.
fn make_unreadable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).expect("файл").permissions();
    permissions.set_mode(0o000);
    std::fs::set_permissions(path, permissions).expect("права");
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
        artifact_files(out.path()),
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
        artifact_files(out.path()),
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
    let card_before = card_text(&out, "cards/card-0001.html");
    let card_after = card_text(&out, "cards/card-0002.html");
    assert!(
        !card_before.contains("src=\"only-before.png\""),
        "отсутствующая картинка не остаётся живой ссылкой: браузер показал бы битый значок вместо факта"
    );
    assert!(
        card_before.contains("class=\"report-media-missing\""),
        "на месте файла видно состояние, а не пустота"
    );
    assert!(card_before.contains("missing: only-before.png"));
    assert!(
        card_before.contains("before"),
        "чип называет состояние, в котором файла нет"
    );
    assert!(
        !card_before.contains("only-after.png"),
        "чужой файл состояния не подставляется"
    );
    assert!(
        !card_after.contains("class=\"report-media-missing\""),
        "в «после» файл на месте: чипа там быть не должно"
    );
    assert!(card_after.contains("src=\"../media/after/only-after.png\""));
    assert!(
        card_after.contains("<img"),
        "настоящая картинка остаётся элементом img"
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
  // Высота равна содержимому плюс запас против дробного округления: ровно
  // содержимое даёт кадру собственный scrollbar.
  assert.ok(
    hello.height >= 480 && hello.height <= 481,
    'высота берётся из содержимого: ' + hello.height
  );

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
  assert.ok(
    height.height >= 900 && height.height <= 901,
    'высота обновляется после загрузки media: ' + height.height
  );
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
    for path in artifact_files(out.path()) {
        if page_of(&path).is_none() {
            continue;
        }
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

/// Иерархия отчёта: сначала сводка, потом служебные данные, потом содержимое.
///
/// Проза отчёта не имеет права растягиваться на всю ширину экрана, но и сжимать
/// её вместе со сравнением нельзя: превью «до»/«после» меряется кадром карточки,
/// а не длиной строки.
#[test]
fn report_keeps_prose_readable_without_narrowing_the_comparison() {
    let before = canonical_base("report-layout-before");
    let after = canonical_base("report-layout-after");
    set_field(&after, "guid-2", 1, "переписанное толкование");

    let out = TempDir::new("report-layout-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let _ = result_of(exit, &document);
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");

    // Мера строки объявлена как отдельная величина, а не как ширина отчёта.
    assert!(
        index.contains("--report-reading-width"),
        "мера строки прозы объявлена переменной"
    );
    assert!(
        index.contains("max-width: var(--report-reading-width)"),
        "проза ограничена мерой строки, а не пределом отчёта"
    );
    // Основной блок отчёта остаётся широким: сравнение обязано помещаться целиком.
    assert!(
        index.contains(".report-main") && index.contains("max-width: none"),
        "ширина сравнения не режется мерой строки"
    );
    assert!(
        index.contains(".report-compare"),
        "сравнение состояний живёт в собственной раскладке"
    );
    // Рамку кадра несёт обёртка, а сам кадр её не имеет: при `border-box` рамка
    // на кадре вычиталась бы из высоты, которую runtime ставит по содержимому, и
    // внутри превью появлялся бы собственный scrollbar.
    let rule = |selector: &str| -> String {
        let start = index.find(selector).expect("селектор правила");
        let open = index[start..].find('{').expect("начало правила") + start;
        let close = index[open..].find('}').expect("конец правила") + open;
        index[open + 1..close].to_string()
    };
    let frame = rule(".report-preview-frame {");
    assert!(
        frame.contains("border: 1px"),
        "рамку кадра рисует обёртка: {frame}"
    );
    let preview = rule(".report-preview {");
    assert!(
        preview.contains("border: 0"),
        "у самого кадра рамки нет: {preview}"
    );
    assert!(
        !preview.contains("border: 1px"),
        "рамка на кадре вернула бы расхождение высоты: {preview}"
    );
}

/// Ограничения отчёта свёрнуты нативным `<details>` и называют свой объём.
///
/// Это не украшение: раздел ограничений описывает инструмент, а не изменение
/// колоды, поэтому он не имеет права занимать первый экран, но обязан оставаться
/// доступным без JavaScript и открываться по одному клику.
#[test]
fn report_keeps_its_limits_collapsed_and_counted() {
    let before = canonical_base("report-limits-before");
    let after = canonical_base("report-limits-after");
    // Ссылка на файл, которого нет: у отчёта появляется настоящая диагностика,
    // и она обязана остаться видимой рядом со свёрнутыми ограничениями.
    set_field(&before, "guid-2", 2, "<img src=\"нет-файла.png\">");
    set_field(&after, "guid-2", 1, "переписанное толкование");

    let out = TempDir::new("report-limits-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let _ = result_of(exit, &document);
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");

    let position = index
        .find("<details")
        .expect("ограничения — раскрывающийся блок");
    let tag_end = index[position..].find('>').expect("тег") + position;
    let summary_end = index[position..]
        .find("</summary>")
        .expect("у блока ограничений есть summary")
        + position;
    let opening = &index[position..tag_end];
    assert!(
        !opening.contains(" open"),
        "блок ограничений свёрнут по умолчанию: {opening}"
    );
    assert!(
        index[position..summary_end].contains("Ограничения этого отчёта"),
        "свёрнутая строка называет раздел"
    );
    assert!(
        index[position..summary_end].contains("пункт"),
        "свёрнутая строка называет число пунктов"
    );
    assert!(
        index.contains("<summary") && !index.contains("<summary open"),
        "раскрытие не требует JavaScript"
    );

    // Служебные разделы и неполнота превью не прячутся в свёрнутый блок: иначе
    // отчёт выглядел бы полным именно там, где он неполон.
    let details_end = index[position..]
        .find("</details>")
        .expect("конец блока ограничений")
        + position;
    let after_limits = &index[details_end..];
    assert!(
        after_limits.contains("<h2>Диагностика</h2>"),
        "диагностика видна вне свёрнутого блока ограничений"
    );
    for path in artifact_files(out.path()) {
        let text = std::fs::read_to_string(out.path().join(&path)).expect("файл отчёта");
        let Some(found) = text.find("<h2>Неподдержанные конструкции шаблонов</h2>")
        else {
            continue;
        };
        let block_end = text.find("</details>").expect("конец блока ограничений");
        assert!(
            found > block_end,
            "{path}: раздел неподдержанных конструкций обязан быть видимым"
        );
    }
}

/// Переключатель темы обязан быть доступен и в середине длинного отчёта.
///
/// Длинный отчёт не читают с одного экрана: если тема переключается только в
/// шапке, до неё нужно возвращаться прокруткой. Панель закреплена и при этом
/// остаётся единственным местом, где тема переключается.
#[test]
fn report_keeps_the_theme_control_reachable_while_scrolling() {
    let before = canonical_base("report-toolbar-before");
    let after = canonical_base("report-toolbar-after");
    set_field(&after, "guid-2", 1, "переписанное толкование");

    let out = TempDir::new("report-toolbar-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let _ = result_of(exit, &document);
    let index = std::fs::read_to_string(out.path().join("index.html")).expect("index.html");

    let toolbar = index
        .find("class=\"report-toolbar\"")
        .expect("панель управления темой");
    let control = index
        .find("class=\"report-theme\"")
        .expect("переключатель темы");
    assert!(
        control > toolbar,
        "переключатель живёт в закреплённой панели, а не в шапке"
    );
    assert_eq!(
        index.matches("class=\"report-toolbar\"").count(),
        1,
        "панель одна на документ, а не повторяется у каждого раздела"
    );
    assert!(
        index.contains("position: sticky"),
        "панель закреплена при прокрутке"
    );
    assert!(
        index.contains("top: 0"),
        "панель держится у верхней кромки окна"
    );

    // Цвета темы объявлены один раз: панель не заводит второй источник истины.
    assert_eq!(
        index.matches("data-report-theme=").count(),
        0,
        "тема хранится в атрибуте, который ставит runtime, а не в разметке панели"
    );
}

/// Значение поля — это HTML, который Anki показывает вместе с шаблоном, то есть
/// недоверенная разметка. Ни один её тег не имеет права исполниться: регистр,
/// кавычки и пробелы в записи тега ничего не меняют.
#[test]
fn data_markup_never_executes_in_any_register() {
    let before = canonical_base("report-trust-before");
    let after = canonical_base("report-trust-after");
    set_field(
        &after,
        "guid-2",
        0,
        "<SCRIPT>alert('верхний регистр')</SCRIPT>\
         <sCrIpT src=\"https://evil.example/x.js\"></ScRiPt>\
         <img src=\"b.png\" ONERROR=\"alert('обработчик')\" onLoad = \"alert('второй')\">\
         <b>законная разметка</b>",
    );
    after.write_media(&["b.png"]);

    let out = TempDir::new("report-trust-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);

    assert_every_page_is_offline(&out);

    let card = card_text(&out, "cards/card-0002.html");
    // Данные показаны текстом: читатель видит, что именно не отрисовано.
    assert_eq!(
        script_bodies(&card).len(),
        1,
        "исполняется только runtime отчёта, и он в файле один"
    );
    assert!(card.contains("&lt;SCRIPT>"), "скрипт показан текстом");
    assert!(card.contains("alert('верхний регистр')"), "видно, что было");
    assert!(
        event_handlers(&card).is_empty(),
        "обработчик события не остаётся атрибутом: {:?}",
        event_handlers(&card)
    );
    // Законная разметка не ломается: превью показывает модель, а не отчёт о ней.
    assert!(
        card.contains("<b>законная разметка</b>"),
        "обычный тег остаётся"
    );
    // Отвергнутое названо: молчание выглядело бы как «этого и не было».
    assert!(
        card.contains("не показано"),
        "превью объясняет, что не показано"
    );
    assert!(
        result["diagnostics"]
            .as_array()
            .expect("диагностика")
            .iter()
            .any(|item| item["code"] == "preview_construct_blocked"),
        "диагностика называет отвергнутую конструкцию: {result}"
    );
}

/// Внешний адрес в значении поля не превращается в запрос.
#[test]
fn data_cannot_start_an_http_request() {
    let before = canonical_base("report-remote-before");
    let after = canonical_base("report-remote-after");
    set_field(
        &after,
        "guid-2",
        0,
        "<img src=\"https://evil.example/a.png\">\
         <img src=\"//evil.example/b.png\">\
         <a href=\"https://evil.example/page\">ссылка</a>\
         <img srcset=\"https://evil.example/c.png 1x, https://evil.example/d.png 2x\">\
         <video poster=\"https://evil.example/e.png\"></video>\
         <object data=\"https://evil.example/f.svg\"></object>\
         <img src=\"нет-файла.png\">",
    );

    let out = TempDir::new("report-remote-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let _ = result_of(exit, &document);
    assert_every_page_is_offline(&out);

    let card = card_text(&out, "cards/card-0002.html");
    let addresses = address_values(&card);
    for (attribute, value) in &addresses {
        assert!(
            !value.trim().is_empty(),
            "пустой адрес — это запрос самого документа, а не «ничего»: {attribute}"
        );
        assert!(
            !value.contains("://") && !value.starts_with("//"),
            "внешний адрес не остаётся адресом: {attribute}={value}"
        );
    }
    assert!(
        addresses.is_empty(),
        "после очистки в превью не остаётся ни одного адреса: {addresses:?}"
    );
    // Ссылка на отсутствующий локальный файл не выдумывается и названа состоянием.
    assert!(
        !card.contains("media/after/нет-файла.png"),
        "отсутствующий файл не подставляется"
    );
    assert!(
        card.contains("нет-файла.png"),
        "имя отсутствующего файла видно"
    );
}

/// `srcset` — список кандидатов: негодный кандидат убирается, потому что за ним
/// браузер пошёл бы в сеть, а показанный файл остаётся картинкой, а не пропадает.
#[test]
fn a_remote_srcset_candidate_does_not_take_the_local_one_with_it() {
    let before = canonical_base("report-srcset-before");
    let after = canonical_base("report-srcset-after");
    set_field(
        &after,
        "guid-2",
        0,
        "<img srcset=\"https://evil.example/c.png 1x, b.png 2x\">",
    );
    after.write_media(&["b.png"]);
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let out = TempDir::new("report-srcset-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_every_page_is_offline(&out);

    let card = card_text(&out, "cards/card-0002.html");
    assert!(
        card.contains("srcset=\"../media/after/b.png 2x\""),
        "показанный кандидат остаётся адресом: {card}"
    );
    assert!(
        !card.contains("evil.example"),
        "внешний кандидат убран из списка, а не оставлен рядом с локальным"
    );
    // Удалённый кандидат не исчезает бесследно: он назван состоянием media.
    assert_eq!(
        result["media"]["states"][1]["remote"],
        json!(["https://evil.example/c.png"]),
        "внешняя ссылка остаётся видимой в сводке media: {result}"
    );
}

/// CSS модели приходит из экспорта и не имеет права тянуть внешнее.
#[test]
fn model_css_cannot_pull_remote_assets() {
    let before = canonical_base("report-css-before");
    let after = canonical_base("report-css-after");
    let mut export = after.deck_json();
    export["note_models"][0]["css"] = json!(
        ".card { background: url(\"https://evil.example/a.png\") no-repeat; }\
         @import url(\"https://evil.example/b.css\");\
         .card::after { content: \"</style><script>alert('выход из стиля')</script>\"; }"
    );
    after.write_canonical_deck_json(&export);
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let out = TempDir::new("report-css-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let _ = result_of(exit, &document);
    assert_every_page_is_offline(&out);

    let card = card_text(&out, "cards/card-0002.html");
    let styles = style_texts(&card);
    let css = styles.join("\n");
    assert!(!css.is_empty(), "CSS страницы превью обязан быть");
    assert!(
        !css.to_ascii_lowercase().contains("@import"),
        "внешняя таблица стилей не подключается: {css}"
    );
    assert!(
        !css.contains("evil.example"),
        "адрес из CSS не остаётся адресом: {css}"
    );
    // Оформление при этом сохраняется: убрать CSS целиком значило бы сломать превью.
    assert!(css.contains(".card"), "CSS модели остаётся подключённым");
    assert!(
        css.contains("background: none"),
        "нелокальный адрес заменён инертным значением: {css}"
    );
    assert!(
        !card.contains("<script>alert"),
        "CSS не может закрыть свой тег и начать разметку"
    );
}

/// Runtime отчёта — единственный исполняемый код в файлах отчёта, и он обязан
/// оставаться работоспособным: тема, высота кадра и звук — это и есть отчёт.
#[test]
fn report_owned_runtime_survives_the_trust_boundary() {
    let before = canonical_base("report-runtime-before");
    let after = canonical_base("report-runtime-after");
    set_field(&after, "guid-2", 0, "<img src=\"b.png\">[sound:a.mp3]");
    for dir in [&before, &after] {
        dir.write_media(&["a.mp3", "b.png"]);
    }
    set_field(&after, "guid-2", 2, "изменение ради превью");

    let out = TempDir::new("report-runtime-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_eq!(
        result["checks"]["every_generated_page_offline"],
        json!(true)
    );
    assert_every_page_is_offline(&out);

    let index = card_text(&out, "index.html");
    assert!(index.contains("data-report-runtime=\"index\""));
    assert!(index.contains("report:hello"));
    assert!(index.contains("report:height"));
    assert!(index.contains("report:theme"));
    assert!(index.contains("data-report-theme-value=\"night\""));
    // Кадр превью не изолируется атрибутом `sandbox`: без `allow-same-origin`
    // документ внутри кадра теряет доступ к своей теме и к своей же media.
    assert!(
        !index.contains("sandbox"),
        "ограничение кадра не должно ломать тему, высоту и звук"
    );

    let card = card_text(&out, "cards/card-0002.html");
    assert!(card.contains("class=\"report-side\""));
    assert!(
        card.contains("../media/after/b.png"),
        "локальная картинка работает"
    );
    assert!(card.contains("class=\"replay-button\""));
    assert!(card.contains("class=\"report-audio\""));
    assert!(
        card.contains("../media/after/a.mp3"),
        "звук своего состояния"
    );
}

/// Отсутствие media и неподдержанная конструкция видны в самом превью: отчёт,
/// который молча показывает меньше, чем должен, вводит читателя в заблуждение.
#[test]
fn blocked_and_missing_content_is_named_in_the_preview() {
    let before = canonical_base("report-honest-before");
    let after = canonical_base("report-honest-after");
    set_field(
        &after,
        "guid-2",
        0,
        "<img src=\"нет.png\"><img src=\"есть.png\">",
    );
    after.write_media(&["есть.png"]);
    set_field(&after, "guid-2", 1, "изменение ради превью");

    let out = TempDir::new("report-honest-out");
    let (exit, document) = report(before.path(), after.path(), out.path(), &[]);
    let result = result_of(exit, &document);
    assert_every_page_is_offline(&out);

    let card = card_text(&out, "cards/card-0002.html");
    assert!(
        card.contains("report-media-missing"),
        "отсутствие названо в превью"
    );
    assert!(
        card.contains("missing: нет.png"),
        "названо, чего именно нет"
    );
    assert!(
        card.contains("../media/after/есть.png"),
        "существующий файл показан"
    );

    let missing: Vec<&str> = result["diagnostics"]
        .as_array()
        .expect("диагностика")
        .iter()
        .filter(|item| item["code"] == "missing_media")
        .map(|item| item["subject"].as_str().expect("subject"))
        .collect();
    assert_eq!(
        missing,
        vec!["after:нет.png"],
        "состояние отсутствия названо"
    );
}
