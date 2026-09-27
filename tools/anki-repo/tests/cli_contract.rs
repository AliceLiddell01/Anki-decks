//! Контракт CLI: аргументы, exit codes, разделение stdout/stderr, JSON envelope.
//!
//! Ни один тест этого файла не знает ни layout репозитория, ни содержимого
//! `decks/`: каждый тест строит фикстуру сам — синтетический экспорт в
//! собственном временном каталоге. Поэтому сьют остаётся корректным, когда
//! состав колод, уровни JLPT, note models и media изменятся или временно
//! отсутствуют.
//!
//! Ожидаемые количества и значения либо заданы локальной фикстурой этого файла
//! явно, либо пересчитаны независимо от CLI через `raw_*`/`raw_qa_counts`
//! helper'ы `common`. Пути в аргументах всегда абсолютны: `run_cli` запускает
//! бинарь без задания рабочего каталога.

mod common;

#[cfg(target_os = "linux")]
use std::fs::File;
use std::process::{Command, Stdio};

use common::{
    QA_CODES, TempDir, base_export, canonical_base_export, canonical_export, cli_binary,
    export_with, mixed_export, parse_json, raw_qa_counts, run_cli, run_cli_in,
};
use serde_json::{Value, json};

/// Абсолютный путь каталога фикстуры как `&str`.
fn path_str(dir: &TempDir) -> String {
    dir.path()
        .to_str()
        .expect("путь временного каталога обязан быть валидным utf-8")
        .to_string()
}

/// Сколько различных значений поля `Толкование` строит [`distribution_export`].
const DISTINCT_VALUES: usize = 5;

/// Предел distribution output, при котором распределение обязано усечься.
const DISTRIBUTION_TOP: usize = 3;

/// Синтетический экспорт с предсказуемым распределением значений поля.
///
/// Форма экспорта та же, что у [`base_export`] (одна колода, одна модель,
/// три поля), меняется только набор заметок: их ровно [`DISTINCT_VALUES`], и у
/// каждой своё значение поля `Толкование`. Так границы `--limit` и `--top`
/// проверяются на количествах, которые тест задаёт сам, а не на фактическом
/// содержимом пользовательской колоды.
fn distribution_export() -> Value {
    export_with(|value| {
        let notes: Vec<Value> = (0..DISTINCT_VALUES)
            .map(|index| {
                json!({
                    "__type__": "Note",
                    "guid": format!("guid-{index}"),
                    "note_model_uuid": "model-1",
                    "tags": [],
                    "fields": [format!("слово-{index}"), format!("значение-{index}"), ""],
                })
            })
            .collect();
        value["notes"] = Value::Array(notes);
    })
}

#[test]
fn help_and_version_succeed() {
    let (code, stdout, _) = run_cli(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("inspect"));
    assert!(stdout.contains("validate"));

    let (code, stdout, _) = run_cli(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("anki-repo"));
}

#[test]
fn missing_subcommand_is_a_usage_error() {
    let (code, stdout, stderr) = run_cli(&[]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let (code, _, stderr) = run_cli(&["нет-такой-команды"]);
    assert_eq!(code, 2);
    assert!(!stderr.is_empty());
}

/// Неполные и несовместимые критерии поиска — всегда usage-ошибка с пустым
/// stdout, независимо от того, отверг их clap или сам инструмент.
///
/// Каталог экспорта валиден: часть этих проверок выполняется уже после загрузки
/// экспорта, и фикстура не должна подменять ожидаемую usage-ошибку ошибкой
/// ввода.
#[test]
fn conflicting_or_incomplete_criteria_are_usage_errors() {
    let dir = TempDir::new("cli-usage");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let cases: [&[&str]; 7] = [
        // Критерий не задан вовсе.
        &["find", path.as_str()],
        // `--guid` и `--field` взаимно исключают друг друга.
        &[
            "find",
            path.as_str(),
            "--guid",
            "x",
            "--field",
            "Заголовок",
            "--value",
            "y",
        ],
        // `--field` без значения.
        &["find", path.as_str(), "--field", "Заголовок"],
        // `--value` без поля.
        &["find", path.as_str(), "--value", "x"],
        // `--match` осмысленен только вместе с `--field` (проверка инструмента).
        &["find", path.as_str(), "--guid", "x", "--match", "exact"],
        // `--limit` вне объявленных границ.
        &[
            "find",
            path.as_str(),
            "--field",
            "Заголовок",
            "--value",
            "x",
            "--limit",
            "900",
        ],
        // `--top` без `--group-by`.
        &["stats", path.as_str(), "--top", "5"],
    ];
    for args in cases {
        let (code, stdout, stderr) = run_cli(args);
        assert_eq!(code, 2, "аргументы {args:?}: stderr {stderr}");
        assert!(stdout.is_empty(), "аргументы {args:?}: stdout {stdout}");
    }
}

/// Ошибки, которые ловит сам clap, не становятся JSON-конвертом даже при
/// `--json`: команда ещё не определена, поэтому текст идёт в stderr. Ошибку
/// несовместимых аргументов находит сам инструмент, и она отдаётся конвертом.
#[test]
fn clap_usage_error_is_text_in_stderr_even_in_json_mode() {
    let dir = TempDir::new("cli-usage-json");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        path.as_str(),
        "--field",
        "Заголовок",
        "--value",
        "x",
        "--limit",
        "900",
    ]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(!stderr.is_empty());

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        path.as_str(),
        "--guid",
        "x",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 2);
    assert!(stderr.is_empty(), "stderr: {stderr}");
    assert_eq!(parse_json(&stdout)["error"]["code"], json!("usage"));
}

