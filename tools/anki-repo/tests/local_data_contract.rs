//! Контракт локальных данных репозитория.
//!
//! Файл проверяет две границы, которые не принадлежат ни одной предметной
//! команде:
//!
//! * `decks/input/` — закрытая область будущего доверенного загрузчика. Ни одна
//!   команда `anki-repo` не имеет права её открыть, и отказ обязан быть
//!   объяснимым, а не «файл не найден»;
//! * `anki-repo migrate-layout` — контролируемая миграция прежней раскладки в
//!   плоский `decks/output/`, которая не должна терять байты, перезаписывать
//!   чужое назначение и повторяться идемпотентно.
//!
//! Ни один тест не знает фактического состава `decks/`: каждая фикстура
//! строится в собственном временном каталоге с синтетическим `deck.json`.

use std::fs;
use std::path::{Path, PathBuf};

use common::{TempDir, parse_json, run_cli};
use serde_json::Value;

use crate::common;

/// Тело синтетического `deck.json`: постоянное, чтобы одинаковые экспорты
/// давали один и тот же отпечаток содержимого.
const DECK_JSON_BODY: &str = r#"{"__type__":"Deck","name":"Локальная колода"}"#;

/// Репозиторий-фикстура с признаком `.git` и пустым `decks/`.
struct Repo {
    _dir: TempDir,
    root: PathBuf,
}

impl Repo {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("признак репозитория");
        fs::create_dir_all(root.join("decks")).expect("каталог decks");
        Self { _dir: dir, root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Создаёт синтетический экспорт по относительному пути.
    fn export(&self, relative: &str, payload: &[u8]) -> PathBuf {
        let directory = self.path(relative);
        fs::create_dir_all(directory.join("media")).expect("каталог экспорта");
        fs::write(directory.join("deck.json"), DECK_JSON_BODY).expect("deck.json");
        fs::write(directory.join("media").join("a.bin"), payload).expect("media");
        directory
    }
}

/// Аргументы запуска `migrate-layout` для корня репозитория.
fn migrate_args(repo: &Repo, apply: bool) -> Vec<String> {
    let mut args = vec![
        "migrate-layout".to_string(),
        "--root".to_string(),
        repo.root().display().to_string(),
    ];
    if apply {
        args.push("--apply".to_string());
    }
    args
}

/// Запускает `migrate-layout` с JSON-выводом.
fn run_migrate(repo: &Repo, apply: bool) -> (i32, Value, String) {
    let mut args = migrate_args(repo, apply);
    args.push("--json".to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_cli(&borrowed);
    let value = if stdout.trim().is_empty() {
        Value::Null
    } else {
        parse_json(&stdout)
    };
    (code, value, stderr)
}

/// Ошибка команды в JSON-конверте.
fn error_reason(value: &Value) -> (&str, &str) {
    let error = &value["error"];
    (
        error["code"].as_str().unwrap_or(""),
        error["details"]["reason"].as_str().unwrap_or(""),
    )
}

#[test]
fn migrate_layout_dry_run_is_the_default_and_changes_nothing() {
    let repo = Repo::new("local-data-dry-run");
    let payload = vec![5_u8; 2048];
    repo.export("decks/japanese/words/Words__A", &payload);

    let (code, value, stderr) = run_migrate(&repo, false);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["command"], "migrate-layout");
    assert_eq!(value["schema_version"], 1);
    let result = &value["result"];
    assert_eq!(result["apply"], false);
    assert_eq!(result["applied"], false);
    assert_eq!(result["summary"]["planned"], 1);
    assert_eq!(result["refused"], false);
    assert_eq!(
        result["output_dir"], "decks/output",
        "назначение описывается относительно корня репозитория"
    );
    assert!(
        repo.path("decks/japanese/words/Words__A/deck.json")
            .is_file(),
        "dry-run не меняет раскладку"
    );
    assert!(!repo.path("decks/output").exists());
}

#[test]
fn migrate_layout_apply_moves_exports_and_preserves_bytes() {
    let repo = Repo::new("local-data-apply");
    let payload = vec![9_u8; 3072];
    repo.export("decks/japanese/words/Words__A", &payload);
    fs::write(repo.path("decks/japanese/words/.gitkeep"), b"").expect("placeholder");

    let (code, value, stderr) = run_migrate(&repo, true);

    assert_eq!(code, 0, "stderr: {stderr}");
    let result = &value["result"];
    assert_eq!(result["applied"], true);
    assert_eq!(result["summary"]["applied"], 1);
    assert_eq!(
        fs::read(repo.path("decks/output/Words__A/media/a.bin")).expect("media"),
        payload,
        "байты перенесены без изменений"
    );
    assert!(!repo.path("decks/japanese/words/Words__A").exists());
    assert!(
        !repo.path("decks/japanese").exists(),
        "placeholder и опустевшие каталоги прежней раскладки сняты"
    );

    let (again_code, again, _) = run_migrate(&repo, true);
    assert_eq!(again_code, 0);
    assert_eq!(again["result"]["summary"]["exports"], 0);
    assert_eq!(again["result"]["applied"], false);
    assert!(
        again["result"]["diagnostics"]
            .as_array()
            .expect("диагностика")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "nothing_to_migrate"),
        "повтор объясняет, что мигрировать нечего"
    );
}

