//! Создание заметок в существующей колоде существующей модели.
//!
//! Что здесь принципиально:
//!
//! - **Никакой семантики из имени поля.** Имена полей берутся из фактической
//!   модели экспорта, значения собираются в порядке `flds[].ord`. Если схема
//!   модели непригодна, операция отказывается работать, а не угадывает.
//! - **Разрешение модели — отдельный шаг со свидетельствами.** Режим `auto`
//!   выбирает модель только по структурным свидетельствам (совместимость схемы
//!   с набором переданных полей и фактическое использование в целевой колоде) и
//!   падает при неоднозначности. См. [`crate::ops::models`].
//! - **Opt-in media.** Routing по UUID и точному полю принадлежит create_media;
//!   разрешены только явно запрошенные canonical VERIFIED kanji img/src.
//! - **Строгий proof.** Сначала доказываются добавленные notes, затем точные
//!   additions media_files. Existing notes и unrelated JSON не меняются.
//! - **`guid` — идентичность.** Отсутствующий `guid` генерируется в формате Anki
//!   (base91, см. [`crate::guid`]) и попадает в `--emit-resolved`, поэтому
//!   повторный прогон разрешённого запроса идемпотентен: заметка с тем же
//!   `guid` и тем же содержимым получает статус `already_applied`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::guid;
use crate::index::ExportIndex;
use crate::loader;
use crate::model::FieldValue;
use crate::ops::create_media::{self, MediaOptions, MediaPlan, Pin, Reference, Routing};
use crate::ops::deck_select::DeckSelector;
use crate::ops::models::{self, ModelMode, ModelSelector, ResolvedModel};
use crate::ops::publish;
use crate::ops::retire;
use crate::ops::source::{
    EditableSource, ValidationDelta, deck_child_paths, internal, load_editable_source, node_mut,
    validation_delta,
};
use crate::ops::structural::{DEFAULT_PATH_LIMIT, changed_paths, deck_path_pointer};
use crate::paths;
use crate::write;

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
/// Тип сущности CrowdAnki для новой заметки.
pub const NOTE_TYPE_NAME: &str = "Note";

/// Одна заказываемая заметка в разобранном виде.
#[derive(Debug, Clone)]
pub struct CreateSpec {
    /// Необязательный стабильный идентификатор для отчёта.
    pub note_id: Option<String>,
    /// Запрошенный `guid`; отсутствие означает генерацию.
    pub guid: Option<String>,
    /// Селектор целевой колоды.
    pub deck: DeckSelector,
    /// Селектор модели заметок.
    pub model: ModelSelector,
    /// Значения полей по именам полей модели.
    pub fields: BTreeMap<String, String>,
    /// Теги новой заметки.
    pub tags: Vec<String>,
}

/// Проверенный запрос на создание заметок.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    /// Заказываемые заметки в порядке запроса.
    pub notes: Vec<CreateSpec>,
    /// Ожидаемые canonical identity/filename/hash выбранного corpus.
    pub media_assets: Vec<Pin>,
}

/// Итог работы с одной заказываемой заметкой.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateStatus {
    /// Заметка добавлена в `deck.json`.
    Created,
    /// Заметка добавлена только в кандидата: файл не тронут.
    DryRun,
    /// Заметка с этим `guid` и этим содержимым уже есть в экспорте.
    AlreadyApplied,
}

impl CreateStatus {
    /// Стабильное machine-readable имя статуса.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::DryRun => "dry_run",
            Self::AlreadyApplied => "already_applied",
        }
    }
}

/// Отчёт по одной заказываемой заметке.
#[derive(Debug, Clone)]
pub struct CreateOutcome {
    /// Позиция в запросе (с нуля).
    pub note_index: usize,
    /// Идентификатор заметки из запроса.
    pub note_id: Option<String>,
    /// Итоговый `guid`.
    pub guid: String,
    /// `guid` был сгенерирован, а не задан в запросе.
    pub guid_generated: bool,
    /// Итоговый статус.
    pub status: CreateStatus,
    /// Полное имя целевой колоды.
    pub deck_path: String,
    /// Идентичность целевой колоды.
    pub deck_uuid: String,
    /// Способ разрешения модели.
    pub model_mode: ModelMode,
    /// Идентичность модели.
    pub model_uuid: String,
    /// Имя модели.
    pub model_name: String,
    /// Какими свидетельствами модель была выбрана.
    pub model_evidence: String,
    /// Число полей заметки.
    pub fields_total: usize,
    /// Имена полей в порядке `ord`.
    pub field_names: Vec<String>,
    /// Теги заметки.
    pub tags: Vec<String>,
    /// Сколько разрешённых media-ссылок найдено в новых значениях.
    pub media_references: usize,
    /// Поля, для которых matched policy включает processor.
    pub processor_fields: Vec<String>,
}

/// Целевой узел, в который дописываются заметки.
#[derive(Debug, Clone)]
pub struct DeckTouch {
    /// Полное имя колоды.
    pub deck_path: String,
    /// Идентичность колоды.
    pub deck_uuid: String,
    /// Сколько заметок было в узле до операции.
    pub notes_before: usize,
    /// Сколько заметок добавлено.
    pub notes_added: usize,
}

