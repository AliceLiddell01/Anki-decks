//! Контролируемая миграция legacy consumer filename на текущее каноническое имя.
//!
//! Зачем это отдельная операция, а не ручная правка одного поля. Канонические
//! consumer filenames двух доменов разведены намеренно: kanji-домен кладёт
//! изображение символа под `<char>.gif|png`, а pitch-домен — под
//! `<surface>.pitch.png`. Пока суффикса `.pitch.png` не существовало, прежний
//! конвейер называл pitch-картинку `<surface>.png`, и для односложного слова с
//! kanji-fallback имя совпадает с каноническим именем изображения символа:
//! `飴.png` — это одновременно pitch-картинка слова `飴` и kanji-fallback
//! символа `飴`. Плоское пространство имён `media/` не допускает двух разных
//! байтов под одним именем, поэтому `create` обязан отказать
//! (`destination_media_conflict`), а не перезаписать чужой файл.
//!
//! Команда переводит такие ссылки на текущий контракт. Она ничего не угадывает:
//! каждая ссылка на legacy-имя обязана лежать в поле того самого обработчика,
//! которому принадлежит каноническое имя, и иметь ровно ту форму ссылки, которую
//! этот обработчик распознаёт, — иначе команда останавливается с
//! [`REASON_UNPROVEN`] и показывает свидетельства. Правило «существующие заметки
//! не меняются» знает ровно одно исключение — доказанную миграцию legacy
//! media-имени; этой же командой оно и исполняется.
//!
//! Операция идемпотентна: повторный прогон, когда мигрировать нечего, ничего не
//! меняет. Байты канонического ресурса берутся из проверенного хранилища домена,
//! а не переименованием старого файла: совпадение SHA даёт `reuse`, иные байты
//! дают `copy`, а занятое чужими байтами каноническое имя — отказ.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use asset_store::{AssetIdentity, VerifiedAssetBytes};
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::model::NoteModel;
use crate::ops::create::MAX_REPORTED_NOTES;
use crate::ops::create_media::{self, MediaOptions, MediaPlan, ProcessorType, Routing, blocker};
use crate::ops::publish::{self, Candidate};
use crate::ops::source::{
    self, EditableSource, ValidationDelta, internal, load_editable_source, validation_delta,
};
use crate::{details, htmlscan, media, write};

/// Стабильная причина отказа, когда ссылку на legacy-имя нельзя доказать.
pub const REASON_UNPROVEN: &str = "legacy_media_reference_unproven";
/// Стабильная причина отказа на некорректный запрос миграции.
pub const REASON_INVALID: &str = "migration_invalid_request";

/// Запрос миграции одного legacy consumer filename.
#[derive(Debug, Clone)]
pub struct MigrationRequest {
    /// Пространство имён домена, которому принадлежит каноническое имя.
    pub namespace: String,
    /// Ключ идентичности внутри домена.
    pub key: String,
    /// Legacy-имя, ссылки на которое переводятся на каноническое.
    pub legacy_filename: String,
}

/// Одна доказанная legacy-ссылка.
#[derive(Debug, Clone, Serialize)]
pub struct MigratedReference {
    /// `guid` заметки.
    pub guid: Option<String>,
    /// Идентичность модели заметки.
    pub model_uuid: String,
    /// Имя модели заметки.
    pub model_name: Option<String>,
    /// Имя поля, в котором лежала ссылка.
    pub field: String,
    /// Позиция поля в модели.
    pub field_ord: i64,
}

/// Свидетельство оставшегося потребителя legacy-имени с иной семантикой.
#[derive(Debug, Clone, Serialize)]
pub struct UnprovenReference {
    /// `guid` заметки.
    pub guid: Option<String>,
    /// Имя поля; `None`, если позиция поля не описана моделью.
    pub field: Option<String>,
    /// Почему ссылку нельзя признать legacy-ссылкой домена.
    pub reason: &'static str,
    /// Сколько раз legacy-имя встретилось в media-позициях поля.
    pub occurrences: usize,
    /// Сколько из них распознаётся как ссылка домена.
    pub claimable: usize,
}

/// Отпечаток освободившегося legacy-файла.
#[derive(Debug, Clone, Serialize)]
pub struct LegacyMediaDigest {
    /// Имя файла.
    pub filename: String,
    /// Длина в байтах.
    pub byte_length: usize,
    /// SHA-256.
    pub sha256: String,
}

