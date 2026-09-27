//! Точечное изменение значений полей существующих заметок.
//!
//! Единственная мутирующая операция toolkit'а. Она никогда не меняет
//! структуру экспорта: правка возможна только для значения уже существующего
//! поля уже существующей заметки, найденной по `guid`.
//!
//! Контракт безопасности строится на том, что исходный `deck.json` обязан быть
//! в канонической форме ([`crate::loader::render_canonical_bytes`]). Тогда
//! каноническая перезапись документа побайтово совпадает с исходником везде,
//! кроме изменённого значения, и правка физически не может задеть остальной
//! файл. Всё, что этому противоречит, отклоняется до записи.
//!
//! Порядок работы:
//!
//! ```text
//! валидация запроса
//! → чтение байтов deck.json
//! → P1 исходник каноничен
//! → пригодность экспорта (ERROR 0 и нет конфликта определений модели)
//! → P2 соответствие заметок в JSON и в типизированном дереве
//! → разрешение всех правок (guid → заметка → модель → ord → значение)
//! → классификация всех правок (noop / применить / уже применено / конфликт)
//! → мутация только значений
//! → каноническая сериализация кандидата
//! → P6 кандидат разбирается ровно в задуманное значение
//! → P7 целевые значения на месте
//! → P8 байтовый diff — ровно запрошенный набор изменений
//! → P9 прирост байтов объясняется длиной токенов
//! → валидация кандидата: ERROR 0
//! → dry-run или атомарная замена файла под эксклюзивной блокировкой
//! ```
//!
//! Публикация кандидата идёт через [`crate::write::replace_atomically`]: замена
//! атомарна для читателя, а проверка предусловия и `rename` образуют одну
//! критическую секцию под advisory-блокировкой целевого файла. Поэтому два
//! конкурентных `edit --apply` одного экспорта не могут молча потерять результат
//! одного из них: проигравший получает [`ErrorCode::SourceChanged`] / exit 7.
//! Точная граница этой гарантии описана в [`crate::write`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{self, ExportIndex};
use crate::loader;
use crate::ops::validate::{self, Severity, SeverityCounts, ValidateResult, warning_codes};
use crate::text::bounded_sample as sample;
use crate::write;

/// Жёсткий максимум числа правок в одном запросе.
pub const MAX_EDITS: usize = 20_000;
/// Жёсткий максимум размера JSON-запроса.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Предел числа правок в отчёте.
pub const MAX_REPORTED_EDITS: usize = 50;
/// Предел числа конфликтов в отчёте.
pub const MAX_REPORTED_CONFLICTS: usize = 50;
/// Предел длины выборки значения в отчёте.
///
/// Реализация общей bounded-выборки живёт в [`crate::text`]; путь
/// `VALUE_SAMPLE_CHARS` сохранён для совместимости с существующими тестами.
pub use crate::text::VALUE_SAMPLE_CHARS;
/// Предел числа кодов и имён в диагностике.
pub const MAX_REPORTED_CODES: usize = 20;
/// Предел числа проблем разрешения в деталях ошибки.
pub const MAX_REPORTED_PROBLEMS: usize = 50;
/// Поддерживаемая версия схемы JSON-запроса.
pub const SUPPORTED_REQUEST_SCHEMA_VERSION: u32 = 1;
/// Имя канала, читаемого вместо файла запроса.
pub const STDIN_REQUEST_SOURCE: &str = "-";

/// Одна правка в запросе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSpec {
    /// Необязательный стабильный идентификатор правки для отчёта.
    pub edit_id: Option<String>,
    /// `guid` заметки.
    pub guid: String,
    /// Имя поля модели заметки.
    pub field: String,
    /// Ожидаемое текущее значение поля.
    pub expected: String,
    /// Новое значение поля.
    pub replacement: String,
}

/// Проверенный запрос на изменение значений полей.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
    /// Правки в порядке запроса.
    pub edits: Vec<EditSpec>,
}

/// Что произошло с одной правкой при вычислении кандидата.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditStatus {
    /// Файл записан, значение заменено.
    Applied,
    /// Значение заменено только в кандидате: файл не тронут.
    DryRun,
    /// Текущее значение уже равно и `expected`, и `replacement`.
    NoopIdentical,
    /// Текущее значение уже равно `replacement`.
    AlreadyApplied,
    /// Текущее значение не совпало ни с `expected`, ни с `replacement`.
    Conflict,
}

impl EditStatus {
    /// Стабильное machine-readable имя статуса.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::DryRun => "dry_run",
            Self::NoopIdentical => "noop_identical",
            Self::AlreadyApplied => "already_applied",
            Self::Conflict => "conflict",
        }
    }

    /// Требует ли статус изменения значения в кандидате.
    const fn is_effective(self) -> bool {
        matches!(self, Self::Applied | Self::DryRun)
    }
}

/// Результат одной правки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    /// Позиция правки в запросе (с нуля).
    pub edit_index: usize,
    /// Идентификатор правки из запроса.
    pub edit_id: Option<String>,
    /// `guid` заметки.
    pub guid: String,
    /// Имя поля.
    pub field: String,
    /// Позиция поля в модели (`ord`).
    pub field_ord: usize,
    /// Позиция заметки в порядке экспорта (с нуля).
    pub note_index: usize,
    /// Путь колоды заметки.
    pub deck_path: String,
    /// Итоговый статус.
    pub status: EditStatus,
    /// Длина прежнего значения в символах.
    pub old_len: usize,
    /// Длина нового значения в символах.
    pub new_len: usize,
    /// Выборка прежнего значения.
    pub old_sample: String,
    /// Выборка нового значения.
    pub new_sample: String,
}

/// Правка, текущее значение которой не совпало с `expected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditConflict {
    /// Позиция правки в запросе (с нуля).
    pub edit_index: usize,
    /// Идентификатор правки из запроса.
    pub edit_id: Option<String>,
    /// `guid` заметки.
    pub guid: String,
    /// Имя поля.
    pub field: String,
    /// Длина ожидаемого значения в символах.
    pub expected_len: usize,
    /// Длина фактического значения в символах.
    pub current_len: usize,
    /// Выборка ожидаемого значения.
    pub expected_sample: String,
    /// Выборка фактического значения.
    pub current_sample: String,
}

/// Счётчики правок по статусам.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditSummaries {
    /// Сколько значений записано в файл.
    pub applied: usize,
    /// Сколько значений заменено только в кандидате.
    pub dry_run: usize,
    /// Сколько правок не требовало изменений.
    pub noop_identical: usize,
    /// Сколько правок уже было применено ранее.
    pub already_applied: usize,
}

/// Сравнение валидации до и после правки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationDelta {
    /// Счётчики исходного экспорта.
    pub before: SeverityCounts,
    /// Счётчики кандидата.
    pub after: SeverityCounts,
    /// Коды ERROR, появившиеся только у кандидата.
    pub new_error_codes: Vec<String>,
    /// Коды WARNING, появившиеся только у кандидата.
    pub new_warning_codes: Vec<String>,
}

impl ValidationDelta {
    /// Появились ли у кандидата новые ERROR.
    #[must_use]
    pub fn has_new_errors(&self) -> bool {
        !self.new_error_codes.is_empty()
    }
}

/// Результаты обязательных проверок консистентности.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditChecks {
    /// Исходник был в канонической форме.
    pub source_canonical: bool,
    /// Кандидат повторно разбирается в задуманное значение.
    pub candidate_reparsed: bool,
    /// Целевые значения действительно оказались в кандидате.
    pub semantic_targets_verified: bool,
    /// Байтовый diff кандидата равен ровно запрошенному набору изменений.
    pub diff_shape_is_exactly_requested: bool,
    /// Прирост байтов объясняется длиной изменённых токенов.
    pub byte_delta_matches_token_delta: bool,
}

