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

    /// Записывает `deck.json` в канонической форме, которую требует `edit`.
    pub fn write_canonical_deck_json(&self, export: &Value) {
        let bytes = anki_repo::loader::render_canonical_bytes(export)
            .expect("fixture должен сериализоваться в каноническую форму");
        self.write_deck_json_bytes(&bytes);
    }

    /// Записывает `deck.json` из готовых байтов.
    pub fn write_deck_json_bytes(&self, bytes: &[u8]) {
        fs::write(self.path.join("deck.json"), bytes).expect("deck.json должен записаться");
    }

    /// Читает текущие байты `deck.json`.
    pub fn deck_json_bytes(&self) -> Vec<u8> {
        fs::read(self.path.join("deck.json")).expect("deck.json должен читаться")
    }

    /// Читает текущий `deck.json` как JSON.
    pub fn deck_json(&self) -> Value {
        let bytes = self.deck_json_bytes();
        serde_json::from_slice(&bytes).expect("deck.json должен быть валидным JSON")
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

/// Запускает CLI с заданным stdin и возвращает (exit code, stdout, stderr).
pub fn run_cli_with_stdin_in(
    cwd: Option<&Path>,
    args: &[&str],
    stdin: &[u8],
) -> (i32, String, String) {
    use std::io::Write;
    use std::process::Stdio;

    let mut command = std::process::Command::new(cli_binary());
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().expect("anki-repo должен запускаться");
    child
        .stdin
        .as_mut()
        .expect("stdin должен быть доступен")
        .write_all(stdin)
        .expect("запрос должен записаться в stdin");
    let output = child
        .wait_with_output()
        .expect("anki-repo должен завершиться");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).expect("stdout — utf-8"),
        String::from_utf8(output.stderr).expect("stderr — utf-8"),
    )
}

/// Собирает JSON-запрос на правку из четвёрок «guid, поле, expected, replacement».
pub fn edit_request(edits: &[(&str, &str, &str, &str)]) -> Value {
    json!({
        "schema_version": 1,
        "edits": edits
            .iter()
            .enumerate()
            .map(|(position, (guid, field, expected, replacement))| json!({
                "edit_id": format!("e{position}"),
                "guid": guid,
                "field": field,
                "expected": expected,
                "replacement": replacement,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Записывает JSON-запрос рядом с экспортом.
pub fn write_request(dir: &TempDir, name: &str, request: &Value) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(
        &path,
        serde_json::to_vec_pretty(request).expect("запрос должен сериализоваться"),
    )
    .expect("запрос должен записаться");
    path
}

/// Разбивает байты на строки без завершающего перевода строки.
pub fn lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let text = std::str::from_utf8(bytes).expect("fixture — utf-8");
    text.split('\n')
        .map(|line| line.as_bytes().to_vec())
        .collect()
}

/// Проверяет, что два документа различаются ровно одной строкой.
///
/// Возвращает пару «было → стало» для этой строки.
pub fn single_line_change(source: &[u8], candidate: &[u8]) -> (String, String) {
    let left = lines(source);
    let right = lines(candidate);

    assert_eq!(
        left.len(),
        right.len(),
        "число строк должно сохраняться: {} → {}",
        left.len(),
        right.len()
    );

    let changed: Vec<usize> = (0..left.len()).filter(|i| left[*i] != right[*i]).collect();
    assert_eq!(
        changed.len(),
        1,
        "должна измениться ровно одна строка, изменились: {changed:?}"
    );

    let position = changed[0];
    (
        String::from_utf8(left[position].clone()).expect("utf-8"),
        String::from_utf8(right[position].clone()).expect("utf-8"),
    )
}
