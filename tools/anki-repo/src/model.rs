//! Гибридная модель CrowdAnki.
//!
//! Стабильное ядро формата описано типизированными структурами, а все
//! неизвестные свойства сохраняются в `extra` через `#[serde(flatten)]`.
//! Ни одна сущность не использует `deny_unknown_fields`.
//!
//! Модель намеренно не описывает весь теоретически возможный формат Anki.

use serde::Deserialize;
use serde::de::Deserializer;
use serde_json::{Map, Value};

/// Значение `ord` у определения поля (`flds`) или шаблона (`tmpls`).
///
/// Намеренно не превращает malformed данные в невнятную parse error:
/// отрицательные, нецелые и отсутствующие значения доезжают до validator'а
/// как [`Ord::Invalid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ord {
    /// Целое значение, в том числе отрицательное.
    Int(i64),
    /// Значение отсутствует или не является целым числом.
    #[default]
    Invalid,
}

impl Ord {
    /// Числовое значение, если оно корректно распознано.
    pub const fn value(self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(value),
            Self::Invalid => None,
        }
    }

    /// Признак распознанного целого значения.
    pub const fn is_valid(self) -> bool {
        matches!(self, Self::Int(_))
    }
}

impl<'de> Deserialize<'de> for Ord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(match value.as_i64() {
            Some(number) => Self::Int(number),
            None => Self::Invalid,
        })
    }
}

impl std::fmt::Display for Ord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Invalid => f.write_str("?"),
        }
    }
}

/// Значение одного поля заметки.
///
/// CrowdAnki в норме хранит строки, но loader не падает на неожиданный тип:
/// validator может диагностировать такую структурную проблему.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// Строковое значение.
    Text(String),
    /// Значение другого JSON-типа.
    Other(Value),
}

impl FieldValue {
    /// Строковое представление, если значение действительно строка.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Other(_) => None,
        }
    }

    /// Признак пустого (но корректного строкового) значения.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Text(text) if text.is_empty())
    }

    /// Приведение к тексту для вывода: строки как есть, остальное — compact JSON.
    pub fn rendered(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Other(value) => value.to_string(),
        }
    }
}

impl<'de> Deserialize<'de> for FieldValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(match value {
            Value::String(text) => Self::Text(text),
            other => Self::Other(other),
        })
    }
}

/// Десериализует отсутствующее или `null` значение как пустой вектор.
fn de_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Десериализует строку, не превращая неожиданный тип в ошибку загрузки.
///
/// Модель намеренно не описывает весь формат Anki, поэтому поле, значение
/// которого пришло не строкой, должно доехать до диагностики, а не сломать
/// разбор всего `deck.json`.
fn de_lenient_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text),
        Some(other) => Some(other.to_string()),
    })
}