/// Выполненные проверки консистентности.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateChecks {
    /// Исходник был в канонической форме.
    pub source_canonical: bool,
    /// Кандидат повторно разбирается в задуманное значение.
    pub candidate_reparsed: bool,
    /// Разрешённая модель предъявила свидетельства о схеме полей.
    pub model_resolution_evidenced: bool,
    /// Новые значения полей не содержат media-ссылок.
    pub media_references_absent: bool,
    /// `guid` новых заметок не конфликтуют с существующими заметками.
    pub guids_resolved_without_conflict: bool,
    /// Структурные различия — ровно добавленные заметки.
    pub only_notes_appended: bool,
    /// Разрешены ровно добавленные notes и ожидаемые additions media_files.
    pub only_notes_and_media_files_appended: bool,
    /// Каждая media reference разрешена в проверенные canonical bytes.
    pub media_assets_verified: bool,
    /// Значения добавленных заметок совпали с задуманными.
    pub appended_notes_verified: bool,
}

/// Результат `create`.
#[derive(Debug, Clone)]
pub struct CreateResult {
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
    /// Сколько заметок было в запросе.
    pub notes_total: usize,
    /// Сколько заметок реально добавлено (или будет добавлено).
    pub notes_created: usize,
    /// Сколько заметок уже присутствовало.
    pub notes_already_applied: usize,
    /// Затронутые целевые узлы в порядке первого появления в запросе.
    pub decks_touched: Vec<DeckTouch>,
    /// Отчёт по заметкам (обрезан до [`MAX_REPORTED_NOTES`]).
    pub outcomes: Vec<CreateOutcome>,
    /// Был ли отчёт по заметкам обрезан.
    pub outcomes_truncated: bool,
    /// Сравнение валидации до и после.
    pub validation: ValidationDelta,
    /// Выполненные проверки консистентности.
    pub checks: CreateChecks,
    /// Разрешённый запрос: те же заметки с явными `guid`, колодой и моделью.
    ///
    /// Это документ, который принимает та же команда без переупаковки, поэтому
    /// его можно записать в файл и применить повторно. В stdout он не попадает:
    /// выводом владеет [`crate::render`], а не операция.
    pub resolved_request: Value,
    /// Media resolution и фактическая materialization отдельно от note status.
    pub media: MediaPlan,
}

/// Форма документа запроса на проводе.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFile {
    schema_version: u32,
    notes: Vec<WireNote>,
    #[serde(default)]
    media_assets: Vec<Pin>,
}

/// Одна заказываемая заметка на проводе.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNote {
    #[serde(default)]
    note_id: Option<String>,
    #[serde(default)]
    guid: Option<String>,
    #[serde(default)]
    deck: Option<DeckSelector>,
    #[serde(default)]
    model: ModelSelector,
    fields: BTreeMap<String, String>,
    #[serde(default)]
    tags: Vec<String>,
}

/// Разбирает JSON-запрос на создание заметок.
///
/// # Errors
///
/// [`ErrorCode::InvalidRequest`] для слишком большого, нечитаемого или
/// структурно некорректного запроса.
pub fn parse_request_bytes(raw: &[u8], label: &str) -> Result<CreateRequest, DomainError> {
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

    let request = CreateRequest {
        media_assets: file.media_assets,
        notes: file
            .notes
            .into_iter()
            .map(|note| CreateSpec {
                note_id: note.note_id,
                guid: note.guid,
                deck: note.deck.unwrap_or_default(),
                model: note.model,
                fields: note.fields,
                tags: note.tags,
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
/// отсутствующего селектора колоды, пустого `guid`, некорректного тега,
/// повторяющегося `note_id` и [`ErrorCode::GuidCollision`] для повторяющегося
/// `guid`.
pub fn validate_request(request: &CreateRequest) -> Result<(), DomainError> {
    if request.notes.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "запрос на создание не содержит ни одной заметки",
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
        note.deck
            .ensure_present(&format!("заметка #{position}"))
            .map_err(|error| request_error(position, error))?;

        if note.fields.is_empty() {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("заметка #{position} не задаёт ни одного значения поля"),
                details! {
                    "reason" => "empty_fields",
                    "note_index" => position,
                },
            ));
        }

        if let Some(guid) = note.guid.as_deref() {
            if !guid::is_valid(guid) {
                return Err(DomainError::with_details(
                    ErrorCode::InvalidRequest,
                    format!(
                        "заметка #{position}: guid {guid:?} не является форматом Anki \
                         (base91, от 1 до {} символов)",
                        guid::MAX_GUID_CHARS
                    ),
                    details! {
                        "reason" => "invalid_guid",
                        "note_index" => position,
                        "guid" => guid,
                    },
                ));
            }
            if let Some(previous) = guids.insert(guid, position) {
                return Err(DomainError::with_details(
                    ErrorCode::GuidCollision,
                    format!("guid {guid:?} заказан дважды: заметки #{previous} и #{position}"),
                    details! {
                        "guid" => guid,
                        "first_note_index" => previous,
                        "duplicate_note_index" => position,
                    },
                ));
            }
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

        let mut seen: Vec<&str> = Vec::new();
        for tag in &note.tags {
            validate_tag(tag, position)?;
            // Повтор ищется тем же правилом, что и у Anki: теги не различают
            // регистр, поэтому `["Дом", "дом"]` — это повтор, а не два тега.
            if seen.iter().any(|existing| retire::same_tag(existing, tag)) {
                return Err(DomainError::with_details(
                    ErrorCode::InvalidRequest,
                    format!("заметка #{position}: тег {tag:?} повторяется"),
                    details! {
                        "reason" => "duplicate_tag",
                        "note_index" => position,
                        "tag" => tag,
                    },
                ));
            }
            seen.push(tag);
        }
    }

    Ok(())
}

/// Переносит ошибку селектора колоды в контекст заметки.
fn request_error(note_index: usize, error: DomainError) -> DomainError {
    DomainError::with_details(
        error.code,
        format!("заметка #{note_index}: {}", error.message),
        details! {
            "reason" => "missing_deck_selector",
            "note_index" => note_index,
        },
    )
}

/// Проверяет тег Anki: непустой, без пробельных символов и управляющих кодов.
///
/// Anki разделяет теги пробелами, поэтому тег с пробелом внутри — это не один
/// тег, а два. Инструмент не имеет права молча «исправить» это.
fn validate_tag(tag: &str, note_index: usize) -> Result<(), DomainError> {
    let bad = tag.is_empty()
        || tag
            .chars()
            .any(|character| character.is_whitespace() || character.is_control());
    if bad {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("заметка #{note_index}: тег {tag:?} пуст или содержит пробельные символы"),
            details! {
                "reason" => "invalid_tag",
                "note_index" => note_index,
                "tag" => tag,
            },
        ));
    }
    Ok(())
}

