//! Загрузка одного CrowdAnki-экспорта с диска.
//!
//! Один invocation работает с одним каталогом экспорта. `deck.json`
//! десериализуется целиком в память: streaming parser, кэш и индексные файлы
//! для текущего масштаба не нужны.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::model::DeckNode;

/// Имя файла экспорта внутри каталога колоды.
pub const DECK_JSON: &str = "deck.json";

/// Разобранный CrowdAnki-экспорт.
#[derive(Debug)]
pub struct LoadedExport {
    /// Каталог экспорта в том виде, в котором его получил CLI.
    pub export_dir: PathBuf,
    /// Полный путь к `deck.json`.
    pub deck_json: PathBuf,
    /// Корневой узел колоды.
    pub root: DeckNode,
}

/// Канонический тег корневой сущности CrowdAnki-экспорта.
pub const DECK_TYPE: &str = "Deck";

/// Читает и разбирает `<export_dir>/deck.json`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InputUnreadable`], [`ErrorCode::DeckJsonMissing`],
/// [`ErrorCode::InvalidJson`], [`ErrorCode::RootNotDeck`] или
/// [`ErrorCode::SchemaInvalid`].
pub fn load_export(export_dir: &Path) -> Result<LoadedExport, DomainError> {
    let metadata = fs::metadata(export_dir).map_err(|error| {
        DomainError::with_details(
            ErrorCode::InputUnreadable,
            format!(
                "каталог экспорта {} недоступен: {error}",
                export_dir.display()
            ),
            details! {
                "path" => export_dir.display().to_string(),
                "io_error" => error.to_string(),
            },
        )
    })?;

    if !metadata.is_dir() {
        return Err(DomainError::with_details(
            ErrorCode::InputUnreadable,
            format!(
                "{} не является каталогом; укажите каталог экспорта, например \
                 decks/japanese/words/Words__N3",
                export_dir.display()
            ),
            details! {
                "path" => export_dir.display().to_string(),
            },
        ));
    }

    let deck_json = export_dir.join(DECK_JSON);
    let raw = fs::read_to_string(&deck_json).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DomainError::with_details(
                ErrorCode::DeckJsonMissing,
                format!("в каталоге {} нет {DECK_JSON}", export_dir.display()),
                details! {
                    "path" => deck_json.display().to_string(),
                },
            )
        } else {
            DomainError::with_details(
                ErrorCode::InputUnreadable,
                format!("не удалось прочитать {}: {error}", deck_json.display()),
                details! {
                    "path" => deck_json.display().to_string(),
                    "io_error" => error.to_string(),
                },
            )
        }
    })?;

    let value = parse_deck_json(&raw, &deck_json)?;
    let root = typed_root(&value, &deck_json)?;

    Ok(LoadedExport {
        export_dir: export_dir.to_path_buf(),
        deck_json,
        root,
    })
}

/// Разбирает текст `deck.json` в JSON-значение.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InvalidJson`] с координатами ошибки.
pub fn parse_deck_json(raw: &str, path: &Path) -> Result<Value, DomainError> {
    serde_json::from_str(raw).map_err(|error| {
        DomainError::with_details(
            ErrorCode::InvalidJson,
            format!("{} не является валидным JSON: {error}", path.display()),
            details! {
                "path" => path.display().to_string(),
                "message" => error.to_string(),
                "line" => error.line(),
                "column" => error.column(),
            },
        )
    })
}

/// Проверяет, что значение является CrowdAnki-экспортом колоды.
///
/// # Errors
///
/// Возвращает [`ErrorCode::RootNotDeck`], если корень не объект или его
/// `__type__` отсутствует либо не равен `Deck`.
pub fn ensure_deck_root(value: &Value, path: &Path) -> Result<(), DomainError> {
    let Some(object) = value.as_object() else {
        return Err(DomainError::with_details(
            ErrorCode::RootNotDeck,
            format!(
                "корень {} не является JSON-объектом CrowdAnki Deck",
                path.display()
            ),
            details! {
                "path" => path.display().to_string(),
                "reason" => "root_not_object",
            },
        ));
    };

    match object.get("__type__").and_then(Value::as_str) {
        Some(DECK_TYPE) => Ok(()),
        Some(other) => Err(DomainError::with_details(
            ErrorCode::RootNotDeck,
            format!(
                "корень {} имеет __type__ = {other:?}, ожидался {DECK_TYPE:?}",
                path.display()
            ),
            details! {
                "path" => path.display().to_string(),
                "reason" => "type_mismatch",
                "observed" => other,
            },
        )),
        None => Err(DomainError::with_details(
            ErrorCode::RootNotDeck,
            format!(
                "в корне {} отсутствует строковый __type__ = {DECK_TYPE:?}",
                path.display()
            ),
            details! {
                "path" => path.display().to_string(),
                "reason" => "type_missing",
            },
        )),
    }
}

/// Приводит проверенное JSON-значение к типизированному корню экспорта.
///
/// # Errors
///
/// Возвращает [`ErrorCode::SchemaInvalid`], если типизированное ядро не
/// собирается из валидного JSON.
pub fn typed_root(value: &Value, path: &Path) -> Result<DeckNode, DomainError> {
    ensure_deck_root(value, path)?;

    serde_json::from_value::<DeckNode>(value.clone()).map_err(|error| {
        DomainError::with_details(
            ErrorCode::SchemaInvalid,
            format!(
                "{} не соответствует ожидаемому типизированному ядру CrowdAnki: {error}",
                path.display()
            ),
            details! {
                "path" => path.display().to_string(),
                "message" => error.to_string(),
            },
        )
    })
}
