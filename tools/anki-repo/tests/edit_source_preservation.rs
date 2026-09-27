//! Инварианты source preservation: правка не должна задевать ничего, кроме
//! значения целевого поля.
//!
//! Эти тесты проверяют не логику статусов, а физический результат: сколько
//! строк файла изменилось, совпадает ли прирост байтов с длиной нового токена,
//! остаются ли нетронутыми посторонние символы и не остаётся ли мусора.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use common::{
    TempDir, base_export, cli_binary, edit_request, lines, run_cli_in, single_line_change,
    write_request,
};
use serde_json::{Value, json};

/// Канонический синтетический экспорт из `base_export`.
fn canonical_fixture(label: &str) -> TempDir {
    let dir = TempDir::new(label);
    dir.write_canonical_deck_json(&base_export());
    dir
}

/// Экспорт с содержимым, которое легко испортить неаккуратной сериализацией.
fn awkward_export() -> Value {
    let long = "長".repeat(300);
    let mut export = json!({
        "__type__": "Deck",
        "name": "Test::Awkward",
        "crowdanki_uuid": "deck-awkward",
        "deck_config_uuid": "cfg-awkward",
        "media_files": ["a.mp3"],
        "неизвестный_ключ_корня": {"вложенный": [1, 2, {"ещё": "значение"}]},
        "children": [
            {
                "__type__": "Deck",
                "name": "Test::Awkward::Child",
                "crowdanki_uuid": "deck-awkward-child",
                "deck_config_uuid": "cfg-awkward",
                "media_files": [],
                "children": [],
                "note_models": [],
                "deck_configurations": [],
                "notes": [
                    {
                        "__type__": "Note",
                        "guid": "child-note",
                        "note_model_uuid": "model-awkward",
                        "tags": [],
                        "fields": ["子", "ребёнок", "子の例"],
                        "неизвестный_ключ_заметки": true
                    }
                ]
            }
        ],
        "note_models": [
            {
                "__type__": "NoteModel",
                "crowdanki_uuid": "model-awkward",
                "name": "Странные значения",
                "css": ".card { font-family: \"Ю\" }",
                "flds": [
                    {"name": "Заголовок", "ord": 0},
                    {"name": "Толкование", "ord": 1},
                    {"name": "Пример", "ord": 2}
                ],
                "tmpls": [
                    {
                        "__type__": "CardTemplate",
                        "name": "Карточка 1",
                        "ord": 0,
                        "qfmt": "{{Заголовок}}",
                        "afmt": "{{Толкование}}{{#Пример}}{{Пример}}{{/Пример}}"
                    }
                ],
                "неизвестный_ключ_модели": "x, y, z"
            }
        ],
        "deck_configurations": [
            {"__type__": "DeckConfig", "crowdanki_uuid": "cfg-awkward", "name": "По умолчанию"}
        ],
        "notes": [
            {
                "__type__": "Note",
                "guid": "quote",
                "note_model_uuid": "model-awkward",
                "tags": ["тэг, с запятой", "日本"],
                "fields": ["\"кавычки\"", "значение с \"кавычками\" и \\ обратным слэшем", "例"]
            },
            {
                "__type__": "Note",
                "guid": "newline",
                "note_model_uuid": "model-awkward",
                "tags": [],
                "fields": ["строка\nс переводом", "", "& < > \" ' <br><b>HTML</b>"]
            },
            {
                "__type__": "Note",
                "guid": "long",
                "note_model_uuid": "model-awkward",
                "tags": [],
                "fields": [long.clone(), "", long.clone()]
            }
        ]
    });
    export["notes"][2]["fields"][0] = Value::String(long.clone());
    export["notes"][2]["fields"][2] = Value::String(long);
    export
}

/// Разбирает `result` из успешного JSON-ответа.
fn result_of(exit: i32, stdout: &str) -> Value {
    assert_eq!(exit, 0, "ожидался успех, stdout: {stdout}");
    let document: Value = serde_json::from_str(stdout).expect("stdout — JSON");
    document["result"].clone()
}

/// Строки, совпавшие у двух документов, посегментно.
fn unchanged_positions(source: &[u8], candidate: &[u8]) -> Vec<usize> {
    let left = lines(source);
    let right = lines(candidate);
    assert_eq!(left.len(), right.len(), "число строк должно сохраняться");
    (0..left.len()).filter(|i| left[*i] == right[*i]).collect()
}

