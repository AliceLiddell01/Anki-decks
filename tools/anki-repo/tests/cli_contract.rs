//! Контракт CLI: аргументы, exit codes, разделение stdout/stderr, JSON envelope.

mod common;

use common::{TempDir, base_export, export_with, parse_json, run_cli, run_cli_in, words_deck};
use serde_json::json;

const DECK: &str = "decks/japanese/words/Words__N1";

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

#[test]
fn conflicting_or_incomplete_criteria_are_usage_errors() {
    let cases: [&[&str]; 7] = [
        &["find", DECK],
        &["find", DECK, "--guid", "x", "--word", "y"],
        &["find", DECK, "--field", "Слово"],
        &["find", DECK, "--value", "x"],
        &["find", DECK, "--word", "x", "--match", "exact"],
        &["find", DECK, "--word", "x", "--limit", "900"],
        &["stats", DECK, "--top", "5"],
    ];
    for args in cases {
        let (code, stdout, stderr) = run_cli(args);
        assert_eq!(code, 2, "аргументы {args:?}: stderr {stderr}");
        assert!(
            stdout.is_empty(),
            "аргументы {args:?}: stdout не должен быть пустым"
        );
    }
}

#[test]
fn inspect_works_with_relative_and_absolute_paths() {
    let root = common::repo_root();
    let (code, stdout, stderr) = run_cli_in(Some(&root), &["inspect", DECK]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Words::N1"));
    assert!(stdout.contains("Слово"));

    let absolute = words_deck(1);
    let dir = TempDir::new("cwd");
    let (code, stdout, stderr) = run_cli_in(
        Some(dir.path()),
        &["inspect", absolute.to_str().expect("utf-8")],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("Words::N1"));
}

#[test]
fn json_flag_works_before_and_after_the_subcommand() {
    for args in [
        vec!["--json", "inspect", DECK],
        vec!["inspect", DECK, "--json"],
    ] {
        let (code, stdout, stderr) = run_cli(&args);
        assert_eq!(code, 0, "аргументы {args:?}: stderr {stderr}");
        let value = parse_json(&stdout);
        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("inspect"));
        assert_eq!(value["result"]["root_deck_name"], json!("Words::N1"));
    }
}

#[test]
fn human_output_is_not_json() {
    let (code, stdout, _) = run_cli(&["inspect", DECK]);
    assert_eq!(code, 0);
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_err());
    assert!(
        stdout.ends_with('\n'),
        "вывод должен заканчиваться переводом строки"
    );
}