/// Результат `edit`.
#[derive(Debug, Clone)]
pub struct EditResult {
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
    /// Сколько строк файла изменилось.
    pub changed_lines: usize,
    /// Номер первой изменённой строки (с единицы).
    pub first_changed_line: Option<usize>,
    /// Сколько правок было в запросе.
    pub edits_total: usize,
    /// Сколько правок реально меняют значение.
    pub effective_edits: usize,
    /// Счётчики по статусам.
    pub summaries: EditSummaries,
    /// Отчёт по правкам (обрезан до [`MAX_REPORTED_EDITS`]).
    pub outcomes: Vec<EditOutcome>,
    /// Был ли отчёт по правкам обрезан.
    pub outcomes_truncated: bool,
    /// Сравнение валидации до и после.
    pub validation: ValidationDelta,
    /// Выполненные проверки консистентности.
    pub checks: EditChecks,
}

/// Разбирает JSON-запрос на правки.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InvalidRequest`] для слишком большого,
/// нечитаемого или структурно некорректного запроса, а также
/// [`ErrorCode::DuplicateEditTarget`] при повторной цели.
pub fn parse_request_bytes(raw: &[u8], label: &str) -> Result<EditRequest, DomainError> {
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

    let request = EditRequest {
        edits: file
            .edits
            .into_iter()
            .map(|edit| EditSpec {
                edit_id: edit.edit_id,
                guid: edit.guid,
                field: edit.field,
                expected: edit.expected,
                replacement: edit.replacement,
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
/// Возвращает [`ErrorCode::InvalidRequest`] для пустого, слишком большого или
/// некорректного запроса и [`ErrorCode::DuplicateEditTarget`], если одна пара
/// «guid — поле» запрошена дважды.
pub fn validate_request(request: &EditRequest) -> Result<(), DomainError> {
    if request.edits.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "запрос на изменение не содержит ни одной правки",
            details! {
                "reason" => "empty_request",
            },
        ));
    }

    if request.edits.len() > MAX_EDITS {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "в запросе {} правок, максимум {MAX_EDITS}",
                request.edits.len()
            ),
            details! {
                "reason" => "too_many_edits",
                "edits" => request.edits.len(),
                "max_edits" => MAX_EDITS,
            },
        ));
    }

    let mut targets: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    let mut edit_ids: BTreeMap<&str, usize> = BTreeMap::new();

    for (position, edit) in request.edits.iter().enumerate() {
        if edit.guid.is_empty() {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("правка #{position} содержит пустой guid"),
                details! {
                    "reason" => "empty_guid",
                    "edit_index" => position,
                },
            ));
        }
        if edit.field.is_empty() {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("правка #{position} содержит пустое имя поля"),
                details! {
                    "reason" => "empty_field",
                    "edit_index" => position,
                    "guid" => edit.guid.as_str(),
                },
            ));
        }

        if let Some(previous) = targets.insert((edit.guid.as_str(), edit.field.as_str()), position)
        {
            return Err(DomainError::with_details(
                ErrorCode::DuplicateEditTarget,
                format!(
                    "пара guid {:?} и поле {:?} запрошена дважды: правки #{previous} и #{position}",
                    edit.guid, edit.field
                ),
                details! {
                    "guid" => edit.guid.as_str(),
                    "field" => edit.field.as_str(),
                    "first_edit_index" => previous,
                    "duplicate_edit_index" => position,
                },
            ));
        }

        if let Some(edit_id) = edit.edit_id.as_deref()
            && let Some(previous) = edit_ids.insert(edit_id, position)
        {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("edit_id {edit_id:?} повторяется: правки #{previous} и #{position}"),
                details! {
                    "reason" => "duplicate_edit_id",
                    "edit_id" => edit_id,
                    "first_edit_index" => previous,
                    "duplicate_edit_index" => position,
                },
            ));
        }
    }

    Ok(())
}

/// Выполняет правку значений полей.
///
/// Без `apply` файл не изменяется: команда только сообщает, что именно она
/// записала бы. С `apply` `deck.json` заменяется атомарно и только тогда, когда
/// все проверки прошли; проверка исходника и замена образуют одну критическую
/// секцию, поэтому конкурентный `edit --apply` не может молча затереть уже
/// опубликованный результат.
///
/// # Errors
///
/// Возвращает доменную ошибку с exit code 3, 4, 6, 7 или 8 из таблицы
/// [`ErrorCode`]: неподходящий исходник, проблемы разрешения, конфликт
/// предусловия (`expected_mismatch` или `source_changed`) или отказ файловой
/// системы.
pub fn edit(
    export_dir: &Path,
    request: &EditRequest,
    apply: bool,
) -> Result<EditResult, DomainError> {
    validate_request(request)?;

    let EditableSource {
        deck_json,
        source,
        mut value,
        root: source_root,
        before,
    } = load_editable_source(export_dir)?;
    let index = ExportIndex::build(&source_root);

    let value_notes = collect_value_notes(&value)?;
    ensure_note_correspondence(&index, &value_notes)?;

    let mut resolved: Vec<ResolvedEdit> = Vec::with_capacity(request.edits.len());
    let mut problems: Vec<Problem> = Vec::new();
    for (position, spec) in request.edits.iter().enumerate() {
        match resolve_edit(&index, position, spec, apply) {
            Ok(entry) => resolved.push(entry),
            Err(problem) => problems.push(problem),
        }
    }
    if !problems.is_empty() {
        return Err(problems_error(&problems, export_dir));
    }

    let conflicts: Vec<EditConflict> = resolved
        .iter()
        .filter(|entry| entry.status == EditStatus::Conflict)
        .map(conflict_of)
        .collect();
    if !conflicts.is_empty() {
        return Err(conflicts_error(
            &conflicts,
            request.edits.len(),
            deck_json.as_path(),
        ));
    }

    let mut summaries = EditSummaries::default();
    let mut effective: Vec<(String, String)> = Vec::new();
    for entry in &resolved {
        match entry.status {
            EditStatus::NoopIdentical => summaries.noop_identical += 1,
            EditStatus::AlreadyApplied => summaries.already_applied += 1,
            EditStatus::Applied => summaries.applied += 1,
            EditStatus::DryRun => summaries.dry_run += 1,
            EditStatus::Conflict => {}
        }
        if entry.status.is_effective() {
            effective.push((entry.current.clone(), entry.replacement.clone()));
        }
    }

    for entry in &resolved {
        if entry.status.is_effective() {
            set_field_value(
                &mut value,
                &value_notes[entry.note_position].path,
                entry.field_ord,
                &entry.replacement,
            )?;
        }
    }

    let candidate = loader::render_canonical_bytes(&value)?;
    let candidate_value = loader::parse_deck_json_bytes(&candidate, &deck_json)?;

    ensure_reparsed(&candidate_value, &value)?;
    verify_targets(&candidate_value, &value_notes, &resolved)?;
    let shape = check_diff_shape(&source, &candidate, &effective)?;
    ensure_byte_delta(&source, &candidate, &effective)?;

    let candidate_root = loader::typed_root(candidate_value, &deck_json)?;
    let after = validate::validate_document(&candidate_root, export_dir);
    let validation = validation_delta(&before, &after);

    if validation.has_new_errors() {
        return Err(DomainError::with_details(
            ErrorCode::ExportInvalid,
            format!(
                "кандидат правки получает ERROR, которых не было в исходном экспорте: {}",
                validation.new_error_codes.join(", ")
            ),
            details! {
                "phase" => "candidate",
                "path" => deck_json.display().to_string(),
                "errors_before" => validation.before.errors,
                "errors_after" => validation.after.errors,
                "new_error_codes" => validation.new_error_codes.clone(),
            },
        ));
    }

    let should_write = apply && !effective.is_empty();
    if should_write {
        write::replace_atomically(&deck_json, &source, &candidate)?;
    }

    let source_bytes = source.len();
    let candidate_bytes = candidate.len();
    let outcomes_truncated = resolved.len() > MAX_REPORTED_EDITS;
    let outcomes = resolved
        .iter()
        .take(MAX_REPORTED_EDITS)
        .map(outcome_of)
        .collect();

    Ok(EditResult {
        export_dir: export_dir.to_path_buf(),
        deck_json,
        dry_run: !apply,
        applied: should_write,
        source_bytes,
        candidate_bytes,
        byte_delta: byte_delta(&source, &candidate),
        changed_lines: shape.changed_lines,
        first_changed_line: shape.first_changed_line,
        edits_total: request.edits.len(),
        effective_edits: effective.len(),
        summaries,
        outcomes,
        outcomes_truncated,
        validation,
        checks: EditChecks {
            source_canonical: true,
            candidate_reparsed: true,
            semantic_targets_verified: true,
            diff_shape_is_exactly_requested: true,
            byte_delta_matches_token_delta: true,
        },
    })
}