#[test]
fn a_single_edit_touches_exactly_one_line() {
    let dir = TempDir::new("preserve-single");
    dir.write_canonical_deck_json(&awkward_export());
    let before = dir.deck_json_bytes();

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "quote",
            "--field",
            "Толкование",
            "--expect",
            "значение с \"кавычками\" и \\ обратным слэшем",
            "--set",
            "значение с \"кавычками\" и \\ обратным слэшем, ещё кусок",
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["changed_lines"], 1);
    assert_eq!(result["checks"]["diff_shape_is_exactly_requested"], true);

    let after = dir.deck_json_bytes();
    let (old_line, new_line) = single_line_change(&before, &after);

    // Изменился только строковый токен значения: экранирование и отступ целы.
    assert!(old_line.contains(r#"значение с \"кавычками\" и \\ обратным слэшем"#));
    assert!(new_line.contains(", ещё кусок\","));
    assert_eq!(
        new_line.matches('"').count(),
        old_line.matches('"').count(),
        "баланс кавычек в строке значения сохраняется"
    );

    // Остальные строки совпадают байт в байт.
    let unchanged = unchanged_positions(&before, &after);
    assert_eq!(unchanged.len(), lines(&before).len() - 1);
}

#[test]
fn newlines_and_unicode_round_trip_through_the_request() {
    let dir = TempDir::new("preserve-newline");
    dir.write_canonical_deck_json(&awkward_export());
    let before = dir.deck_json_bytes();

    let replacement = "строка\nс переводом\nи 日本語 🙂 \"кавычками\"";
    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "newline",
            "--field",
            "Заголовок",
            "--expect",
            "строка\nс переводом",
            "--set",
            replacement,
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(
        result["changed_lines"], 1,
        "перевод строки остаётся внутри строки файла"
    );
    assert_eq!(result["checks"]["byte_delta_matches_token_delta"], true);
    assert_eq!(
        result["outcomes"][0]["new_sample"]
            .as_str()
            .map(|s| s.contains("\\n")),
        Some(true)
    );

    // Значение читается обратно ровно как задумано.
    assert_eq!(dir.deck_json()["notes"][1]["fields"][0], replacement);

    let (old_line, new_line) = single_line_change(&before, &dir.deck_json_bytes());
    assert!(
        !old_line.contains('\n') && !new_line.contains('\n'),
        "строка файла одна"
    );
    assert!(new_line.contains(r"\nи 日本語"));
}

#[test]
fn batch_edits_change_exactly_one_line_each() {
    let dir = TempDir::new("preserve-batch");
    dir.write_canonical_deck_json(&awkward_export());
    let before = dir.deck_json_bytes();

    let request = edit_request(&[
        ("quote", "Заголовок", "\"кавычки\"", "\"кавычки\"!"),
        ("quote", "Пример", "例", "例!"),
        ("newline", "Толкование", "", "непусто"),
        (
            "newline",
            "Пример",
            "& < > \" ' <br><b>HTML</b>",
            "&amp; < > \" ' <br><b>HTML</b>",
        ),
        ("long", "Толкование", "", "коротко"),
        ("long", "Пример", &"長".repeat(300), &"短".repeat(300)),
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
            "--apply",
        ],
    );

    let result = result_of(exit, &stdout);
    assert_eq!(result["effective_edits"], 6);
    assert_eq!(result["changed_lines"], 6);

    let after = dir.deck_json_bytes();
    let left = lines(&before);
    let right = lines(&after);
    assert_eq!(left.len(), right.len());
    assert_eq!(
        left.iter()
            .zip(right.iter())
            .filter(|(a, b)| a != b)
            .count(),
        6,
        "ровно шесть строк, ни больше ни меньше"
    );

    // Байтовый прирост совпал с суммой длин новых токенов.
    let delta = after.len() as i64 - before.len() as i64;
    assert_eq!(result["byte_delta"].as_i64(), Some(delta));
    assert_eq!(result["checks"]["byte_delta_matches_token_delta"], true);
}

#[test]
fn repeated_apply_never_rewrites_the_file() {
    let dir = canonical_fixture("preserve-idempotent");
    let args = |extra: &str| {
        let mut args = vec![
            "edit".to_string(),
            dir.path().to_str().expect("путь").to_string(),
            "--json".to_string(),
            "--guid".to_string(),
            "guid-1".to_string(),
            "--field".to_string(),
            "Толкование".to_string(),
            "--expect".to_string(),
            "случайность".to_string(),
            "--set".to_string(),
            "случайность!".to_string(),
        ];
        if !extra.is_empty() {
            args.push(extra.to_string());
        }
        args
    };

    let apply = args("--apply");
    let (exit, stdout, _) = run_cli_in(None, &apply.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(result_of(exit, &stdout)["applied"], true);
    let settled = dir.deck_json_bytes();

    // Ни dry-run, ни повторный --apply не меняют байты.
    for extra in ["", "--apply"] {
        let attempt = args(extra);
        let (exit, stdout, _) = run_cli_in(
            None,
            &attempt.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let result = result_of(exit, &stdout);
        assert_eq!(result["applied"], false);
        assert_eq!(result["changed_lines"], 0);
        assert_eq!(dir.deck_json_bytes(), settled, "файл не переписывался");
    }

    // Даже mtime не должен меняться: записи не было.
    let modified = std::fs::metadata(dir.path().join("deck.json"))
        .expect("metadata")
        .modified()
        .expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let attempt = args("--apply");
    let (exit, _, _) = run_cli_in(
        None,
        &attempt.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(exit, 0);
    assert_eq!(
        std::fs::metadata(dir.path().join("deck.json"))
            .expect("metadata")
            .modified()
            .expect("mtime"),
        modified,
        "идемпотентный повтор не должен перезаписывать файл"
    );
}

#[test]
fn non_canonical_variants_are_rejected_without_writing() {
    let canonical = {
        let value = awkward_export();
        anki_repo::loader::render_canonical_bytes(&value).expect("каноническая форма")
    };
    let text = String::from_utf8(canonical.clone()).expect("utf-8");
    let compact = serde_json::to_string(&awkward_export()).expect("compact");

    // BOM — уже не JSON для serde, поэтому у него собственный код.
    let variants: Vec<(&str, Vec<u8>, &str)> = vec![
        ("compact", compact.into_bytes(), "source_not_canonical"),
        (
            "two-space",
            serde_json::to_vec_pretty(&awkward_export()).expect("pretty"),
            "source_not_canonical",
        ),
        (
            "trailing-newline",
            [canonical.clone(), b"\n".to_vec()].concat(),
            "source_not_canonical",
        ),
        (
            "crlf",
            text.replace('\n', "\r\n").into_bytes(),
            "source_not_canonical",
        ),
        (
            "bom",
            [b"\xef\xbb\xbf".to_vec(), canonical.clone()].concat(),
            "invalid_json",
        ),
    ];

    for (label, bytes, expected_code) in variants {
        let dir = TempDir::new(&format!("preserve-{label}"));
        dir.write_deck_json_bytes(&bytes);

        let (exit, stdout, _) = run_cli_in(
            None,
            &[
                "edit",
                dir.path().to_str().expect("путь"),
                "--json",
                "--guid",
                "quote",
                "--field",
                "Толкование",
                "--expect",
                "значение с \"кавычками\" и \\ обратным слэшем",
                "--set",
                "другое",
                "--apply",
            ],
        );

        assert_ne!(exit, 0, "вариант {label} не должен приниматься");
        let document: Value = serde_json::from_str(&stdout).expect("stdout — JSON");
        assert_eq!(
            document["error"]["code"], expected_code,
            "вариант {label} должен отвергаться"
        );
        assert_eq!(
            dir.deck_json_bytes(),
            bytes,
            "вариант {label}: файл не должен меняться"
        );
    }
}

#[test]
fn broken_utf8_is_rejected_as_invalid_json() {
    let dir = TempDir::new("preserve-broken-utf8");
    dir.write_deck_json_bytes(&[0xff, 0xfe, 0x00]);

    let (exit, stdout, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--json",
            "--guid",
            "quote",
            "--field",
            "Толкование",
            "--expect",
            "x",
            "--set",
            "y",
        ],
    );

    let document: Value = serde_json::from_str(&stdout).expect("stdout — JSON");
    assert_eq!(document["error"]["code"], "invalid_json");
    assert_ne!(exit, 0);
}

/// Параллельные `edit --apply` из одного snapshot не теряют результат и не
/// повреждают экспорт.
///
/// Каждый процесс читает `deck.json` целиком, вычисляет кандидат и публикует его
/// целиком — под эксклюзивной блокировкой целевого файла. Независимо от того, кто
/// именно выиграл гонку, инвариант один:
///
/// * процесс с exit code `0` применил свою правку;
/// * процесс с exit code `7` получил контролируемый `source_changed` и не оставил
///   следов;
/// * в любом случае итог — валидный канонический экспорт, а не «смесь» кандидатов
///   и не потеря обеих правок.
///
/// Тест намеренно не фиксирует победителя: он зависит от порядка захвата
/// блокировки, который задаёт ядро, а не тест. Проверяется инвариант, который
/// обязан держаться при любом порядке.
///
/// Детерминированная регрессия на само окно между проверкой и публикацией живёт
/// на уровне write-layer:
/// `write::tests::concurrent_publication_publishes_one_candidate_and_reports_conflict`.
#[test]
fn parallel_applies_never_corrupt_or_lose_the_export() {
    let dir = canonical_fixture("preserve-parallel-apply");
    let snapshot = dir.deck_json_bytes();

    // Каждая правка — в своём поле, поэтому «оба применились» тоже допустимо.
    let plans: [(&str, &str, &str, &str, usize, &str); 2] = [
        (
            "a.json",
            "guid-1",
            "случайность",
            "случайность A",
            0,
            "случайность",
        ),
        (
            "b.json",
            "guid-2",
            "неизбежность",
            "неизбежность B",
            1,
            "неизбежность",
        ),
    ];

    let jobs: Vec<(std::path::PathBuf, usize, &str)> = plans
        .iter()
        .map(
            |(name, guid, expected, replacement, note_index, original)| {
                let request = edit_request(&[(*guid, "Толкование", expected, replacement)]);
                (write_request(&dir, name, &request), *note_index, *original)
            },
        )
        .collect();

    let children: Vec<std::process::Child> = jobs
        .iter()
        .map(|(request, _, _)| {
            let mut command = Command::new(cli_binary());
            command.args([
                "edit",
                dir.path().to_str().expect("путь"),
                "--json",
                "--request",
                request.to_str().expect("путь"),
                "--apply",
            ]);
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            command.spawn().expect("anki-repo должен запускаться")
        })
        .collect();

    // Итог каждого процесса: применился он или получил контролируемый конфликт.
    let mut applied = Vec::new();
    for (index, child) in children.into_iter().enumerate() {
        let output = child
            .wait_with_output()
            .expect("процесс должен завершиться");
        let stdout = String::from_utf8(output.stdout).expect("stdout — utf-8");
        let stderr = String::from_utf8(output.stderr).expect("stderr — utf-8");
        let code = output.status.code();
        let document: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("правка #{index}: stdout не JSON ({error}): {stdout}"));

        match code {
            Some(0) => {
                assert_eq!(document["result"]["applied"], true, "правка #{index}");
                applied.push(index);
            }
            Some(7) => {
                assert_eq!(
                    document["error"]["code"], "source_changed",
                    "правка #{index}: устаревший исходник — контролируемый конфликт"
                );
            }
            other => panic!(
                "правка #{index} завершилась непредвиденным кодом {other:?}, \
                 stdout: {stdout}, stderr: {stderr}"
            ),
        }
    }

    assert!(
        !applied.is_empty(),
        "хотя бы одна правка должна примениться"
    );

    // Файл остаётся валидным каноническим экспортом.
    let after = dir.deck_json_bytes();
    assert_ne!(after, snapshot, "файл должен измениться");
    let document: Value = serde_json::from_slice(&after).expect("итог — валидный JSON");
    assert_eq!(
        anki_repo::loader::render_canonical_bytes(&document).expect("канонизация"),
        after,
        "итог обязан оставаться в канонической форме"
    );

    // Состояние каждого поля объясняется исходом его процесса: успех — новое
    // значение, конфликт — исходное, без следов отвергнутого кандидата.
    for (index, (_, _, _, replacement, note_index, original)) in plans.iter().enumerate() {
        let value = document["notes"][*note_index]["fields"][1]
            .as_str()
            .expect("значение поля");
        if applied.contains(&index) {
            assert_eq!(value, *replacement, "правка #{index} применилась");
        } else {
            assert_eq!(value, *original, "правка #{index} не оставила следов");
            assert_ne!(
                value, *replacement,
                "отвергнутый кандидат не должен попасть в файл"
            );
        }
    }

    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("каталог")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(entries.is_empty(), "остались временные файлы: {entries:?}");
}

#[test]
fn no_temporary_files_are_left_behind() {
    let dir = canonical_fixture("preserve-temp-files");

    for args in [vec!["--apply"], vec![], vec!["--apply"]] {
        let mut full = vec![
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "Толкование",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
        ];
        full.extend(args);
        let (exit, _, _) = run_cli_in(None, &full);
        assert_eq!(exit, 0);
    }

    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("каталог")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    assert_eq!(entries, ["deck.json"], "лишних файлов быть не должно");
}

#[cfg(unix)]
#[test]
fn atomic_replacement_keeps_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = canonical_fixture("preserve-permissions");
    let path = dir.path().join("deck.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");

    let (exit, _, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "Толкование",
            "--expect",
            "случайность",
            "--set",
            "случайность!",
            "--apply",
        ],
    );

    assert_eq!(exit, 0);
    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640, "права доступа к deck.json должны сохраниться");
}