/// Заказывает создание заметок по запросу.
///
/// `resolved_artifact` — необязательный путь, по которому команда обязана
/// оставить разрешённый запрос. Он публикуется **до** первой мутации экспорта, и
/// это часть контракта, а не деталь реализации: отказ его записи обязан
/// оставлять `deck.json` исходным, а успешный `--apply` — совместимым с уже
/// лежащим на диске разрешённым запросом. Поэтому же запрещена запись артефакта
/// поверх самого `deck.json`: это уничтожило бы экспорт уже после того, как
/// безопасная публикация прошла.
///
/// # Errors
///
/// Ошибки чтения исходника, разрешения колоды и модели, media-ссылок в новых
/// значениях, конфликта `guid`, проверки кандидата, записи файла и записи
/// resolved-артефакта.
pub fn create(
    export_dir: &Path,
    request: &CreateRequest,
    apply: bool,
    resolved_artifact: Option<&Path>,
) -> Result<CreateResult, DomainError> {
    create_with_options(
        export_dir,
        request,
        apply,
        resolved_artifact,
        &MediaOptions::default(),
    )
}

/// Создание с явным isolated policy/store override.
pub fn create_with_options(
    export_dir: &Path,
    request: &CreateRequest,
    apply: bool,
    resolved_artifact: Option<&Path>,
    options: &MediaOptions,
) -> Result<CreateResult, DomainError> {
    validate_request(request)?;
    let source = load_editable_source(export_dir)?;
    let routing = Routing::load(export_dir, options)?;
    let index = ExportIndex::build(&source.root);
    let child_paths = deck_child_paths(&source.value)?;

    let mut references = Vec::new();
    let mut planned: Vec<PlannedNote> = Vec::new();
    for (note_index, spec) in request.notes.iter().enumerate() {
        planned.push(plan_note(
            &index,
            &child_paths,
            note_index,
            spec,
            apply,
            &routing,
            &mut references,
        )?);
    }

    let mut media_plan = routing.resolve(export_dir, references, &request.media_assets)?;

    let mut duplicate_guids: BTreeMap<&str, usize> = BTreeMap::new();
    for note in &planned {
        if let Some(previous) = duplicate_guids.insert(note.guid.as_str(), note.note_index) {
            return Err(DomainError::with_details(
                ErrorCode::GuidCollision,
                format!(
                    "guid {:?} заказан дважды: заметки #{previous} и #{}",
                    note.guid, note.note_index
                ),
                details! {
                    "guid" => note.guid.as_str(),
                    "first_note_index" => previous,
                    "duplicate_note_index" => note.note_index,
                },
            ));
        }
    }

    let mut ranges: Vec<AppendRange> = Vec::new();
    let mut buckets: Vec<Vec<Value>> = Vec::new();
    let mut outcomes: Vec<CreateOutcome> = Vec::with_capacity(planned.len());

    for note in &planned {
        let status = match existing_note(&index, note) {
            Some(Existing::Identical) => CreateStatus::AlreadyApplied,
            Some(Existing::Different { reason, detail }) => {
                return Err(DomainError::with_details(
                    ErrorCode::GuidConflict,
                    format!(
                        "заметка #{}: guid {:?} уже занят заметкой с другим содержимым ({reason})",
                        note.note_index, note.guid
                    ),
                    details! {
                        "guid" => note.guid.as_str(),
                        "note_index" => note.note_index,
                        "conflict" => reason,
                        "detail" => detail,
                        "existing_deck_path" => existing_deck_path(&index, note),
                    },
                ));
            }
            Some(Existing::Duplicated(count)) => {
                return Err(DomainError::with_details(
                    ErrorCode::GuidCollision,
                    format!(
                        "заметка #{}: guid {:?} встречается в экспорте {count} раз, \
                         идентичность не уникальна",
                        note.note_index, note.guid
                    ),
                    details! {
                        "guid" => note.guid.as_str(),
                        "note_index" => note.note_index,
                        "occurrences" => count,
                    },
                ));
            }
            None => {
                let position = match ranges
                    .iter()
                    .position(|range| range.children == note.deck_children)
                {
                    Some(position) => position,
                    None => {
                        ranges.push(AppendRange {
                            children: note.deck_children.clone(),
                            deck_path: note.deck_path.clone(),
                            notes_before: note.notes_before,
                            added: 0,
                        });
                        buckets.push(Vec::new());
                        ranges.len() - 1
                    }
                };
                ranges[position].added += 1;
                buckets[position].push(note.value.clone());

                if apply {
                    CreateStatus::Created
                } else {
                    CreateStatus::DryRun
                }
            }
        };

        outcomes.push(note.outcome(status));
    }

    let notes_created = outcomes
        .iter()
        .filter(|outcome| outcome.status != CreateStatus::AlreadyApplied)
        .count();
    let notes_already_applied = outcomes.len() - notes_created;

    let mut resolved_request = resolved_document(&planned);
    if !media_plan.is_empty() {
        resolved_request["media_assets"] =
            serde_json::to_value(media_plan.pins()).map_err(|e| internal(e.to_string()))?;
    }
    let mut notes_value = source.value.clone();
    for (range, bucket) in ranges.iter().zip(&buckets) {
        append_notes(&mut notes_value, &range.children, bucket)?;
    }
    verify_only_notes_appended(&source.value, &notes_value, &ranges, &buckets)?;
    let mut candidate_value = notes_value.clone();
    add_media_declarations(&mut candidate_value, &index, &mut media_plan)?;
    verify_media_additions(
        &notes_value,
        &candidate_value,
        &media_plan.declarations_added,
    )?;
    let changed = candidate_value != source.value;
    let candidate = if changed {
        Some(publish::prepare(&source, export_dir, candidate_value)?)
    } else {
        None
    };
    protect_resolved(resolved_artifact, export_dir, &media_plan)?;
    routing.protect_artifact(resolved_artifact)?;
    commit_resolved(resolved_artifact, &resolved_request, &source)?;
    if apply && (changed || !media_plan.is_empty()) {
        create_media::checkpoint("before_export_lock")?;
        let guard = write::ExportLock::acquire(export_dir)?;
        guard.check_source(&source.deck_json, &source.source)?;
        if !media_plan.is_empty() {
            media_plan.materialize(&guard)?;
        }
        create_media::checkpoint("before_deck_publish")?;
        if let Some(candidate) = &candidate {
            guard.replace(&source.deck_json, &source.source, &candidate.bytes)?;
        }
    }
    let applied = apply && changed;
    let validation = candidate.as_ref().map_or_else(
        || validation_delta(&source.before, &source.before),
        |c| c.validation.clone(),
    );

    let candidate_bytes = candidate
        .as_ref()
        .map_or(source.source.len(), |candidate| candidate.bytes.len());

    // Проверки записи — свидетельство, а не второй канал ошибки: провал любой из
    // них прерывает команду отказом (см. `ensure_reparsed` и родственные), поэтому
    // в успешном результате они перечислены как прошедшие. Повторный прогон, в
    // котором писать нечего, тоже успешен: `false` в этих полях читалось бы как
    // провал обязательной проверки, хотя файл остался ровно тем же.

    Ok(CreateResult {
        export_dir: export_dir.to_path_buf(),
        deck_json: source.deck_json.clone(),
        dry_run: !apply,
        applied,
        source_bytes: source.source.len(),
        candidate_bytes,
        byte_delta: candidate_bytes as i64 - source.source.len() as i64,
        notes_total: request.notes.len(),
        notes_created,
        notes_already_applied,
        decks_touched: ranges
            .iter()
            .map(|range| DeckTouch {
                deck_path: range.deck_path.clone(),
                deck_uuid: planned
                    .iter()
                    .find(|note| note.deck_children == range.children)
                    .and_then(|note| note.deck_uuid.clone())
                    .unwrap_or_default(),
                notes_before: range.notes_before,
                notes_added: range.added,
            })
            .collect(),
        outcomes_truncated: outcomes.len() > MAX_REPORTED_NOTES,
        outcomes: outcomes.into_iter().take(MAX_REPORTED_NOTES).collect(),
        validation,
        checks: CreateChecks {
            source_canonical: true,
            candidate_reparsed: true,
            model_resolution_evidenced: true,
            media_references_absent: media_plan.is_empty(),
            guids_resolved_without_conflict: true,
            only_notes_appended: media_plan.declarations_added.is_empty(),
            only_notes_and_media_files_appended: true,
            media_assets_verified: true,
            appended_notes_verified: true,
        },
        resolved_request,
        media: media_plan,
    })
}