/// Одна разрешённая правка: всё нужное для отчёта и мутации.
///
/// Структура и [`resolve_edit`] переиспользуются `review-check`: проверка
/// предложений агента обязана разрешать `guid` и поле ровно теми же правилами,
/// что и сама запись, иначе её вердикт разошёлся бы с вердиктом `edit`.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedEdit {
    pub(crate) edit_index: usize,
    pub(crate) edit_id: Option<String>,
    pub(crate) guid: String,
    pub(crate) deck_path: String,
    pub(crate) note_position: usize,
    pub(crate) field: String,
    pub(crate) field_ord: usize,
    pub(crate) current: String,
    pub(crate) expected: String,
    pub(crate) replacement: String,
    pub(crate) status: EditStatus,
}

/// Проблема разрешения одной правки.
#[derive(Debug, Clone)]
pub(crate) struct Problem {
    pub(crate) edit_index: usize,
    pub(crate) guid: String,
    pub(crate) field: String,
    pub(crate) code: &'static str,
    pub(crate) kind: ProblemKind,
    pub(crate) message: String,
}

/// Вид проблемы разрешения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProblemKind {
    /// Заметки с таким `guid` нет.
    NoteNotFound,
    /// Имени поля нет ни в одной модели экспорта.
    UnknownField,
    /// Имя поля есть в экспорте, но не в модели этой заметки, либо модель
    /// не позволяет однозначно разрешить `ord`.
    FieldNotInModel,
    /// `guid` встречается больше одного раза.
    Ambiguous,
}

/// Соответствие заметки в JSON-дереве и в типизированном дереве.
#[derive(Debug)]
struct ValueNote {
    /// Позиция в JSON-дереве.
    path: ValueNotePath,
    /// `guid` из JSON.
    guid: Option<String>,
    /// `note_model_uuid` из JSON.
    model_uuid: Option<String>,
}

/// Позиция заметки в JSON-дереве.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValueNotePath {
    /// Индексы `children` от корня экспорта.
    children: Vec<usize>,
    /// Индекс заметки в `notes` узла.
    note: usize,
}

/// Обходит JSON-дерево в том же порядке, что и [`ExportIndex`].
fn collect_value_notes(root: &Value) -> Result<Vec<ValueNote>, DomainError> {
    let mut notes = Vec::new();
    collect_notes_in(root, &mut Vec::new(), &mut notes)?;
    Ok(notes)
}

fn collect_notes_in(
    node: &Value,
    children: &mut Vec<usize>,
    notes: &mut Vec<ValueNote>,
) -> Result<(), DomainError> {
    let Some(object) = node.as_object() else {
        return Err(internal(format!(
            "узел колоды в JSON не является объектом, а его тип: {}",
            type_name(node)
        )));
    };

    match object.get("notes") {
        None | Some(Value::Null) => {}
        Some(Value::Array(entries)) => {
            for (position, entry) in entries.iter().enumerate() {
                let Some(note) = entry.as_object() else {
                    return Err(internal(format!(
                        "заметка #{position} не является объектом, а её тип: {}",
                        type_name(entry)
                    )));
                };
                notes.push(ValueNote {
                    path: ValueNotePath {
                        children: children.clone(),
                        note: position,
                    },
                    guid: string_property(note.get("guid")),
                    model_uuid: string_property(note.get("note_model_uuid")),
                });
            }
        }
        Some(other) => {
            return Err(internal(format!(
                "notes не является массивом, а его тип: {}",
                type_name(other)
            )));
        }
    }

    match object.get("children") {
        None | Some(Value::Null) => {}
        Some(Value::Array(entries)) => {
            for (position, entry) in entries.iter().enumerate() {
                children.push(position);
                collect_notes_in(entry, children, notes)?;
                children.pop();
            }
        }
        Some(other) => {
            return Err(internal(format!(
                "children не является массивом, а его тип: {}",
                type_name(other)
            )));
        }
    }

    Ok(())
}

/// Проверяет, что обе проекции экспорта видят одни и те же заметки.
///
/// Значения полей берутся из типизированного дерева, а мутируется JSON-дерево.
/// Совпадение позиций и идентификаторов — то, что делает эту связь законной.
fn ensure_note_correspondence(
    index: &ExportIndex<'_>,
    value_notes: &[ValueNote],
) -> Result<(), DomainError> {
    if index.notes.len() != value_notes.len() {
        return Err(internal(format!(
            "число заметок в типизированном дереве ({}) и в JSON ({}) различается",
            index.notes.len(),
            value_notes.len()
        )));
    }

    for (position, (typed, value)) in index.notes.iter().zip(value_notes.iter()).enumerate() {
        if typed.note.guid.as_deref() != value.guid.as_deref()
            || typed.note.note_model_uuid.as_deref() != value.model_uuid.as_deref()
        {
            return Err(internal(format!(
                "заметка #{position} различается между проекциями: \
                 типизированная guid {:?}/модель {:?}, JSON guid {:?}/модель {:?}",
                typed.note.guid, typed.note.note_model_uuid, value.guid, value.model_uuid
            )));
        }
    }

    Ok(())
}

/// Разрешает одну правку до конкретного значения поля.
pub(crate) fn resolve_edit(
    index: &ExportIndex<'_>,
    edit_index: usize,
    spec: &EditSpec,
    apply: bool,
) -> Result<ResolvedEdit, Problem> {
    let problem = |kind: ProblemKind, code: &'static str, message: String| Problem {
        edit_index,
        guid: spec.guid.clone(),
        field: spec.field.clone(),
        code,
        kind,
        message,
    };

    let note_position = match index.note_positions_by_guid(&spec.guid) {
        [] => {
            return Err(problem(
                ProblemKind::NoteNotFound,
                "note_not_found",
                format!("в экспорте нет заметки с guid {:?}", spec.guid),
            ));
        }
        [only] => *only,
        many => {
            return Err(problem(
                ProblemKind::Ambiguous,
                "ambiguous",
                format!(
                    "guid {:?} встречается {count} раз в экспорте",
                    spec.guid,
                    count = many.len()
                ),
            ));
        }
    };

    let entry = &index.notes[note_position];
    let Some(model_uuid) = entry.note.note_model_uuid.as_deref() else {
        return Err(problem(
            ProblemKind::FieldNotInModel,
            "internal_error",
            format!("заметка {:?} не содержит note_model_uuid", spec.guid),
        ));
    };
    let Some(model) = index.model_by_uuid(model_uuid) else {
        return Err(problem(
            ProblemKind::FieldNotInModel,
            "internal_error",
            format!("модель {model_uuid:?} заметки {:?} не найдена", spec.guid),
        ));
    };

    let fields = index::resolve_named_fields(entry.note, model);
    let Some(field) = fields.iter().find(|field| field.name == spec.field) else {
        let message = if index.known_field_names().contains(spec.field.as_str()) {
            let available: Vec<&str> = fields.iter().map(|field| field.name).collect();
            format!(
                "в модели заметки {:?} нет поля {:?}; доступны: {}",
                spec.guid,
                spec.field,
                available.join(", ")
            )
        } else {
            format!(
                "ни одна модель экспорта не содержит поля {:?}; доступны: {}",
                spec.field,
                describe_known_fields(index)
            )
        };
        return Err(problem(ProblemKind::UnknownField, "unknown_field", message));
    };

    let Some(ord) = field.ord.value().and_then(|ord| usize::try_from(ord).ok()) else {
        return Err(problem(
            ProblemKind::FieldNotInModel,
            "internal_error",
            format!(
                "поле {:?} заметки {:?} не имеет корректного ord",
                spec.field, spec.guid
            ),
        ));
    };

    let Some(current) = field.value.and_then(|value| value.as_text()) else {
        return Err(problem(
            ProblemKind::FieldNotInModel,
            "internal_error",
            format!(
                "поле {:?} заметки {:?} не содержит строкового значения по позиции {ord}",
                spec.field, spec.guid
            ),
        ));
    };

    let mut resolved = ResolvedEdit {
        edit_index,
        edit_id: spec.edit_id.clone(),
        guid: spec.guid.clone(),
        deck_path: index.note_deck_path(entry).to_string(),
        note_position,
        field: spec.field.clone(),
        field_ord: ord,
        current: current.to_string(),
        expected: spec.expected.clone(),
        replacement: spec.replacement.clone(),
        status: EditStatus::Conflict,
    };
    resolved.status = classify(&resolved, apply);
    Ok(resolved)
}

