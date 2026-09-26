//! Операция `find`: поиск небольшого набора заметок по предсказуемым критериям.
//!
//! Stage 1 сознательно не делает HTML-to-headword extraction, fuzzy matching,
//! regex и японскую морфологию. Поиск идёт по сырым значениям полей.

use std::path::Path;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{ExportIndex, NoteRef, field_value_by_name, resolve_named_fields};
use crate::ops::NamedField;

/// Поле-сокращение для `--word`.
pub const WORD_SHORTCUT_FIELD: &str = "Слово";

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

/// Критерий поиска.
#[derive(Debug, Clone)]
pub enum FindCriteria {
    /// Поиск по идентичности `guid`.
    Guid {
        /// Искомый идентификатор заметки.
        guid: String,
    },
    /// Поиск по именованному полю.
    Field {
        /// Имя поля модели.
        field: String,
        /// Искомое значение.
        value: String,
        /// Режим сопоставления.
        mode: MatchMode,
    },
}

/// Параметры запроса `find`.
#[derive(Debug, Clone)]
pub struct FindQuery {
    /// Критерий поиска.
    pub criteria: FindCriteria,
    /// Необязательное ограничение по пути колоды.
    pub deck: Option<String>,
    /// Предел числа возвращаемых заметок.
    pub limit: usize,
}

/// Результат `find`.
#[derive(Debug)]
pub struct FindResult {
    /// Каталог экспорта.
    pub export_dir: String,
    /// Сводка критерия.
    pub criteria: CriteriaSummary,
    /// Сколько заметок подошло до применения `limit`.
    pub matched_total: usize,
    /// Сколько заметок реально возвращено.
    pub returned: usize,
    /// Был ли результат усечён `limit`.
    pub truncated: bool,
    /// Найденные заметки в порядке экспорта.
    pub notes: Vec<FoundNote>,
}

/// Machine-readable описание критерия поиска.
#[derive(Debug)]
pub struct CriteriaSummary {
    /// `guid` или `field`.
    pub kind: &'static str,
    /// Искомый `guid`.
    pub guid: Option<String>,
    /// Имя поля.
    pub field: Option<String>,
    /// Искомое значение.
    pub value: Option<String>,
    /// Режим сопоставления.
    pub match_mode: Option<&'static str>,
    /// Ограничение по колоде.
    pub deck: Option<String>,
    /// Предел результата.
    pub limit: usize,
}

/// Найденная заметка с именованными полями.
#[derive(Debug)]
pub struct FoundNote {
    /// Идентификатор заметки.
    pub guid: Option<String>,
    /// Путь колоды.
    pub deck_path: String,
    /// Имя модели заметки.
    pub note_model_name: Option<String>,
    /// Идентичность модели заметки.
    pub note_model_uuid: Option<String>,
    /// Теги заметки.
    pub tags: Vec<String>,
    /// Поля заметки в порядке `ord` модели.
    pub fields: Vec<NamedField>,
}

/// Выполняет `find`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::UnknownDeck`], [`ErrorCode::UnknownField`],
/// [`ErrorCode::NotFound`] и [`ErrorCode::Ambiguous`].
pub fn find(
    export_dir: &Path,
    index: &ExportIndex<'_>,
    query: &FindQuery,
) -> Result<FindResult, DomainError> {
    let deck_scope = resolve_deck_scope(index, query.deck.as_deref())?;
    let criteria = summarize_criteria(query);
    let export_dir = export_dir.display().to_string();

    let matched: Vec<usize> = match &query.criteria {
        FindCriteria::Guid { guid } => index
            .note_positions_by_guid(guid)
            .iter()
            .copied()
            .filter(|position| in_scope(index, &index.notes[*position], deck_scope))
            .collect(),
        FindCriteria::Field { field, value, mode } => {
            ensure_known_field(index, field)?;
            index
                .notes
                .iter()
                .enumerate()
                .filter(|(_, entry)| in_scope(index, entry, deck_scope))
                .filter(|(_, entry)| note_field_matches(index, entry, field, value, *mode))
                .map(|(position, _)| position)
                .collect()
        }
    };

    if let FindCriteria::Guid { guid } = &query.criteria {
        return match matched.as_slice() {
            [] => Err(DomainError::with_details(
                ErrorCode::NotFound,
                format!("заметка с guid {guid:?} не найдена"),
                details! {
                    "guid" => guid,
                    "deck" => query.deck,
                },
            )),
            [position] => Ok(FindResult {
                export_dir,
                criteria,
                matched_total: 1,
                returned: 1,
                truncated: false,
                notes: vec![build_found_note(index, &index.notes[*position])],
            }),
            positions => Err(DomainError::with_details(
                ErrorCode::Ambiguous,
                format!(
                    "guid {guid:?} встречается в экспорте {count} раз; идентичность неоднозначна",
                    count = positions.len()
                ),
                details! {
                    "guid" => guid,
                    "matches" => positions.len(),
                },
            )),
        };
    }

    let returned = matched.len().min(query.limit);
    if matched.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::NotFound,
            "по заданному критерию не найдено ни одной заметки".to_string(),
            details! {
                "field" => criteria.field,
                "value" => criteria.value,
                "match_mode" => criteria.match_mode,
                "deck" => criteria.deck,
            },
        ));
    }
    let notes = matched
        .iter()
        .take(returned)
        .map(|position| build_found_note(index, &index.notes[*position]))
        .collect();

    Ok(FindResult {
        export_dir,
        criteria,
        matched_total: matched.len(),
        returned,
        truncated: matched.len() > returned,
        notes,
    })
}