#[test]
fn migrate_layout_refuses_a_name_collision_without_moving_anything() {
    let repo = Repo::new("local-data-collision");
    repo.export("decks/words/Words__A", b"a");
    repo.export("decks/other/WORDS__a", b"b");

    let (code, value, _) = run_migrate(&repo, true);

    assert_eq!(code, 3, "отказ — отдельный код возврата, а не успех");
    assert_eq!(value["result"]["refused"], true);
    assert_eq!(value["result"]["summary"]["applied"], 0);
    assert!(
        repo.path("decks/words/Words__A/deck.json").is_file(),
        "ни один экспорт не перенесён"
    );
    assert!(!repo.path("decks/output").exists());
}

#[test]
fn migrate_layout_never_touches_the_closed_input_area() {
    let repo = Repo::new("local-data-input");
    repo.export("decks/input/Hidden", b"hidden");
    repo.export("decks/words/Words__A", b"a");

    let (code, value, _) = run_migrate(&repo, true);

    assert_eq!(code, 0);
    assert_eq!(value["result"]["summary"]["exports"], 1);
    assert!(
        repo.path("decks/input/Hidden/deck.json").is_file(),
        "закрытая область не тронута"
    );
    assert!(!repo.path("decks/output/Hidden").exists());
}

#[test]
fn every_export_directory_command_refuses_the_closed_input_area() {
    let repo = Repo::new("local-data-guard");
    let hidden = repo.export("decks/input/Hidden", b"hidden");
    let path = hidden.display().to_string();
    let report = repo.path("report").display().to_string();
    let proposals = repo.path("decks/input/Hidden/deck.json");
    let proposals = proposals.display().to_string();

    let cases: Vec<Vec<&str>> = vec![
        vec!["inspect", &path],
        vec!["stats", &path],
        vec!["validate", &path],
        vec!["qa", &path],
        vec!["models", &path],
        vec!["find", &path, "--guid", "g"],
        vec!["review", &path, "--all"],
        vec!["review-check", &path, "--proposals", &proposals],
        vec![
            "edit", &path, "--guid", "g", "--field", "f", "--set", "v", "--expect", "e",
        ],
        vec!["retire", &path, "--guid", "g", "--tag", "t"],
        vec!["create", &path, "--request", &proposals],
        vec![
            "migrate-media",
            &path,
            "--namespace",
            "n",
            "--key",
            "k",
            "--from",
            "f.png",
        ],
        vec![
            "visual-report",
            "--before",
            &path,
            "--after",
            &path,
            "--out",
            &report,
        ],
    ];

    for mut case in cases {
        case.push("--json");
        let args = case;
        let (code, stdout, stderr) = run_cli(&args);
        let command = args[0];
        assert_eq!(
            code, 3,
            "{command}: ожидался отказ границы, stderr: {stderr}"
        );
        let value = parse_json(&stdout);
        let (error_code, reason) = error_reason(&value);
        assert_eq!(error_code, "invalid_request", "{command}: {stdout}");
        assert_eq!(reason, "input_area_forbidden", "{command}: {stdout}");
        let message = value["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains("decks/input"),
            "{command}: объяснение обязано называть закрытую область: {stdout}"
        );
        assert!(
            stderr.is_empty(),
            "{command}: при --json диагностика идёт в stdout, а не в stderr: {stderr}"
        );
    }

    assert!(
        hidden.join("deck.json").is_file(),
        "отказ границы ничего не изменил"
    );
    assert!(!repo.path("report").exists(), "отчёт не создан");
}

#[test]
fn a_deck_inside_output_stays_readable() {
    let repo = Repo::new("local-data-output-readable");
    let export = repo.export("decks/output/Words__A", b"a");
    let path = export.display().to_string();

    let (code, stdout, stderr) = run_cli(&["validate", &path, "--json"]);

    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["command"], "validate");
}

#[test]
fn a_root_inside_the_closed_area_is_refused() {
    let repo = Repo::new("local-data-root-guard");
    let input = repo.path("decks/input");
    fs::create_dir_all(&input).expect("закрытая область");
    let path = input.display().to_string();

    let (code, stdout, stderr) = run_cli(&["migrate-layout", "--root", &path, "--json"]);

    assert_eq!(code, 3, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let (error_code, reason) = error_reason(&value);
    assert_eq!(error_code, "invalid_request");
    assert_eq!(reason, "input_area_forbidden");
}

#[test]
fn a_repository_without_the_legacy_area_reports_it_without_failing() {
    let repo = Repo::new("local-data-absent");
    fs::remove_dir(repo.path("decks")).expect("репозиторий без прежней области");

    let (code, value, stderr) = run_migrate(&repo, true);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["result"]["summary"]["exports"], 0);
    assert_eq!(value["result"]["applied"], false);
    assert!(
        value["result"]["diagnostics"]
            .as_array()
            .expect("диагностика")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "legacy_area_absent")
    );
}