/// Результат миграции.
#[derive(Debug)]
pub struct MigrationResult {
    /// Каталог экспорта.
    pub export_dir: PathBuf,
    /// Путь к `deck.json`.
    pub deck_json: PathBuf,
    /// Команда работала как dry-run.
    pub dry_run: bool,
    /// `deck.json` записан.
    pub applied: bool,
    /// Что-то изменилось; иначе повторный прогон — no-op.
    pub changed: bool,
    /// Идентичность мигрируемого ресурса.
    pub identity: AssetIdentity,
    /// Каноническое consumer-имя домена.
    pub canonical_filename: String,
    /// Legacy-имя.
    pub legacy_filename: String,
    /// SHA-256 канонических байтов.
    pub canonical_sha256: String,
    /// Действие размещения канонического файла: `copy` или `reuse`.
    pub canonical_action: String,
    /// Число доказанных ссылок.
    pub references_total: usize,
    /// Ссылки усечены до [`MAX_REPORTED_NOTES`].
    pub references_truncated: bool,
    /// Доказанные ссылки.
    pub references: Vec<MigratedReference>,
    /// Добавленные объявления `media_files`.
    pub media_files_added: Vec<String>,
    /// Удалённые объявления `media_files`.
    pub media_files_removed: Vec<String>,
    /// Освободившееся legacy-имя объявлено в `media_files`.
    pub legacy_declared: bool,
    /// Отпечаток освободившегося файла, если он физически есть.
    pub legacy_media: Option<LegacyMediaDigest>,
    /// Сравнение валидации до и после.
    pub validation: ValidationDelta,
}

/// Переводит ссылки на legacy-имя на каноническое имя домена.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InvalidRequest`] на некорректный запрос,
/// [`ErrorCode::ExpectedMismatch`] с причиной [`REASON_UNPROVEN`], если хотя бы
/// одна ссылка на legacy-имя не доказуемо принадлежит обработчику домена, а
/// также доменные блокеры чтения проверенного хранилища и размещения файла.
pub fn migrate(
    export_dir: &Path,
    request: &MigrationRequest,
    options: &MediaOptions,
    apply: bool,
) -> Result<MigrationResult, DomainError> {
    let legacy = validated_legacy_name(&request.legacy_filename)?;
    let kind = ProcessorType::for_namespace(&request.namespace).ok_or_else(|| {
        blocker(
            ErrorCode::InvalidRequest,
            "media_domain_unknown",
            json!({"domain": request.namespace}),
        )
    })?;
    let identity = AssetIdentity::new(kind.namespace(), &request.key).map_err(|message| {
        DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("некорректная идентичность миграции: {message}"),
            details! {"reason" => REASON_INVALID, "domain" => kind.namespace(), "key" => request.key},
        )
    })?;
    kind.domain_policy()
        .validate_identity(&identity)
        .map_err(|error| {
            DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("идентичность недопустима для домена: {}", error.message),
                details! {"reason" => REASON_INVALID, "domain" => kind.namespace(), "key" => request.key},
            )
        })?;

    let source = load_editable_source(export_dir)?;
    let routing = Routing::load(export_dir, options)?;
    let index = ExportIndex::build(&source.root);
    let canonical = read_canonical(export_dir, &routing, kind, &identity)?;
    let canonical_filename = canonical.record.consumer_filename.clone();
    let canonical_sha256 = canonical.record.sha256.clone();
    if canonical_filename == legacy {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "legacy-имя уже совпадает с каноническим: мигрировать нечего",
            details! {"reason" => REASON_INVALID, "domain" => kind.namespace(), "filename" => legacy},
        ));
    }

    let scan = scan_references(&source, &index, &routing, kind, &legacy)?;
    if !scan.unproven.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::ExpectedMismatch,
            format!(
                "ссылка на {legacy:?} вне поля обработчика {} не доказывает legacy-семантику",
                kind.name()
            ),
            details! {
                "reason" => REASON_UNPROVEN,
                "domain" => kind.namespace(),
                "filename" => legacy,
                "canonical_filename" => canonical_filename,
                "references" => serde_json::to_value(&scan.unproven).map_err(|e| internal(e.to_string()))?,
            },
        ));
    }

    let legacy_declared = declared_once(&source.value, &legacy)?;
    let mut candidate_value = source.value.clone();
    rewrite_references(&mut candidate_value, &scan.rewrites, &canonical_filename)?;
    let declarations = reconcile_declarations(
        &mut candidate_value,
        &legacy,
        &canonical_filename,
        !scan.rewrites.is_empty(),
    )?;

    let mut plan = MediaPlan::for_asset(canonical, export_dir)?;
    plan.declarations_added.clone_from(&declarations.added);
    let canonical_action = plan.action().unwrap_or_else(|| "copy".to_owned());

    let changed = candidate_value != source.value;
    let candidate: Option<Candidate> = if changed {
        Some(publish::prepare(&source, export_dir, candidate_value)?)
    } else {
        None
    };
    let validation = candidate.as_ref().map_or_else(
        || validation_delta(&source.before, &source.before),
        |candidate| candidate.validation.clone(),
    );
    let legacy_media = read_legacy_digest(export_dir, &legacy)?;

    if apply && changed {
        let guard = write::ExportLock::acquire(export_dir)?;
        guard.check_source(&source.deck_json, &source.source)?;
        // Канонические байты размещаются раньше публикации и удаления: ссылка
        // никогда не остаётся без файла, а освобождение legacy-имени видит
        // только тот, кто ещё читает старый `deck.json`.
        plan.materialize(&guard)?;
        if let Some(candidate) = &candidate {
            guard.replace(&source.deck_json, &source.source, &candidate.bytes)?;
        }
        if declarations.release_legacy {
            create_media::remove_media_file(&guard, &legacy)?;
        }
    }

    Ok(MigrationResult {
        export_dir: export_dir.to_path_buf(),
        deck_json: source.deck_json.clone(),
        dry_run: !apply,
        applied: apply && changed,
        changed,
        identity,
        canonical_filename,
        legacy_filename: legacy,
        canonical_sha256,
        canonical_action,
        references_total: scan.rewrites.len(),
        references_truncated: scan.rewrites.len() > MAX_REPORTED_NOTES,
        references: scan
            .rewrites
            .iter()
            .take(MAX_REPORTED_NOTES)
            .map(|rewrite| rewrite.reference.clone())
            .collect(),
        media_files_added: declarations.added,
        media_files_removed: declarations.removed,
        legacy_declared,
        legacy_media,
        validation,
    })
}

