//! Доменные ошибки и их отображение на process exit codes.
//!
//! Machine-readable контракт — только значения [`ErrorCode`] в snake_case.
//! Rust-имена типов наружу не выходят.

use std::fmt;

use serde_json::{Map, Value};

/// Стабильные machine-readable коды доменных ошибок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorCode {
    /// Некорректная комбинация аргументов, обнаруженная вне clap.
    Usage,
    /// Не удалось прочитать каталог экспорта.
    InputUnreadable,
    /// В каталоге экспорта нет `deck.json`.
    DeckJsonMissing,
    /// `deck.json` не является валидным JSON.
    InvalidJson,
    /// Корневую сущность нельзя интерпретировать как CrowdAnki `Deck`.
    RootNotDeck,
    /// Валидный JSON не соответствует ожидаемому типизированному ядру.
    SchemaInvalid,
    /// Запрошенное имя поля отсутствует во всех note models экспорта.
    UnknownField,
    /// Запрошенный deck path отсутствует в экспорте.
    UnknownDeck,
    /// `find` по идентичности не нашёл ни одного совпадения.
    NotFound,
    /// `find` по идентичности нашёл больше одного совпадения.
    Ambiguous,
    /// Непредвиденный внутренний сбой.
    Internal,
}

impl ErrorCode {
    /// Стабильное snake_case представление кода.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::InputUnreadable => "input_unreadable",
            Self::DeckJsonMissing => "deck_json_missing",
            Self::InvalidJson => "invalid_json",
            Self::RootNotDeck => "root_not_deck",
            Self::SchemaInvalid => "schema_invalid",
            Self::UnknownField => "unknown_field",
            Self::UnknownDeck => "unknown_deck",
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::Internal => "internal_error",
        }
    }

    /// Process exit code, соответствующий коду ошибки.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::InputUnreadable
            | Self::DeckJsonMissing
            | Self::InvalidJson
            | Self::RootNotDeck
            | Self::SchemaInvalid
            | Self::UnknownField
            | Self::UnknownDeck => 3,
            Self::NotFound => 4,
            Self::Ambiguous => 5,
            Self::Internal => 70,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Доменная ошибка выполнения команды.
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct DomainError {
    /// Стабильный machine-readable код.
    pub code: ErrorCode,
    /// Человекочитаемое объяснение (на русском языке).
    pub message: String,
    /// Дополнительные machine-readable детали.
    pub details: Value,
}

impl DomainError {
    /// Ошибка без дополнительных деталей.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Object(Map::new()),
        }
    }

    /// Ошибка с дополнительными деталями.
    pub fn with_details(code: ErrorCode, message: impl Into<String>, details: Value) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }

    /// Process exit code этой ошибки.
    pub const fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }
}

/// Собирает JSON-объект деталей из пар «ключ — значение».
#[macro_export]
macro_rules! details {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = ::serde_json::Map::new();
        $(
            map.insert(
                ::std::string::String::from($key),
                ::serde_json::json!($value),
            );
        )*
        ::serde_json::Value::Object(map)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_process_contract() {
        assert_eq!(ErrorCode::Usage.exit_code(), 2);
        assert_eq!(ErrorCode::InputUnreadable.exit_code(), 3);
        assert_eq!(ErrorCode::DeckJsonMissing.exit_code(), 3);
        assert_eq!(ErrorCode::InvalidJson.exit_code(), 3);
        assert_eq!(ErrorCode::RootNotDeck.exit_code(), 3);
        assert_eq!(ErrorCode::SchemaInvalid.exit_code(), 3);
        assert_eq!(ErrorCode::UnknownField.exit_code(), 3);
        assert_eq!(ErrorCode::UnknownDeck.exit_code(), 3);
        assert_eq!(ErrorCode::NotFound.exit_code(), 4);
        assert_eq!(ErrorCode::Ambiguous.exit_code(), 5);
        assert_eq!(ErrorCode::Internal.exit_code(), 70);
    }

    #[test]
    fn codes_are_stable_snake_case() {
        let codes = [
            ErrorCode::Usage,
            ErrorCode::InputUnreadable,
            ErrorCode::DeckJsonMissing,
            ErrorCode::InvalidJson,
            ErrorCode::RootNotDeck,
            ErrorCode::SchemaInvalid,
            ErrorCode::UnknownField,
            ErrorCode::UnknownDeck,
            ErrorCode::NotFound,
            ErrorCode::Ambiguous,
            ErrorCode::Internal,
        ];
        for code in codes {
            let text = code.as_str();
            assert!(
                text.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "код {text} не является snake_case"
            );
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn domain_error_reports_its_exit_code() {
        let error = DomainError::new(ErrorCode::NotFound, "нет совпадений");
        assert_eq!(error.exit_code(), 4);
        assert!(error.details.is_object());
    }

    #[test]
    fn details_macro_builds_object_with_values() {
        let details = crate::details! {
            "count" => 3,
            "name" => "значение",
        };
        assert_eq!(details["count"], serde_json::json!(3));
        assert_eq!(details["name"], serde_json::json!("значение"));
    }
}
