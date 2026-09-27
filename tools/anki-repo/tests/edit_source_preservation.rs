//! Инварианты source preservation: правка не должна задевать ничего, кроме
//! значения целевого поля.
//!
//! Эти тесты проверяют не логику статусов, а физический результат: сколько
//! строк файла изменилось, совпадает ли прирост байтов с длиной нового токена,
//! остаются ли нетронутыми посторонние символы и не остаётся ли мусора.

mod common;

use std::process::{Command, Stdio};

use common::{
    TempDir, base_export, cli_binary, edit_request, lines, run_cli_in, single_line_change,
    words_deck, write_request,
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
                    {"name": "Слово", "ord": 0},
                    {"name": "Значение", "ord": 1},
                    {"name": "Пример", "ord": 2}
                ],
                "tmpls": [
                    {
                        "__type__": "CardTemplate",
                        "name": "Карточка 1",
                        "ord": 0,
                        "qfmt": "{{Слово}}",
                        "afmt": "{{Значение}}{{#Пример}}{{Пример}}{{/Пример}}"
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
            "Значение",
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
            "Слово",
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
        ("quote", "Слово", "\"кавычки\"", "\"кавычки\"!"),
        ("quote", "Пример", "例", "例!"),
        ("newline", "Значение", "", "непусто"),
        (
            "newline",
            "Пример",
            "& < > \" ' <br><b>HTML</b>",
            "&amp; < > \" ' <br><b>HTML</b>",
        ),
        ("long", "Значение", "", "коротко"),
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
            "Значение".to_string(),
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
                "Значение",
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
            "Значение",
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
                let request = edit_request(&[(*guid, "Значение", expected, replacement)]);
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
            "Значение",
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
            "Значение",
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
            "Значение",
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

/// Копирует канонический `deck.json` реальной колоды во временный каталог.
fn real_deck_copy(level: u8) -> TempDir {
    let dir = TempDir::new(&format!("preserve-real-n{level}"));
    let source = words_deck(level).join("deck.json");
    let bytes = std::fs::read(&source).expect("реальный deck.json должен читаться");
    dir.write_deck_json_bytes(&bytes);
    dir
}

/// Первые `count` заметок колоды как тройки «guid, значение Значение».
fn sample_notes(text: &[u8], count: usize) -> Vec<(String, String)> {
    let document: Value = serde_json::from_slice(text).expect("deck.json — JSON");
    document["notes"]
        .as_array()
        .expect("notes — массив")
        .iter()
        .take(count)
        .map(|note| {
            (
                note["guid"].as_str().expect("guid").to_string(),
                note["fields"][2].as_str().expect("Значение").to_string(),
            )
        })
        .collect()
}

#[test]
fn real_word_decks_are_edited_one_line_at_a_time() {
    for level in 1..=5 {
        let canonical = std::fs::read(words_deck(level).join("deck.json"))
            .expect("реальный deck.json должен читаться");
        let dir = real_deck_copy(level);
        let before = dir.deck_json_bytes();
        assert_eq!(
            before, canonical,
            "копия совпадает с каноническим источником"
        );

        let notes = sample_notes(&before, 1);
        let (guid, value) = notes.first().expect("хотя бы одна заметка").clone();
        let replacement = format!("{value} [source-preservation]");
        let request = edit_request(&[(&guid, "Значение", &value, &replacement)]);
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
        assert_eq!(result["changed_lines"], 1, "колода N{level}: одна строка");
        assert_eq!(result["effective_edits"], 1);
        assert!(
            result["validation"]["new_error_codes"]
                .as_array()
                .expect("коды")
                .is_empty(),
            "колода N{level}: новые ERROR недопустимы"
        );
        assert!(
            result["validation"]["before"]["errors"] == 0
                && result["validation"]["after"]["errors"] == 0
        );

        let after = dir.deck_json_bytes();
        let (old_line, new_line) = single_line_change(&before, &after);
        assert!(old_line.trim_end_matches(',').ends_with('"'), "N{level}");
        assert!(
            new_line.contains("[source-preservation]\","),
            "N{level}: изменённая строка должна содержать ровно новый токен: {new_line}"
        );
        assert!(
            unchanged_positions(&before, &after).len() == lines(&before).len() - 1,
            "N{level}: все прочие строки обязаны совпадать байт в байт"
        );

        // Канонический источник в репозитории не тронут.
        assert_eq!(
            std::fs::read(words_deck(level).join("deck.json")).expect("источник"),
            canonical,
            "колода N{level}: файл репозитория не должен меняться"
        );
    }
}

#[test]
fn real_word_deck_batch_stays_surgical() {
    for level in [1, 3, 5] {
        let dir = real_deck_copy(level);
        let before = dir.deck_json_bytes();

        let count = 50;
        let notes = sample_notes(&before, count);
        assert_eq!(notes.len(), count, "N{level}: в колоде достаточно заметок");

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
                        "Значение".to_string(),
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
        assert_eq!(result["edits_total"], count);
        assert_eq!(result["effective_edits"], count, "N{level}");
        assert_eq!(result["changed_lines"], count, "N{level}: строка на правку");
        assert_eq!(result["checks"]["byte_delta_matches_token_delta"], true);

        let after = dir.deck_json_bytes();
        let left = lines(&before);
        let right = lines(&after);
        assert_eq!(
            left.len(),
            right.len(),
            "N{level}: структура файла сохранена"
        );
        assert_eq!(
            left.iter()
                .zip(right.iter())
                .filter(|(a, b)| a != b)
                .count(),
            count,
            "N{level}: изменены ровно запрошенные строки"
        );

        // Ни одна другая заметка не получила маркер.
        let document: Value = serde_json::from_slice(&after).expect("JSON");
        let marked = document["notes"]
            .as_array()
            .expect("notes")
            .iter()
            .filter(|note| {
                note["fields"][2]
                    .as_str()
                    .is_some_and(|value| value.ends_with(" [batch]"))
            })
            .count();
        assert_eq!(marked, count, "N{level}: маркер только у целевых заметок");
    }
}

#[test]
fn real_word_deck_dry_run_never_writes() {
    for level in 1..=5 {
        let canonical = std::fs::read(words_deck(level).join("deck.json"))
            .expect("реальный deck.json должен читаться");
        let dir = real_deck_copy(level);
        let before = dir.deck_json_bytes();

        let (guid, value) = sample_notes(&before, 1)
            .first()
            .expect("хотя бы одна заметка")
            .clone();
        let request = edit_request(&[(&guid, "Значение", &value, &format!("{value} [dry]"))]);
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
            assert_eq!(result["changed_lines"], 1, "N{level}");

            if extra.is_none() {
                assert_eq!(result["dry_run"], true);
                assert_eq!(result["applied"], false);
            }

            assert_eq!(
                dir.deck_json_bytes().len(),
                before.len() + if extra.is_some() { 6 } else { 0 },
                "N{level}: размер меняется только после --apply"
            );
        }

        assert_eq!(
            std::fs::read(words_deck(level).join("deck.json")).expect("источник"),
            canonical,
            "колода N{level}: файл репозитория не должен меняться"
        );
    }
}