fn add_media_declarations(
    value: &mut Value,
    index: &ExportIndex<'_>,
    plan: &mut MediaPlan,
) -> Result<(), DomainError> {
    if plan.is_empty() {
        return Ok(());
    }
    let mut declared = BTreeSet::new();
    let required: BTreeSet<_> = plan.filenames().into_iter().collect();
    let mut counts = BTreeMap::new();
    for node in &index.nodes {
        for name in &node.node.media_files {
            *counts.entry(name.clone()).or_insert(0usize) += 1;
            declared.insert(name.clone());
        }
    }
    if let Some(name) = required
        .iter()
        .find(|n| counts.get(*n).is_some_and(|c| *c > 1))
    {
        return Err(create_media::blocker(
            ErrorCode::InvalidRequest,
            "media_declaration_conflict",
            json!({"filename": name}),
        ));
    }
    plan.declarations_added = plan
        .filenames()
        .into_iter()
        .filter(|n| !declared.contains(n))
        .collect();
    if plan.declarations_added.is_empty() {
        return Ok(());
    }
    let map = value
        .as_object_mut()
        .ok_or_else(|| internal("корень не object"))?;
    let array = map
        .entry("media_files")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| internal("media_files не array"))?;
    array.extend(plan.declarations_added.iter().map(|n| json!(n)));
    Ok(())
}

