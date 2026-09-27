//! Вывод заметок из обращения: тег вместо физического удаления.
//!
//! CrowdAnki не удаляет заметки, исчезнувшие из JSON: при импорте совпадение
//! идёт по `guid`, и отсутствие заметки в файле не является сигналом удаления.
//! Поэтому физическое удаление заметки из экспорта — это не «убрать из Anki», а
//! «потерять связь»: заметка осталась бы в коллекции, но перестала бы
//! обновляться. Вывод из обращения моделируется тегом, а прежнее содержимое
//! остаётся на месте.
//!
//! Отсюда три свойства операции:
//!
//! - меняется **только** массив `tags` указанных заметок, и это доказывается по
//!   путям JSON ([`crate::ops::structural`]), а не обещается в документации;
//! - `guid` заметки остаётся разрешимым и после операции: тег не удаляет
//!   идентичность;
//! - тег задаёт запрос, а не инструмент. Собственного «магического» тега у
//!   toolkit'а нет: какой тег означает вывод из обращения — решение колоды.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::ops::publish::{self, Candidate};
use crate::ops::source::{
    ValidationDelta, ValueNote, ValueNotePath, collect_value_notes, deck_child_paths,
    ensure_note_correspondence, internal, load_editable_source, node_mut, validation_delta,
};
use crate::ops::structural::{DEFAULT_PATH_LIMIT, changed_paths, deck_path_pointer};

/// Жёсткий максимум числа заметок в одном запросе.
pub const MAX_NOTES: usize = 20_000;
/// Жёсткий максимум размера JSON-запроса.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Предел числа заметок в отчёте.
pub const MAX_REPORTED_NOTES: usize = 50;
/// Поддерживаемая версия схемы JSON-запроса.
pub const SUPPORTED_REQUEST_SCHEMA_VERSION: u32 = 1;
/// Имя канала, читаемого вместо файла запроса.
pub const STDIN_REQUEST_SOURCE: &str = "-";

/// Одна выводимая из обращения заметка.
#[derive(Debug, Clone)]
pub struct RetireSpec {
    /// Необязательный стабильный идентификатор для отчёта.
    pub note_id: Option<String>,
    /// `guid` заметки.
    pub guid: String,
}

/// Проверенный запрос на вывод заметок из обращения.
#[derive(Debug, Clone)]
pub struct RetireRequest {
    /// Тег, которым помечается вывод из обращения.
    pub tag: String,
    /// Заметки в порядке запроса.
    pub notes: Vec<RetireSpec>,
}

/// Итог работы с одной заметкой.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireStatus {
    /// Тег дописан в `deck.json`.
    Retired,
    /// Тег дописан только в кандидата: файл не тронут.
    DryRun,
    /// Тег уже стоял у заметки.
    AlreadyRetired,
}

impl RetireStatus {
    /// Стабильное machine-readable имя статуса.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retired => "retired",
            Self::DryRun => "dry_run",
            Self::AlreadyRetired => "already_retired",
        }
    }
}

/// Отчёт по одной заметке.
#[derive(Debug, Clone)]
pub struct RetireOutcome {
    /// Позиция в запросе (с нуля).
    pub note_index: usize,
    /// Идентификатор заметки из запроса.
    pub note_id: Option<String>,
    /// `guid` заметки.
    pub guid: String,
    /// Итоговый статус.
    pub status: RetireStatus,
    /// Путь колоды заметки.
    pub deck_path: String,
    /// Позиция заметки в порядке экспорта (с нуля).
    pub note_position: usize,
    /// Теги до операции.
    pub previous_tags: Vec<String>,
    /// Теги после операции.
    pub tags: Vec<String>,
}

/// Выполненные проверки консистентности.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetireChecks {
    /// Исходник был в канонической форме.
    pub source_canonical: bool,
    /// Кандидат повторно разбирается в задуманное значение.
    pub candidate_reparsed: bool,
    /// Структурные различия — ровно дописанные теги.
    pub only_tags_appended: bool,
    /// Дописанные теги совпали с задуманными.
    pub tags_appended_verified: bool,
    /// Выведенные из обращения заметки остались разрешимыми по `guid`.
    pub retired_notes_still_resolvable: bool,
}