#[test]
fn failed_conflict_leaves_the_export_directory_untouched() {
    let dir = canonical_fixture("preserve-conflict");
    let before = dir.deck_json_bytes();
    let listed: Vec<String> = std::fs::read_dir(dir.path())
        .expect("каталог")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let (exit, _, _) = run_cli_in(
        None,
        &[
            "edit",
            dir.path().to_str().expect("путь"),
            "--guid",
            "guid-1",
            "--field",
            "Толкование",
            "--expect",
            "совсем другое",
            "--set",
            "новое",
            "--apply",
        ],
    );

    assert_eq!(exit, 7);
    assert_eq!(dir.deck_json_bytes(), before);
    let after: Vec<String> = std::fs::read_dir(dir.path())
        .expect("каталог")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(after, listed);
}

// --- Локальная синтетическая колода вместо колод репозитория ---------------
//
// Тесты ниже раньше работали на реальных колодах `Words__N1..N5`. Это делало
// default test suite заложником содержимого репозитория: состав колод, уровни JLPT,
// note models и media могли измениться или временно отсутствовать, а тест
// продолжал бы требовать их наличия. Поэтому колоду целиком строит сам тест: он
// больше не знает ни про layout репозитория, ни про имена полей реальных колод, а
// правит только собственные байты в своём временном каталоге.