/// Один и тот же синтетический экспорт читается и по абсолютному пути из
/// произвольного рабочего каталога, и по относительному пути, разрешённому
/// относительно рабочего каталога процесса.
#[test]
fn inspect_works_with_relative_and_absolute_paths() {
    let dir = canonical_base_export("cli-paths");
    let absolute = path_str(&dir);

    let elsewhere = TempDir::new("cli-paths-cwd");
    let (code, stdout, stderr) = run_cli_in(Some(elsewhere.path()), &["inspect", &absolute]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Test::Deck"));
    assert!(stdout.contains("Заголовок"));

    let (code, stdout, stderr) = run_cli_in(Some(dir.path()), &["inspect", "."]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Test::Deck"));
}

#[test]
fn json_flag_works_before_and_after_the_subcommand() {
    let dir = TempDir::new("cli-json-flag");
    dir.write_export(&base_export());
    let path = path_str(&dir);

    for args in [
        vec!["--json", "inspect", path.as_str()],
        vec!["inspect", path.as_str(), "--json"],
    ] {
        let (code, stdout, stderr) = run_cli(&args);
        assert_eq!(code, 0, "аргументы {args:?}: stderr {stderr}");
        let value = parse_json(&stdout);
        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("inspect"));
        assert_eq!(value["result"]["root_deck_name"], json!("Test::Deck"));
    }
}

#[test]
fn human_output_is_not_json() {
    let dir = TempDir::new("cli-human");
    dir.write_export(&base_export());
    let path = path_str(&dir);

    let (code, stdout, _) = run_cli(&["inspect", &path]);
    assert_eq!(code, 0);
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_err());
    assert!(
        stdout.ends_with('\n'),
        "вывод должен заканчиваться переводом строки"
    );
}