fn verify_media_additions(
    before: &Value,
    after: &Value,
    additions: &[String],
) -> Result<(), DomainError> {
    let mut expected = before.clone();
    if !additions.is_empty() {
        let array = expected
            .as_object_mut()
            .ok_or_else(|| internal("корень не object"))?
            .entry("media_files")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| internal("media_files не array"))?;
        array.extend(additions.iter().map(|n| json!(n)));
    }
    if &expected != after {
        return Err(internal("неожиданное изменение вне добавлений media_files"));
    }
    Ok(())
}

fn protect_resolved(
    path: Option<&Path>,
    export: &Path,
    plan: &MediaPlan,
) -> Result<(), DomainError> {
    let Some(path) = path else {
        return Ok(());
    };
    // Проверка media здесь; policy и corpus защищает Routing.
    let absolute = paths::canonical_ish(path);
    let media_dir = paths::canonical_ish(&export.join("media"));
    if absolute.starts_with(&media_dir)
        || plan
            .filenames()
            .iter()
            .any(|n| paths::paths_alias(path, &export.join("media").join(n)))
    {
        return Err(create_media::blocker(
            ErrorCode::InvalidRequest,
            "emit_resolved_protected_path",
            json!({}),
        ));
    }
    Ok(())
}

/// Публикует разрешённый запрос по заказанному пути.
///
/// Проверка алиаса обязана стоять здесь, а не в CLI: путь приходит из аргументов
/// и может указывать на тот же файл, что и `deck.json`, — напрямую, через
/// `..` или через символическую ссылку. Запись поверх экспорта после успешной
/// публикации уничтожила бы его, поэтому отказ выдаётся до любых изменений.
fn commit_resolved(
    resolved_artifact: Option<&Path>,
    resolved_request: &Value,
    source: &EditableSource,
) -> Result<(), DomainError> {
    let Some(path) = resolved_artifact else {
        return Ok(());
    };

    if paths::paths_alias(path, &source.deck_json) {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "--emit-resolved указывает на сам {}: разрешённый запрос не может \
                 перезаписать экспорт, который эта же команда публикует",
                source.deck_json.display()
            ),
            details! {
                "reason" => "emit_resolved_aliases_source",
                "path" => path.display().to_string(),
                "deck_json" => source.deck_json.display().to_string(),
            },
        ));
    }

    let bytes = loader::render_canonical_bytes(resolved_request)?;
    write::replace_document_atomically(path, &bytes)?;
    Ok(())
}

/// Заказанная заметка после разрешения колоды, модели и `guid`.
#[derive(Debug, Clone)]
struct PlannedNote {
    note_index: usize,
    note_id: Option<String>,
    guid: String,
    guid_generated: bool,
    deck_path: String,
    deck_uuid: Option<String>,
    deck_preorder: usize,
    deck_children: Vec<usize>,
    notes_before: usize,
    model: ResolvedModel,
    field_names: Vec<String>,
    values: Vec<(String, String)>,
    tags: Vec<String>,
    value: Value,
    processor_fields: Vec<String>,
    media_references: usize,
}

impl PlannedNote {
    fn outcome(&self, status: CreateStatus) -> CreateOutcome {
        CreateOutcome {
            note_index: self.note_index,
            note_id: self.note_id.clone(),
            guid: self.guid.clone(),
            guid_generated: self.guid_generated,
            status,
            deck_path: self.deck_path.clone(),
            deck_uuid: self.deck_uuid.clone().unwrap_or_default(),
            model_mode: self.model.mode,
            model_uuid: self.model.crowdanki_uuid.clone(),
            model_name: self.model.name.clone(),
            model_evidence: self.model.evidence.clone(),
            fields_total: self.values.len(),
            field_names: self.field_names.clone(),
            tags: self.tags.clone(),
            media_references: self.media_references,
            processor_fields: self.processor_fields.clone(),
        }
    }
}

/// Разрешает одну заметку: колода, модель, значения полей, `guid`.
fn plan_note(
    index: &ExportIndex<'_>,
    child_paths: &[Vec<usize>],
    note_index: usize,
    spec: &CreateSpec,
    apply: bool,
    routing: &Routing,
    references: &mut Vec<Reference>,
) -> Result<PlannedNote, DomainError> {
    let deck = spec
        .deck
        .resolve(index, child_paths)
        .map_err(|error| with_note_index(note_index, error))?;

    if apply && deck.crowdanki_uuid.is_none() {
        return Err(DomainError::with_details(
            ErrorCode::UnresolvedDeckIdentity,
            format!(
                "заметка #{note_index}: колода «{}» не объявляет crowdanki_uuid, \
                 поэтому проверить адресацию цели после записи нечем: --apply отклонён",
                deck.path
            ),
            details! {
                "note_index" => note_index,
                "deck_path" => deck.path.clone(),
                "reason" => "deck_identity_missing",
            },
        ));
    }

    let required: BTreeSet<String> = spec.fields.keys().cloned().collect();
    let model = models::resolve(index, &deck, &spec.model, &required)
        .map_err(|error| with_note_index(note_index, error))?;

    let values = assemble_values(&model, &spec.fields, note_index)?;
    let before_refs = references.len();
    let processor_fields = routing
        .collect(note_index, &model.crowdanki_uuid, &values, references)
        .map_err(|e| with_note_index(note_index, e))?;
    let media_references = references.len() - before_refs;

    let (guid, guid_generated) = match spec.guid.as_deref() {
        Some(guid) => (guid.to_string(), false),
        None => (
            free_generated_guid(note_index, guid::generate, |candidate| {
                !index.note_positions_by_guid(candidate).is_empty()
            })?,
            true,
        ),
    };
    guid::validate(&guid, "notes[].guid").map_err(|error| with_note_index(note_index, error))?;

    let field_names: Vec<String> = values.iter().map(|(name, _)| name.clone()).collect();
    let value = build_note_value(&guid, &model.crowdanki_uuid, &values, &spec.tags);

    Ok(PlannedNote {
        note_index,
        note_id: spec.note_id.clone(),
        guid,
        guid_generated,
        deck_path: deck.path,
        deck_uuid: deck.crowdanki_uuid,
        deck_preorder: deck.preorder,
        deck_children: deck.children,
        notes_before: deck.notes_in_deck,
        model,
        field_names,
        values,
        tags: spec.tags.clone(),
        value,
        processor_fields,
        media_references,
    })
}