/// Отвергает legacy-имя, которое не является безопасным простым basename.
fn validated_legacy_name(raw: &str) -> Result<String, DomainError> {
    let name = media::normalize_media_name(raw);
    if raw.is_empty() || name != raw || matches!(raw, "." | "..") || raw.contains('\0') {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("legacy-имя {raw:?} не является безопасным простым basename"),
            details! {"reason" => REASON_INVALID, "filename" => raw},
        ));
    }
    Ok(name)
}

/// Читает проверенный канонический ресурс домена.
fn read_canonical(
    export_dir: &Path,
    routing: &Routing,
    kind: ProcessorType,
    identity: &AssetIdentity,
) -> Result<VerifiedAssetBytes, DomainError> {
    let mut assets = routing.read_domain(export_dir, kind, std::slice::from_ref(identity), None)?;
    match assets.len() {
        1 => Ok(assets.remove(0)),
        count => Err(internal(format!(
            "проверенное хранилище вернуло {count} ресурсов вместо одного"
        ))),
    }
}

/// Отпечаток физического файла `media/<legacy>`, если он есть.
fn read_legacy_digest(
    export_dir: &Path,
    legacy: &str,
) -> Result<Option<LegacyMediaDigest>, DomainError> {
    create_media::media_file_digest(export_dir, legacy).map(|digest| {
        digest.map(|(byte_length, sha256)| LegacyMediaDigest {
            filename: legacy.to_owned(),
            byte_length,
            sha256,
        })
    })
}

/// Объявлено ли имя в `media_files` ровно один раз.
fn declared_once(value: &Value, name: &str) -> Result<bool, DomainError> {
    let count = value
        .get("media_files")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.as_str() == Some(name))
                .count()
        })
        .unwrap_or(0);
    match count {
        0 => Ok(false),
        1 => Ok(true),
        declarations => Err(blocker(
            ErrorCode::InvalidRequest,
            "media_declaration_conflict",
            json!({"filename": name, "declarations": declarations}),
        )),
    }
}

/// Что нужно изменить в документе.
struct Scan {
    /// Доказанные ссылки и их точные диапазоны.
    rewrites: Vec<Rewrite>,
    /// Оставшиеся потребители legacy-имени с иной семантикой.
    unproven: Vec<UnprovenReference>,
}