#[test]
fn missing_directory_is_an_input_error() {
    let dir = TempDir::new("cli-missing");
    let missing = dir
        .path()
        .join("НетТакой")
        .to_str()
        .expect("utf-8")
        .to_string();

    let (code, stdout, stderr) = run_cli(&["inspect", &missing]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn deck_json_path_instead_of_directory_is_an_input_error() {
    let dir = TempDir::new("cli-deck-json-path");
    dir.write_export(&base_export());
    let deck_json = dir
        .path()
        .join("deck.json")
        .to_str()
        .expect("utf-8")
        .to_string();

    let (code, _, stderr) = run_cli(&["inspect", &deck_json]);
    assert_eq!(code, 3);
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn invalid_json_is_an_input_error_for_inspect() {
    let dir = TempDir::new("cli-invalid-json");
    dir.write_raw_deck_json("{ это не JSON");

    let (code, _, stderr) = run_cli(&["inspect", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 3);
    assert!(stderr.contains("invalid_json"));
}

#[test]
fn root_that_is_not_a_deck_is_an_input_error() {
    let dir = TempDir::new("cli-root");
    dir.write_export(&json!({"__type__": "Note"}));

    let (code, _, stderr) = run_cli(&["stats", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 3);
    assert!(stderr.contains("root_not_deck"));
}

#[test]
fn find_without_matches_exits_with_four() {
    let dir = TempDir::new("cli-not-found");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "find",
        path.as_str(),
        "--field",
        "Заголовок",
        "--value",
        "ЭТОГО_НЕТ",
    ]);
    assert_eq!(code, 4);
    assert!(stdout.is_empty());
    assert!(stderr.contains("not_found"));

    let (code, stdout, _) = run_cli(&[
        "--json",
        "find",
        path.as_str(),
        "--field",
        "Заголовок",
        "--value",
        "ЭТОГО_НЕТ",
    ]);
    assert_eq!(code, 4);
    let value = parse_json(&stdout);
    assert_eq!(value["error"]["code"], json!("not_found"));
    assert_eq!(value["schema_version"], json!(1));
}

#[test]
fn ambiguous_guid_exits_with_five() {
    let dir = TempDir::new("cli-ambiguous");
    dir.write_export(&export_with(|value| {
        value["notes"][1]["guid"] = json!("guid-1");
    }));
    dir.write_media(&["a.mp3"]);

    let (code, _, stderr) = run_cli(&[
        "find",
        dir.path().to_str().expect("utf-8"),
        "--guid",
        "guid-1",
    ]);
    assert_eq!(code, 5);
    assert!(stderr.contains("ambiguous"));
}

#[test]
fn validate_reports_invalid_export_with_exit_six() {
    let dir = TempDir::new("cli-validate-invalid");
    dir.write_export(&export_with(|value| {
        value["notes"][0]["note_model_uuid"] = json!("нет-такой");
    }));
    dir.write_media(&["a.mp3"]);

    let (code, stdout, stderr) = run_cli(&["validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6, "stderr: {stderr}");
    assert!(stdout.contains("Валиден: нет"));
    assert!(stdout.contains("note_model_unresolved"));

    let (code, stdout, _) = run_cli(&["--json", "validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6);
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["valid"], json!(false));
    assert!(value["result"]["summary"]["errors"].as_u64().unwrap_or(0) >= 1);
}

/// Каждый валидный синтетический экспорт обязан проходить `validate` с exit 0,
/// в том числе экспорт другой формы ([`mixed_export`]) и экспорт с другим
/// набором заметок ([`distribution_export`]).
#[test]
fn validate_of_canonical_synthetic_exports_exits_with_zero() {
    let cases: [(&str, Value); 3] = [
        ("base", base_export()),
        ("mixed", mixed_export()),
        ("distribution", distribution_export()),
    ];

    for (label, export) in cases {
        let dir = canonical_export(label, &export);
        let path = path_str(&dir);
        let (code, stdout, stderr) = run_cli(&["validate", &path]);
        assert_eq!(code, 0, "{label}: stderr {stderr}");
        assert!(stdout.contains("Валиден: да"), "{label}: stdout {stdout}");
    }
}

#[test]
fn invalid_json_inside_validate_is_reported_as_issue() {
    let dir = TempDir::new("cli-validate-json");
    dir.write_raw_deck_json("{");

    let (code, stdout, _) = run_cli(&["validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6);
    assert!(stdout.contains("invalid_json"));

    let (code, stdout, _) = run_cli(&["--json", "validate", dir.path().to_str().expect("utf-8")]);
    assert_eq!(code, 6);
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["summary"]["errors"], json!(1));
}

#[test]
fn json_error_goes_to_stdout_only() {
    let dir = TempDir::new("cli-json-error");
    dir.write_export(&base_export());
    let path = path_str(&dir);
    let missing = dir
        .path()
        .join("НетТакого")
        .to_str()
        .expect("utf-8")
        .to_string();

    let cases: [Vec<&str>; 4] = [
        vec!["--json", "inspect", &missing],
        vec![
            "--json",
            "find",
            &path,
            "--field",
            "Заголовок",
            "--value",
            "ЭТОГО_НЕТ",
        ],
        vec!["--json", "stats", &missing],
        vec!["--json", "validate", &missing],
    ];
    for args in &cases {
        let (code, stdout, stderr) = run_cli(args);
        assert_ne!(code, 0, "аргументы {args:?}");
        assert!(stderr.is_empty(), "аргументы {args:?}: stderr {stderr}");
        let value = parse_json(&stdout);
        assert_eq!(value["schema_version"], json!(1));
        assert!(value["error"]["code"].is_string(), "аргументы {args:?}");
        assert!(value["error"]["message"].is_string());
        assert!(value["error"]["details"].is_object());
    }
}

#[test]
fn human_error_goes_to_stderr_only() {
    let dir = TempDir::new("cli-human-error");
    let missing = dir
        .path()
        .join("НетТакого")
        .to_str()
        .expect("utf-8")
        .to_string();

    let (code, stdout, stderr) = run_cli(&["stats", &missing]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn find_json_contains_criteria_limit_and_notes() {
    let dir = TempDir::new("cli-find-json");
    dir.write_export(&distribution_export());
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Толкование",
        "--value",
        "значение-",
        "--limit",
        "1",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    assert_eq!(result["criteria"]["kind"], json!("field"));
    assert_eq!(result["criteria"]["field"], json!("Толкование"));
    assert_eq!(result["criteria"]["match_mode"], json!("contains"));
    assert_eq!(result["criteria"]["limit"], json!(1));
    // Значение критерия встречается у всех заметок фикстуры, но `--limit`
    // возвращает только первую, сохраняя полное число совпадений.
    assert_eq!(result["matched_total"], json!(DISTINCT_VALUES));
    assert_eq!(result["returned"], json!(1));
    assert_eq!(result["truncated"], json!(true));
    assert!(result["notes"][0]["fields"]["Толкование"].is_string());
}

#[test]
fn stats_json_contains_group_by_distribution() {
    let dir = TempDir::new("cli-stats-json");
    dir.write_export(&distribution_export());
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "stats",
        &path,
        "--group-by",
        "Толкование",
        "--top",
        &DISTRIBUTION_TOP.to_string(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let group = &value["result"]["group_by"];
    assert_eq!(group["field"], json!("Толкование"));
    assert_eq!(group["distinct_values"], json!(DISTINCT_VALUES));
    assert_eq!(group["truncated"], json!(true));
    assert_eq!(
        group["values"].as_array().map(Vec::len),
        Some(DISTRIBUTION_TOP)
    );
}

#[test]
fn inspect_verbose_json_adds_diagnostics() {
    let dir = TempDir::new("cli-inspect-verbose");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&["--json", "inspect", &path, "--verbose"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let verbose = &value["result"]["verbose"];
    assert!(verbose.is_object(), "ожидался блок verbose");
    assert_eq!(
        verbose["deck_json"],
        json!(dir.path().join("deck.json").to_string_lossy())
    );
    // Фикстура `base_export` — одна колода без потомков.
    assert_eq!(verbose["nodes"].as_array().map(Vec::len), Some(1));
    assert_eq!(verbose["guid_duplicates"], json!(0));
    assert_eq!(
        verbose["model_templates"][0]["templates"][0]["name"],
        json!("Карточка 1")
    );

    let (_, stdout, _) = run_cli(&["--json", "inspect", &path]);
    let value = parse_json(&stdout);
    assert!(
        value["result"]["verbose"].is_null(),
        "без --verbose блока быть не должно"
    );
}

#[test]
fn commands_do_not_modify_the_export() {
    let dir = canonical_base_export("cli-read-only");
    dir.write_media(&["a.mp3"]);
    let deck_before = std::fs::read(dir.path().join("deck.json")).expect("deck.json");
    let media_before = std::fs::read_dir(dir.path().join("media"))
        .expect("media")
        .count();
    let path = path_str(&dir);

    for args in [
        vec!["inspect".to_string(), path.clone()],
        vec!["inspect".to_string(), path.clone(), "--verbose".to_string()],
        vec![
            "find".to_string(),
            path.clone(),
            "--field".to_string(),
            "Заголовок".to_string(),
            "--value".to_string(),
            "偶然".to_string(),
        ],
        vec!["stats".to_string(), path.clone()],
        vec![
            "stats".to_string(),
            path.clone(),
            "--group-by".to_string(),
            "Толкование".to_string(),
        ],
        vec!["validate".to_string(), path.clone()],
    ] {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, stderr) = run_cli(&borrowed);
        assert_eq!(code, 0, "аргументы {args:?}: stderr {stderr}");
    }

    assert_eq!(
        deck_before,
        std::fs::read(dir.path().join("deck.json")).expect("deck.json")
    );
    assert_eq!(
        media_before,
        std::fs::read_dir(dir.path().join("media"))
            .expect("media")
            .count()
    );
    assert!(!dir.path().join("anki-repo.tmp").exists());
}

#[test]
fn repeated_runs_produce_byte_identical_output() {
    let base = TempDir::new("cli-deterministic-base");
    base.write_export(&base_export());
    base.write_media(&["a.mp3"]);
    let base_path = path_str(&base);

    let distribution = TempDir::new("cli-deterministic-distribution");
    distribution.write_export(&distribution_export());
    let distribution_path = path_str(&distribution);

    for args in [
        vec!["inspect", base_path.as_str()],
        vec!["--json", "inspect", base_path.as_str(), "--verbose"],
        vec!["stats", base_path.as_str(), "--group-by", "Толкование"],
        vec!["--json", "validate", base_path.as_str()],
        vec![
            "--json",
            "find",
            distribution_path.as_str(),
            "--field",
            "Толкование",
            "--value",
            "значение-",
        ],
    ] {
        let (code_one, first, _) = run_cli(&args);
        let (code_two, second, _) = run_cli(&args);
        assert_eq!(code_one, code_two, "аргументы {args:?}");
        assert_eq!(
            first, second,
            "аргументы {args:?}: вывод должен быть детерминированным"
        );
    }
}

#[test]
fn find_exact_match_requires_the_whole_value() {
    let dir = TempDir::new("cli-exact");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = dir.path().to_str().expect("utf-8");

    let (code, _, _) = run_cli(&[
        "find",
        path,
        "--field",
        "Заголовок",
        "--value",
        "必然",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 0);

    let (code, _, _) = run_cli(&[
        "find",
        path,
        "--field",
        "Заголовок",
        "--value",
        "必",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 4);

    let (code, _, _) = run_cli(&["find", path, "--field", "Заголовок", "--value", "必"]);
    assert_eq!(code, 0, "режим по умолчанию — contains");
}

#[test]
fn unknown_field_and_deck_are_reported() {
    let dir = TempDir::new("cli-unknown");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let path = path_str(&dir);

    let (code, _, stderr) = run_cli(&["find", &path, "--field", "НетТакого", "--value", "x"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown_field"));

    let (code, _, stderr) = run_cli(&[
        "find",
        &path,
        "--field",
        "Заголовок",
        "--value",
        "偶然",
        "--deck",
        "Нет::Такой",
    ]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown_deck"));
}

/// Закрытый читателем pipe не должен приводить ни к panic, ни к внутренней
/// ошибке: команда уже выполнилась, сохраняется её собственный exit code.
#[test]
fn closed_stdout_pipe_keeps_the_command_exit_code() {
    let dir = TempDir::new("closed-stdout");
    let long_value = "偶然".repeat(60);
    let notes: Vec<Value> = (0..500)
        .map(|index| {
            json!({
                "__type__": "Note",
                "guid": format!("guid-{index}"),
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": [long_value, "случайность", "偶然の一致"],
            })
        })
        .collect();
    let mut value = base_export();
    value["notes"] = Value::Array(notes);
    dir.write_export(&value);
    dir.write_media(&["a.mp3"]);

    // Вывод заведомо больше буфера pipe, поэтому запись не сможет завершиться
    // успешно после закрытия читающего конца.
    let export_dir = dir.path().to_string_lossy().into_owned();
    let mut child = Command::new(cli_binary())
        .args([
            "find",
            &export_dir,
            "--field",
            "Заголовок",
            "--value",
            "偶然",
            "--json",
            "--limit",
            "500",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("anki-repo должен запускаться");
    drop(child.stdout.take());

    let output = child.wait_with_output().expect("ожидание процесса");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        !stderr.contains("panicked"),
        "процесс не должен паниковать: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "читающий конец закрыт, данных в stdout быть не может"
    );
}

/// Отказ записи вывода по другой причине — это внутренняя ошибка: `ENOSPC`
/// получается записью в `/dev/full`.
#[test]
#[cfg(target_os = "linux")]
fn unwritable_stdout_is_an_internal_error() {
    let dir = TempDir::new("unwritable-stdout");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);

    let export_dir = dir.path().to_string_lossy().into_owned();
    let full = File::options()
        .write(true)
        .open("/dev/full")
        .expect("/dev/full должен открываться на запись");

    let output = Command::new(cli_binary())
        .args(["inspect", &export_dir, "--json"])
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .output()
        .expect("anki-repo должен запускаться");
    assert_eq!(
        output.status.code(),
        Some(70),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Снятый флаг `--word` не должен вернуться незаметно: там, где критерий задаётся
/// парой `--field`/`--value`, clap обязан отклонить неизвестный аргумент
/// (exit 2, только stderr), а не молча его проигнорировать.
#[test]
fn removed_word_flag_is_rejected() {
    let dir = canonical_base_export("cli-no-word");
    let path = path_str(&dir);

    let cases: [&[&str]; 2] = [
        &["find", path.as_str(), "--word", "偶然"],
        // `--all` дополнительно закрывает обязательную группу критериев
        // `review`, чтобы ошибкой был именно неизвестный аргумент.
        &["review", path.as_str(), "--all", "--word", "偶然"],
    ];
    for args in cases {
        let (code, stdout, stderr) = run_cli(args);
        assert_eq!(code, 2, "аргументы {args:?}: stderr {stderr}");
        assert!(stdout.is_empty(), "аргументы {args:?}: stdout {stdout}");
        assert!(
            stderr.contains("--word"),
            "аргументы {args:?}: stderr {stderr}"
        );
    }
}

/// Удалённое правило `duplicate_primary_field` не должно вернуться в реестр QA:
/// `qa --code` принимает только коды реестра, а неизвестный код — доменная
/// ошибка `unknown_qa_code` (exit 3), а не молчаливо пустой результат.
#[test]
fn removed_qa_rule_code_is_unknown() {
    let dir = canonical_base_export("cli-no-primary-dup");
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&["qa", &path, "--code", "duplicate_primary_field"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(stderr.contains("unknown_qa_code"), "stderr: {stderr}");

    let (code, stdout, stderr) =
        run_cli(&["--json", "qa", &path, "--code", "duplicate_primary_field"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["error"]["code"], json!("unknown_qa_code"));
    assert_eq!(
        value["error"]["details"]["unknown_codes"],
        json!(["duplicate_primary_field"])
    );
    // Реестр наружу — ровно `QA_CODES`, в порядке реестра.
    let available: Vec<Value> = QA_CODES.iter().map(|code| json!(code)).collect();
    assert_eq!(
        value["error"]["details"]["available_codes"],
        json!(available)
    );
}

/// Реестр QA состоит ровно из [`QA_CODES`]: каждый код принимается, фильтр
/// выбирает только своё правило, а число findings совпадает с независимым
/// пересчётом `raw_qa_counts` из того же сырого JSON.
#[test]
fn qa_accepts_exactly_the_registry_codes() {
    let raw = base_export();
    let dir = canonical_export("cli-qa-codes", &raw);
    let path = path_str(&dir);
    let expected = raw_qa_counts(&raw);

    for code in QA_CODES {
        let (exit, stdout, stderr) = run_cli(&["--json", "qa", &path, "--code", code]);
        assert_eq!(exit, 0, "код {code}: stderr {stderr}");
        let value = parse_json(&stdout);
        let result = &value["result"];
        assert_eq!(result["codes"], json!([code]), "код {code}");
        assert_eq!(
            result["by_code"].as_array().map(Vec::len),
            Some(1),
            "код {code}: фильтр обязан оставить одно правило"
        );
        assert_eq!(result["by_code"][0]["code"], json!(code), "код {code}");
        assert_eq!(
            result["findings_total"],
            json!(expected[code]),
            "код {code}: CLI и независимый пересчёт обязаны совпасть"
        );
    }
}

/// Читающие команды обязаны работать и на экспорте другой формы: другое число
/// узлов колоды, другое число моделей и полей. Ожидания вычисляются из самой
/// фикстуры `mixed_export()`, а не из состава `decks/`.
#[test]
fn read_commands_support_mixed_export() {
    let dir = TempDir::new("cli-mixed");
    let raw = mixed_export();
    dir.write_export(&raw);
    let path = path_str(&dir);

    let (code, stdout, stderr) = run_cli(&["--json", "inspect", &path]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    assert_eq!(result["root_deck_name"], json!("Группа"));
    assert_eq!(result["deck_nodes"], json!(3));
    assert_eq!(result["notes_total"], json!(3));
    assert_eq!(result["note_models"].as_array().map(Vec::len), Some(2));

    // Поля модели перечисляются в порядке `ord`, а не в порядке объявления `flds`.
    let models = common::raw_models(&raw);
    let mut expected: Vec<(i64, String)> = models["model-out-of-order"]
        .iter()
        .map(|(name, ord)| (*ord, name.clone()))
        .collect();
    expected.sort();
    let expected: Vec<String> = expected.into_iter().map(|(_, name)| name).collect();
    let reported: Vec<&str> = result["note_models"][0]["fields"]
        .as_array()
        .expect("поля модели")
        .iter()
        .map(|field| field["name"].as_str().expect("имя поля"))
        .collect();
    assert_eq!(reported, expected);

    // Распределение разрешается по `ord`: `Гамма` объявлена первой в `flds`,
    // но её `ord` — 2, то есть позиция 2 в `fields`.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "stats",
        &path,
        "--group-by",
        "Гамма",
        "--top",
        "5",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let group = &value["result"]["group_by"];
    assert_eq!(group["notes_with_field"], json!(2));
    assert_eq!(group["notes_without_field"], json!(1));
    assert_eq!(group["distinct_values"], json!(2));
    assert_eq!(group["truncated"], json!(false));

    let (code, stdout, stderr) = run_cli(&["--json", "validate", &path]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(parse_json(&stdout)["result"]["valid"], json!(true));

    let (code, stdout, stderr) =
        run_cli(&["--json", "qa", &path, "--code", "duplicate_note_content"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["notes_total"], json!(3));
    assert_eq!(value["result"]["findings_total"], json!(0));
}

/// Значение поля разрешается по `ord` модели, а не по позиции значения в
/// `fields`: у `model-out-of-order` значения намеренно «перепутаны»
/// относительно имён полей, поэтому ожидания берутся из сырого JSON.
#[test]
fn mixed_export_resolves_field_values_by_ord() {
    let dir = TempDir::new("cli-mixed-ord");
    let raw = mixed_export();
    dir.write_export(&raw);
    let path = path_str(&dir);

    let models = common::raw_models(&raw);
    let mut notes = Vec::new();
    common::collect_notes(&raw, &mut notes);
    let first = notes[0];

    let position = common::raw_field_position(&models, first, "Альфа");
    let expected = first["fields"][position]
        .as_str()
        .expect("значение поля — строка")
        .to_string();

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Альфа",
        "--value",
        &expected,
        "--match",
        "exact",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    // Значение `Альфа` одинаково у обеих заметок с этой моделью.
    assert_eq!(result["matched_total"], json!(2));
    assert_eq!(result["notes"][0]["guid"], json!("первая-1"));
    assert_eq!(result["notes"][0]["fields"]["Альфа"], json!(expected));

    // Того же значения под именем `Гамма` в экспорте нет: имя поля не
    // «угадывается» по позиции значения.
    let (code, _, stderr) = run_cli(&[
        "find",
        &path,
        "--field",
        "Гамма",
        "--value",
        &expected,
        "--match",
        "exact",
    ]);
    assert_eq!(code, 4, "stderr: {stderr}");

    // Позиция 2 у `Гамма` — её значение есть только у первой заметки.
    let position = common::raw_field_position(&models, first, "Гамма");
    let gamma = first["fields"][position]
        .as_str()
        .expect("значение поля — строка")
        .to_string();
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Гамма",
        "--value",
        &gamma,
        "--match",
        "exact",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["matched_total"], json!(1));
    assert_eq!(value["result"]["notes"][0]["guid"], json!("первая-1"));

    // Ограничение по колоде не мешает разрешению поля по `ord`.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Альфа",
        "--value",
        &expected,
        "--deck",
        "Группа::Вторая",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["matched_total"], json!(1));
    assert_eq!(value["result"]["notes"][0]["guid"], json!("вторая-1"));

    // Модель с единственным полем ищется в том же экспорте.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "find",
        &path,
        "--field",
        "Единственное поле",
        "--value",
        "одно поле",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["result"]["matched_total"], json!(1));
    assert_eq!(value["result"]["notes"][0]["guid"], json!("первая-2"));
}