#[test]
fn missing_directory_is_an_input_error() {
    let (code, stdout, stderr) = run_cli(&["inspect", "decks/japanese/words/НетТакой"]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn deck_json_path_instead_of_directory_is_an_input_error() {
    let path = format!("{DECK}/deck.json");
    let (code, _, stderr) = run_cli(&["inspect", &path]);
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
    let (code, stdout, stderr) = run_cli(&["find", DECK, "--word", "ЭТОГО_НЕТ"]);
    assert_eq!(code, 4);
    assert!(stdout.is_empty());
    assert!(stderr.contains("not_found"));

    let (code, stdout, _) = run_cli(&["--json", "find", DECK, "--word", "ЭТОГО_НЕТ"]);
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

#[test]
fn validate_of_canonical_deck_exits_with_zero() {
    for level in 1..=5 {
        let path = format!("decks/japanese/words/Words__N{level}");
        let (code, stdout, stderr) = run_cli(&["validate", &path]);
        assert_eq!(code, 0, "N{level}: stderr {stderr}");
        assert!(stdout.contains("Валиден: да"), "N{level}");
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
    for args in [
        vec!["--json", "inspect", "нет-такого-каталога"],
        vec!["--json", "find", DECK, "--word", "ЭТОГО_НЕТ"],
        vec!["--json", "stats", "нет-такого-каталога"],
        vec!["--json", "validate", "нет-такого-каталога"],
    ] {
        let (code, stdout, stderr) = run_cli(&args);
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
    let (code, stdout, stderr) = run_cli(&["stats", "нет-такого-каталога"]);
    assert_eq!(code, 3);
    assert!(stdout.is_empty());
    assert!(stderr.contains("input_unreadable"));
}

#[test]
fn find_json_contains_criteria_limit_and_notes() {
    let (code, stdout, stderr) =
        run_cli(&["--json", "find", DECK, "--word", "偶然", "--limit", "1"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let result = &value["result"];
    assert_eq!(result["criteria"]["kind"], json!("field"));
    assert_eq!(result["criteria"]["field"], json!("Слово"));
    assert_eq!(result["criteria"]["match_mode"], json!("contains"));
    assert_eq!(result["criteria"]["limit"], json!(1));
    assert!(result["matched_total"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(result["returned"], json!(1));
    assert!(result["notes"][0]["fields"]["Слово"].is_string());
}

#[test]
fn stats_json_contains_group_by_distribution() {
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "stats",
        DECK,
        "--group-by",
        "Часть речи",
        "--top",
        "3",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let group = &value["result"]["group_by"];
    assert_eq!(group["field"], json!("Часть речи"));
    assert_eq!(group["truncated"], json!(true));
    assert_eq!(group["values"].as_array().map(Vec::len), Some(3));
    assert!(group["distinct_values"].as_u64().unwrap_or(0) > 3);
}

#[test]
fn inspect_verbose_json_adds_diagnostics() {
    let (code, stdout, stderr) = run_cli(&["--json", "inspect", DECK, "--verbose"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let verbose = &value["result"]["verbose"];
    assert!(verbose.is_object(), "ожидался блок verbose");
    assert!(verbose["deck_json"].is_string());
    assert_eq!(verbose["nodes"].as_array().map(Vec::len), Some(1));
    assert_eq!(verbose["guid_duplicates"], json!(0));
    assert!(verbose["model_templates"][0]["templates"][0]["name"].is_string());

    let (_, stdout, _) = run_cli(&["--json", "inspect", DECK]);
    let value = parse_json(&stdout);
    assert!(
        value["result"]["verbose"].is_null(),
        "без --verbose блока быть не должно"
    );
}

#[test]
fn commands_do_not_modify_the_export() {
    let dir = TempDir::new("cli-read-only");
    dir.write_export(&base_export());
    dir.write_media(&["a.mp3"]);
    let deck_before = std::fs::read(dir.path().join("deck.json")).expect("deck.json");
    let media_before = std::fs::read_dir(dir.path().join("media"))
        .expect("media")
        .count();
    let path = dir.path().to_str().expect("utf-8").to_string();

    for args in [
        vec!["inspect".to_string(), path.clone()],
        vec!["inspect".to_string(), path.clone(), "--verbose".to_string()],
        vec![
            "find".to_string(),
            path.clone(),
            "--word".to_string(),
            "偶然".to_string(),
        ],
        vec!["stats".to_string(), path.clone()],
        vec![
            "stats".to_string(),
            path.clone(),
            "--group-by".to_string(),
            "Значение".to_string(),
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
    for args in [
        vec!["inspect", DECK],
        vec!["--json", "inspect", DECK, "--verbose"],
        vec!["stats", DECK, "--group-by", "Часть речи"],
        vec!["--json", "validate", DECK],
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
        "Слово",
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
        "Слово",
        "--value",
        "必",
        "--match",
        "exact",
    ]);
    assert_eq!(code, 4);

    let (code, _, _) = run_cli(&["find", path, "--field", "Слово", "--value", "必"]);
    assert_eq!(code, 0, "режим по умолчанию — contains");
}

#[test]
fn unknown_field_and_deck_are_reported() {
    let (code, _, stderr) = run_cli(&["find", DECK, "--field", "НетТакого", "--value", "x"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown_field"));

    let (code, _, stderr) = run_cli(&["find", DECK, "--word", "偶然", "--deck", "Нет::Такой"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown_deck"));
}
