//! Правила дубликатов: одинаковое содержимое заметок и одинаковое головное поле.
//!
//! Оба правила групповые: finding адресуется группе, а не каждой её заметке, и
//! несёт `related_*`-поля, чтобы ревьюер видел всех участников сразу и не
//! собирал тот же факт из нескольких записей.
//!
//! Группировка идёт по разрешённой модели (`note_model_uuid`) и по сырым
//! значениям полей. Никакого «похоже» здесь нет: сравнение точное, поэтому
//! правило детерминировано и не требует эвристик.

use std::collections::BTreeMap;

use serde_json::json;

use crate::qa::{NoteView, RawFinding, RuleContext};
use crate::selection::PRIMARY_FIELD;
use crate::text::bounded_sample;

/// Предел числа участников группы в evidence.
pub const MAX_RELATED_NOTES: usize = 20;

/// Заметки двух разных участников группы.
type Group<'a> = Vec<&'a NoteView<'a>>;

/// Несколько заметок одной модели имеют полностью одинаковые значения `fields`.
///
/// Одинаковым считается набор значений всех полей заметки. Порядок полей —
/// порядок `ord` модели, поэтому две заметки с одним содержимым, но разным
/// расположением значений, дубликатами не считаются.
pub fn duplicate_note_content(context: &RuleContext<'_>) -> Vec<RawFinding> {
    let mut groups: BTreeMap<(String, String), Group<'_>> = BTreeMap::new();
    for view in context.addressable() {
        let Some(uuid) = view.note.note_model_uuid.as_deref() else {
            continue;
        };
        groups
            .entry((uuid.to_string(), content_fingerprint(view)))
            .or_default()
            .push(view);
    }

    let mut findings = Vec::new();
    for members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        let first = members[0];
        let related = Related::new(&members);
        findings.push(RawFinding {
            note_position: first.position,
            field: None,
            field_ord: None,
            message: format!(
                "{} заметок модели {} имеют полностью одинаковые значения fields",
                members.len(),
                model_label(first)
            ),
            evidence: json!({
                "group_size": members.len(),
                "fields_total": first.note.fields.len(),
                "related_guids": related.guids,
                "related_note_indices": related.positions,
                "related_truncated": related.truncated,
            }),
        });
    }

    findings.sort_by_key(|finding| finding.note_position);
    findings
}

/// Применимо ли правило головного поля.
///
/// Модель без поля [`PRIMARY_FIELD`] — не ошибка: правило к такому экспорту
/// просто не применяется, и это документированный нейтральный результат.
#[must_use]
pub fn primary_field_applicable(context: &RuleContext<'_>) -> bool {
    context
        .views
        .iter()
        .any(|view| view.text(PRIMARY_FIELD).is_some())
}

/// Несколько заметок имеют одинаковое сырое значение головного поля.
///
/// Правило не утверждает, что это ошибка: одинаковое головное поле может быть
/// намеренным. Пустое значение сюда не попадает — его владелец
/// `empty_field_value`.
pub fn duplicate_primary_field(context: &RuleContext<'_>) -> Vec<RawFinding> {
    let mut groups: BTreeMap<String, Group<'_>> = BTreeMap::new();
    for view in context.addressable() {
        let Some(text) = view.text(PRIMARY_FIELD) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        groups.entry(text.to_string()).or_default().push(view);
    }

    let mut findings = Vec::new();
    for (raw, members) in groups {
        if members.len() < 2 {
            continue;
        }
        let first = members[0];
        let related = Related::new(&members);
        findings.push(RawFinding {
            note_position: first.position,
            field: Some(PRIMARY_FIELD.to_string()),
            field_ord: first
                .field(PRIMARY_FIELD)
                .and_then(|field| field.ord.value()),
            message: format!(
                "{} заметок имеют одинаковое сырое значение поля {PRIMARY_FIELD}",
                members.len()
            ),
            evidence: json!({
                "raw_value": bounded_sample(&raw),
                "raw_value_chars": raw.chars().count(),
                "group_size": members.len(),
                "related_guids": related.guids,
                "related_note_indices": related.positions,
                "related_truncated": related.truncated,
            }),
        });
    }

    findings.sort_by_key(|finding| finding.note_position);
    findings
}

/// Ограниченный список участников группы.
struct Related {
    guids: Vec<String>,
    positions: Vec<usize>,
    truncated: bool,
}

impl Related {
    fn new(members: &[&NoteView<'_>]) -> Self {
        let guids = members
            .iter()
            .filter_map(|view| view.note.guid.clone())
            .filter(|guid| !guid.is_empty())
            .take(MAX_RELATED_NOTES)
            .collect();
        let positions = members
            .iter()
            .map(|view| view.position)
            .take(MAX_RELATED_NOTES)
            .collect();
        Self {
            guids,
            positions,
            truncated: members.len() > MAX_RELATED_NOTES,
        }
    }
}

/// Отпечаток содержимого заметки: значения полей в порядке `fields`.
///
/// Каждое значение предваряется собственной длиной, поэтому склейка разных
/// наборов значений не может дать одинаковый отпечаток.
fn content_fingerprint(view: &NoteView<'_>) -> String {
    let mut fingerprint = String::new();
    for field in &view.note.fields {
        let rendered = field.rendered();
        fingerprint.push_str(&rendered.len().to_string());
        fingerprint.push(':');
        fingerprint.push_str(&rendered);
        fingerprint.push('\u{1}');
    }
    fingerprint
}

/// Имя модели заметки для human/message-текста.
fn model_label(view: &NoteView<'_>) -> String {
    view.model
        .and_then(|model| model.name.as_deref())
        .unwrap_or("(без имени)")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ExportIndex;
    use crate::qa::{Finding, RULES, RuleContext, collect};
    use crate::test_support::{MINIMAL_EXPORT, deck_node, export_with};

    fn findings_for(json: &str, code: &str) -> Vec<Finding> {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);
        collect(&context)
            .into_iter()
            .filter(|finding| finding.code == code)
            .collect()
    }

