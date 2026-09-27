//! Операция `find`: поиск небольшого набора заметок по предсказуемым критериям.
//!
//! Toolkit сознательно не делает HTML-to-headword extraction, fuzzy matching,
//! regex и японскую морфологию. Поиск идёт по сырым значениям полей, а имя поля
//! всегда задаёт вызывающая сторона.

use std::path::Path;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{ExportIndex, NoteRef};
use crate::ops::NoteSummary;
use crate::selection;

/// Режим сопоставления значения поля.
///
/// Определён в общем слое выбора ([`crate::selection`]) и переэкспортируется
/// здесь, потому что исторически принадлежит контракту `find`.
pub use crate::selection::MatchMode;

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
///
/// Сводка заметки общая для читающих команд ([`crate::ops::NoteSummary`]);
/// `FoundNote` сохраняет историческое имя контракта `find`.
pub type FoundNote = NoteSummary;

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
    let deck_scope = selection::resolve_deck_scope(index, query.deck.as_deref())?;
    let criteria = summarize_criteria(query);
    let export_dir = export_dir.display().to_string();

    let matched: Vec<usize> = match &query.criteria {
        FindCriteria::Guid { guid } => selection::positions_by_guid(index, guid, deck_scope),
        FindCriteria::Field { field, value, mode } => {
            selection::ensure_known_field(index, field)?;
            selection::positions_matching_field(index, field, value, *mode, deck_scope)
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
    NoteSummary::build(index, entry)
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
            &query_field("Заголовок", "偶然", MatchMode::Contains),
        )
        .expect("поиск должен найти заметку");

        assert_eq!(result.matched_total, 1);
        assert_eq!(result.returned, 1);
        assert!(!result.truncated);
        let note = &result.notes[0];
        assert_eq!(note.guid.as_deref(), Some("guid-1"));
        assert_eq!(note.deck_path, "Test::Deck");
        assert_eq!(note.note_model_name.as_deref(), Some("Тестовая модель"));
        assert_eq!(note.tags, vec!["тэг".to_string()]);
        let names: Vec<&str> = note
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, vec!["Заголовок", "Толкование"]);
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
                &query_field("Заголовок", "必然", MatchMode::Exact),
            )
            .is_ok()
        );
        let error = find(
            Path::new("."),
            &index,
            &query_field("Заголовок", "必", MatchMode::Exact),
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
            &query_field("Заголовок", "нет-такого", MatchMode::Contains),
        )
        .expect_err("пустой результат — not_found");
        assert_eq!(error.code, ErrorCode::NotFound);
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn limit_truncates_but_keeps_matched_total() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let mut query = query_field("Заголовок", "", MatchMode::Contains);
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

        let mut query = query_field("Заголовок", "", MatchMode::Contains);
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

        let mut query = query_field("Заголовок", "偶然", MatchMode::Contains);
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
            &query_field("Заголовок", "", MatchMode::Contains),
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