/// Сколько раз подряд разрешено заново генерировать `guid`.
///
/// Anki-совместимый `guid` — это `base91` от случайного `u64`, поэтому одна
/// попытка совпадает с существующей заметкой с вероятностью порядка `2⁻⁶⁴`.
/// Предел нужен не ради вероятности, а ради честного отказа: бесконечный цикл
/// скрыл бы сломанный источник энтропии или усечённый алфавит.
const GENERATED_GUID_ATTEMPTS: usize = 8;

/// Подбирает свободный `guid` для новой заметки.
///
/// Уникальность внутри экспорта обеспечивает вызывающий, а не [`guid::generate`]:
/// генератор отвечает только за формат. Сгенерированный `guid` — не то, что заказал
/// пользователь, поэтому совпадение здесь повод повторить попытку, а не отказ:
/// отказ остаётся на случай, когда свободного `guid` не нашлось.
fn free_generated_guid(
    note_index: usize,
    mut next: impl FnMut() -> Result<String, DomainError>,
    occupied: impl Fn(&str) -> bool,
) -> Result<String, DomainError> {
    for _ in 0..GENERATED_GUID_ATTEMPTS {
        let candidate = next()?;
        if !occupied(&candidate) {
            return Ok(candidate);
        }
    }

    Err(DomainError::with_details(
        ErrorCode::GuidCollision,
        format!(
            "заметка #{note_index}: свободный guid не найден: {GENERATED_GUID_ATTEMPTS} \
             сгенерированных подряд уже заняты"
        ),
        details! {
            "note_index" => note_index,
            "attempts" => GENERATED_GUID_ATTEMPTS,
            "reason" => "no_free_generated_guid",
        },
    ))
}

/// Собирает значения полей в порядке `ord` модели.
fn assemble_values(
    model: &ResolvedModel,
    provided: &BTreeMap<String, String>,
    note_index: usize,
) -> Result<Vec<(String, String)>, DomainError> {
    let known: BTreeSet<&str> = model
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect();

    let missing: Vec<String> = model
        .fields
        .iter()
        .filter(|field| !provided.contains_key(&field.name))
        .map(|field| field.name.clone())
        .collect();
    if !missing.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::MissingFieldValue,
            format!(
                "заметка #{note_index}: не заданы значения полей модели «{}»: {}",
                model.name,
                missing.join(", ")
            ),
            details! {
                "note_index" => note_index,
                "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                "missing_fields" => missing,
            },
        ));
    }

    let extra: Vec<String> = provided
        .keys()
        .filter(|name| !known.contains(name.as_str()))
        .cloned()
        .collect();
    if !extra.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::UnknownField,
            format!(
                "заметка #{note_index}: заданы поля, которых нет в модели «{}»: {}",
                model.name,
                extra.join(", ")
            ),
            details! {
                "note_index" => note_index,
                "crowdanki_uuid" => model.crowdanki_uuid.clone(),
                "unknown_fields" => extra,
                "known_fields" => known.iter().map(|name| (*name).to_string()).collect::<Vec<_>>(),
            },
        ));
    }

    Ok(model
        .fields
        .iter()
        .map(|field| {
            (
                field.name.clone(),
                provided.get(&field.name).cloned().unwrap_or_default(),
            )
        })
        .collect())
}

/// Собирает JSON новой заметки.
///
/// Ключи те же, что у заметок реального экспорта: `__type__`, `fields`, `guid`,
/// `note_model_uuid`, `tags`. Порядок ключей в файле задаёт каноническая
/// сериализация, а не порядок вставки.
fn build_note_value(
    guid: &str,
    model_uuid: &str,
    values: &[(String, String)],
    tags: &[String],
) -> Value {
    let mut note = Map::new();
    note.insert("__type__".to_string(), json!(NOTE_TYPE_NAME));
    note.insert(
        "fields".to_string(),
        Value::Array(values.iter().map(|(_, value)| json!(value)).collect()),
    );
    note.insert("guid".to_string(), json!(guid));
    note.insert("note_model_uuid".to_string(), json!(model_uuid));
    note.insert(
        "tags".to_string(),
        Value::Array(tags.iter().map(|tag| json!(tag)).collect()),
    );
    Value::Object(note)
}

/// Дописывает заметки в конец `notes` целевого узла.
fn append_notes(root: &mut Value, children: &[usize], values: &[Value]) -> Result<(), DomainError> {
    let pointer = deck_path_pointer(children);
    let node = node_mut(root, children)
        .ok_or_else(|| internal(format!("целевой узел {pointer} исчез из JSON-дерева")))?;
    let object = node
        .as_object_mut()
        .ok_or_else(|| internal(format!("целевой узел {pointer} не является объектом")))?;
    let notes = object.get_mut("notes").ok_or_else(|| {
        DomainError::with_details(
            ErrorCode::ExportNotMutable,
            format!("целевой узел {pointer} не объявляет массив notes"),
            details! {
                "reason" => "deck_without_notes_array",
                "deck_pointer" => pointer,
            },
        )
    })?;
    let array = notes
        .as_array_mut()
        .ok_or_else(|| internal(format!("notes узла {pointer} не является массивом")))?;
    array.extend(values.iter().cloned());
    Ok(())
}