/// Определение поля модели заметки (`note_models[].flds[]`).
#[derive(Debug, Clone, Deserialize)]
pub struct FieldDef {
    /// Имя поля.
    #[serde(default)]
    pub name: String,
    /// Позиция поля, соответствующая индексу в `Note.fields`.
    #[serde(default)]
    pub ord: Ord,
    /// Описание поля из Anki (ключ `description`). В реальных экспортах часто
    /// пустое: подпись поля, а не источник семантики его содержимого.
    #[serde(default, deserialize_with = "de_lenient_string")]
    pub description: Option<String>,
    /// Неизвестные свойства определения поля.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Шаблон карточки (`note_models[].tmpls[]`) в объёме, нужном validation.
#[derive(Debug, Clone, Deserialize)]
pub struct TemplateDef {
    /// Имя шаблона.
    #[serde(default)]
    pub name: Option<String>,
    /// Позиция шаблона.
    #[serde(default)]
    pub ord: Ord,
    /// Лицевая сторона карточки.
    #[serde(default)]
    pub qfmt: String,
    /// Обратная сторона карточки.
    #[serde(default)]
    pub afmt: String,
    /// Неизвестные свойства шаблона.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Модель заметки (`note_models[]`).
#[derive(Debug, Clone, Deserialize)]
pub struct NoteModel {
    /// Идентичность CrowdAnki.
    #[serde(default)]
    pub crowdanki_uuid: Option<String>,
    /// Имя модели.
    #[serde(default)]
    pub name: Option<String>,
    /// Значение ключа `type`: `0` — обычная модель, `1` — cloze.
    ///
    /// Неожиданное значение не ломает загрузку: оно доезжает как
    /// [`Ord::Invalid`] и становится диагностикой, а не ошибкой разбора.
    #[serde(rename = "type", default)]
    pub model_type: Ord,
    /// CSS модели. Anki подключает его как отдельный `<style>` на карточку.
    #[serde(default, deserialize_with = "de_lenient_string")]
    pub css: Option<String>,
    /// Значение ключа `req` как есть.
    ///
    /// `req` — legacy-кэш требований генерации карт: Anki пересчитывает его сам,
    /// а решение о генерации карточки принимается по фронт-шаблону, а не по
    /// этому ключу. Toolkit поэтому его не интерпретирует, но показывает как
    /// факт формата.
    #[serde(default)]
    pub req: Option<Value>,
    /// Определения полей.
    #[serde(default, deserialize_with = "de_vec")]
    pub flds: Vec<FieldDef>,
    /// Шаблоны карточек.
    #[serde(default, deserialize_with = "de_vec")]
    pub tmpls: Vec<TemplateDef>,
    /// Неизвестные свойства модели.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Конфигурация колоды (`deck_configurations[]`).
#[derive(Debug, Clone, Deserialize)]
pub struct DeckConfig {
    /// Идентичность CrowdAnki.
    #[serde(default)]
    pub crowdanki_uuid: Option<String>,
    /// Имя конфигурации.
    #[serde(default)]
    pub name: Option<String>,
    /// Неизвестные свойства конфигурации.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Заметка (`notes[]`).
#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    /// Идентификатор заметки Anki.
    #[serde(default)]
    pub guid: Option<String>,
    /// Связь с моделью заметки.
    #[serde(default)]
    pub note_model_uuid: Option<String>,
    /// Значения полей в порядке, заданном моделью.
    #[serde(default, deserialize_with = "de_vec")]
    pub fields: Vec<FieldValue>,
    /// Теги заметки.
    #[serde(default, deserialize_with = "de_vec")]
    pub tags: Vec<String>,
    /// Неизвестные свойства заметки.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Узел колоды (`Deck`) — рекурсивная структура CrowdAnki-экспорта.
#[derive(Debug, Clone, Deserialize)]
pub struct DeckNode {
    /// Тег типа сериализованной сущности.
    #[serde(rename = "__type__", default)]
    pub type_name: Option<String>,
    /// Полное имя колоды в Anki-нотации (`Родитель::Дочерняя`).
    #[serde(default)]
    pub name: String,
    /// Идентичность CrowdAnki этого узла.
    #[serde(default)]
    pub crowdanki_uuid: Option<String>,
    /// Вложенные колоды.
    #[serde(default, deserialize_with = "de_vec")]
    pub children: Vec<DeckNode>,
    /// Заметки, объявленные непосредственно в этом узле.
    #[serde(default, deserialize_with = "de_vec")]
    pub notes: Vec<Note>,
    /// Объявленные имена media-файлов этого поддерева.
    #[serde(default, deserialize_with = "de_vec")]
    pub media_files: Vec<String>,
    /// Связь с конфигурацией колоды.
    #[serde(default)]
    pub deck_config_uuid: Option<String>,
    /// Объявленные в этом узле модели заметок.
    #[serde(default, deserialize_with = "de_vec")]
    pub note_models: Vec<NoteModel>,
    /// Объявленные в этом узле конфигурации колод.
    #[serde(default, deserialize_with = "de_vec")]
    pub deck_configurations: Vec<DeckConfig>,
    /// Неизвестные свойства узла.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl DeckNode {
    /// Имя колоды для вывода; пустое имя заменяется на placeholder.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            "(без имени)"
        } else {
            &self.name
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ord_accepts_integers_including_negative() {
        let ord: Ord = serde_json::from_str("0").expect("ord 0");
        assert_eq!(ord, Ord::Int(0));
        let ord: Ord = serde_json::from_str("-1").expect("ord -1");
        assert_eq!(ord, Ord::Int(-1));
        let ord: Ord = serde_json::from_str("12").expect("ord 12");
        assert_eq!(ord.value(), Some(12));
    }

    #[test]
    fn ord_is_invalid_for_non_integers_and_missing_values() {
        let ord: Ord = serde_json::from_str("null").expect("ord null");
        assert_eq!(ord, Ord::Invalid);
        let ord: Ord = serde_json::from_str("1.5").expect("ord 1.5");
        assert_eq!(ord, Ord::Invalid);
        let ord: Ord = serde_json::from_str("\"2\"").expect("ord строка");
        assert_eq!(ord, Ord::Invalid);
        assert_eq!(Ord::default(), Ord::Invalid);
    }

    #[test]
    fn malformed_ord_does_not_break_model_loading() {
        let json = r#"{
            "__type__": "NoteModel",
            "crowdanki_uuid": "m",
            "name": "Модель",
            "flds": [
                {"name": "A", "ord": "строка"},
                {"name": "B", "ord": -1}
            ],
            "tmpls": []
        }"#;
        let model: NoteModel = serde_json::from_str(json).expect("модель должна загрузиться");
        assert_eq!(model.flds.len(), 2);
        assert_eq!(model.flds[0].ord, Ord::Invalid);
        assert_eq!(model.flds[1].ord, Ord::Int(-1));
    }