fn build_found_note(index: &ExportIndex<'_>, entry: &NoteRef<'_>) -> FoundNote {
    let model = entry
        .note
        .note_model_uuid
        .as_deref()
        .and_then(|uuid| index.model_by_uuid(uuid));

    let fields = model.map_or_else(Vec::new, |model| {
        resolve_named_fields(entry.note, model)
            .into_iter()
            .map(|field| NamedField {
                name: field.name.to_string(),
                ord: field.ord.value(),
                value: field.value.map(crate::model::FieldValue::rendered),
            })
            .collect()
    });

    FoundNote {
        guid: entry.note.guid.clone(),
        deck_path: index.note_deck_path(entry).to_string(),
        note_model_name: model.and_then(|model| model.name.clone()),
        note_model_uuid: entry.note.note_model_uuid.clone(),
        tags: entry.note.tags.clone(),
        fields,
    }
}

fn summarize_criteria(query: &FindQuery) -> CriteriaSummary {
    match &query.criteria {
        FindCriteria::Guid { guid } => CriteriaSummary {
            kind: "guid",
            guid: Some(guid.clone()),
            field: None,
            value: None,
            match_mode: None,
            deck: query.deck.clone(),
            limit: query.limit,
        },
        FindCriteria::Field { field, value, mode } => CriteriaSummary {
            kind: "field",
            guid: None,
            field: Some(field.clone()),
            value: Some(value.clone()),
            match_mode: Some(mode.as_str()),
            deck: query.deck.clone(),
            limit: query.limit,
        },
    }
}