/// Классифицирует правку по четвёрке «текущее, expected, replacement».
pub(crate) fn classify(entry: &ResolvedEdit, apply: bool) -> EditStatus {
    if entry.current == entry.expected {
        if entry.expected == entry.replacement {
            EditStatus::NoopIdentical
        } else if apply {
            EditStatus::Applied
        } else {
            EditStatus::DryRun
        }
    } else if entry.current == entry.replacement {
        EditStatus::AlreadyApplied
    } else {
        EditStatus::Conflict
    }
}

/// Ограниченный список известных имён полей для диагностики.
fn describe_known_fields(index: &ExportIndex<'_>) -> String {
    let mut names: Vec<&str> = index.known_field_names().iter().copied().collect();
    let truncated = names.len() > MAX_REPORTED_CODES;
    names.truncate(MAX_REPORTED_CODES);
    let mut text = names.join(", ");
    if truncated {
        text.push_str(", …");
    }
    text
}

/// Приводит список проблем разрешения к одной доменной ошибке.
///
/// Все проблемы собираются за один проход: агент должен видеть весь список
/// неверных целей сразу, а не первую из них. Код выбирается по старшинству.
fn problems_error(problems: &[Problem], export_dir: &Path) -> DomainError {
    let code = if problems
        .iter()
        .any(|problem| problem.kind == ProblemKind::Ambiguous)
    {
        ErrorCode::Ambiguous
    } else if problems
        .iter()
        .any(|problem| problem.kind == ProblemKind::NoteNotFound)
    {
        ErrorCode::NoteNotFound
    } else if problems
        .iter()
        .any(|problem| problem.kind == ProblemKind::UnknownField)
    {
        ErrorCode::UnknownField
    } else {
        ErrorCode::Internal
    };

    let codes: Vec<&str> = problems.iter().map(|problem| problem.code).collect();
    let listed: Vec<Value> = problems
        .iter()
        .take(MAX_REPORTED_PROBLEMS)
        .map(|problem| {
            json!({
                "edit_index": problem.edit_index,
                "guid": problem.guid,
                "field": problem.field,
                "code": problem.code,
                "message": problem.message,
            })
        })
        .collect();

    let message = if problems.len() == 1 {
        problems[0].message.clone()
    } else {
        format!(
            "не удалось разрешить {} правок из запроса: {}",
            problems.len(),
            problems
                .iter()
                .take(3)
                .map(|problem| problem.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    };

    DomainError::with_details(
        code,
        message,
        details! {
            "export_dir" => export_dir.display().to_string(),
            "problems" => listed,
            "problems_total" => problems.len(),
            "problems_truncated" => problems.len() > MAX_REPORTED_PROBLEMS,
            "codes" => codes,
        },
    )
}

/// Готовит отчёт по конфликтам предусловия.
fn conflicts_error(conflicts: &[EditConflict], total: usize, deck_json: &Path) -> DomainError {
    let listed: Vec<Value> = conflicts
        .iter()
        .take(MAX_REPORTED_CONFLICTS)
        .map(|conflict| {
            json!({
                "edit_index": conflict.edit_index,
                "edit_id": conflict.edit_id,
                "guid": conflict.guid,
                "field": conflict.field,
                "expected_len": conflict.expected_len,
                "current_len": conflict.current_len,
                "expected_sample": conflict.expected_sample,
                "current_sample": conflict.current_sample,
            })
        })
        .collect();

    let preview = conflicts
        .iter()
        .take(3)
        .map(|conflict| {
            format!(
                "{:?}/{:?}: ожидалось {:?}, сейчас {:?}",
                conflict.guid, conflict.field, conflict.expected_sample, conflict.current_sample
            )
        })
        .collect::<Vec<_>>()
        .join("; ");

    DomainError::with_details(
        ErrorCode::ExpectedMismatch,
        format!(
            "текущее значение не совпало с expected в {count} правках из {total}: {preview}",
            count = conflicts.len()
        ),
        details! {
            "path" => deck_json.display().to_string(),
            "conflicts" => listed,
            "conflicts_total" => conflicts.len(),
            "conflicts_truncated" => conflicts.len() > MAX_REPORTED_CONFLICTS,
            "edits_total" => total,
        },
    )
}

/// Разобранный исходник без предварительных проверок границы записи.
///
/// Один и тот же набор проверок нужен двум командам: `edit` пишет по этому
/// исходнику, а `review-check` обещает агенту запрос, который граница записи
/// обязана принять. Держать эти проверки в одном месте — единственный способ
/// не разойтись в том, какой экспорт вообще можно править.
pub(crate) struct EditableSource {
    /// Полный путь к `deck.json`.
    pub deck_json: PathBuf,
    /// Сырые байты `deck.json`: каноничность проверяется побайтово.
    pub source: Vec<u8>,
    /// Разобранное значение: `edit` меняет именно его.
    pub value: Value,
    /// Типизированный корень того же файла.
    pub root: crate::model::DeckNode,
    /// Проверки экспорта до правки: `edit` сравнивает с ними результат.
    pub before: ValidateResult,
}

/// Читает и разбирает `deck.json` без оценки права на запись.
///
/// # Errors
///
/// Возвращает ошибки чтения и разбора [`loader::read_deck_json_bytes`] и
/// [`loader::parse_deck_json_bytes`].
pub(crate) fn read_source(export_dir: &Path) -> Result<EditableSource, DomainError> {
    let (deck_json, source) = loader::read_deck_json_bytes(export_dir)?;
    let value = loader::parse_deck_json_bytes(&source, &deck_json)?;
    let root = loader::typed_root(
        loader::parse_deck_json_bytes(&source, &deck_json)?,
        &deck_json,
    )?;
    let before = validate::validate_document(&root, export_dir);

    Ok(EditableSource {
        deck_json,
        source,
        value,
        root,
        before,
    })
}

/// Собирает причины, по которым этот исходник нельзя править.
///
/// Проверки ровно те же, на которых `edit` останавливается до классификации
/// правок. `edit` берёт первую причину и отказывается работать;
/// `review-check` называет их все, потому что его отчёт обязан объяснить, из-за
/// чего запрос не выпущен. Порядок причин фиксирован: каноническая форма,
/// `ERROR` экспорта, неоднозначный порядок полей модели.
#[must_use]
pub(crate) fn source_blockers(source: &EditableSource) -> Vec<DomainError> {
    let mut blockers = Vec::new();

    if let Err(error) = ensure_source_is_canonical(&source.value, &source.source, &source.deck_json)
    {
        blockers.push(error);
    }
    blockers.extend(mutable_blockers(&source.before, &source.deck_json));

    blockers
}

/// Читает `deck.json` и требует, чтобы исходник можно было править.
///
/// # Errors
///
/// Возвращает ошибки чтения и разбора [`read_source`], а также первую из
/// [`source_blockers`].
pub(crate) fn load_editable_source(export_dir: &Path) -> Result<EditableSource, DomainError> {
    let source = read_source(export_dir)?;

    match source_blockers(&source).into_iter().next() {
        Some(blocker) => Err(blocker),
        None => Ok(source),
    }
}

/// Отклоняет исходник, который не в канонической форме.
fn ensure_source_is_canonical(
    value: &Value,
    source: &[u8],
    deck_json: &Path,
) -> Result<(), DomainError> {
    let canonical = loader::render_canonical_bytes(value)?;
    if canonical == source {
        return Ok(());
    }

    Err(DomainError::with_details(
        ErrorCode::SourceNotCanonical,
        format!(
            "{} не в канонической форме; правка отклонена, чтобы не переписать файл целиком",
            deck_json.display()
        ),
        details! {
            "path" => deck_json.display().to_string(),
            "reason" => "canonical_round_trip_mismatch",
            "source_bytes" => source.len(),
            "canonical_bytes" => canonical.len(),
            "first_difference_offset" => first_difference(source, &canonical),
        },
    ))
}

/// Причины, по которым экспорт нельзя безопасно править.
///
/// Возвращает `ERROR`-экспорт и неоднозначный порядок полей модели в
/// фиксированном порядке: сначала непригодный экспорт, затем небезопасный.
fn mutable_blockers(before: &ValidateResult, deck_json: &Path) -> Vec<DomainError> {
    let mut blockers = Vec::new();
    let errors: Vec<&str> = distinct_codes(before, Severity::Error);
    if !errors.is_empty() {
        blockers.push(DomainError::with_details(
            ErrorCode::ExportInvalid,
            format!(
                "экспорт содержит ERROR ({}); правка значений полей возможна только в валидном экспорте",
                errors.join(", ")
            ),
            details! {
                "phase" => "source",
                "path" => deck_json.display().to_string(),
                "errors" => before.summary.errors,
                "error_codes" => errors,
            },
        ));
    }

    let blocked = warning_codes::CONFLICTING_NOTE_MODEL_DEFINITION;
    if before
        .issues
        .iter()
        .any(|issue| issue.severity == Severity::Warning && issue.code == blocked)
    {
        blockers.push(DomainError::with_details(
            ErrorCode::ExportNotMutable,
            format!(
                "экспорт содержит WARNING {blocked}: порядок полей неоднозначен, правка по имени поля небезопасна"
            ),
            details! {
                "phase" => "source",
                "path" => deck_json.display().to_string(),
                "code" => blocked,
            },
        ));
    }

    blockers
}

/// Уникальные коды issues указанной серьёзности.
fn distinct_codes(result: &ValidateResult, severity: Severity) -> Vec<&'static str> {
    let mut codes: Vec<&'static str> = result
        .issues
        .iter()
        .filter(|issue| issue.severity == severity)
        .map(|issue| issue.code)
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes.truncate(MAX_REPORTED_CODES);
    codes
}

/// Сравнивает валидацию до и после правки.
fn validation_delta(before: &ValidateResult, after: &ValidateResult) -> ValidationDelta {
    let before_errors = distinct_codes(before, Severity::Error);
    let before_warnings = distinct_codes(before, Severity::Warning);

    ValidationDelta {
        before: before.summary,
        after: after.summary,
        new_error_codes: distinct_codes(after, Severity::Error)
            .into_iter()
            .filter(|code| !before_errors.contains(code))
            .map(ToString::to_string)
            .collect(),
        new_warning_codes: distinct_codes(after, Severity::Warning)
            .into_iter()
            .filter(|code| !before_warnings.contains(code))
            .map(ToString::to_string)
            .collect(),
    }
}

/// Заменяет значение поля в JSON-дереве.
fn set_field_value(
    root: &mut Value,
    path: &ValueNotePath,
    ord: usize,
    replacement: &str,
) -> Result<(), DomainError> {
    let note = note_mut(root, path).ok_or_else(|| {
        internal(format!(
            "заметка с позицией {} в узле {:?} исчезла из JSON-дерева",
            path.note, path.children
        ))
    })?;
    let fields = note
        .as_object_mut()
        .and_then(|note| note.get_mut("fields"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| internal(format!("заметка #{} не содержит массива fields", path.note)))?;

    let Some(slot) = fields.get_mut(ord) else {
        return Err(internal(format!(
            "в заметке с позицией {} в узле {:?} нет позиции поля {ord}",
            path.note, path.children
        )));
    };
    *slot = Value::String(replacement.to_string());
    Ok(())
}

/// Проверяет, что целевые значения действительно попали в кандидат.
fn verify_targets(
    candidate: &Value,
    value_notes: &[ValueNote],
    resolved: &[ResolvedEdit],
) -> Result<(), DomainError> {
    for entry in resolved {
        let expected_after = if entry.status.is_effective() {
            entry.replacement.as_str()
        } else {
            entry.current.as_str()
        };

        let Some(note) = note_ref(candidate, &value_notes[entry.note_position].path) else {
            return Err(internal(format!(
                "заметка {:?} исчезла из кандидата",
                entry.guid
            )));
        };
        let observed = note
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|fields| fields.get(entry.field_ord))
            .and_then(Value::as_str);

        if observed != Some(expected_after) {
            return Err(internal(format!(
                "поле {:?} заметки {:?} в кандидате оказалось не тем, что запланировано",
                entry.field, entry.guid
            )));
        }
    }

    Ok(())
}

/// Номер первого различающегося байта двух срезов.
fn first_difference(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| left.len().min(right.len()))
}