/// Поле синтетической колоды, значение которого правят тесты ниже.
const RICH_FIELD: &str = "Разбор";

/// Заметки с разными формами значения — именно они ломают наивную пересборку JSON.
///
/// Перечень фиксирован: каждый `guid` обязан существовать в [`rich_export`], иначе
/// тест упадёт с понятным сообщением, а не молча сузит покрытие.
const RICH_TARGETS: [&str; 5] = [
    "rich-plain",
    "rich-html",
    "rich-sound",
    "rich-escapes",
    "rich-long",
];

/// Сколько «массовых» заметок основной модели строит фикстура.
///
/// Их должно хватать на несколько независимых пакетных прогонов по 50 правок, а
/// не ровно на один: пакетный тест идёт по разным окнам выборки, как раньше он шёл
/// по разным реальным колодам.
const RICH_BULK_NOTES: usize = 160;

/// Сколько заметок лежит во вложенной колоде на второй (однополевой) модели.
const RICH_CHILD_NOTES: usize = 3;

/// Синтетическая колода, заменяющая реальные колоды репозитория.
///
/// Форма намеренно неудобная, чтобы тест не мог пройти случайно:
///
/// * у основной модели шесть полей, а `flds` объявлены не в порядке `ord`:
///   позиция значения в `fields` не совпадает с порядком объявления полей, и
///   поле приходится разрешать через `note_model_uuid` + `flds`, как это делает
///   сама команда `edit`;
/// * в значениях живут HTML, `[sound:...]`, кавычки, обратный слэш и не-ASCII;
/// * у корня, заметок и модели есть лишние неизвестные ключи, которые
///   канонический проход обязан сохранить;
/// * `media_files` ссылается на физически существующие файлы `media/`, которые
///   правка поля не имеет права трогать;
/// * вложенная колода использует вторую модель с единственным полем, то есть
///   заметки одного экспорта заведомо неоднородны.
fn rich_export() -> Value {
    let mut notes: Vec<Value> = vec![
        json!({
            "__type__": "Note",
            "guid": "rich-plain",
            "note_model_uuid": "model-rich",
            "tags": [],
            "fields": ["山", "существительное", "гора", "1", "山の例", ""],
            "неизвестный_ключ_заметки": {"порядок": 0}
        }),
        json!({
            "__type__": "Note",
            "guid": "rich-html",
            "note_model_uuid": "model-rich",
            "tags": ["html"],
            "fields": [
                "<b>川</b>",
                "существительное",
                "река &amp; вода",
                "1",
                "<br><i>川</i>の例 & < > \" '",
                ""
            ],
            "неизвестный_ключ_заметки": "html"
        }),
        json!({
            "__type__": "Note",
            "guid": "rich-sound",
            "note_model_uuid": "model-rich",
            "tags": ["звук"],
            "fields": [
                "[sound:rich-a.mp3]空",
                "существительное",
                "небо",
                "1",
                "[sound:rich-a.mp3]空の例",
                ""
            ]
        }),
        json!({
            "__type__": "Note",
            "guid": "rich-escapes",
            "note_model_uuid": "model-rich",
            "tags": ["кавычки, с запятой", "日本"],
            "fields": [
                "\"кавычки\"",
                "наречие",
                "значение с \"кавычками\" и \\ обратным слэшем",
                "2",
                "\\",
                ""
            ]
        }),
        json!({
            "__type__": "Note",
            "guid": "rich-long",
            "note_model_uuid": "model-rich",
            "tags": [],
            "fields": ["長", "существительное", "長".repeat(300), "3", "長".repeat(64), ""]
        }),
    ];

    for index in 0..RICH_BULK_NOTES {
        notes.push(json!({
            "__type__": "Note",
            "guid": format!("rich-root-{index:03}"),
            "note_model_uuid": "model-rich",
            "tags": ["пакет"],
            "fields": [
                format!("[sound:rich-b.png]語{index:03}"),
                "существительное",
                format!("значение {index:03}"),
                (index % 5).to_string(),
                format!("пример <b>{index:03}</b> & < >"),
                ""
            ]
        }));
    }

    let child_notes: Vec<Value> = (0..RICH_CHILD_NOTES)
        .map(|index| {
            json!({
                "__type__": "Note",
                "guid": format!("rich-child-{index}"),
                "note_model_uuid": "model-mini",
                "tags": [],
                "fields": [format!("дочернее значение {index}")]
            })
        })
        .collect();

    json!({
        "__type__": "Deck",
        "name": "Синтетика::Слова",
        "crowdanki_uuid": "deck-rich-root",
        "deck_config_uuid": "cfg-rich",
        "media_files": ["rich-a.mp3", "rich-b.png"],
        "неизвестный_ключ_корня": {"вложенный": [1, 2, {"ещё": "значение"}]},
        "children": [
            {
                "__type__": "Deck",
                "name": "Синтетика::Слова::Дочерняя",
                "crowdanki_uuid": "deck-rich-child",
                "deck_config_uuid": "cfg-rich",
                "children": [],
                "media_files": [],
                "note_models": [],
                "deck_configurations": [],
                "notes": child_notes
            }
        ],
        "note_models": [
            {
                "__type__": "NoteModel",
                "crowdanki_uuid": "model-rich",
                "name": "Синтетическая модель с шестью полями",
                "css": ".card { font-family: \"Ю\" }",
                "flds": [
                    {"name": "Разбор", "ord": 2},
                    {"name": "Иллюстрация", "ord": 4},
                    {"name": "Термин", "ord": 0},
                    {"name": "Связи", "ord": 5},
                    {"name": "Категория", "ord": 1},
                    {"name": "Метка", "ord": 3}
                ],
                "tmpls": [
                    {
                        "__type__": "CardTemplate",
                        "name": "Карточка 1",
                        "ord": 0,
                        "qfmt": "{{Термин}}",
                        "afmt": "{{FrontSide}}<hr id=answer>{{Разбор}}{{#Иллюстрация}}<br>{{Иллюстрация}}{{/Иллюстрация}}"
                    }
                ],
                "неизвестный_ключ_модели": "x, y, z"
            },
            {
                "__type__": "NoteModel",
                "crowdanki_uuid": "model-mini",
                "name": "Модель с одним полем",
                "css": "",
                "flds": [
                    {"name": "Единственное поле", "ord": 0}
                ],
                "tmpls": [
                    {
                        "__type__": "CardTemplate",
                        "name": "Карточка 1",
                        "ord": 0,
                        "qfmt": "{{Единственное поле}}",
                        "afmt": "{{FrontSide}}"
                    }
                ]
            }
        ],
        "deck_configurations": [
            {"__type__": "DeckConfig", "crowdanki_uuid": "cfg-rich", "name": "По умолчанию"}
        ],
        "notes": notes
    })
}

