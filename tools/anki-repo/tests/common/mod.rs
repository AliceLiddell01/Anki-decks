//! Общие helpers интеграционных тестов.
//!
//! Каждый fixture живёт в собственном временном каталоге и удаляется через
//! `Drop`, поэтому тесты можно запускать параллельно.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

/// Корень репозитория Anki-decks (на два уровня выше `tools/anki-repo`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("каталог tools")
        .parent()
        .expect("корень репозитория")
        .to_path_buf()
}

/// Путь к канонической словарной колоде N1–N5.
pub fn words_deck(level: u8) -> PathBuf {
    repo_root().join(format!("decks/japanese/words/Words__N{level}"))
}

/// Путь к собранному test-binary `anki-repo`.
pub fn cli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_anki-repo"))
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Временный каталог, удаляемый при разрушении.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Создаёт пустой временный каталог.
    pub fn new(label: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "anki-repo-test-{}-{label}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("временный каталог должен создаваться");
        Self { path }
    }

    /// Путь каталога.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Записывает `deck.json` из готового значения.
    pub fn write_export(&self, export: &Value) {
        let text = serde_json::to_string_pretty(export).expect("fixture должен сериализоваться");
        self.write_raw_deck_json(&text);
    }

    /// Записывает `deck.json` из готового текста.
    pub fn write_raw_deck_json(&self, text: &str) {
        fs::write(self.path.join("deck.json"), text).expect("deck.json должен записаться");
    }

    /// Создаёт каталог `media/` с указанными именами файлов.
    pub fn write_media(&self, names: &[&str]) {
        let media = self.path.join("media");
        fs::create_dir_all(&media).expect("media должен создаваться");
        for name in names {
            fs::write(media.join(name), b"binary")
                .unwrap_or_else(|error| panic!("файл media {name}: {error}"));
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Минимальный валидный экспорт: одна колода, одна модель, одна конфигурация.
///
/// # Panics
///
/// Паникует только при ошибке в самом fixture.
pub fn base_export() -> Value {
    json!({
        "__type__": "Deck",
        "name": "Test::Deck",
        "crowdanki_uuid": "deck-uuid-1",
        "deck_config_uuid": "cfg-1",
        "children": [],
        "media_files": ["a.mp3"],
        "note_models": [
            {
                "__type__": "NoteModel",
                "crowdanki_uuid": "model-1",
                "name": "Слова",
                "css": "",
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
                        "afmt": "{{FrontSide}}{{#Пример}}{{Пример}}{{/Пример}}"
                    }
                ]
            }
        ],
        "deck_configurations": [
            {"__type__": "DeckConfig", "crowdanki_uuid": "cfg-1", "name": "По умолчанию"}
        ],
        "notes": [
            {
                "__type__": "Note",
                "guid": "guid-1",
                "note_model_uuid": "model-1",
                "tags": ["тэг"],
                "fields": ["[sound:a.mp3]偶然", "случайность", "偶然の一致"]
            },
            {
                "__type__": "Note",
                "guid": "guid-2",
                "note_model_uuid": "model-1",
                "tags": [],
                "fields": ["必然", "неизбежность", ""]
            }
        ]
    })
}

/// Строит экспорт из базового с произвольной мутацией.
pub fn export_with(mutate: impl FnOnce(&mut Value)) -> Value {
    let mut value = base_export();
    mutate(&mut value);
    value
}

/// Запускает `anki-repo` из корня репозитория, чтобы относительные пути в
/// аргументах совпадали с реальными путями репозитория.
pub fn run_cli(args: &[&str]) -> (i32, String, String) {
    run_cli_in(Some(&repo_root()), args)
}

/// Запускает `anki-repo` в заданном рабочем каталоге.
pub fn run_cli_in(cwd: Option<&Path>, args: &[&str]) -> (i32, String, String) {
    let mut command = std::process::Command::new(cli_binary());
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().expect("anki-repo должен запускаться");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout — utf-8"),
        String::from_utf8(output.stderr).expect("stderr — utf-8"),
    )
}

/// Разбирает JSON из stdout.
pub fn parse_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("stdout не JSON: {error}\n{text}"))
}