/// Разница размеров в байтах.
fn byte_delta(source: &[u8], candidate: &[u8]) -> i64 {
    candidate.len() as i64 - source.len() as i64
}

/// Прирост байтов, который должен дать ровно запрошенный набор изменений.
fn token_delta(effective: &[(String, String)]) -> i64 {
    effective
        .iter()
        .map(|(expected, replacement)| {
            (token_bytes(replacement) as i64) - (token_bytes(expected) as i64)
        })
        .sum()
}

/// Длина значения в канонической JSON-форме вместе с кавычками.
fn token_bytes(text: &str) -> usize {
    serde_json::to_string(text).map_or(0, |encoded| encoded.len())
}

/// Форма байтового diff между исходником и кандидатом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiffShape {
    changed_lines: usize,
    first_changed_line: Option<usize>,
}

/// Проверяет, что байтовый diff равен ровно запрошенному набору изменений.
///
/// Каноническая форма даёт по одному строковому токену на строку, поэтому
/// каждая изменённая строка обязана целиком состоять из одного значения поля.
/// Сравниваются мультимножества пар «было → стало»: это одновременно ловит
/// лишние изменения, неверные значения и изменения вне значений полей.
fn check_diff_shape(
    source: &[u8],
    candidate: &[u8],
    effective: &[(String, String)],
) -> Result<DiffShape, DomainError> {
    let source_lines: Vec<&[u8]> = source.split(|byte| *byte == b'\n').collect();
    let candidate_lines: Vec<&[u8]> = candidate.split(|byte| *byte == b'\n').collect();

    if source_lines.len() != candidate_lines.len() {
        return Err(diff_violation(format!(
            "число строк изменилось: {} → {}",
            source_lines.len(),
            candidate_lines.len()
        )));
    }

    let mut observed: Vec<(String, String)> = Vec::new();
    let mut first_changed_line = None;

    for (position, (left, right)) in source_lines.iter().zip(candidate_lines.iter()).enumerate() {
        if left == right {
            continue;
        }

        let before = line_token(left).ok_or_else(|| {
            diff_violation(format!(
                "строка {} исходника не является одиночным строковым токеном",
                position + 1
            ))
        })?;
        let after = line_token(right).ok_or_else(|| {
            diff_violation(format!(
                "строка {} кандидата не является одиночным строковым токеном",
                position + 1
            ))
        })?;

        if before.indent != after.indent || before.comma != after.comma {
            return Err(diff_violation(format!(
                "строка {} изменилась не только значением (отступ {}→{}, запятая {}→{})",
                position + 1,
                before.indent,
                after.indent,
                before.comma,
                after.comma
            )));
        }

        first_changed_line.get_or_insert(position + 1);
        observed.push((before.text, after.text));
    }

    if observed.len() != effective.len() {
        return Err(diff_violation(format!(
            "изменённых строк {}, а эффективных правок {}",
            observed.len(),
            effective.len()
        )));
    }

    let mut expected_pairs: Vec<(String, String)> = effective.to_vec();
    observed.sort_unstable();
    expected_pairs.sort_unstable();

    if observed != expected_pairs {
        let mismatch = observed
            .iter()
            .zip(expected_pairs.iter())
            .find(|(observed, expected)| observed != expected)
            .map(|(observed, expected)| {
                format!(
                    "наблюдалось {:?} → {:?}, ожидалось {:?} → {:?}",
                    observed.0, observed.1, expected.0, expected.1
                )
            })
            .unwrap_or_default();
        return Err(diff_violation(format!(
            "набор изменений не совпал с запрошенным: {mismatch}"
        )));
    }

    Ok(DiffShape {
        changed_lines: observed.len(),
        first_changed_line,
    })
}