/// Одна доказанная legacy-ссылка вместе с её местом в документе.
struct Rewrite {
    /// JSON-путь заметки.
    path: source::ValueNotePath,
    /// Позиция поля в заметке.
    field_ord: usize,
    /// Диапазон значения атрибута внутри текста поля.
    range: Range<usize>,
    /// Свидетельство для отчёта.
    reference: MigratedReference,
}

/// Изменения `media_files`.
struct Declarations {
    /// Добавленные объявления.
    added: Vec<String>,
    /// Удалённые объявления.
    removed: Vec<String>,
    /// Освободившееся legacy-имя нужно убрать из `media_files` и с диска.
    release_legacy: bool,
}

/// Ищет ссылки на legacy-имя и доказывает семантику каждой из них.
fn scan_references(
    source: &EditableSource,
    index: &ExportIndex<'_>,
    routing: &Routing,
    kind: ProcessorType,
    legacy: &str,
) -> Result<Scan, DomainError> {
    let mut scan = Scan {
        rewrites: Vec::new(),
        unproven: Vec::new(),
    };
    for note in source::collect_value_notes(&source.value)? {
        let Some(object) = source::note_ref(&source.value, &note.path).and_then(Value::as_object)
        else {
            return Err(internal(
                "заметка по вычисленному пути не является объектом",
            ));
        };
        let Some(fields) = object.get("fields").and_then(Value::as_array) else {
            return Err(internal("заметка не содержит массив fields"));
        };
        // Дешёвый предфильтр: любую запись, которую `normalize_media_name` свёл
        // бы к этому basename, имя содержит подстрокой, поэтому отсутствие
        // подстроки доказывает отсутствие ссылки.
        if !fields
            .iter()
            .any(|value| value.as_str().is_some_and(|text| text.contains(legacy)))
        {
            continue;
        }
        let model_uuid = note.model_uuid.clone();
        let model = model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid));
        let names: Vec<String> = model
            .map(|model| model.flds.iter().map(|field| field.name.clone()).collect())
            .unwrap_or_default();
        let enabled = match model_uuid.as_deref() {
            Some(uuid) if model.is_some() => routing.fields(uuid, &names)?,
            _ => BTreeSet::new(),
        };
        let owner_uuid = model_uuid.clone().unwrap_or_default();
        for (position, value) in fields.iter().enumerate() {
            let Some(text) = value.as_str() else {
                continue;
            };
            if !text.contains(legacy) {
                continue;
            }
            let described = model.and_then(|model| field_at(model, position));
            let claimable = claimable_ranges(text, legacy);
            let occurrences = occurrences_of(text, legacy);
            let owned = described.is_some_and(|(name, _)| {
                enabled.contains(name) && routing.owns_field(&owner_uuid, name, kind)
            });
            let Some((field, ord)) = described.filter(|_| owned) else {
                scan.unproven.push(UnprovenReference {
                    guid: note.guid.clone(),
                    field: described.map(|(name, _)| name.to_owned()),
                    reason: if described.is_none() {
                        "field_position_not_described_by_model"
                    } else if !owned {
                        "field_not_owned_by_domain"
                    } else {
                        "reference_shape_not_claimable"
                    },
                    occurrences,
                    claimable: claimable.len(),
                });
                continue;
            };
            if claimable.len() != occurrences {
                scan.unproven.push(UnprovenReference {
                    guid: note.guid.clone(),
                    field: Some(field.to_owned()),
                    reason: "reference_shape_not_claimable",
                    occurrences,
                    claimable: claimable.len(),
                });
                continue;
            }
            for range in claimable {
                scan.rewrites.push(Rewrite {
                    path: note.path.clone(),
                    field_ord: position,
                    range,
                    reference: MigratedReference {
                        guid: note.guid.clone(),
                        model_uuid: owner_uuid.clone(),
                        model_name: model.and_then(|model| model.name.clone()),
                        field: field.to_owned(),
                        field_ord: ord,
                    },
                });
            }
        }
    }
    Ok(scan)
}

/// Имя и `ord` поля модели по позиции в `Note.fields`.
fn field_at(model: &NoteModel, position: usize) -> Option<(&str, i64)> {
    model.flds.iter().find_map(|field| {
        let ord = field.ord.value()?;
        (usize::try_from(ord).ok()? == position).then_some((field.name.as_str(), ord))
    })
}

