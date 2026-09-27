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
    let (deck_json, raw) = read_deck_json(export_dir)?;
    let value = parse_deck_json(&raw, &deck_json)?;
    let root = typed_root(value, &deck_json)?;

    Ok(LoadedExport {
        export_dir: export_dir.to_path_buf(),
        deck_json,
        root,
    })
}

/// Читает текст `deck.json` из каталога экспорта.
///
/// Единая точка проверки каталога и чтения файла: `op::validate` разбирает
/// содержимое сам (ошибки JSON становятся issues, а не domain error), но
/// путь, сообщения и коды ошибок должны совпадать с [`load_export`].
///
/// # Errors
///
/// Возвращает [`ErrorCode::InputUnreadable`], если каталог недоступен или не
/// является каталогом, и [`ErrorCode::DeckJsonMissing`], если файла нет.
pub fn read_deck_json(export_dir: &Path) -> Result<(PathBuf, String), DomainError> {
    let deck_json = deck_json_path(export_dir)?;
    let raw =
        fs::read_to_string(&deck_json).map_err(|error| read_deck_json_error(&deck_json, &error))?;

    Ok((deck_json, raw))
}

/// Читает `deck.json` как сырые байты.
///
/// Нужна мутирующему пути: там важно байтовое равенство исходника и
/// канонической формы, а не текст. Контракт каталога и коды ошибок полностью
/// совпадают с [`read_deck_json`].
///
/// # Errors
///
/// Возвращает [`ErrorCode::InputUnreadable`] или [`ErrorCode::DeckJsonMissing`]
/// по тем же правилам, что и [`read_deck_json`].
pub fn read_deck_json_bytes(export_dir: &Path) -> Result<(PathBuf, Vec<u8>), DomainError> {
    let deck_json = deck_json_path(export_dir)?;
    let raw = fs::read(&deck_json).map_err(|error| read_deck_json_error(&deck_json, &error))?;

    Ok((deck_json, raw))
}

/// Проверяет каталог экспорта и возвращает путь к его `deck.json`.
///
/// Единственная реализация каталог-контракта: и текстовое, и байтовое чтение
/// обязаны сообщать об одной и той же проблеме одинаково.
fn deck_json_path(export_dir: &Path) -> Result<PathBuf, DomainError> {
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

    Ok(export_dir.join(DECK_JSON))
}