/// Результат `retire`.
#[derive(Debug, Clone)]
pub struct RetireResult {
    /// Каталог экспорта в том виде, в котором его получил CLI.
    pub export_dir: PathBuf,
    /// Полный путь к `deck.json`.
    pub deck_json: PathBuf,
    /// True, если запись не выполнялась.
    pub dry_run: bool,
    /// True, если `deck.json` был заменён.
    pub applied: bool,
    /// Размер исходника в байтах.
    pub source_bytes: usize,
    /// Размер кандидата в байтах.
    pub candidate_bytes: usize,
    /// Разница размеров (кандидат минус исходник).
    pub byte_delta: i64,
    /// Тег вывода из обращения.
    pub tag: String,
    /// Сколько заметок было в запросе.
    pub notes_total: usize,
    /// Сколько заметок реально помечено.
    pub notes_retired: usize,
    /// Сколько заметок уже было помечено.
    pub notes_already_retired: usize,
    /// Отчёт по заметкам (обрезан до [`MAX_REPORTED_NOTES`]).
    pub outcomes: Vec<RetireOutcome>,
    /// Был ли отчёт по заметкам обрезан.
    pub outcomes_truncated: bool,
    /// Сравнение валидации до и после.
    pub validation: ValidationDelta,
    /// Выполненные проверки консистентности.
    pub checks: RetireChecks,
}

/// Форма документа запроса на проводе.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    schema_version: u32,
    tag: String,
    notes: Vec<WireNote>,
}

/// Одна заметка на проводе.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNote {
    #[serde(default)]
    note_id: Option<String>,
    guid: String,
}

/// Разбирает JSON-запрос на вывод заметок из обращения.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`] для слишком большого, нечитаемого или
/// структурно некорректного запроса.
pub fn parse_request_bytes(raw: &[u8], label: &str) -> Result<RetireRequest, DomainError> {
    if raw.len() > MAX_REQUEST_BYTES {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "запрос {label} слишком большой: {} байт, максимум {MAX_REQUEST_BYTES}",
                raw.len()
            ),
            details! {
                "reason" => "request_too_large",
                "source" => label,
                "bytes" => raw.len(),
                "max_bytes" => MAX_REQUEST_BYTES,
            },
        ));
    }

    let file: RequestFile = serde_json::from_slice(raw).map_err(|error| {
        DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("запрос {label} не соответствует схеме: {error}"),
            details! {
                "reason" => "malformed_request",
                "source" => label,
                "message" => error.to_string(),
                "line" => error.line(),
                "column" => error.column(),
            },
        )
    })?;

    if file.schema_version != SUPPORTED_REQUEST_SCHEMA_VERSION {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "запрос {label} имеет schema_version = {}, поддерживается {SUPPORTED_REQUEST_SCHEMA_VERSION}",
                file.schema_version
            ),
            details! {
                "reason" => "unsupported_schema_version",
                "source" => label,
                "observed" => file.schema_version,
                "expected" => SUPPORTED_REQUEST_SCHEMA_VERSION,
            },
        ));
    }

    let request = RetireRequest {
        tag: file.tag,
        notes: file
            .notes
            .into_iter()
            .map(|note| RetireSpec {
                note_id: note.note_id,
                guid: note.guid,
            })
            .collect(),
    };

    validate_request(&request)?;
    Ok(request)
}

/// Проверяет запрос до любого обращения к файловой системе.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`] для пустого или слишком большого запроса,
/// некорректного тега, пустого `guid` и повторяющегося `note_id` или `guid`.
pub fn validate_request(request: &RetireRequest) -> Result<(), DomainError> {
    validate_tag(&request.tag)?;

    if request.notes.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "запрос на вывод из обращения не содержит ни одной заметки",
            details! { "reason" => "empty_request" },
        ));
    }

    if request.notes.len() > MAX_NOTES {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "в запросе {} заметок, максимум {MAX_NOTES}",
                request.notes.len()
            ),
            details! {
                "reason" => "too_many_notes",
                "notes" => request.notes.len(),
                "max_notes" => MAX_NOTES,
            },
        ));
    }

    let mut note_ids: BTreeMap<&str, usize> = BTreeMap::new();
    let mut guids: BTreeMap<&str, usize> = BTreeMap::new();

    for (position, note) in request.notes.iter().enumerate() {
        if note.guid.is_empty() {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("заметка #{position} содержит пустой guid"),
                details! {
                    "reason" => "empty_guid",
                    "note_index" => position,
                },
            ));
        }

        if let Some(previous) = guids.insert(note.guid.as_str(), position) {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!(
                    "guid {:?} заказан дважды: заметки #{previous} и #{position}",
                    note.guid
                ),
                details! {
                    "reason" => "duplicate_guid",
                    "guid" => note.guid.as_str(),
                    "first_note_index" => previous,
                    "duplicate_note_index" => position,
                },
            ));
        }

        if let Some(note_id) = note.note_id.as_deref()
            && let Some(previous) = note_ids.insert(note_id, position)
        {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("note_id {note_id:?} повторяется: заметки #{previous} и #{position}"),
                details! {
                    "reason" => "duplicate_note_id",
                    "note_id" => note_id,
                    "first_note_index" => previous,
                    "duplicate_note_index" => position,
                },
            ));
        }
    }

    Ok(())
}