/// Канонический источник фикстуры: один каталог на тест, из которого берутся байты.
///
/// Источник не правится: тест работает с копией в отдельном каталоге и проверяет,
/// что сам источник остался байт в байт тем же.
fn fixture_source(label: &str) -> TempDir {
    let dir = TempDir::new(label);
    dir.write_canonical_deck_json(&rich_export());
    dir
}

/// Рабочая копия синтетической колоды в новом временном каталоге.
///
/// Байты переносятся через `fs::read` + запись, а не разделяются между тестами:
/// каждый тест правит свою копию, поэтому параллельный прогон не сталкивается на
/// общем `deck.json`. Вместе с копией материализуются media-файлы, объявленные в
/// `media_files`, — иначе было бы нечем доказать, что правка поля не трогает
/// `media/`.
fn fixture_copy(label: &str, source: &TempDir) -> TempDir {
    let dir = TempDir::new(label);
    let bytes = std::fs::read(source.path().join("deck.json")).expect("источник должен читаться");
    dir.write_deck_json_bytes(&bytes);
    dir.write_media(&["rich-a.mp3", "rich-b.png"]);
    dir
}

/// Все заметки экспорта как пары «guid, значение поля `field`».
///
/// Позиция поля разрешается так же, как в самой команде: через
/// `note_model_uuid` заметки и `note_models[].flds`, а не фиксированным индексом
/// `fields[2]`. Иначе тест читал бы и проверял не то поле, которое правит `edit`,
/// и оставался бы зелёным при переупорядочивании полей модели. Обход идёт по
/// индексу экспорта, поэтому заметки вложенных колод тоже попадают в выборку.
///
/// Заметки модели, у которой такого поля нет, пропускаются: их `edit` этим полем
/// править и не может.
fn resolved_notes(text: &[u8], field: &str) -> Vec<(String, String)> {
    let document: Value = serde_json::from_slice(text).expect("deck.json — JSON");
    let root = anki_repo::loader::typed_root(document, Path::new("deck.json"))
        .expect("типизированное ядро экспорта");
    let index = anki_repo::index::ExportIndex::build(&root);

    index
        .notes
        .iter()
        .filter_map(|entry| {
            let guid = entry.note.guid.clone().expect("guid заметки");
            let model = entry
                .note
                .note_model_uuid
                .as_deref()
                .and_then(|uuid| index.model_by_uuid(uuid))
                .unwrap_or_else(|| panic!("модель заметки {guid:?} не найдена"));
            let value = anki_repo::index::field_value_by_name(entry.note, model, field)?;
            let text = value
                .as_text()
                .unwrap_or_else(|| panic!("поле {field} заметки {guid:?} не строка"))
                .to_string();

            Some((guid, text))
        })
        .collect()
}