/// Строковый токен, занимающий строку канонического JSON.
struct LineToken {
    indent: usize,
    comma: bool,
    text: String,
}

/// Разбирает строку канонического JSON как одно значение поля.
///
/// Возвращает `None`, если строка содержит что-то кроме одного строкового
/// токена и отделяющей запятой.
fn line_token(line: &[u8]) -> Option<LineToken> {
    let indent = line.iter().take_while(|byte| **byte == b' ').count();
    let mut rest = &line[indent..];
    let comma = rest.last() == Some(&b',');
    if comma {
        rest = &rest[..rest.len() - 1];
    }
    if rest.is_empty() {
        return None;
    }

    let text: String = serde_json::from_slice(rest).ok()?;
    Some(LineToken {
        indent,
        comma,
        text,
    })
}

/// Готовит внутреннюю ошибку нарушения байтового инварианта.
fn diff_violation(message: String) -> DomainError {
    DomainError::with_details(
        ErrorCode::Internal,
        format!("нарушен байтовый инвариант правки: {message}"),
        details! {
            "reason" => "diff_shape_violation",
            "message" => message,
        },
    )
}

/// Проверяет, что повторный разбор кандидата даёт ровно задуманное значение.
///
/// Проверка обязательна, а её результат не просто сообщается в отчёте: если
/// канонический рендер и разбор перестали быть взаимно обратными, кандидат
/// описывает не то, что запланировано, и публиковать его нельзя. Молчаливая
/// запись с `candidate_reparsed: false` в отчёте означала бы, что файл заменён
/// байтами с неизвестным содержимым.
///
/// # Errors
///
/// [`ErrorCode::Internal`], если значения разошлись: это признак ошибки в tool,
/// а не в данных, поэтому запись не выполняется.
fn ensure_reparsed(reparsed: &Value, intended: &Value) -> Result<(), DomainError> {
    if reparsed == intended {
        return Ok(());
    }

    Err(DomainError::with_details(
        ErrorCode::Internal,
        "нарушен инвариант правки: кандидат повторно разбирается не в задуманное значение",
        details! {
            "reason" => "candidate_reparse_violation",
        },
    ))
}

/// Проверяет, что прирост байтов объясняется длиной изменённых токенов.
///
/// Проверка обязательна по той же причине, что и [`ensure_reparsed`]: расхождение
/// означает, что байтовый diff изменил больше запрошенного набора изменений.
///
/// # Errors
///
/// [`ErrorCode::Internal`], если прирост байтов не сходится с суммой разниц
/// запрошенных токенов; запись не выполняется.
fn ensure_byte_delta(
    source: &[u8],
    candidate: &[u8],
    effective: &[(String, String)],
) -> Result<(), DomainError> {
    let actual = byte_delta(source, candidate);
    let expected = token_delta(effective);
    if actual == expected {
        return Ok(());
    }

    Err(diff_violation(format!(
        "прирост байтов {actual} не объясняется длиной изменённых токенов {expected}"
    )))
}

/// Готовит отчёт по одной правке.
fn outcome_of(entry: &ResolvedEdit) -> EditOutcome {
    EditOutcome {
        edit_index: entry.edit_index,
        edit_id: entry.edit_id.clone(),
        guid: entry.guid.clone(),
        field: entry.field.clone(),
        field_ord: entry.field_ord,
        note_index: entry.note_position,
        deck_path: entry.deck_path.clone(),
        status: entry.status,
        old_len: entry.current.chars().count(),
        new_len: entry.replacement.chars().count(),
        old_sample: sample(&entry.current),
        new_sample: sample(&entry.replacement),
    }
}

/// Готовит отчёт по одной конфликтующей правке.
fn conflict_of(entry: &ResolvedEdit) -> EditConflict {
    EditConflict {
        edit_index: entry.edit_index,
        edit_id: entry.edit_id.clone(),
        guid: entry.guid.clone(),
        field: entry.field.clone(),
        expected_len: entry.expected.chars().count(),
        current_len: entry.current.chars().count(),
        expected_sample: sample(&entry.expected),
        current_sample: sample(&entry.current),
    }
}

/// Имя типа JSON-значения для диагностики.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Строковое свойство JSON-объекта.
fn string_property(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(ToString::to_string)
}

/// Ссылка на заметку по её позиции в JSON-дереве.
fn note_ref<'a>(root: &'a Value, path: &ValueNotePath) -> Option<&'a Value> {
    let mut current = root;
    for step in &path.children {
        current = current.get("children")?.as_array()?.get(*step)?;
    }
    current.get("notes")?.as_array()?.get(path.note)
}

/// Изменяемая ссылка на заметку по её позиции в JSON-дереве.
fn note_mut<'a>(root: &'a mut Value, path: &ValueNotePath) -> Option<&'a mut Value> {
    let mut current = root;
    for step in &path.children {
        current = current
            .as_object_mut()?
            .get_mut("children")?
            .as_array_mut()?
            .get_mut(*step)?;
    }
    current
        .as_object_mut()?
        .get_mut("notes")?
        .as_array_mut()?
        .get_mut(path.note)
}

/// Готовит внутреннюю ошибку домена.
fn internal(message: impl Into<String>) -> DomainError {
    DomainError::with_details(
        ErrorCode::Internal,
        message,
        details! {
            "reason" => "edit_internal_invariant",
        },
    )
}

/// Структура JSON-запроса.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    schema_version: u32,
    edits: Vec<RequestEdit>,
}

