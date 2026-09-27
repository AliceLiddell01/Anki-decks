//! Выбор целевой колоды: единственное место, где селектор становится узлом.
//!
//! Селектор колоды умеет три независимых способа адресации: полный путь
//! (`DeckNode.name`, с `::`), идентичность CrowdAnki (`crowdanki_uuid`) и
//! позиция в preorder-обходе. Если селектор не сводится ровно к одному узлу,
//! операция обязана упасть, а не выбрать «первое подходящее»: создание заметки
//! в неверной колоде — это ошибка идентичности, а не удобство.
//!
//! Несколько способов адресации в одном запросе разрешены и обязаны указывать
//! на один и тот же узел. Это даёт агенту возможность одной строкой выразить
//! и «куда писать», и «что именно там ожидается»: путь читается человеком, а
//! `crowdanki_uuid` не меняется при переименовании колоды.

use serde::Deserialize;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::ops::source::internal;

/// Селектор целевой колоды из запроса.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckSelector {
    /// Полное имя колоды (`DeckNode.name`).
    #[serde(default)]
    pub path: Option<String>,
    /// Идентичность CrowdAnki узла колоды.
    #[serde(default)]
    pub crowdanki_uuid: Option<String>,
    /// Позиция узла в preorder-обходе экспорта.
    #[serde(default)]
    pub preorder: Option<usize>,
}

/// Разрешённая целевая колода.
#[derive(Debug, Clone)]
pub struct ResolvedDeck {
    /// Позиция в preorder-обходе.
    pub preorder: usize,
    /// Полное имя колоды.
    pub path: String,
    /// Идентичность CrowdAnki, если объявлена.
    pub crowdanki_uuid: Option<String>,
    /// Путь до узла по массивам `children` в JSON-дереве.
    pub children: Vec<usize>,
    /// Число заметок, объявленных непосредственно в этом узле.
    pub notes_in_deck: usize,
}