/// Окно заметок экспорта в том же разрешении поля: `count` штук начиная с `offset`.
///
/// Окно нужно, чтобы пакетный тест правил разные наборы заметок одной и той же
/// фикстуры, а не один и тот же префикс.
fn sample_notes(text: &[u8], field: &str, offset: usize, count: usize) -> Vec<(String, String)> {
    resolved_notes(text, field)
        .into_iter()
        .skip(offset)
        .take(count)
        .collect()
}

/// Правка ценного значения не задевает остальной файл: строк меняется ровно
/// столько, сколько запрошено, а всё прочее совпадает байт в байт.
///
/// Прогон идёт по разным формам значения (простое, HTML, `[sound:...]`, кавычки
/// и обратный слэш, длинный текст), потому что именно они ломают наивную
/// пересборку JSON. Каждая итерация правит копию канонического источника, а сам
/// источник обязан остаться неизменным.
#[test]
fn synthetic_notes_are_edited_one_line_at_a_time() {
    let source = fixture_source("preserve-rich-source");
    let canonical = std::fs::read(source.path().join("deck.json")).expect("источник читается");

    for (position, guid) in RICH_TARGETS.iter().enumerate() {
        let dir = fixture_copy(&format!("preserve-rich-{position}"), &source);
        let before = dir.deck_json_bytes();
        assert_eq!(
            before, canonical,
            "копия совпадает с каноническим источником байт в байт"
        );

        let (_, value) = resolved_notes(&before, RICH_FIELD)
            .into_iter()
            .find(|(note, _)| note == guid)
            .unwrap_or_else(|| panic!("в фикстуре нет заметки {guid} с полем {RICH_FIELD}"));
        let replacement = format!("{value} [source-preservation]");
        let request = edit_request(&[(guid, RICH_FIELD, &value, &replacement)]);
        let path = write_request(&dir, "request.json", &request);

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
        assert_eq!(result["changed_lines"], 1, "{guid}: одна строка");
        assert_eq!(result["effective_edits"], 1, "{guid}");
        assert_eq!(
            result["checks"]["diff_shape_is_exactly_requested"], true,
            "{guid}: байтовый diff обязан равняться ровно запрошенной правке"
        );
        assert_eq!(
            result["checks"]["byte_delta_matches_token_delta"], true,
            "{guid}: прирост байтов обязан объясняться длиной нового токена"
        );
        assert!(
            result["validation"]["new_error_codes"]
                .as_array()
                .expect("коды")
                .is_empty(),
            "{guid}: новые ERROR недопустимы"
        );
        assert!(
            result["validation"]["before"]["errors"] == 0
                && result["validation"]["after"]["errors"] == 0,
            "{guid}: синтетическая колода обязана быть валидной до и после правки"
        );

        let after = dir.deck_json_bytes();
        let (old_line, new_line) = single_line_change(&before, &after);
        assert!(
            old_line.trim_end_matches(',').ends_with('"'),
            "{guid}: исходная строка — значение поля, а не что-то ещё: {old_line}"
        );
        assert!(
            new_line.contains("[source-preservation]\","),
            "{guid}: изменённая строка должна содержать ровно новый токен: {new_line}"
        );
        assert_eq!(
            unchanged_positions(&before, &after).len(),
            lines(&before).len() - 1,
            "{guid}: все прочие строки обязаны совпадать байт в байт"
        );

        // Media правкой не затрагиваются: изменяется только значение поля.
        assert_eq!(
            std::fs::read(dir.path().join("media").join("rich-a.mp3")).expect("media читается"),
            b"binary",
            "{guid}: правка не должна трогать media/"
        );

        // Канонический источник фикстуры не тронут: правилась копия.
        assert_eq!(
            std::fs::read(source.path().join("deck.json")).expect("источник"),
            canonical,
            "{guid}: источник фикстуры не должен меняться"
        );
    }
}