/// Разрешённый диапазон добавленных заметок одного целевого узла.
struct AppendRange {
    /// Путь узла по массивам `children`.
    children: Vec<usize>,
    /// Полное имя колоды для сообщений.
    deck_path: String,
    /// Сколько заметок было в узле до операции.
    notes_before: usize,
    /// Сколько заметок добавлено.
    added: usize,
}

impl AppendRange {
    /// Префикс путей добавленных заметок.
    fn prefix(&self) -> String {
        format!("{}/notes/", deck_path_pointer(&self.children))
    }
}

/// Доказывает, что различия — ровно добавленные заметки.
///
/// Разрешены только пути `…/notes/<i>` для затронутых узлов, где `i` не меньше
/// исходного числа заметок. Число найденных различий обязано совпасть с числом
/// добавленных заметок, а значения по разрешённым путям — с задуманными.
fn verify_only_notes_appended(
    before: &Value,
    after: &Value,
    appended: &[AppendRange],
    values: &[Vec<Value>],
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

    let expected: usize = appended.iter().map(|range| range.added).sum();
    if found.total != expected {
        return Err(DomainError::with_details(
            ErrorCode::Internal,
            format!(
                "структурных различий {} вместо ожидаемых {expected}",
                found.total
            ),
            details! {
                "reason" => "unexpected_change_count",
                "total" => found.total,
                "expected" => expected,
                "paths" => found.paths.clone(),
            },
        ));
    }

    let prefix = |range: &AppendRange| range.prefix();

    if let Some(unexpected) = found.first_outside(
        &appended
            .iter()
            .map(prefix)
            .collect::<Vec<String>>()
            .iter()
            .map(String::as_str)
            .collect::<Vec<&str>>(),
    ) {
        return Err(DomainError::with_details(
            ErrorCode::Internal,
            format!("неожиданное изменение вне добавленных заметок: {unexpected}"),
            details! {
                "reason" => "unexpected_change_path",
                "path" => unexpected,
                "paths" => found.paths.clone(),
            },
        ));
    }

    for path in &found.paths {
        let Some(range) = appended
            .iter()
            .find(|range| path.starts_with(&prefix(range)))
        else {
            return Err(internal(format!(
                "путь {path} не относится ни к одному целевому узлу"
            )));
        };
        let tail = &path[prefix(range).len()..];
        let Ok(index) = tail.parse::<usize>() else {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!("изменение {path} не является добавленной заметкой"),
                details! {
                    "reason" => "unexpected_change_shape",
                    "path" => path.clone(),
                },
            ));
        };
        if index < range.notes_before {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!("изменение {path} затрагивает существующую заметку"),
                details! {
                    "reason" => "existing_note_touched",
                    "path" => path.clone(),
                    "notes_before" => range.notes_before,
                },
            ));
        }
    }

    for (position, range) in appended.iter().enumerate() {
        let node = source_node(after, &range.children)?;
        let notes = node
            .get("notes")
            .and_then(Value::as_array)
            .ok_or_else(|| internal(format!("notes узла {} исчезли", range.prefix())))?;

        if notes.len() != range.notes_before + range.added {
            return Err(DomainError::with_details(
                ErrorCode::Internal,
                format!(
                    "в колоде «{}» {} заметок вместо ожидаемых {}",
                    range.deck_path,
                    notes.len(),
                    range.notes_before + range.added
                ),
                details! {
                    "reason" => "note_count_changed",
                    "deck_path" => range.deck_path.clone(),
                    "observed" => notes.len(),
                    "expected" => range.notes_before + range.added,
                },
            ));
        }

        for (offset, value) in values[position].iter().enumerate() {
            let index = range.notes_before + offset;
            if notes.get(index) != Some(value) {
                return Err(DomainError::with_details(
                    ErrorCode::Internal,
                    format!(
                        "добавленная заметка {}/notes/{index} не совпала с задуманной",
                        deck_path_pointer(&range.children)
                    ),
                    details! {
                        "reason" => "appended_note_mismatch",
                        "deck_path" => range.deck_path.clone(),
                        "index" => index,
                    },
                ));
            }
        }
    }

    Ok(())
}

/// Узел JSON-дерева по `children`-пути.
fn source_node<'a>(root: &'a Value, children: &[usize]) -> Result<&'a Value, DomainError> {
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

/// Что нашлось в экспорте по `guid` заказываемой заметки.
enum Existing {
    /// Заметка с тем же содержимым уже есть.
    Identical,
    /// Заметка есть, но содержимое отличается.
    Different {
        reason: &'static str,
        detail: String,
    },
    /// `guid` не уникален внутри экспорта.
    Duplicated(usize),
}