impl DeckSelector {
    /// Указан ли хотя бы один способ адресации.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.path.is_none() && self.crowdanki_uuid.is_none() && self.preorder.is_none()
    }

    /// Требует, чтобы селектор содержал хотя бы один способ адресации.
    ///
    /// # Errors
    ///
    /// Возвращает [`ErrorCode::InvalidRequest`] для пустого селектора.
    pub fn ensure_present(&self, subject: &str) -> Result<(), DomainError> {
        if self.is_empty() {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!("{subject}: не указан ни path, ни crowdanki_uuid, ни preorder"),
                details! { "field" => "deck" },
            ));
        }
        Ok(())
    }

    /// Разрешает селектор ровно в один узел колоды.
    ///
    /// `child_paths` обязателен: это результат
    /// [`crate::ops::source::deck_child_paths`] по тому же JSON-дереву, из
    /// которого построен `index`. Совпадение позиций обеспечивается тем, что
    /// оба обхода идут в preorder-порядке.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::UnknownDeck`] — способ адресации не совпал ни с одним узлом;
    /// [`ErrorCode::AmbiguousDeck`] — совпал более чем с одним;
    /// [`ErrorCode::DeckIdentityMismatch`] — способы указывают на разные узлы;
    /// [`ErrorCode::Internal`] — рассинхронизация проекций дерева.
    pub fn resolve(
        &self,
        index: &ExportIndex<'_>,
        child_paths: &[Vec<usize>],
    ) -> Result<ResolvedDeck, DomainError> {
        self.ensure_present("селектор целевой колоды")?;

        if child_paths.len() != index.nodes.len() {
            return Err(internal(format!(
                "число узлов в JSON ({}) и в типизированном дереве ({}) различается",
                child_paths.len(),
                index.nodes.len()
            )));
        }

        let mut resolved: Vec<(String, usize)> = Vec::new();

        if let Some(path) = self.path.as_deref() {
            resolved.push(("path".to_string(), self.match_path(index, path)?));
        }
        if let Some(uuid) = self.crowdanki_uuid.as_deref() {
            resolved.push(("crowdanki_uuid".to_string(), self.match_uuid(index, uuid)?));
        }
        if let Some(preorder) = self.preorder {
            resolved.push((
                "preorder".to_string(),
                self.match_preorder(index, preorder)?,
            ));
        }

        let first = resolved
            .first()
            .map(|(_, preorder)| *preorder)
            .ok_or_else(empty_selector_error)?;

        if let Some((kind, other)) = resolved
            .iter()
            .find(|(_, preorder)| *preorder != first)
            .map(|(kind, preorder)| (kind.clone(), *preorder))
        {
            return Err(DomainError::with_details(
                ErrorCode::DeckIdentityMismatch,
                format!(
                    "селекторы колоды указывают на разные узлы: {kind} → «{}», а первый способ → «{}»",
                    index.nodes[other].path, index.nodes[first].path
                ),
                details! {
                    "field" => "deck",
                    "selector" => kind,
                    "resolved_preorder" => other,
                    "expected_preorder" => first,
                    "resolved_path" => index.nodes[other].path,
                    "expected_path" => index.nodes[first].path,
                },
            ));
        }

        let node = &index.nodes[first];
        Ok(ResolvedDeck {
            preorder: first,
            path: node.path.to_string(),
            crowdanki_uuid: node.node.crowdanki_uuid.clone(),
            children: child_paths[first].clone(),
            notes_in_deck: index.notes.iter().filter(|note| note.node == first).count(),
        })
    }

    fn match_path(&self, index: &ExportIndex<'_>, path: &str) -> Result<usize, DomainError> {
        let matches: Vec<usize> = index
            .nodes
            .iter()
            .filter(|node| node.path == path)
            .map(|node| node.preorder)
            .collect();
        single(matches, "path", path, || {
            let known: Vec<&str> = index.nodes.iter().map(|node| node.path).collect();
            details! {
                "field" => "deck.path",
                "path" => path,
                "known_paths" => known,
            }
        })
    }

    fn match_uuid(&self, index: &ExportIndex<'_>, uuid: &str) -> Result<usize, DomainError> {
        let matches: Vec<usize> = index
            .nodes
            .iter()
            .filter(|node| node.node.crowdanki_uuid.as_deref() == Some(uuid))
            .map(|node| node.preorder)
            .collect();
        single(matches, "crowdanki_uuid", uuid, || {
            details! {
                "field" => "deck.crowdanki_uuid",
                "crowdanki_uuid" => uuid,
            }
        })
    }

    fn match_preorder(
        &self,
        index: &ExportIndex<'_>,
        preorder: usize,
    ) -> Result<usize, DomainError> {
        if preorder < index.nodes.len() {
            return Ok(preorder);
        }

        Err(DomainError::with_details(
            ErrorCode::UnknownDeck,
            format!(
                "позиция preorder {preorder} вне дерева колод экспорта (узлов: {})",
                index.nodes.len()
            ),
            details! {
                "field" => "deck.preorder",
                "preorder" => preorder,
                "nodes" => index.nodes.len(),
            },
        ))
    }
}

/// Ошибка пустого селектора: без способа адресации выбирать не из чего.
fn empty_selector_error() -> DomainError {
    DomainError::with_details(
        ErrorCode::InvalidRequest,
        "селектор целевой колоды: не указан ни path, ни crowdanki_uuid, ни preorder",
        details! { "field" => "deck" },
    )
}