fn resolve_deck_scope(
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

fn ensure_known_field(index: &ExportIndex<'_>, field: &str) -> Result<(), DomainError> {
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

fn in_scope(index: &ExportIndex<'_>, entry: &NoteRef<'_>, scope: Option<usize>) -> bool {
    scope.is_none_or(|node| index.subtree_range(node).contains(&entry.node))
}

fn note_field_matches(
    index: &ExportIndex<'_>,
    entry: &NoteRef<'_>,
    field: &str,
    value: &str,
    mode: MatchMode,
) -> bool {
    let Some(model) = entry
        .note
        .note_model_uuid
        .as_deref()
        .and_then(|uuid| index.model_by_uuid(uuid))
    else {
        return false;
    };
    let Some(raw) = field_value_by_name(entry.note, model, field) else {
        return false;
    };
    let Some(text) = raw.as_text() else {
        return false;
    };
    match mode {
        MatchMode::Contains => text.contains(value),
        MatchMode::Exact => text == value,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::index::ExportIndex;
    use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT, deck_node, export_with};

    fn query_field(field: &str, value: &str, mode: MatchMode) -> FindQuery {
        FindQuery {
            criteria: FindCriteria::Field {
                field: field.to_string(),
                value: value.to_string(),
                mode,
            },
            deck: None,
            limit: 20,
        }
    }

    #[test]
    fn contains_search_returns_named_fields() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let result = find(
            Path::new("."),
            &index,
            &query_field("Слово", "偶然", MatchMode::Contains),
        )
        .expect("поиск должен найти заметку");

        assert_eq!(result.matched_total, 1);
        assert_eq!(result.returned, 1);
        assert!(!result.truncated);
        let note = &result.notes[0];
        assert_eq!(note.guid.as_deref(), Some("guid-1"));
        assert_eq!(note.deck_path, "Test::Deck");
        assert_eq!(note.note_model_name.as_deref(), Some("Слова"));
        assert_eq!(note.tags, vec!["тэг".to_string()]);
        let names: Vec<&str> = note
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, vec!["Слово", "Значение"]);
        assert_eq!(note.fields[1].value.as_deref(), Some("случайность"));
    }

    #[test]
    fn exact_search_requires_full_value() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        assert!(
            find(
                Path::new("."),
                &index,
                &query_field("Слово", "必然", MatchMode::Exact),
            )
            .is_ok()
        );
        let error = find(
            Path::new("."),
            &index,
            &query_field("Слово", "必", MatchMode::Exact),
        )
        .expect_err("точное совпадение не должно находиться по подстроке");
        assert_eq!(error.code, ErrorCode::NotFound);
    }

    #[test]
    fn content_search_without_matches_is_not_found() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let error = find(
            Path::new("."),
            &index,
            &query_field("Слово", "нет-такого", MatchMode::Contains),
        )
        .expect_err("пустой результат — not_found");
        assert_eq!(error.code, ErrorCode::NotFound);
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn limit_truncates_but_keeps_matched_total() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let mut query = query_field("Слово", "", MatchMode::Contains);
        query.limit = 1;
        let result = find(Path::new("."), &index, &query).expect("поиск");
        assert_eq!(result.matched_total, 2);
        assert_eq!(result.returned, 1);
        assert!(result.truncated);
        assert_eq!(result.notes.len(), 1);
    }

    #[test]
    fn guid_lookup_is_identity_lookup() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let query = FindQuery {
            criteria: FindCriteria::Guid {
                guid: "guid-2".to_string(),
            },
            deck: None,
            limit: 20,
        };
        let result = find(Path::new("."), &index, &query).expect("guid должен найтись");
        assert_eq!(result.matched_total, 1);
        assert_eq!(result.notes[0].guid.as_deref(), Some("guid-2"));

        let missing = FindQuery {
            criteria: FindCriteria::Guid {
                guid: "нет-такого".to_string(),
            },
            deck: None,
            limit: 20,
        };
        let error = find(Path::new("."), &index, &missing).expect_err("guid не найден");
        assert_eq!(error.code, ErrorCode::NotFound);
    }

    #[test]
    fn duplicate_guid_is_ambiguous() {
        let duplicated = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][1]["guid"] = serde_json::json!("guid-1");
        });
        let node = deck_node(&duplicated);
        let index = ExportIndex::build(&node);
        let query = FindQuery {
            criteria: FindCriteria::Guid {
                guid: "guid-1".to_string(),
            },
            deck: None,
            limit: 20,
        };
        let error = find(Path::new("."), &index, &query).expect_err("дубль guid неоднозначен");
        assert_eq!(error.code, ErrorCode::Ambiguous);
        assert_eq!(error.exit_code(), 5);
    }

    #[test]
    fn deck_restriction_covers_descendants() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);

        let mut query = query_field("Слово", "", MatchMode::Contains);
        query.deck = Some("Root::Child".to_string());
        let result = find(Path::new("."), &index, &query).expect("поиск по поддереву");
        assert_eq!(result.matched_total, 2);
        let decks: Vec<&str> = result
            .notes
            .iter()
            .map(|note| note.deck_path.as_str())
            .collect();
        assert_eq!(decks, vec!["Root::Child", "Root::Child::Leaf"]);

        query.deck = Some("Root::Child::Leaf".to_string());
        let result = find(Path::new("."), &index, &query).expect("поиск по листу");
        assert_eq!(result.matched_total, 1);
        assert_eq!(result.notes[0].deck_path, "Root::Child::Leaf");
    }

    #[test]
    fn unknown_field_and_deck_are_rejected() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);

        let error = find(
            Path::new("."),
            &index,
            &query_field("НетТакого", "x", MatchMode::Contains),
        )
        .expect_err("неизвестное поле");
        assert_eq!(error.code, ErrorCode::UnknownField);
        assert!(error.details["available_fields"].is_array());

        let mut query = query_field("Слово", "偶然", MatchMode::Contains);
        query.deck = Some("Нет::Такой".to_string());
        let error = find(Path::new("."), &index, &query).expect_err("неизвестная колода");
        assert_eq!(error.code, ErrorCode::UnknownDeck);
    }

    #[test]
    fn results_keep_export_order() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let result = find(
            Path::new("."),
            &index,
            &query_field("Слово", "", MatchMode::Contains),
        )
        .expect("поиск");
        let guids: Vec<&str> = result
            .notes
            .iter()
            .map(|note| note.guid.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(guids, vec!["guid-1", "guid-2"]);
    }
}