/// Проверяет тег Anki: непустой, без пробельных символов и управляющих кодов.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`] для непригодного тега.
pub fn validate_tag(tag: &str) -> Result<(), DomainError> {
    let bad = tag.is_empty()
        || tag
            .chars()
            .any(|character| character.is_whitespace() || character.is_control());
    if bad {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("тег {tag:?} пуст или содержит пробельные символы"),
            details! {
                "reason" => "invalid_tag",
                "tag" => tag,
            },
        ));
    }
    Ok(())
}

/// Помечает указанные заметки тегом вывода из обращения.
///
/// # Errors
///
/// Ошибки чтения исходника, разрешения `guid`, структуры `tags`, проверки
/// кандидата и записи файла.
pub fn retire(
    export_dir: &Path,
    request: &RetireRequest,
    apply: bool,
) -> Result<RetireResult, DomainError> {
    validate_tag(&request.tag)?;
    let source = load_editable_source(export_dir)?;
    let index = ExportIndex::build(&source.root);
    let child_paths = deck_child_paths(&source.value)?;
    let value_notes = collect_value_notes(&source.value)?;
    ensure_note_correspondence(&index, &value_notes)?;

    let mut targets: Vec<Target> = Vec::with_capacity(request.notes.len());
    for (note_index, spec) in request.notes.iter().enumerate() {
        targets.push(resolve_target(
            &index,
            &value_notes,
            &child_paths,
            note_index,
            spec,
            &request.tag,
        )?);
    }

    let mut pending: Vec<usize> = Vec::new();
    let mut outcomes: Vec<RetireOutcome> = Vec::with_capacity(targets.len());
    for (position, target) in targets.iter().enumerate() {
        let status = if target.already_tagged {
            RetireStatus::AlreadyRetired
        } else {
            pending.push(position);
            if apply {
                RetireStatus::Retired
            } else {
                RetireStatus::DryRun
            }
        };
        outcomes.push(target.outcome(status, &request.tag));
    }

    let notes_retired = pending.len();
    let notes_already_retired = outcomes.len() - notes_retired;

    let (candidate, applied, validation) = if notes_retired == 0 {
        (
            None,
            false,
            validation_delta(&source.before, &source.before),
        )
    } else {
        let mut candidate_value = source.value.clone();
        for position in &pending {
            append_tag(&mut candidate_value, &targets[*position], &request.tag)?;
        }

        verify_only_tags_appended(
            &source.value,
            &candidate_value,
            &targets,
            &pending,
            &request.tag,
        )?;
        let candidate = publish::prepare(&source, export_dir, candidate_value)?;
        ensure_still_resolvable(&candidate, &targets, &pending, &request.tag)?;
        let publication = publish::publish(&source, &candidate, apply)?;
        let validation = candidate.validation.clone();
        (Some(candidate), publication.applied, validation)
    };

    let candidate_bytes = candidate
        .as_ref()
        .map_or(source.source.len(), |candidate| candidate.bytes.len());

    Ok(RetireResult {
        export_dir: export_dir.to_path_buf(),
        deck_json: source.deck_json.clone(),
        dry_run: !apply,
        applied,
        source_bytes: source.source.len(),
        candidate_bytes,
        byte_delta: candidate_bytes as i64 - source.source.len() as i64,
        tag: request.tag.clone(),
        notes_total: request.notes.len(),
        notes_retired,
        notes_already_retired,
        outcomes_truncated: outcomes.len() > MAX_REPORTED_NOTES,
        outcomes: outcomes.into_iter().take(MAX_REPORTED_NOTES).collect(),
        validation,
        checks: RetireChecks {
            source_canonical: true,
            candidate_reparsed: candidate.is_some(),
            only_tags_appended: candidate.is_some(),
            tags_appended_verified: candidate.is_some(),
            retired_notes_still_resolvable: candidate.is_some(),
        },
    })
}

/// Разрешённая цель вывода из обращения.
#[derive(Debug, Clone)]
struct Target {
    note_index: usize,
    note_id: Option<String>,
    guid: String,
    position: usize,
    deck_path: String,
    children: Vec<usize>,
    note: usize,
    previous_tags: Vec<String>,
    already_tagged: bool,
}