/// Ищет заметку с заказанным `guid` среди существующих.
fn existing_note(index: &ExportIndex<'_>, note: &PlannedNote) -> Option<Existing> {
    let positions = index.note_positions_by_guid(&note.guid);
    match positions {
        [] => None,
        [position] => {
            let existing = &index.notes[*position];
            let expected: Vec<FieldValue> = note
                .values
                .iter()
                .map(|(_, value)| FieldValue::Text(value.clone()))
                .collect();

            if existing.node != note.deck_preorder {
                return Some(Existing::Different {
                    reason: "deck",
                    detail: format!(
                        "существующая заметка лежит в колоде «{}», запрошена «{}»",
                        index.note_deck_path(existing),
                        note.deck_path
                    ),
                });
            }
            if existing.note.note_model_uuid.as_deref() != Some(note.model.crowdanki_uuid.as_str())
            {
                return Some(Existing::Different {
                    reason: "note_model_uuid",
                    detail: format!(
                        "существующая заметка использует модель {:?}, запрошена {:?}",
                        existing.note.note_model_uuid, note.model.crowdanki_uuid
                    ),
                });
            }
            if existing.note.fields != expected {
                return Some(Existing::Different {
                    reason: "fields",
                    detail: format!(
                        "значения полей различаются: существует {} полей, запрошено {}",
                        existing.note.fields.len(),
                        expected.len()
                    ),
                });
            }
            if existing.note.tags != note.tags {
                return Some(Existing::Different {
                    reason: "tags",
                    detail: format!(
                        "теги различаются: существует {:?}, запрошено {:?}",
                        existing.note.tags, note.tags
                    ),
                });
            }
            Some(Existing::Identical)
        }
        many => Some(Existing::Duplicated(many.len())),
    }
}

/// Путь колоды существующей заметки с тем же `guid`.
fn existing_deck_path(index: &ExportIndex<'_>, note: &PlannedNote) -> String {
    index
        .note_positions_by_guid(&note.guid)
        .first()
        .map(|position| index.note_deck_path(&index.notes[*position]).to_string())
        .unwrap_or_default()
}

/// Разрешённый запрос: тот же документ с явными идентичностями.
///
/// Документ принимает та же команда без переупаковки, поэтому он и есть
/// контракт идемпотентного повторного прогона.
fn resolved_document(planned: &[PlannedNote]) -> Value {
    let notes: Vec<Value> = planned
        .iter()
        .map(|note| {
            let mut deck = Map::new();
            deck.insert("path".to_string(), json!(note.deck_path));
            if let Some(uuid) = &note.deck_uuid {
                deck.insert("crowdanki_uuid".to_string(), json!(uuid));
            }

            let mut model = Map::new();
            model.insert("mode".to_string(), json!(ModelMode::Explicit.as_str()));
            model.insert(
                "crowdanki_uuid".to_string(),
                json!(note.model.crowdanki_uuid),
            );

            let mut object = Map::new();
            if let Some(note_id) = &note.note_id {
                object.insert("note_id".to_string(), json!(note_id));
            }
            object.insert("guid".to_string(), json!(note.guid));
            object.insert("deck".to_string(), Value::Object(deck));
            object.insert("model".to_string(), Value::Object(model));
            object.insert(
                "fields".to_string(),
                Value::Object(
                    note.values
                        .iter()
                        .map(|(name, value)| (name.clone(), json!(value)))
                        .collect(),
                ),
            );
            object.insert(
                "tags".to_string(),
                Value::Array(note.tags.iter().map(|tag| json!(tag)).collect()),
            );
            Value::Object(object)
        })
        .collect();

    let mut document = Map::new();
    document.insert(
        "schema_version".to_string(),
        json!(SUPPORTED_REQUEST_SCHEMA_VERSION),
    );
    document.insert("notes".to_string(), Value::Array(notes));
    Value::Object(document)
}

/// Переносит ошибку в контекст заметки, сохраняя её детали.
fn with_note_index(note_index: usize, error: DomainError) -> DomainError {
    let mut details = error.details;
    if let Some(object) = details.as_object_mut() {
        object.insert("note_index".to_string(), json!(note_index));
    } else {
        details = details! { "note_index" => note_index };
    }

    DomainError::with_details(
        error.code,
        format!("заметка #{note_index}: {}", error.message),
        details,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Занятые имена отвечают на вопрос «свободен ли `guid`», а генератор здесь
    /// подменён: проверяется подбор, а не источник случайности.
    #[test]
    fn a_generated_guid_that_is_taken_is_generated_again() {
        let mut candidates = ["занят", "тоже занят", "свободен"].into_iter();
        let guid = free_generated_guid(
            3,
            || Ok(candidates.next().expect("кандидат").to_string()),
            |candidate| candidate != "свободен",
        )
        .expect("свободный guid обязан найтись");

        assert_eq!(guid, "свободен");
    }

    /// Отказ, а не бесконечный цикл: если свободного `guid` нет, заметка не
    /// добавляется, и причина названа кодом возврата.
    #[test]
    fn a_generated_guid_that_stays_taken_is_a_collision() {
        let error = free_generated_guid(2, || Ok("занят".to_string()), |_| true)
            .expect_err("свободного guid нет");

        assert_eq!(error.code, ErrorCode::GuidCollision);
        assert_eq!(error.details["reason"], "no_free_generated_guid");
        assert_eq!(error.details["note_index"], 2);
        assert_eq!(error.details["attempts"], GENERATED_GUID_ATTEMPTS);
    }

    /// Ошибка генератора не подменяется отказом гейта: причина остаётся своей.
    #[test]
    fn a_broken_entropy_source_is_not_reported_as_a_collision() {
        let error = free_generated_guid(
            1,
            || {
                Err(DomainError::with_details(
                    ErrorCode::Internal,
                    "источник энтропии сломан".to_string(),
                    details! { "source" => "getrandom" },
                ))
            },
            |_| false,
        )
        .expect_err("генерация не удалась");

        assert_eq!(error.code, ErrorCode::Internal);
    }
}
