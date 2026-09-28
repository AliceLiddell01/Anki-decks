//! Общая граница чтения и оценки мутируемого `deck.json`.
//!
//! Все структурные writes toolkit'а (`edit`, `create`, `retire`) обязаны
//! начинать с одного и того же состояния: канонические байты `deck.json`,
//! разобранное JSON-дерево, типизированный корень того же файла и результат
//! `validate` до изменения. Если бы каждая операция собирала это сама, они
//! разошлись бы в том, какой экспорт вообще допустимо менять, и вторая
//! реализация проверки канонической формы неизбежно ослабла бы.
//!
//! Здесь нет ни одной мутации: модуль только читает и называет причины отказа.
//! `edit` остаётся владельцем изменения **значений** полей, а `create`/`retire`
//! добавляют к этому собственные структурные операции поверх того же исходника.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::loader;
use crate::ops::validate::{self, Severity, SeverityCounts, ValidateResult, warning_codes};

/// Сравнение валидации до и после изменения.
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

/// Разобранный исходник без предварительных проверок границы записи.
///
/// Один и тот же набор проверок нужен нескольким командам: `edit` пишет по
/// этому исходнику, `review-check` обещает агенту запрос, который граница
/// записи обязана принять, а `create`/`retire` добавляют по нему структуру.
pub struct EditableSource {
    /// Полный путь к `deck.json`.
    pub deck_json: PathBuf,
    /// Сырые байты `deck.json`: каноничность проверяется побайтово.
    pub source: Vec<u8>,
    /// Разобранное значение: мутирующие операции меняют именно его.
    pub value: Value,
    /// Типизированный корень того же файла.
    pub root: crate::model::DeckNode,
    /// Проверки экспорта до изменения: результат кандидата сравнивается с ними.
    pub before: ValidateResult,
}

/// Читает и разбирает `deck.json` без оценки права на запись.
///
/// # Errors
///
/// Возвращает ошибки чтения и разбора [`loader::read_deck_json_bytes`] и
/// [`loader::parse_deck_json_bytes`].
pub fn read_source(export_dir: &Path) -> Result<EditableSource, DomainError> {
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
pub fn source_blockers(source: &EditableSource) -> Vec<DomainError> {
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
pub fn load_editable_source(export_dir: &Path) -> Result<EditableSource, DomainError> {
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
pub fn distinct_codes(result: &ValidateResult, severity: Severity) -> Vec<&'static str> {
    let mut codes: Vec<&'static str> = result
        .issues
        .iter()
        .filter(|issue| issue.severity == severity)
        .map(|issue| issue.code)
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes
}

/// Сравнивает валидацию до и после изменения.
#[must_use]
pub fn validation_delta(before: &ValidateResult, after: &ValidateResult) -> ValidationDelta {
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

/// Соответствие заметки в JSON-дереве и в типизированном дереве.
#[derive(Debug)]
pub struct ValueNote {
    /// Позиция в JSON-дереве.
    pub path: ValueNotePath,
    /// `guid` из JSON.
    pub guid: Option<String>,
    /// `note_model_uuid` из JSON.
    pub model_uuid: Option<String>,
}

/// Позиция заметки в JSON-дереве.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueNotePath {
    /// Индексы `children` от корня экспорта.
    pub children: Vec<usize>,
    /// Индекс заметки в `notes` узла.
    pub note: usize,
}

/// Обходит JSON-дерево в том же порядке, что и [`ExportIndex`].
///
/// # Errors
///
/// Возвращает [`ErrorCode::Internal`], если `deck.json` структурно не является
/// деревом объектов с массивами `notes`/`children`.
pub fn collect_value_notes(root: &Value) -> Result<Vec<ValueNote>, DomainError> {
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

/// Возвращает `children`-путь до каждого узла дерева в preorder-порядке.
///
/// Индекс результата совпадает с [`crate::index::NodeRef::preorder`], поэтому
/// позиция узла в типизированном обходе и в JSON-дереве связываются без второй
/// реализации обхода дерева.
///
/// # Errors
///
/// Возвращает [`ErrorCode::Internal`], если `children` не является массивом
/// объектов.
pub fn deck_child_paths(root: &Value) -> Result<Vec<Vec<usize>>, DomainError> {
    let mut paths = Vec::new();
    let mut current = Vec::new();
    collect_deck_paths(root, &mut current, &mut paths)?;
    Ok(paths)
}

fn collect_deck_paths(
    node: &Value,
    current: &mut Vec<usize>,
    paths: &mut Vec<Vec<usize>>,
) -> Result<(), DomainError> {
    let Some(object) = node.as_object() else {
        return Err(internal(format!(
            "узел колоды в JSON не является объектом, а его тип: {}",
            type_name(node)
        )));
    };
    paths.push(current.clone());

    match object.get("children") {
        None | Some(Value::Null) => {}
        Some(Value::Array(entries)) => {
            for (position, entry) in entries.iter().enumerate() {
                current.push(position);
                collect_deck_paths(entry, current, paths)?;
                current.pop();
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
///
/// # Errors
///
/// Возвращает [`ErrorCode::Internal`], если число заметок или их `guid`/
/// `note_model_uuid` различаются между проекциями.
pub fn ensure_note_correspondence(
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

/// Ссылка на узел колоды по его `children`-пути.
pub fn node_ref<'a>(root: &'a Value, children: &[usize]) -> Option<&'a Value> {
    let mut current = root;
    for step in children {
        current = current.get("children")?.as_array()?.get(*step)?;
    }
    Some(current)
}

/// Изменяемая ссылка на узел колоды по его `children`-пути.
pub fn node_mut<'a>(root: &'a mut Value, children: &[usize]) -> Option<&'a mut Value> {
    let mut current = root;
    for step in children {
        current = current
            .as_object_mut()?
            .get_mut("children")?
            .as_array_mut()?
            .get_mut(*step)?;
    }
    Some(current)
}

/// Ссылка на заметку по её позиции в JSON-дереве.
pub fn note_ref<'a>(root: &'a Value, path: &ValueNotePath) -> Option<&'a Value> {
    node_ref(root, &path.children)?
        .get("notes")?
        .as_array()?
        .get(path.note)
}

/// Изменяемая ссылка на заметку по её позиции в JSON-дереве.
pub fn note_mut<'a>(root: &'a mut Value, path: &ValueNotePath) -> Option<&'a mut Value> {
    node_mut(root, &path.children)?
        .as_object_mut()?
        .get_mut("notes")?
        .as_array_mut()?
        .get_mut(path.note)
}