impl Target {
    fn outcome(&self, status: RetireStatus, tag: &str) -> RetireOutcome {
        let mut tags = self.previous_tags.clone();
        if !self.already_tagged {
            tags.push(tag.to_string());
        }
        RetireOutcome {
            note_index: self.note_index,
            note_id: self.note_id.clone(),
            guid: self.guid.clone(),
            status,
            deck_path: self.deck_path.clone(),
            note_position: self.position,
            previous_tags: self.previous_tags.clone(),
            tags,
        }
    }

    /// Префикс путей внутри массива `tags` этой заметки.
    fn tag_prefix(&self) -> String {
        format!(
            "{}/notes/{}/tags/",
            deck_path_pointer(&self.children),
            self.note
        )
    }
}

/// Разрешает `guid` в заметку экспорта и её позицию в JSON-дереве.
fn resolve_target(
    index: &ExportIndex<'_>,
    value_notes: &[ValueNote],
    child_paths: &[Vec<usize>],
    note_index: usize,
    spec: &RetireSpec,
    tag: &str,
) -> Result<Target, DomainError> {
    let positions = index.note_positions_by_guid(&spec.guid);
    let position = match positions {
        [] => {
            return Err(DomainError::with_details(
                ErrorCode::UnresolvedGuid,
                format!(
                    "заметка #{note_index}: в экспорте нет заметки с guid {:?}",
                    spec.guid
                ),
                details! {
                    "note_index" => note_index,
                    "guid" => spec.guid.as_str(),
                },
            ));
        }
        [position] => *position,
        many => {
            return Err(DomainError::with_details(
                ErrorCode::GuidCollision,
                format!(
                    "заметка #{note_index}: guid {:?} встречается в экспорте {} раз, \
                     идентичность не уникальна",
                    spec.guid,
                    many.len()
                ),
                details! {
                    "note_index" => note_index,
                    "guid" => spec.guid.as_str(),
                    "occurrences" => many.len(),
                },
            ));
        }
    };

    let entry = &index.notes[position];
    let value = value_notes.get(position).ok_or_else(|| {
        internal(format!(
            "заметка #{position} отсутствует в JSON-проекции экспорта"
        ))
    })?;

    if value.path.children.len() > child_paths.len() {
        return Err(internal(format!(
            "узел {} заметки #{position} отсутствует в дереве колод",
            deck_path_pointer(&value.path.children)
        )));
    }

    let tags = entry
        .note
        .tags
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<&str>>();

    Ok(Target {
        note_index,
        note_id: spec.note_id.clone(),
        guid: spec.guid.clone(),
        position,
        deck_path: index.note_deck_path(entry).to_string(),
        children: value.path.children.clone(),
        note: value.path.note,
        previous_tags: entry.note.tags.clone(),
        already_tagged: tags.contains(tag),
    })
}

/// Дописывает тег в конец массива `tags` заметки.
fn append_tag(root: &mut Value, target: &Target, tag: &str) -> Result<(), DomainError> {
    let path = ValueNotePath {
        children: target.children.clone(),
        note: target.note,
    };
    let pointer = format!("{}/notes/{}", deck_path_pointer(&path.children), path.note);

    let node = node_mut(root, &path.children)
        .ok_or_else(|| internal(format!("узел {pointer} исчез из JSON-дерева")))?;
    let note = node
        .get_mut("notes")
        .and_then(Value::as_array_mut)
        .and_then(|notes| notes.get_mut(path.note))
        .ok_or_else(|| internal(format!("заметка {pointer} исчезла из JSON-дерева")))?;
    let object = note
        .as_object_mut()
        .ok_or_else(|| internal(format!("заметка {pointer} не является объектом")))?;
    let tags = object.get_mut("tags").ok_or_else(|| {
        DomainError::with_details(
            ErrorCode::ExportNotMutable,
            format!("заметка {pointer} не объявляет массив tags"),
            details! {
                "reason" => "note_without_tags_array",
                "guid" => target.guid.as_str(),
            },
        )
    })?;
    let array = tags
        .as_array_mut()
        .ok_or_else(|| internal(format!("tags заметки {pointer} не является массивом")))?;
    array.push(json!(tag));
    Ok(())
}