/// Одна правка в JSON-запросе.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEdit {
    #[serde(default)]
    edit_id: Option<String>,
    guid: String,
    field: String,
    expected: String,
    replacement: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Запрос из одной правки.
    fn one(guid: &str, field: &str, expected: &str, replacement: &str) -> EditRequest {
        EditRequest {
            edits: vec![EditSpec {
                edit_id: None,
                guid: guid.to_string(),
                field: field.to_string(),
                expected: expected.to_string(),
                replacement: replacement.to_string(),
            }],
        }
    }

    /// Разрешённая правка с заданной четвёркой значений.
    fn resolved(current: &str, expected: &str, replacement: &str) -> ResolvedEdit {
        ResolvedEdit {
            edit_index: 0,
            edit_id: None,
            guid: "guid-1".to_string(),
            deck_path: "Тестовая колода::Вложенная".to_string(),
            note_position: 0,
            field: "Толкование".to_string(),
            field_ord: 2,
            current: current.to_string(),
            expected: expected.to_string(),
            replacement: replacement.to_string(),
            status: EditStatus::Conflict,
        }
    }

    #[test]
    fn request_rejects_empty_edits() {
        let error =
            validate_request(&EditRequest { edits: Vec::new() }).expect_err("пустой запрос");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "empty_request");
    }

    #[test]
    fn request_rejects_repeated_target() {
        let mut request = one("g", "f", "a", "b");
        request.edits.push(EditSpec {
            edit_id: None,
            guid: "g".to_string(),
            field: "f".to_string(),
            expected: "b".to_string(),
            replacement: "c".to_string(),
        });

        let error = validate_request(&request).expect_err("повтор цели");
        assert_eq!(error.code, ErrorCode::DuplicateEditTarget);
        assert_eq!(error.details["first_edit_index"], 0);
        assert_eq!(error.details["duplicate_edit_index"], 1);
    }

    #[test]
    fn request_allows_same_guid_with_other_field() {
        let mut request = one("g", "f1", "a", "b");
        request.edits.push(EditSpec {
            edit_id: None,
            guid: "g".to_string(),
            field: "f2".to_string(),
            expected: "a".to_string(),
            replacement: "b".to_string(),
        });

        validate_request(&request).expect("разные поля одной заметки допустимы");
    }

    #[test]
    fn request_rejects_repeated_edit_id() {
        let mut request = one("g1", "f", "a", "b");
        request.edits[0].edit_id = Some("same".to_string());
        request.edits.push(EditSpec {
            edit_id: Some("same".to_string()),
            guid: "g2".to_string(),
            field: "f".to_string(),
            expected: "a".to_string(),
            replacement: "b".to_string(),
        });

        let error = validate_request(&request).expect_err("повтор edit_id");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "duplicate_edit_id");
    }

    #[test]
    fn request_rejects_empty_guid_and_field() {
        let guid_error = validate_request(&one("", "f", "a", "b")).expect_err("пустой guid");
        assert_eq!(guid_error.details["reason"], "empty_guid");

        let field_error = validate_request(&one("g", "", "a", "b")).expect_err("пустое поле");
        assert_eq!(field_error.details["reason"], "empty_field");
    }

    #[test]
    fn request_rejects_too_many_edits() {
        let edits = (0..=MAX_EDITS)
            .map(|position| EditSpec {
                edit_id: None,
                guid: format!("guid-{position}"),
                field: "Толкование".to_string(),
                expected: "a".to_string(),
                replacement: "b".to_string(),
            })
            .collect();

        let error = validate_request(&EditRequest { edits }).expect_err("слишком много правок");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "too_many_edits");
        assert_eq!(error.details["max_edits"], MAX_EDITS);
    }

    #[test]
    fn request_file_is_parsed_with_strict_schema() {
        let raw = br#"{"schema_version": 1, "edits": [
            {"edit_id": "e1", "guid": "g", "field": "f", "expected": "a", "replacement": "b"}
        ]}"#;

        let request = parse_request_bytes(raw, "тест").expect("валидный запрос");
        assert_eq!(request.edits.len(), 1);
        assert_eq!(request.edits[0].edit_id.as_deref(), Some("e1"));

        let unknown = br#"{"schema_version": 1, "edits": [
            {"guid": "g", "field": "f", "expected": "a", "replacement": "b", "extra": 1}
        ]}"#;
        let error = parse_request_bytes(unknown, "тест").expect_err("неизвестный ключ");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "malformed_request");

        let missing = br#"{"schema_version": 1, "edits": [{"guid": "g", "field": "f"}]}"#;
        let error = parse_request_bytes(missing, "тест").expect_err("нет expected");
        assert_eq!(error.details["reason"], "malformed_request");

        let not_json = b"{";
        let error = parse_request_bytes(not_json, "тест").expect_err("не JSON");
        assert_eq!(error.details["reason"], "malformed_request");
    }

    #[test]
    fn request_file_rejects_unsupported_schema_version() {
        let raw = br#"{"schema_version": 2, "edits": [
            {"guid": "g", "field": "f", "expected": "a", "replacement": "b"}
        ]}"#;

        let error = parse_request_bytes(raw, "тест").expect_err("чужая версия");
        assert_eq!(error.details["reason"], "unsupported_schema_version");
        assert_eq!(error.details["expected"], SUPPORTED_REQUEST_SCHEMA_VERSION);
    }

    #[test]
    fn request_file_rejects_oversized_payload() {
        let raw = vec![b' '; MAX_REQUEST_BYTES + 1];

        let error = parse_request_bytes(&raw, "тест").expect_err("слишком большой запрос");
        assert_eq!(error.details["reason"], "request_too_large");
    }

    #[test]
    fn classification_covers_all_four_cases() {
        // current == expected == replacement: менять нечего ни в одном режиме.
        assert_eq!(
            classify(&resolved("a", "a", "a"), true),
            EditStatus::NoopIdentical
        );
        assert_eq!(
            classify(&resolved("a", "a", "a"), false),
            EditStatus::NoopIdentical
        );

        // current == expected != replacement: эффективная правка.
        assert_eq!(
            classify(&resolved("a", "a", "b"), true),
            EditStatus::Applied
        );
        assert_eq!(
            classify(&resolved("a", "a", "b"), false),
            EditStatus::DryRun
        );

        // current == replacement != expected: уже применено, успех без записи.
        assert_eq!(
            classify(&resolved("b", "a", "b"), true),
            EditStatus::AlreadyApplied
        );

        // Ничего не совпало: конфликт.
        assert_eq!(
            classify(&resolved("c", "a", "b"), true),
            EditStatus::Conflict
        );
        assert_eq!(
            classify(&resolved("c", "a", "b"), false),
            EditStatus::Conflict
        );
    }

    #[test]
    fn only_effective_statuses_change_the_document() {
        assert!(EditStatus::Applied.is_effective());
        assert!(EditStatus::DryRun.is_effective());
        assert!(!EditStatus::NoopIdentical.is_effective());
        assert!(!EditStatus::AlreadyApplied.is_effective());
        assert!(!EditStatus::Conflict.is_effective());
    }

    #[test]
    fn line_token_accepts_only_single_string_values() {
        let with_comma = line_token(b"                \"\xd0\xb7\",".as_slice()).expect("токен");
        assert_eq!(with_comma.indent, 16);
        assert!(with_comma.comma);
        assert_eq!(with_comma.text, "з");

        let last = line_token(b"                \"\xd0\xb7\"".as_slice()).expect("токен");
        assert!(!last.comma);
        assert_eq!(last.text, "з");

        assert!(line_token(b"").is_none());
        assert!(line_token(b"                ").is_none());
        assert!(line_token(b"            \"fields\": [").is_none());
        assert!(line_token(b"                42,").is_none());
        assert!(line_token(b"                [],").is_none());
    }

    #[test]
    fn diff_shape_accepts_exactly_requested_change() {
        let source = "{\n    \"fields\": [\n        \"a\",\n        \"b\"\n    ]\n}";
        let candidate = "{\n    \"fields\": [\n        \"a\",\n        \"b!\"\n    ]\n}";

        let shape = check_diff_shape(
            source.as_bytes(),
            candidate.as_bytes(),
            &[("b".to_string(), "b!".to_string())],
        )
        .expect("форма верна");

        assert_eq!(shape.changed_lines, 1);
        assert_eq!(shape.first_changed_line, Some(4));
    }

    #[test]
    fn diff_shape_rejects_extra_or_missing_changes() {
        let source = "{\n    \"fields\": [\n        \"a\",\n        \"b\"\n    ]\n}";
        let candidate = "{\n    \"fields\": [\n        \"a!\",\n        \"b!\"\n    ]\n}";

        let error = check_diff_shape(
            source.as_bytes(),
            candidate.as_bytes(),
            &[("b".to_string(), "b!".to_string())],
        )
        .expect_err("лишнее изменение");
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.details["reason"], "diff_shape_violation");

        let error = check_diff_shape(
            source.as_bytes(),
            source.as_bytes(),
            &[("b".to_string(), "b!".to_string())],
        )
        .expect_err("изменения нет");
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[test]
    fn diff_shape_rejects_wrong_new_value() {
        let source = "{\n    \"fields\": [\"a\"]\n}";
        let candidate = "{\n    \"fields\": [\"c\"]\n}";

        let error = check_diff_shape(
            source.as_bytes(),
            candidate.as_bytes(),
            &[("a".to_string(), "b".to_string())],
        )
        .expect_err("не то значение");
        assert_eq!(error.details["reason"], "diff_shape_violation");
    }

    #[test]
    fn diff_shape_rejects_changed_line_geometry() {
        let source = "{\n    \"fields\": [\"a\"]\n}";
        let candidate = "{\n    \"fields\": [\"a\", \"b\"]\n}";

        let error = check_diff_shape(
            source.as_bytes(),
            candidate.as_bytes(),
            &[("a".to_string(), "\"a\", \"b\"".to_string())],
        )
        .expect_err("изменилась геометрия строки");
        assert_eq!(error.details["reason"], "diff_shape_violation");
    }

    #[test]
    fn diff_shape_reports_no_change_for_identical_bytes() {
        let source = "{\n    \"fields\": [\"a\"]\n}";

        let shape =
            check_diff_shape(source.as_bytes(), source.as_bytes(), &[]).expect("нет изменений");

        assert_eq!(shape.changed_lines, 0);
        assert_eq!(shape.first_changed_line, None);
    }

    #[test]
    fn token_delta_matches_rendered_lengths() {
        let effective = vec![("a".to_string(), "abcd".to_string())];
        // Обе стороны в кавычках, поэтому разница равна разнице длин значений.
        assert_eq!(token_delta(&effective), 3);

        let escaped = vec![("a".to_string(), "\n".to_string())];
        // "\n" кодируется двумя байтами экранирования вместо одного.
        assert_eq!(token_delta(&escaped), 1);
        assert_eq!(token_bytes("\n"), 4);
    }

    #[test]
    fn byte_delta_is_signed() {
        assert_eq!(byte_delta(b"abc", b"abcde"), 2);
        assert_eq!(byte_delta(b"abcde", b"abc"), -2);
        assert_eq!(byte_delta(b"", b""), 0);
    }

    #[test]
    fn reparse_guard_accepts_equal_values() {
        let intended = json!({ "notes": [{ "fields": ["a"] }] });
        assert!(ensure_reparsed(&intended.clone(), &intended).is_ok());
    }

    #[test]
    fn reparse_guard_refuses_divergent_value() {
        let intended = json!({ "notes": [{ "fields": ["a"] }] });
        let reparsed = json!({ "notes": [{ "fields": ["b"] }] });

        let error = ensure_reparsed(&reparsed, &intended).expect_err("расхождение обязательно");

        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.exit_code(), 70);
        assert_eq!(error.details["reason"], "candidate_reparse_violation");
    }

    #[test]
    fn byte_delta_guard_accepts_explained_growth() {
        let effective = vec![("a".to_string(), "abcd".to_string())];
        let source = br#"{"v": "a"}"#;
        let candidate = br#"{"v": "abcd"}"#;

        assert!(ensure_byte_delta(source, candidate, &effective).is_ok());
    }

    #[test]
    fn byte_delta_guard_refuses_unexplained_growth() {
        let effective = vec![("a".to_string(), "ab".to_string())];
        // Кандидат длиннее ровно на байт, но запрошенный токен обещает прирост 1,
        // а байты меняются вне изменённого значения.
        let source = br#"{"v": "a", "w": ""}"#;
        let candidate = br#"{"v": "ab", "w": ""}"#;
        assert_eq!(byte_delta(source, candidate), token_delta(&effective));

        // Ломаем соответствие: рост есть, а эффективных правок нет.
        let error = ensure_byte_delta(source, candidate, &[])
            .expect_err("необъяснённый прирост обязателен к отказу");

        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.exit_code(), 70);
        assert_eq!(error.details["reason"], "diff_shape_violation");
    }

    #[test]
    fn first_difference_finds_offset_or_shorter_length() {
        assert_eq!(first_difference(b"abc", b"abd"), 2);
        assert_eq!(first_difference(b"abc", b"abc"), 3);
        assert_eq!(first_difference(b"abc", b"abcdef"), 3);
    }

    #[test]
    fn sample_is_bounded_and_single_line() {
        assert_eq!(sample("короткое"), "короткое");
        assert_eq!(sample("а\nб"), "а\\nб");
        assert_eq!(sample("а\tб"), "а\\tб");
        assert_eq!(sample("\u{1}"), "·");

        let long = "я".repeat(VALUE_SAMPLE_CHARS + 10);
        let truncated = sample(&long);
        assert_eq!(truncated.chars().count(), VALUE_SAMPLE_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn note_paths_address_nested_children() {
        let mut document: Value = serde_json::from_str(
            r#"{
                "notes": [{"guid": "root"}],
                "children": [
                    {"notes": [], "children": []},
                    {"notes": [{"guid": "leaf"}], "children": []}
                ]
            }"#,
        )
        .expect("JSON");

        let notes = collect_value_notes(&document).expect("обход");
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].guid.as_deref(), Some("root"));
        assert_eq!(notes[1].guid.as_deref(), Some("leaf"));
        assert_eq!(
            notes[1].path,
            ValueNotePath {
                children: vec![1],
                note: 0
            }
        );

        let leaf = note_ref(&document, &notes[1].path).expect("ссылка");
        assert_eq!(leaf["guid"], "leaf");

        let leaf_mut = note_mut(&mut document, &notes[1].path).expect("ссылка");
        assert_eq!(leaf_mut["guid"], "leaf");
    }

    #[test]
    fn missing_notes_and_children_behave_like_empty_arrays() {
        let document: Value = serde_json::from_str("{}").expect("JSON");
        assert!(collect_value_notes(&document).expect("обход").is_empty());

        let nulls: Value =
            serde_json::from_str(r#"{"notes": null, "children": null}"#).expect("JSON");
        assert!(collect_value_notes(&nulls).expect("обход").is_empty());
    }

    #[test]
    fn malformed_note_containers_are_internal_errors() {
        let wrong_notes: Value = serde_json::from_str(r#"{"notes": 7}"#).expect("JSON");
        let error = collect_value_notes(&wrong_notes).expect_err("notes не массив");
        assert_eq!(error.code, ErrorCode::Internal);

        let wrong_child: Value = serde_json::from_str(r#"{"children": [1]}"#).expect("JSON");
        let error = collect_value_notes(&wrong_child).expect_err("узел не объект");
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[test]
    fn problems_error_prefers_the_most_specific_code() {
        let problem = |kind, code| Problem {
            edit_index: 0,
            guid: "g".to_string(),
            field: "f".to_string(),
            code,
            kind,
            message: "m".to_string(),
        };
        let path = Path::new("decks/x");

        let unknown = problems_error(&[problem(ProblemKind::UnknownField, "unknown_field")], path);
        assert_eq!(unknown.code, ErrorCode::UnknownField);
        assert_eq!(unknown.exit_code(), 3);

        let missing = problems_error(
            &[problem(ProblemKind::NoteNotFound, "note_not_found")],
            path,
        );
        assert_eq!(missing.code, ErrorCode::NoteNotFound);
        assert_eq!(missing.exit_code(), 4);

        let mixed = problems_error(
            &[
                problem(ProblemKind::UnknownField, "unknown_field"),
                problem(ProblemKind::NoteNotFound, "note_not_found"),
            ],
            path,
        );
        assert_eq!(mixed.code, ErrorCode::NoteNotFound);
        assert_eq!(mixed.details["problems_total"], 2);

        let ambiguous = problems_error(
            &[
                problem(ProblemKind::NoteNotFound, "note_not_found"),
                problem(ProblemKind::Ambiguous, "ambiguous"),
            ],
            path,
        );
        assert_eq!(ambiguous.code, ErrorCode::Ambiguous);
    }
}
