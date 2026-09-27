//! Переиспользуемый слой выбора заметок: область колоды, критерии и
//! сопоставление значений полей.
//!
//! `find` и `review` обязаны выбирать заметки одинаково: если семантика
//! `--deck`, разрешения `guid` или сопоставления `--field/--value/--match`
//! разъедется между командами, агент получит два разных ответа на один вопрос.
//! Поэтому единственная реализация этих правил живёт здесь, а команды только
//! добавляют к ней своё представление результата.
//!
//! Здесь нет ни файлового ввода-вывода, ни доменных результатов: только чистые
//! функции над [`ExportIndex`] и доменные ошибки для неразрешимых критериев.

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{ExportIndex, NoteRef, field_value_by_name};

/// Имя головного поля словарной модели.
///
/// Используется сокращением `--word` в `find`/`review` и QA-правилом
/// `duplicate_primary_field`. Это имя поля конкретной модели, а не позиция в
/// `fields`: любое использование обязано разрешаться через саму модель.
pub const PRIMARY_FIELD: &str = "Слово";

/// Режим сопоставления значения поля.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Подстрока в сыром значении поля.
    Contains,
    /// Полное совпадение с сырым значением поля.
    Exact,
}

impl MatchMode {
    /// Стабильное machine-readable имя режима.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Exact => "exact",
        }
    }
}

/// Разрешает точный путь колоды в индекс узла.
///
/// `None` означает отсутствие ограничения: вся область экспорта.
///
/// # Errors
///
/// Возвращает [`ErrorCode::UnknownDeck`], если узла с таким точным путём нет.
pub fn resolve_deck_scope(
    index: &ExportIndex<'_>,
    deck: Option<&str>,
) -> Result<Option<usize>, DomainError> {
    let Some(deck) = deck else {
        return Ok(None);
    };
    index.node_index_by_path(deck).map(Some).ok_or_else(|| {
        let available: Vec<String> = index
            .nodes
            .iter()
            .map(|entry| entry.path.to_string())
            .collect();
        DomainError::with_details(
            ErrorCode::UnknownDeck,
            format!("колода {deck:?} не найдена в экспорте"),
            details! {
                "deck" => deck,
                "available_decks" => available,
            },
        )
    })
}

/// Входит ли заметка в область выбора.
pub fn in_scope(index: &ExportIndex<'_>, entry: &NoteRef<'_>, scope: Option<usize>) -> bool {
    scope.is_none_or(|node| index.subtree_range(node).contains(&entry.node))
}

/// Проверяет, что имя поля известно хотя бы одной модели экспорта.
///
/// # Errors
///
/// Возвращает [`ErrorCode::UnknownField`] со списком известных имён.
pub fn ensure_known_field(index: &ExportIndex<'_>, field: &str) -> Result<(), DomainError> {
    if index.known_field_names().contains(field) {
        return Ok(());
    }
    let available: Vec<String> = index
        .known_field_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    Err(DomainError::with_details(
        ErrorCode::UnknownField,
        format!("поле {field:?} отсутствует во всех note models экспорта"),
        details! {
            "field" => field,
            "available_fields" => available,
        },
    ))
}

/// Сопоставляет сырое строковое значение поля заметки с критерием.
///
/// Заметка без разрешимой модели, без такого поля в модели или с нестроковым
/// значением критерию не соответствует: это структурные проблемы, а не
/// content-критерий.
pub fn note_field_matches(
    index: &ExportIndex<'_>,
    entry: &NoteRef<'_>,
    field: &str,
    value: &str,
    mode: MatchMode,
) -> bool {
    let Some(raw) = note_field_text(index, entry, field) else {
        return false;
    };
    match mode {
        MatchMode::Contains => raw.contains(value),
        MatchMode::Exact => raw == value,
    }
}

/// Сырое строковое значение поля заметки, если оно разрешается.
pub fn note_field_text<'a>(
    index: &ExportIndex<'a>,
    entry: &NoteRef<'a>,
    field: &str,
) -> Option<&'a str> {
    let model = entry
        .note
        .note_model_uuid
        .as_deref()
        .and_then(|uuid| index.model_by_uuid(uuid))?;
    field_value_by_name(entry.note, model, field)?.as_text()
}