/// Имя типа JSON-значения для диагностики.
pub fn type_name(value: &Value) -> &'static str {
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
pub fn string_property(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(ToString::to_string)
}

/// Номер первого различающегося байта двух срезов.
pub fn first_difference(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| left.len().min(right.len()))
}

/// Готовит внутреннюю ошибку домена.
pub fn internal(message: impl Into<String>) -> DomainError {
    DomainError::with_details(
        ErrorCode::Internal,
        message,
        details! {
            "reason" => "mutation_internal_invariant",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeckNode;
    use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT};

    fn value(json: &str) -> Value {
        serde_json::from_str(json).expect("тестовый JSON должен разбираться")
    }

    fn typed(json: &str) -> DeckNode {
        serde_json::from_str(json).expect("тестовый JSON должен разбираться")
    }

    #[test]
    fn deck_paths_follow_preorder_of_the_typed_walk() {
        let json = value(NESTED_EXPORT);
        let paths = deck_child_paths(&json).expect("обход");
        assert_eq!(paths, vec![vec![], vec![0], vec![0, 0]]);

        let tree = typed(NESTED_EXPORT);
        let index = ExportIndex::build(&tree);
        assert_eq!(index.nodes.len(), paths.len());
        assert_eq!(
            node_ref(&json, &paths[1]).and_then(|node| node.get("name")),
            Some(&Value::String(index.nodes[1].path.to_string()))
        );
    }

    #[test]
    fn value_notes_match_the_typed_projection() {
        let json = value(MINIMAL_EXPORT);
        let notes = collect_value_notes(&json).expect("заметки");
        let tree = typed(MINIMAL_EXPORT);
        let index = ExportIndex::build(&tree);

        assert_eq!(notes.len(), index.notes.len());
        ensure_note_correspondence(&index, &notes).expect("проекции совпадают");
        assert_eq!(notes[1].guid.as_deref(), Some("guid-2"));
        assert_eq!(
            notes[1].path,
            ValueNotePath {
                children: vec![],
                note: 1
            }
        );
    }

    #[test]
    fn nested_note_paths_address_nested_children() {
        let mut json = value(NESTED_EXPORT);
        let notes = collect_value_notes(&json).expect("заметки");
        let leaf = notes
            .iter()
            .find(|note| note.guid.as_deref() == Some("guid-leaf"))
            .expect("заметка листа");

        note_mut(&mut json, &leaf.path)
            .and_then(|note| note.get_mut("tags"))
            .map(|tags| *tags = serde_json::json!(["помечено"]))
            .expect("теги");
        assert_eq!(
            note_ref(&json, &leaf.path).expect("заметка")["tags"],
            serde_json::json!(["помечено"])
        );
    }

    #[test]
    fn missing_collections_behave_like_empty_arrays() {
        let json = value(r#"{"__type__": "Deck", "nope": 1}"#);
        assert!(collect_value_notes(&json).expect("заметки").is_empty());
        assert_eq!(
            deck_child_paths(&json).expect("пути"),
            vec![Vec::<usize>::new()]
        );
        assert!(node_ref(&json, &[]).is_some());
        assert!(node_ref(&json, &[0]).is_none());
    }

    #[test]
    fn malformed_note_containers_are_internal_errors() {
        let json = value(r#"{"__type__": "Deck", "notes": 5}"#);
        let error = collect_value_notes(&json).expect_err("не массив");
        assert_eq!(error.code, ErrorCode::Internal);

        let json = value(r#"{"__type__": "Deck", "children": "нет"}"#);
        let error = deck_child_paths(&json).expect_err("не массив");
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[test]
    fn first_difference_reports_offset_or_shorter_length() {
        assert_eq!(first_difference(b"abc", b"abd"), 2);
        assert_eq!(first_difference(b"ab", b"abc"), 2);
        assert_eq!(first_difference(b"", b"x"), 0);
    }

    #[test]
    fn correspondence_rejects_divergent_projections() {
        let tree = typed(MINIMAL_EXPORT);
        let index = ExportIndex::build(&tree);
        let mut notes = collect_value_notes(&value(MINIMAL_EXPORT)).expect("заметки");
        notes[0].guid = Some("другой".to_string());
        let error = ensure_note_correspondence(&index, &notes).expect_err("расхождение");
        assert_eq!(error.code, ErrorCode::Internal);

        notes.truncate(1);
        let error = ensure_note_correspondence(&index, &notes).expect_err("число заметок");
        assert_eq!(error.code, ErrorCode::Internal);
    }
}