/// Требует ровно одно совпадение способа адресации.
fn single(
    matches: Vec<usize>,
    kind: &str,
    value: &str,
    extra: impl FnOnce() -> serde_json::Value,
) -> Result<usize, DomainError> {
    match matches.as_slice() {
        [only] => Ok(*only),
        [] => Err(DomainError::with_details(
            ErrorCode::UnknownDeck,
            format!("в экспорте нет узла колоды с {kind} = {value:?}"),
            extra(),
        )),
        many => Err(DomainError::with_details(
            ErrorCode::AmbiguousDeck,
            format!(
                "{kind} = {value:?} совпадает с {} узлами колоды; \
                 выбери узел по crowdanki_uuid или preorder",
                many.len()
            ),
            extra(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeckNode;
    use crate::ops::source::deck_child_paths;
    use crate::{test_support::MINIMAL_EXPORT, test_support::NESTED_EXPORT};

    fn setup(json: &str) -> (serde_json::Value, DeckNode) {
        (
            serde_json::from_str(json).expect("JSON"),
            serde_json::from_str(json).expect("JSON"),
        )
    }

    fn resolve(json: &str, selector: DeckSelector) -> Result<ResolvedDeck, DomainError> {
        let (values, tree) = setup(json);
        let paths = deck_child_paths(&values).expect("пути");
        let index = ExportIndex::build(&tree);
        selector.resolve(&index, &paths)
    }

    #[test]
    fn nested_selector_by_path_finds_the_child_node() {
        let deck = resolve(
            NESTED_EXPORT,
            DeckSelector {
                path: Some("Root::Child".to_string()),
                ..DeckSelector::default()
            },
        )
        .expect("узел");
        assert_eq!(deck.preorder, 1);
        assert_eq!(deck.children, vec![0]);
        assert_eq!(deck.notes_in_deck, 1);
        assert_eq!(deck.crowdanki_uuid.as_deref(), Some("deck-child"));
    }

    #[test]
    fn preorder_and_path_and_uuid_agree() {
        let deck = resolve(
            NESTED_EXPORT,
            DeckSelector {
                path: Some("Root".to_string()),
                crowdanki_uuid: Some("deck-root".to_string()),
                preorder: Some(0),
            },
        )
        .expect("один и тот же узел");
        assert_eq!(deck.path, "Root");
    }

    #[test]
    fn disagreeing_selectors_are_rejected() {
        let error = resolve(
            NESTED_EXPORT,
            DeckSelector {
                path: Some("Root".to_string()),
                crowdanki_uuid: None,
                preorder: Some(1),
            },
        )
        .expect_err("разные узлы");
        assert_eq!(error.code, ErrorCode::DeckIdentityMismatch);
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn unknown_and_out_of_range_selectors_fail_closed() {
        let error = resolve(
            MINIMAL_EXPORT,
            DeckSelector {
                path: Some("Нет такой".to_string()),
                ..DeckSelector::default()
            },
        )
        .expect_err("нет узла");
        assert_eq!(error.code, ErrorCode::UnknownDeck);

        let error = resolve(
            MINIMAL_EXPORT,
            DeckSelector {
                crowdanki_uuid: Some("нет-такого-uuid".to_string()),
                ..DeckSelector::default()
            },
        )
        .expect_err("нет uuid");
        assert_eq!(error.code, ErrorCode::UnknownDeck);

        let error = resolve(
            MINIMAL_EXPORT,
            DeckSelector {
                preorder: Some(9),
                ..DeckSelector::default()
            },
        )
        .expect_err("вне дерева");
        assert_eq!(error.code, ErrorCode::UnknownDeck);
    }

    #[test]
    fn duplicated_deck_name_is_ambiguous() {
        let json = r#"{
            "__type__": "Deck",
            "name": "Слова",
            "crowdanki_uuid": "deck-root",
            "notes": [],
            "children": [
                {"__type__": "Deck", "name": "Слова", "crowdanki_uuid": "uuid-a", "notes": [], "children": []},
                {"__type__": "Deck", "name": "Слова", "crowdanki_uuid": "uuid-b", "notes": [], "children": []}
            ]
        }"#;
        let error = resolve(
            json,
            DeckSelector {
                path: Some("Слова".to_string()),
                ..DeckSelector::default()
            },
        )
        .expect_err("неоднозначно");
        assert_eq!(error.code, ErrorCode::AmbiguousDeck);
        assert_eq!(error.exit_code(), 5);

        let error = resolve(
            json,
            DeckSelector {
                path: Some("Слова".to_string()),
                crowdanki_uuid: Some("uuid-b".to_string()),
                ..DeckSelector::default()
            },
        )
        .expect_err("uuid не снимает неоднозначность пути: путь должен быть однозначен сам");
        assert_eq!(error.code, ErrorCode::AmbiguousDeck);
    }

    #[test]
    fn empty_selector_is_a_request_error() {
        let selector = DeckSelector::default();
        assert!(selector.is_empty());
        let error = selector
            .ensure_present("заметка #1")
            .expect_err("пустой селектор");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
}