/// Пакетная правка остаётся хирургической: одна изменённая строка на правку.
///
/// Прогонов несколько, и каждый берёт своё окно заметок — как раньше несколько
/// реальных колод, но на локальной фикстуре: меняются целевые заметки, а не
/// источник. Батч идёт по заметкам основной модели, а заметки вложенной колоды
/// живут на другой модели: ни одна из них не должна получить маркер, потому что
/// `edit` адресуется конкретным `guid`.
#[test]
fn synthetic_deck_batch_stays_surgical() {
    let source = fixture_source("preserve-rich-batch-source");
    let canonical = std::fs::read(source.path().join("deck.json")).expect("источник читается");

    let count = 50;
    assert_eq!(
        resolved_notes(&canonical, RICH_FIELD).len(),
        RICH_TARGETS.len() + RICH_BULK_NOTES,
        "фикстура обязана нести заявленное число заметок основной модели"
    );
    assert_eq!(
        resolved_notes(&canonical, RICH_FIELD).len()
            + resolved_notes(&canonical, "Единственное поле").len(),
        RICH_TARGETS.len() + RICH_BULK_NOTES + RICH_CHILD_NOTES,
        "фикстура состоит ровно из двух моделей: шесть полей и одно поле"
    );

    // Окна обязаны быть разными: иначе три прогона проверяли бы одни и те же
    // заметки и «широта» батча была бы мнимой.
    let mut edited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (batch, offset) in [0, count, 2 * count].into_iter().enumerate() {
        let dir = fixture_copy(&format!("preserve-rich-batch-{batch}"), &source);
        let before = dir.deck_json_bytes();
        assert_eq!(before, canonical, "копия совпадает с источником");

        let notes = sample_notes(&before, RICH_FIELD, offset, count);
        assert_eq!(
            notes.len(),
            count,
            "окно {batch}: в фикстуре хватает заметок"
        );
        for (guid, _) in &notes {
            assert!(
                edited.insert(guid.clone()),
                "окно {batch}: заметка {guid} правится повторно"
            );
        }

        let pairs: Vec<(String, String)> = notes
            .iter()
            .map(|(guid, value)| (guid.clone(), format!("{value} [batch]")))
            .collect();
        let request = {
            let edits: Vec<(String, String, String, String)> = notes
                .iter()
                .zip(pairs.iter())
                .map(|((guid, value), (_, replacement))| {
                    (
                        guid.clone(),
                        RICH_FIELD.to_string(),
                        value.clone(),
                        replacement.clone(),
                    )
                })
                .collect();
            let json_edits: Vec<Value> = edits
                .iter()
                .map(|(guid, field, expected, replacement)| {
                    json!({
                        "guid": guid,
                        "field": field,
                        "expected": expected,
                        "replacement": replacement,
                    })
                })
                .collect();
            json!({"schema_version": 1, "edits": json_edits})
        };
        let path = write_request(&dir, "request.json", &request);

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
        assert_eq!(result["edits_total"], count, "окно {batch}");
        assert_eq!(result["effective_edits"], count, "окно {batch}");
        assert_eq!(
            result["changed_lines"], count,
            "окно {batch}: строка на правку"
        );
        assert_eq!(
            result["checks"]["byte_delta_matches_token_delta"], true,
            "окно {batch}"
        );

        let after = dir.deck_json_bytes();
        let left = lines(&before);
        let right = lines(&after);
        assert_eq!(left.len(), right.len(), "окно {batch}: структура сохранена");
        assert_eq!(
            left.iter()
                .zip(right.iter())
                .filter(|(a, b)| a != b)
                .count(),
            count,
            "окно {batch}: изменены ровно запрошенные строки"
        );

        // Байтовый прирост совпал с суммой длин новых токенов.
        let delta = after.len() as i64 - before.len() as i64;
        assert_eq!(result["byte_delta"].as_i64(), Some(delta), "окно {batch}");
        assert_eq!(
            result["checks"]["byte_delta_matches_token_delta"], true,
            "окно {batch}"
        );

        // Ни одна другая заметка не получила маркер: ни заметка основной модели
        // вне окна, ни заметка вложенной колоды на однополевой модели.
        let marked = resolved_notes(&after, RICH_FIELD)
            .into_iter()
            .filter(|(_, value)| value.ends_with(" [batch]"))
            .count();
        assert_eq!(
            marked, count,
            "окно {batch}: маркер только у целевых заметок"
        );

        let other = resolved_notes(&after, "Единственное поле");
        assert_eq!(
            other.len(),
            RICH_CHILD_NOTES,
            "окно {batch}: вложенная колода сохранила свои заметки"
        );
        assert!(
            other.iter().all(|(_, value)| !value.contains("[batch]")),
            "окно {batch}: заметки другой модели не должны получать маркер: {other:?}"
        );
    }

    // Источник фикстуры не тронут ни одним из прогонов.
    assert_eq!(
        std::fs::read(source.path().join("deck.json")).expect("источник"),
        canonical,
        "источник фикстуры не должен меняться"
    );
}

