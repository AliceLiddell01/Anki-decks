//! Общие helpers интеграционных тестов.
//!
//! Каждый fixture живёт в собственном временном каталоге и удаляется через
//! `Drop`, поэтому тесты можно запускать параллельно.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anki_repo::selection::PRIMARY_FIELD;
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

/// Собирает документ предложений — публичный вход `review-check`.
pub fn proposals_document(proposals: &[(&str, &str, &str, &str)]) -> Value {
    json!({
        "schema_version": 1,
        "proposals": proposals
            .iter()
            .enumerate()
            .map(|(position, (guid, field, expected, replacement))| json!({
                "proposal_id": format!("p{position}"),
                "guid": guid,
                "field": field,
                "expected": expected,
                "replacement": replacement,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Записывает документ предложений рядом с экспортом.
pub fn write_proposals(dir: &TempDir, name: &str, document: &Value) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(
        &path,
        serde_json::to_vec_pretty(document).expect("документ должен сериализоваться"),
    )
    .expect("документ должен записаться");
    path
}

/// Сырой JSON канонической колоды `Words__N{level}`.
pub fn raw_json(level: u8) -> Value {
    let path = words_deck(level).join("deck.json");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Все заметки сырого экспорта в порядке обхода дерева.
///
/// Порядок совпадает с порядком `note_index` tool'а и не зависит от него: это
/// независимый источник для проверки адресов findings.
pub fn collect_notes<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(notes) = value.get("notes").and_then(Value::as_array) {
        out.extend(notes);
    }
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            collect_notes(child, out);
        }
    }
}

/// Состав моделей из сырого JSON: `crowdanki_uuid` → (имя поля → `ord`).
pub fn raw_models(value: &Value) -> BTreeMap<String, BTreeMap<String, i64>> {
    fn walk(value: &Value, out: &mut BTreeMap<String, BTreeMap<String, i64>>) {
        if let Some(models) = value.get("note_models").and_then(Value::as_array) {
            for model in models {
                let Some(uuid) = model.get("crowdanki_uuid").and_then(Value::as_str) else {
                    continue;
                };
                let mut fields = BTreeMap::new();
                if let Some(flds) = model.get("flds").and_then(Value::as_array) {
                    for field in flds {
                        let name = field
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let ord = field.get("ord").and_then(Value::as_i64).unwrap_or_default();
                        fields.insert(name.to_string(), ord);
                    }
                }
                out.insert(uuid.to_string(), fields);
            }
        }
        if let Some(children) = value.get("children").and_then(Value::as_array) {
            for child in children {
                walk(child, out);
            }
        }
    }

    let mut models = BTreeMap::new();
    walk(value, &mut models);
    models
}

/// Возвращает `ord` поля заметки так, как он задан её моделью в сыром JSON.
///
/// Позиция в `fields` не предполагается равной `ord`: она всегда разрешается
/// через `note_model_uuid`.
pub fn raw_field_ord(
    models: &BTreeMap<String, BTreeMap<String, i64>>,
    note: &Value,
    name: &str,
) -> i64 {
    let uuid = note
        .get("note_model_uuid")
        .and_then(Value::as_str)
        .expect("у заметки должен быть note_model_uuid");
    models
        .get(uuid)
        .unwrap_or_else(|| panic!("модель {uuid} не найдена в note_models"))
        .get(name)
        .unwrap_or_else(|| panic!("поле {name} не найдено в модели {uuid}"))
        .to_owned()
}

/// Позиция поля заметки в массиве `fields`, вычисленная по `ord` модели.
pub fn raw_field_position(
    models: &BTreeMap<String, BTreeMap<String, i64>>,
    note: &Value,
    name: &str,
) -> usize {
    usize::try_from(raw_field_ord(models, note, name)).expect("ord должен быть неотрицательным")
}

/// Коды QA-правил: ключи [`raw_qa_counts`].
pub const QA_CODES: [&str; 6] = [
    "empty_field_value",
    "leading_whitespace",
    "trailing_whitespace",
    "forbidden_white_span",
    "duplicate_note_content",
    "duplicate_primary_field",
];

/// Независимый пересчёт ожидаемых counts QA прямо из сырого JSON.
///
/// Это oracle теста, а не второй экземпляр правил: он сознательно написан
/// иначе (прямой обход `serde_json` без индексов экспорта), чтобы расхождение
/// с `anki-repo` было видно. Ни одно ожидаемое значение не зашито в тест —
/// они пересчитываются из текущего содержимого колоды.
///
/// Допущения, верные для канонических колод (`validate` даёт 0 ERROR):
/// значения полей — строки, `guid` непусты и уникальны, `note_model_uuid`
/// разрешается. Проверки адресуемости ниже повторяют это независимо, поэтому
/// колода с нарушением допущения не сломает oracle молча: она изменит counts,
/// и тест это увидит.
pub fn raw_qa_counts(value: &Value) -> BTreeMap<&'static str, usize> {
    let models = raw_models(value);
    let mut notes = Vec::new();
    collect_notes(value, &mut notes);

    let mut guid_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for note in &notes {
        if let Some(guid) = note
            .get("guid")
            .and_then(Value::as_str)
            .filter(|guid| !guid.is_empty())
        {
            *guid_counts.entry(guid).or_default() += 1;
        }
    }

    let addressable: Vec<&&Value> = notes
        .iter()
        .filter(|note| {
            let ok_guid = note
                .get("guid")
                .and_then(Value::as_str)
                .is_some_and(|guid| !guid.is_empty() && guid_counts[guid] == 1);
            let ok_model = note
                .get("note_model_uuid")
                .and_then(Value::as_str)
                .is_some_and(|uuid| models.contains_key(uuid));
            ok_guid && ok_model
        })
        .collect();

    let mut counts = BTreeMap::new();
    let mut empty = 0;
    let mut leading = 0;
    let mut trailing = 0;
    let mut spans = 0;

    for note in &notes {
        let Some(uuid) = note.get("note_model_uuid").and_then(Value::as_str) else {
            continue;
        };
        let Some(model) = models.get(uuid) else {
            continue;
        };
        let Some(fields) = note.get("fields").and_then(Value::as_array) else {
            continue;
        };
        for ord in model.values() {
            let Some(text) = fields
                .get(usize::try_from(*ord).expect("ord неотрицателен"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if text.is_empty() {
                empty += 1;
            }
            if text.starts_with(char::is_whitespace) {
                leading += 1;
            }
            if text.ends_with(char::is_whitespace) {
                trailing += 1;
            }
            spans += raw_white_spans(text);
        }
    }

    counts.insert("empty_field_value", empty);
    counts.insert("leading_whitespace", leading);
    counts.insert("trailing_whitespace", trailing);
    counts.insert("forbidden_white_span", spans);
    counts.insert(
        "duplicate_note_content",
        raw_content_duplicate_groups(&addressable),
    );
    counts.insert(
        "duplicate_primary_field",
        raw_primary_duplicate_groups(&addressable, &models),
    );
    counts
}

/// Группы заметок с полностью одинаковыми значениями `fields`.
fn raw_content_duplicate_groups(notes: &[&&Value]) -> usize {
    let mut groups: BTreeMap<(String, String), usize> = BTreeMap::new();
    for note in notes {
        let uuid = note
            .get("note_model_uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let fields = serde_json::to_string(note.get("fields").unwrap_or(&Value::Null))
            .expect("fields сериализуются");
        *groups.entry((uuid.to_string(), fields)).or_default() += 1;
    }
    groups.values().filter(|size| **size >= 2).count()
}

/// Группы заметок с одинаковым сырым головным полем.
fn raw_primary_duplicate_groups(
    notes: &[&&Value],
    models: &BTreeMap<String, BTreeMap<String, i64>>,
) -> usize {
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for note in notes {
        let Some(uuid) = note.get("note_model_uuid").and_then(Value::as_str) else {
            continue;
        };
        let Some(ord) = models.get(uuid).and_then(|model| model.get(PRIMARY_FIELD)) else {
            continue;
        };
        let Some(text) = note
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|fields| fields.get(usize::try_from(*ord).expect("ord неотрицателен")))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        *groups.entry(text.to_string()).or_default() += 1;
    }
    groups.values().filter(|size| **size >= 2).count()
}

/// Сколько раз в значении встречается `<span>` с белым цветом текста.
///
/// Упрощённый независимый скан: теги обходятся как текст, а `style`
/// разбирается по `;` и `:`. Этого достаточно, чтобы пересчитать ожидаемое
/// число findings правила `forbidden_white_span`, не вызывая сам toolkit.
fn raw_white_spans(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut found = 0;

    while let Some(start) = rest.find("<span") {
        let after = &rest[start..];
        let following = after[5..].chars().next();
        let is_tag = following.is_none_or(|character| {
            character.is_ascii_whitespace() || character == '>' || character == '/'
        });
        let Some(end) = after.find('>') else { break };
        let tag = &after[..=end];
        if is_tag && tag_declares_white_color(tag) {
            found += 1;
        }
        rest = &after[end + 1..];
    }

    found
}

/// Объявлен ли в теге `<span>` белый `color`.
fn tag_declares_white_color(tag: &str) -> bool {
    let mut rest = tag;
    while let Some(position) = rest.find("style") {
        rest = &rest[position + "style".len()..];
        let rest_trimmed = rest.trim_start();
        if !rest_trimmed.starts_with('=') {
            continue;
        }
        let value = rest_trimmed[1..].trim_start();
        let style = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let inner = &value[quote.len_utf8()..];
                match inner.find(quote) {
                    Some(end) => &inner[..end],
                    None => inner,
                }
            }
            _ => {
                let end = value
                    .find(|character: char| character == '>' || character.is_ascii_whitespace())
                    .unwrap_or(value.len());
                &value[..end]
            }
        };

        if style.split(';').any(|declaration| {
            let Some((property, raw)) = declaration.split_once(':') else {
                return false;
            };
            property.trim() == "color" && white_color_value(raw)
        }) {
            return true;
        }
    }
    false
}

/// Эквивалентно ли значение CSS-свойства `color` белому.
fn white_color_value(raw: &str) -> bool {
    let value = raw.trim().to_ascii_lowercase();
    let value = value.strip_suffix("!important").unwrap_or(&value).trim();
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();

    if matches!(compact.as_str(), "white" | "#fff" | "#ffffff") {
        return true;
    }
    let Some(arguments) = compact
        .strip_prefix("rgb(")
        .or_else(|| compact.strip_prefix("rgba("))
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let components: Vec<&str> = arguments.split(',').collect();
    components.len() >= 3
        && components[..3]
            .iter()
            .all(|component| *component == "255" || *component == "100%")
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