/// Диапазоны значений атрибутов, которые домен вправе распознать как ссылку.
///
/// Это ровно `<img src="<legacy>">` — та же форма, которой владеет обработчик
/// домена. Всё остальное (`srcset`, CSS, `[sound:]`, незакрытый тег, чужое имя
/// элемента, имя с каталогом) остаётся вне набора и приводит к отказу.
fn claimable_ranges(text: &str, legacy: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    for tag in htmlscan::scan_tags(text) {
        let htmlscan::Tag::Element(element) = tag else {
            continue;
        };
        if !element.name.eq_ignore_ascii_case("img") {
            continue;
        }
        for attribute in &element.attributes {
            if !attribute.name.eq_ignore_ascii_case("src") {
                continue;
            }
            let Some(value_range) = attribute.value_range.clone() else {
                continue;
            };
            for candidate in media::split_attribute_values(attribute.name, attribute.value) {
                if candidate.text != legacy {
                    continue;
                }
                found.push(
                    value_range.start + candidate.range.start
                        ..value_range.start + candidate.range.end,
                );
            }
        }
    }
    found
}

/// Сколько раз legacy-имя встречается в media-позициях текста.
fn occurrences_of(text: &str, legacy: &str) -> usize {
    let mut observed = media::forbidden_media_references(text);
    observed.extend(
        media::sound_references(text)
            .into_iter()
            .map(|reference| reference.name.to_string()),
    );
    observed
        .into_iter()
        .filter(|name| media::normalize_media_name(name) == legacy)
        .count()
}

/// Переписывает доказанные ссылки на каноническое имя.
fn rewrite_references(
    value: &mut Value,
    rewrites: &[Rewrite],
    canonical: &str,
) -> Result<(), DomainError> {
    for rewrite in rewrites {
        let Some(field) = source::note_mut(value, &rewrite.path)
            .and_then(Value::as_object_mut)
            .and_then(|note| note.get_mut("fields"))
            .and_then(Value::as_array_mut)
            .and_then(|fields| fields.get_mut(rewrite.field_ord))
        else {
            return Err(internal("поле для подстановки ссылки исчезло"));
        };
        let text = field
            .as_str()
            .ok_or_else(|| internal("поле для подстановки ссылки перестало быть строкой"))?;
        if !text.is_char_boundary(rewrite.range.start) || !text.is_char_boundary(rewrite.range.end)
        {
            return Err(internal("диапазон ссылки не совпал с границами символов"));
        }
        let mut rewritten = String::with_capacity(text.len() + canonical.len());
        rewritten.push_str(&text[..rewrite.range.start]);
        rewritten.push_str(canonical);
        rewritten.push_str(&text[rewrite.range.end..]);
        *field = Value::String(rewritten);
    }
    Ok(())
}

/// Приводит `media_files` в соответствие с фактическими ссылками.
///
/// Объявления меняются только тогда, когда ссылки действительно переписаны:
/// прогон, в котором мигрировать нечего, обязан оставить документ побайтово
/// прежним. Снятие объявления legacy-имени — часть той же операции, а не
/// отдельная уборка «на всякий случай».
fn reconcile_declarations(
    value: &mut Value,
    legacy: &str,
    canonical: &str,
    migrated: bool,
) -> Result<Declarations, DomainError> {
    let mut declarations = Declarations {
        added: Vec::new(),
        removed: Vec::new(),
        release_legacy: false,
    };
    if !migrated {
        return Ok(declarations);
    }
    let map = value
        .as_object_mut()
        .ok_or_else(|| internal("корень экспорта не является объектом"))?;
    let array = map
        .entry("media_files")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| internal("media_files не является массивом"))?;

    // Повтор объявления legacy-имени уже отвергнут `declared_once` до правки:
    // здесь снимается ровно одно объявление, потому что оно ровно одно.
    if let Some(position) = array
        .iter()
        .position(|entry| entry.as_str() == Some(legacy))
    {
        array.remove(position);
        declarations.removed.push(legacy.to_owned());
        declarations.release_legacy = true;
    }
    if !array.iter().any(|entry| entry.as_str() == Some(canonical)) {
        array.push(json!(canonical));
        declarations.added.push(canonical.to_owned());
    }
    Ok(declarations)
}

#[cfg(test)]
#[path = "migrate_media_tests.rs"]
mod tests;