/// Dry-run ничего не пишет, а `--apply` меняет файл ровно на длину нового токена.
#[test]
fn synthetic_deck_dry_run_never_writes() {
    let source = fixture_source("preserve-rich-dry-source");
    let canonical = std::fs::read(source.path().join("deck.json")).expect("источник читается");

    /// Суффикс, который `--apply` обязан добавить к значению.
    const SUFFIX: &str = " [dry]";

    for (position, guid) in RICH_TARGETS.iter().enumerate() {
        let dir = fixture_copy(&format!("preserve-rich-dry-{position}"), &source);
        let before = dir.deck_json_bytes();

        let (_, value) = resolved_notes(&before, RICH_FIELD)
            .into_iter()
            .find(|(note, _)| note == guid)
            .unwrap_or_else(|| panic!("в фикстуре нет заметки {guid} с полем {RICH_FIELD}"));
        let replacement = format!("{value}{SUFFIX}");
        let request = edit_request(&[(guid, RICH_FIELD, &value, &replacement)]);
        let path = write_request(&dir, "request.json", &request);

        for extra in [None, Some("--apply")] {
            let mut args = vec![
                "edit",
                dir.path().to_str().expect("путь"),
                "--json",
                "--request",
                path.to_str().expect("путь"),
            ];
            if let Some(extra) = extra {
                args.push(extra);
            }

            let (exit, stdout, _) = run_cli_in(None, &args);
            let result = result_of(exit, &stdout);
            assert_eq!(result["changed_lines"], 1, "{guid}");

            if extra.is_none() {
                assert_eq!(result["dry_run"], true);
                assert_eq!(result["applied"], false);
                assert_eq!(
                    dir.deck_json_bytes(),
                    before,
                    "{guid}: dry-run не пишет файл вообще"
                );
            } else {
                assert_eq!(
                    result["checks"]["byte_delta_matches_token_delta"], true,
                    "{guid}: прирост байтов обязан объясняться новым токеном"
                );
                let (_, new_line) = single_line_change(&before, &dir.deck_json_bytes());
                assert!(
                    new_line.contains(&format!("{SUFFIX}\",")),
                    "{guid}: изменена ровно строка значения: {new_line}"
                );
            }

            assert_eq!(
                dir.deck_json_bytes().len(),
                before.len() + if extra.is_some() { SUFFIX.len() } else { 0 },
                "{guid}: размер меняется только после --apply"
            );
        }

        assert_eq!(
            std::fs::read(source.path().join("deck.json")).expect("источник"),
            canonical,
            "{guid}: источник фикстуры не должен меняться"
        );
    }
}