    #[test]
    fn unknown_properties_survive_deserialization() {
        let json = crate::test_support::MINIMAL_EXPORT;
        let node: DeckNode = serde_json::from_str(json).expect("экспорт должен загрузиться");
        assert!(node.extra.contains_key("x_unknown_root_key"));
        assert!(
            node.note_models[0]
                .extra
                .contains_key("x_unknown_model_key")
        );
        assert!(node.notes[0].extra.contains_key("x_unknown_note_key"));
        assert_eq!(node.notes.len(), 2);
        assert_eq!(node.note_models[0].flds.len(), 2);
    }

    #[test]
    fn null_and_missing_collections_become_empty() {
        let json = r#"{
            "__type__": "Deck",
            "name": "Пустая",
            "children": null,
            "notes": null,
            "media_files": null,
            "note_models": null,
            "deck_configurations": null
        }"#;
        let node: DeckNode = serde_json::from_str(json).expect("экспорт должен загрузиться");
        assert!(node.children.is_empty());
        assert!(node.notes.is_empty());
        assert!(node.media_files.is_empty());
        assert!(node.note_models.is_empty());
        assert!(node.deck_configurations.is_empty());
    }

    #[test]
    fn field_value_keeps_non_string_values_without_failing() {
        let note: Note = serde_json::from_str(
            r#"{"guid": "g", "note_model_uuid": "m", "fields": ["текст", 42]}"#,
        )
        .expect("заметка должна загрузиться");
        assert_eq!(note.fields[0].as_text(), Some("текст"));
        assert_eq!(note.fields[1].as_text(), None);
        assert!(!note.fields[1].is_empty());
        assert_eq!(note.fields[1].rendered(), "42");
    }

    #[test]
    fn display_name_replaces_empty_name() {
        let node: DeckNode = serde_json::from_str(r#"{"__type__": "Deck"}"#).expect("узел");
        assert_eq!(node.display_name(), "(без имени)");
    }
}