/// Отображает ошибку чтения `deck.json` на доменную ошибку.
fn read_deck_json_error(deck_json: &Path, error: &std::io::Error) -> DomainError {
    if error.kind() == std::io::ErrorKind::NotFound {
        let export_dir = deck_json.parent().unwrap_or(deck_json);
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

/// Разбирает байты `deck.json` в JSON-значение.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InvalidJson`], если байты не являются валидным
/// UTF-8 или валидным JSON.
pub fn parse_deck_json_bytes(raw: &[u8], path: &Path) -> Result<Value, DomainError> {
    let text = std::str::from_utf8(raw).map_err(|error| {
        DomainError::with_details(
            ErrorCode::InvalidJson,
            format!("{} не является валидным UTF-8: {error}", path.display()),
            details! {
                "path" => path.display().to_string(),
                "message" => error.to_string(),
            },
        )
    })?;

    parse_deck_json(text, path)
}

/// Отступ канонической формы `deck.json` этого репозитория.
pub const CANONICAL_INDENT: &[u8] = b"    ";

/// Сериализует JSON-значение в каноническую форму экспорта.
///
/// Каноническая форма — это то, что уже лежит в репозитории: LF, отступ в
/// четыре пробела, ключи по алфавиту (следствие `BTreeMap` в `serde_json`
/// без `preserve_order`) и отсутствие завершающего перевода строки. Именно
/// побайтовое воспроизведение этой формы позволяет менять одно значение поля,
/// не переписывая остальной файл.
///
/// # Errors
///
/// Возвращает [`ErrorCode::Internal`]: сериализация уже разобранного значения
/// не может упасть по вине входных данных.
pub fn render_canonical_bytes(value: &Value) -> Result<Vec<u8>, DomainError> {
    let mut out: Vec<u8> = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut out,
        serde_json::ser::PrettyFormatter::with_indent(CANONICAL_INDENT),
    );

    serde::Serialize::serialize(value, &mut serializer).map_err(|error| {
        DomainError::new(
            ErrorCode::Internal,
            format!("не удалось сериализовать документ в каноническую форму: {error}"),
        )
    })?;

    Ok(out)
}

/// Признак того, что байты уже находятся в канонической форме.
///
/// Проверка нужна мутирующему пути: изменение точечного значения допустимо
/// только тогда, когда перезапись гарантированно не тронет остальной файл.
pub fn is_canonical_bytes(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };

    render_canonical_bytes(&value).is_ok_and(|rendered| rendered == raw)
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
/// Значение передаётся по владению: `serde_json::from_value` потребляет его
/// целиком, поэтому копия многомегабайтного дерева не нужна.
///
/// # Errors
///
/// Возвращает [`ErrorCode::SchemaInvalid`], если типизированное ядро не
/// собирается из валидного JSON.
pub fn typed_root(value: Value, path: &Path) -> Result<DeckNode, DomainError> {
    ensure_deck_root(&value, path)?;

    serde_json::from_value::<DeckNode>(value).map_err(|error| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    /// Небольшой документ в канонической форме этого репозитория.
    const CANONICAL: &str = "{\n    \"__type__\": \"Deck\",\n    \"name\": \"D\"\n}";

    #[test]
    fn canonical_render_reproduces_repository_layout() {
        let value: Value = serde_json::from_str(CANONICAL).expect("JSON");
        let rendered = render_canonical_bytes(&value).expect("сериализация");

        assert_eq!(
            std::str::from_utf8(&rendered).expect("utf-8"),
            CANONICAL,
            "каноническая форма должна совпадать с исходным текстом байт в байт"
        );
    }

    #[test]
    fn canonical_form_has_no_trailing_newline_and_sorted_keys() {
        let value: Value =
            serde_json::from_str(r#"{"b": 1, "a": {"d": 2, "c": 3}}"#).expect("JSON");
        let rendered = render_canonical_bytes(&value).expect("сериализация");
        let text = std::str::from_utf8(&rendered).expect("utf-8");

        assert_eq!(
            text,
            "{\n    \"a\": {\n        \"c\": 3,\n        \"d\": 2\n    },\n    \"b\": 1\n}"
        );
        assert!(!text.ends_with('\n'));
        assert!(!text.contains('\r'));
    }

    #[test]
    fn canonical_form_keeps_non_ascii_raw() {
        let value: Value = serde_json::from_str(r#"{"значение": "偶然"}"#).expect("JSON");
        let rendered = render_canonical_bytes(&value).expect("сериализация");

        assert_eq!(
            std::str::from_utf8(&rendered).expect("utf-8"),
            "{\n    \"значение\": \"偶然\"\n}"
        );
    }

    #[test]
    fn canonical_detection_accepts_only_canonical_bytes() {
        assert!(is_canonical_bytes(CANONICAL.as_bytes()));

        let value: Value = serde_json::from_str(CANONICAL).expect("JSON");
        let compact = serde_json::to_string(&value).expect("compact");

        for variant in [
            compact,
            format!("{CANONICAL}\n"),
            CANONICAL.replace('\n', "\r\n"),
            serde_json::to_string_pretty(&value).expect("pretty"),
        ] {
            assert!(
                !is_canonical_bytes(variant.as_bytes()),
                "неканоническая форма не должна приниматься: {variant:?}"
            );
        }

        assert!(!is_canonical_bytes("не json".as_bytes()));
        assert!(!is_canonical_bytes(&[0xff, 0xfe, 0xfd]));
    }

    #[test]
    fn byte_reader_matches_text_reader() {
        let dir = TempDir::new("loader-bytes");
        fs::write(dir.path().join(DECK_JSON), CANONICAL).expect("deck.json");

        let (text_path, text) = read_deck_json(dir.path()).expect("текстовое чтение");
        let (byte_path, bytes) = read_deck_json_bytes(dir.path()).expect("байтовое чтение");

        assert_eq!(text_path, byte_path);
        assert_eq!(bytes, text.as_bytes());
        assert_eq!(
            parse_deck_json_bytes(&bytes, &byte_path).expect("разбор"),
            parse_deck_json(&text, &text_path).expect("разбор")
        );
    }

    #[test]
    fn byte_reader_reports_the_same_directory_problems() {
        let dir = TempDir::new("loader-missing");

        let text_error = read_deck_json(dir.path()).expect_err("нет deck.json");
        let byte_error = read_deck_json_bytes(dir.path()).expect_err("нет deck.json");
        assert_eq!(text_error.code, ErrorCode::DeckJsonMissing);
        assert_eq!(byte_error.code, ErrorCode::DeckJsonMissing);
        assert_eq!(text_error.message, byte_error.message);

        let file = dir.path().join("файл");
        fs::write(&file, b"x").expect("файл");
        let by_file = read_deck_json_bytes(&file).expect_err("не каталог");
        assert_eq!(by_file.code, ErrorCode::InputUnreadable);
    }

    #[test]
    fn byte_reader_rejects_invalid_utf8() {
        let dir = TempDir::new("loader-utf8");
        fs::write(dir.path().join(DECK_JSON), [0xff, 0xfe]).expect("deck.json");

        let (path, bytes) = read_deck_json_bytes(dir.path()).expect("чтение");
        let error = parse_deck_json_bytes(&bytes, &path).expect_err("не utf-8");
        assert_eq!(error.code, ErrorCode::InvalidJson);
    }
}