/// Позиции заметок, подходящих под критерий поля, в порядке экспорта.
pub fn positions_matching_field(
    index: &ExportIndex<'_>,
    field: &str,
    value: &str,
    mode: MatchMode,
    scope: Option<usize>,
) -> Vec<usize> {
    index
        .notes
        .iter()
        .enumerate()
        .filter(|(_, entry)| in_scope(index, entry, scope))
        .filter(|(_, entry)| note_field_matches(index, entry, field, value, mode))
        .map(|(position, _)| position)
        .collect()
}

/// Позиции заметок с данным `guid` внутри области, в порядке экспорта.
pub fn positions_by_guid(index: &ExportIndex<'_>, guid: &str, scope: Option<usize>) -> Vec<usize> {
    index
        .note_positions_by_guid(guid)
        .iter()
        .copied()
        .filter(|position| in_scope(index, &index.notes[*position], scope))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT, deck_node};

    #[test]
    fn deck_scope_requires_exact_path() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);

        assert_eq!(
            resolve_deck_scope(&index, None).expect("без ограничения"),
            None
        );
        assert_eq!(
            resolve_deck_scope(&index, Some("Root::Child")).expect("точный путь"),
            Some(1)
        );

        let error = resolve_deck_scope(&index, Some("Child")).expect_err("нет такого пути");
        assert_eq!(error.code, ErrorCode::UnknownDeck);
        assert!(error.details["available_decks"].is_array());
    }

    #[test]
    fn scope_covers_descendants_only() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        let scope = resolve_deck_scope(&index, Some("Root::Child")).expect("область");

        let included: Vec<&str> = index
            .notes
            .iter()
            .filter(|entry| in_scope(&index, entry, scope))
            .map(|entry| index.note_deck_path(entry))
            .collect();
        assert_eq!(included, vec!["Root::Child", "Root::Child::Leaf"]);
    }

    #[test]
    fn field_positions_follow_export_order() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);

        let positions = positions_matching_field(&index, "Слово", "", MatchMode::Contains, None);
        assert_eq!(positions, vec![0, 1]);

        let exact = positions_matching_field(&index, "Слово", "必然", MatchMode::Exact, None);
        assert_eq!(exact, vec![1]);
        assert!(
            positions_matching_field(&index, "Слово", "必", MatchMode::Exact, None).is_empty(),
            "exact не должен совпадать по подстроке"
        );
    }

    #[test]
    fn unknown_field_is_rejected_with_available_names() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let error = ensure_known_field(&index, "НетТакого").expect_err("нет такого поля");
        assert_eq!(error.code, ErrorCode::UnknownField);
        assert_eq!(
            error.details["available_fields"].as_array().map(Vec::len),
            Some(2)
        );
        ensure_known_field(&index, "Слово").expect("поле модели известно");
    }

    #[test]
    fn non_string_values_never_match() {
        let json = crate::test_support::export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][0] = serde_json::json!(42);
        });
        let node = deck_node(&json);
        let index = ExportIndex::build(&node);

        assert_eq!(note_field_text(&index, &index.notes[0], "Слово"), None);
        assert!(!note_field_matches(
            &index,
            &index.notes[0],
            "Слово",
            "42",
            MatchMode::Contains
        ));
        assert_eq!(
            positions_matching_field(&index, "Слово", "42", MatchMode::Contains, None),
            Vec::<usize>::new(),
            "нестроковое значение не участвует в критерии"
        );
        assert_eq!(
            positions_matching_field(&index, "Слово", "必然", MatchMode::Exact, None),
            vec![1],
            "строковое значение другой заметки по-прежнему находится"
        );
    }

    #[test]
    fn guid_positions_respect_scope() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);

        assert_eq!(positions_by_guid(&index, "guid-leaf", None), vec![2]);
        assert!(
            positions_by_guid(&index, "нет-такого", None).is_empty(),
            "неизвестный guid не даёт позиций"
        );

        let scope = resolve_deck_scope(&index, Some("Root")).expect("область");
        assert_eq!(positions_by_guid(&index, "guid-leaf", scope), vec![2]);

        let child = resolve_deck_scope(&index, Some("Root::Child::Leaf")).expect("область");
        assert!(positions_by_guid(&index, "guid-root", child).is_empty());
    }
}