/// Доказывает, что различия — ровно дописанные теги указанных заметок.
fn verify_only_tags_appended(
    before: &Value,
    after: &Value,
    targets: &[Target],
    pending: &[usize],
    tag: &str,
) -> Result<(), DomainError> {
    let found = changed_paths(before, after, DEFAULT_PATH_LIMIT);

    if found.truncated {
        return Err(DomainError::with_details(
            ErrorCode::Internal,
            format!(
                "изменений больше {}, чем инструмент готов доказывать",
                DEFAULT_PATH_LIMIT
            ),
            details! {
                "reason" => "too_many_changes",
                "total" => found.total,
                "limit" => DEFAULT_PATH_LIMIT,
            },
        ));
    }

    if found.total != pending.len() {
        return Err(DomainError::with_details(
            ErrorCode::Internal,
            format!(
                "структурных различий {} вместо ожидаемых {}",
                found.total,
                pending.len()
            ),
            details! {
                "reason" => "unexpected_change_count",
                "total" => found.total,
                "expected" => pending.len(),
                "paths" => found.paths.clone(),
            },
        ));
    }

    let prefixes: Vec<String> = pending
        .iter()
        .map(|position| targets[*position].tag_prefix())
        .collect();
    if let Some(unexpected) =
        found.first_outside(&prefixes.iter().map(String::as_str).collect::<Vec<&str>>())
    {
        return Err(DomainError::with_details(
            ErrorCode::Internal,
            format!("неожиданное изменение вне массива tags целевых заметок: {unexpected}"),
            details! {
                "reason" => "unexpected_change_path",
                "path" => unexpected,
                "paths" => found.paths.clone(),
            },
        ));
    }

    for position in pending {
        let target = &targets[*position];
        let expected_index = target.previous_tags.len();
        let pointer = format!("{}{expected_index}", target.tag_prefix());
        if !found.paths.iter().any(|path| path == &pointer) {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!("тег {tag:?} не дописан в {pointer}"),
                details! {
                    "reason" => "tag_not_appended",
                    "guid" => target.guid.as_str(),
                    "path" => pointer,
                },
            ));
        }
    }

    for position in pending {
        let target = &targets[*position];
        let node = json_node(after, &target.children)?;
        let note = node
            .get("notes")
            .and_then(Value::as_array)
            .and_then(|notes| notes.get(target.note))
            .ok_or_else(|| internal("заметка исчезла при сборке кандидата"))?;
        let tags = note
            .get("tags")
            .and_then(Value::as_array)
            .ok_or_else(|| internal("tags заметки исчезли при сборке кандидата"))?;

        let prefix_matches = tags
            .iter()
            .take(target.previous_tags.len())
            .zip(&target.previous_tags)
            .all(|(value, expected)| value == &json!(expected));

        if tags.len() != target.previous_tags.len() + 1
            || tags.last() != Some(&json!(tag))
            || !prefix_matches
        {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!(
                    "теги заметки {} не являются прежним списком с дописанным тегом {tag:?}",
                    target.guid
                ),
                details! {
                    "reason" => "tags_not_appended",
                    "guid" => target.guid.as_str(),
                },
            ));
        }
    }

    Ok(())
}

/// Проверяет, что помеченные заметки по-прежнему разрешимы по `guid`.
///
/// Это и есть содержательная проверка отказа от физического удаления: заметка
/// осталась в экспорте и несёт тег, а не исчезла.
fn ensure_still_resolvable(
    candidate: &Candidate,
    targets: &[Target],
    pending: &[usize],
    tag: &str,
) -> Result<(), DomainError> {
    let index = ExportIndex::build(&candidate.root);

    for position in pending {
        let target = &targets[*position];
        let positions = index.note_positions_by_guid(&target.guid);
        let [position] = positions else {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!(
                    "после вывода из обращения guid {:?} разрешается в {} заметок",
                    target.guid,
                    positions.len()
                ),
                details! {
                    "reason" => "retired_note_not_resolvable",
                    "guid" => target.guid.as_str(),
                    "occurrences" => positions.len(),
                },
            ));
        };
        let entry = &index.notes[*position];
        if !entry.note.tags.iter().any(|existing| existing == tag) {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!(
                    "заметка {:?} после вывода из обращения не несёт тег {tag:?}",
                    target.guid
                ),
                details! {
                    "reason" => "retired_note_without_tag",
                    "guid" => target.guid.as_str(),
                    "tag" => tag,
                },
            ));
        }
    }

    Ok(())
}

/// Узел JSON-дерева по `children`-пути.
fn json_node<'a>(root: &'a Value, children: &[usize]) -> Result<&'a Value, DomainError> {
    let mut current = root;
    for step in children {
        current = current
            .get("children")
            .and_then(Value::as_array)
            .and_then(|children| children.get(*step))
            .ok_or_else(|| internal(format!("узел по пути {children:?} не найден")))?;
    }
    Ok(current)
}