    /// Экспорт с тремя заметками: вторая повторяет первую, третья уникальна.
    fn duplicated_export() -> String {
        export_with(MINIMAL_EXPORT, |value| {
            let first = value["notes"][0].clone();
            let mut copy = first.clone();
            copy["guid"] = json!("guid-3");
            value["notes"].as_array_mut().expect("notes").push(copy);
            let mut other = first;
            other["guid"] = json!("guid-4");
            other["fields"] = json!(["偶然", "случайность"]);
            value["notes"].as_array_mut().expect("notes").push(other);
        })
    }

    #[test]
    fn identical_fields_are_grouped_once() {
        let found = findings_for(&duplicated_export(), "duplicate_note_content");
        assert_eq!(found.len(), 1, "одна группа, а не три finding'а");
        assert_eq!(found[0].note_position, 0);
        assert_eq!(found[0].field, None);
        assert_eq!(found[0].evidence["group_size"], 2);
        assert_eq!(
            found[0].evidence["related_guids"],
            json!(["guid-1", "guid-3"])
        );
        assert_eq!(found[0].evidence["related_note_indices"], json!([0, 2]));
        assert_eq!(found[0].evidence["related_truncated"], false);
    }

    #[test]
    fn same_values_in_different_models_are_not_duplicates() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut model = value["note_models"][0].clone();
            model["crowdanki_uuid"] = json!("model-2");
            model["name"] = json!("Другая модель");
            value["note_models"]
                .as_array_mut()
                .expect("models")
                .push(model);

            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-other-model");
            copy["note_model_uuid"] = json!("model-2");
            value["notes"].as_array_mut().expect("notes").push(copy);
        });

        assert!(
            findings_for(&json, "duplicate_note_content").is_empty(),
            "разные модели — разные наборы совместимости"
        );
    }

    #[test]
    fn different_field_order_is_not_a_duplicate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-3");
            copy["fields"] = json!(["случайность", "[sound:a.mp3]偶然"]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        });
        assert!(findings_for(&json, "duplicate_note_content").is_empty());
    }

    #[test]
    fn notes_without_guid_do_not_participate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!(null);
            value["notes"].as_array_mut().expect("notes").push(copy);
        });
        assert!(
            findings_for(&json, "duplicate_note_content").is_empty(),
            "группу нельзя адресовать без guid"
        );
    }

    #[test]
    fn unresolved_models_do_not_participate() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            for note in value["notes"].as_array_mut().expect("notes") {
                note["note_model_uuid"] = json!("нет-такой-модели");
            }
        });
        assert!(findings_for(&json, "duplicate_note_content").is_empty());
    }

    #[test]
    fn duplicate_primary_field_groups_by_raw_value() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-3");
            copy["fields"] = json!(["[sound:a.mp3]偶然", "другое значение"]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        });

        let found = findings_for(&json, "duplicate_primary_field");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].note_position, 0);
        assert_eq!(found[0].field.as_deref(), Some(PRIMARY_FIELD));
        assert_eq!(found[0].field_ord, Some(0));
        assert_eq!(found[0].evidence["group_size"], 2);
        assert_eq!(found[0].evidence["raw_value"], "[sound:a.mp3]偶然");
        assert_eq!(
            found[0].evidence["related_guids"],
            json!(["guid-1", "guid-3"])
        );
    }

    #[test]
    fn empty_primary_values_belong_to_the_empty_rule() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-3");
            copy["fields"] = json!(["", "значение"]);
            value["notes"].as_array_mut().expect("notes").push(copy);
            value["notes"][0]["fields"][0] = json!("");
        });
        assert!(
            findings_for(&json, "duplicate_primary_field").is_empty(),
            "пустое головное поле не считается дубликатом"
        );
    }

    #[test]
    fn rule_is_not_applicable_without_primary_field() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["note_models"][0]["flds"] = json!([
                {"name": "Термин", "ord": 0},
                {"name": "Значение", "ord": 1}
            ]);
            let mut copy = value["notes"][0].clone();
            copy["guid"] = json!("guid-3");
            value["notes"].as_array_mut().expect("notes").push(copy);
        });

        let node = deck_node(&json);
        let index = ExportIndex::build(&node);
        let context = RuleContext::build(&index);
        let rule = RULES
            .iter()
            .find(|rule| rule.code == "duplicate_primary_field")
            .expect("правило");
        assert!(
            !(rule.applicable)(&context),
            "модель без головного поля делает правило неприменимым"
        );
        assert!(rule.findings(&context).is_empty());
    }

    #[test]
    fn related_lists_are_bounded() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let template = value["notes"][0].clone();
            for index in 0..(MAX_RELATED_NOTES + 5) {
                let mut copy = template.clone();
                copy["guid"] = json!(format!("guid-bulk-{index}"));
                value["notes"].as_array_mut().expect("notes").push(copy);
            }
        });

        let found = findings_for(&json, "duplicate_note_content");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].evidence["group_size"],
            MAX_RELATED_NOTES + 6,
            "размер группы считается целиком"
        );
        assert_eq!(
            found[0].evidence["related_guids"]
                .as_array()
                .expect("guids")
                .len(),
            MAX_RELATED_NOTES
        );
        assert_eq!(found[0].evidence["related_truncated"], true);
    }
}
