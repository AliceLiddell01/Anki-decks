//! Операция `stats`: структурная агрегированная статистика экспорта.

use std::collections::BTreeMap;
use std::path::Path;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::index::{ExportIndex, field_value_by_name, model_fields_in_ord_order};
use crate::media::collect_media;
use crate::ops::{CountEntry, MediaCounters};

/// Параметры запроса `stats`.
#[derive(Debug, Clone)]
pub struct StatsQuery {
    /// Необязательная группировка по сырым значениям поля.
    pub group_by: Option<String>,
    /// Предел размера distribution output.
    pub top: usize,
}

/// Результат `stats`.
#[derive(Debug, PartialEq)]
pub struct StatsResult {
    /// Каталог экспорта.
    pub export_dir: String,
    /// Общее число заметок.
    pub notes_total: usize,
    /// Число узлов дерева колод.
    pub deck_nodes: usize,
    /// Заметки по путям колод в preorder-порядке.
    pub notes_by_deck: Vec<CountEntry>,
    /// Заметки по моделям в порядке объявления моделей.
    pub notes_by_model: Vec<CountEntry>,
    /// Заметки с неразрешённой моделью.
    pub unresolved_model_notes: usize,
    /// Использование конфигураций колод: сколько узлов ссылается на конфигурацию.
    pub config_usage: Vec<CountEntry>,
    /// Статистика по полям в порядке, заданном моделями.
    pub fields: Vec<FieldStats>,
    /// Media-счётчики.
    pub media: MediaCounters,
    /// Распределение сырых значений поля.
    pub group_by: Option<GroupByResult>,
}

/// Статистика одного имени поля.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FieldStats {
    /// Имя поля.
    pub name: String,
    /// Сколько заметок имеют такое поле в своей модели.
    pub total: usize,
    /// Сколько значений непусты.
    pub nonempty: usize,
    /// Сколько значений пусты или отсутствуют.
    pub empty: usize,
}

/// Распределение сырых значений поля.
#[derive(Debug, PartialEq)]
pub struct GroupByResult {
    /// Имя поля.
    pub field: String,
    /// Сколько заметок имеют это поле в своей модели.
    pub notes_with_field: usize,
    /// Сколько заметок не имеют этого поля.
    pub notes_without_field: usize,
    /// Сколько различных сырых значений встретилось.
    pub distinct_values: usize,
    /// Значения с количествами после применения `top`.
    pub buckets: Vec<CountEntry>,
    /// Было ли распределение усечено.
    pub truncated: bool,
}

/// Выполняет `stats`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::UnknownField`] для неизвестного `--group-by`.
pub fn stats(
    export_dir: &Path,
    index: &ExportIndex<'_>,
    query: &StatsQuery,
) -> Result<StatsResult, DomainError> {
    let notes_by_deck = index
        .nodes
        .iter()
        .map(|entry| CountEntry {
            key: entry.path.to_string(),
            count: entry.node.notes.len(),
        })
        .collect();

    let mut unresolved_model_notes = 0usize;
    let mut model_counts: Vec<CountEntry> = index
        .models
        .iter()
        .map(|model| CountEntry {
            key: model
                .name
                .clone()
                .unwrap_or_else(|| "(без имени)".to_string()),
            count: 0,
        })
        .collect();

    for entry in &index.notes {
        let resolved = entry
            .note
            .note_model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid));
        if let Some(model) = resolved {
            let position = index.models.iter().position(|candidate| {
                candidate.crowdanki_uuid.as_deref() == model.crowdanki_uuid.as_deref()
            });
            if let Some(position) = position {
                model_counts[position].count += 1;
            }
        } else {
            unresolved_model_notes += 1;
        }
    }

    let config_usage = index
        .configs
        .iter()
        .map(|config| {
            let count = config.crowdanki_uuid.as_deref().map_or(0, |uuid| {
                index
                    .nodes
                    .iter()
                    .filter(|entry| entry.node.deck_config_uuid.as_deref() == Some(uuid))
                    .count()
            });
            CountEntry {
                key: config
                    .name
                    .clone()
                    .unwrap_or_else(|| "(без имени)".to_string()),
                count,
            }
        })
        .collect();

    let fields = collect_field_stats(index);
    let media = MediaCounters::from_report(&collect_media(export_dir, index));
    let group_by = match query.group_by.as_deref() {
        Some(field) => Some(collect_group_by(index, field, query.top)?),
        None => None,
    };

    Ok(StatsResult {
        export_dir: export_dir.display().to_string(),
        notes_total: index.notes.len(),
        deck_nodes: index.nodes.len(),
        notes_by_deck,
        notes_by_model: model_counts,
        unresolved_model_notes,
        config_usage,
        fields,
        media,
        group_by,
    })
}

fn collect_field_stats(index: &ExportIndex<'_>) -> Vec<FieldStats> {
    let mut order: Vec<String> = Vec::new();
    let mut stats: BTreeMap<String, FieldStats> = BTreeMap::new();

    for &model in &index.models {
        for field in model_fields_in_ord_order(model) {
            if !stats.contains_key(&field.name) {
                order.push(field.name.clone());
                stats.insert(
                    field.name.clone(),
                    FieldStats {
                        name: field.name.clone(),
                        ..FieldStats::default()
                    },
                );
            }
        }
    }

    for entry in &index.notes {
        let Some(model) = entry
            .note
            .note_model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid))
        else {
            continue;
        };
        for field in &model.flds {
            let Some(bucket) = stats.get_mut(&field.name) else {
                continue;
            };
            bucket.total += 1;
            let value = field
                .ord
                .value()
                .and_then(|ord| usize::try_from(ord).ok())
                .and_then(|position| entry.note.fields.get(position));
            if value.is_some_and(|value| !value.is_empty()) {
                bucket.nonempty += 1;
            } else {
                bucket.empty += 1;
            }
        }
    }

    order
        .into_iter()
        .filter_map(|name| stats.remove(&name))
        .collect()
}

fn collect_group_by(
    index: &ExportIndex<'_>,
    field: &str,
    top: usize,
) -> Result<GroupByResult, DomainError> {
    if !index.known_field_names().contains(field) {
        let available: Vec<String> = index
            .known_field_names()
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        return Err(DomainError::with_details(
            ErrorCode::UnknownField,
            format!("поле {field:?} отсутствует во всех note models экспорта"),
            details! {
                "field" => field,
                "available_fields" => available,
            },
        ));
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut notes_with_field = 0usize;

    for entry in &index.notes {
        let Some(model) = entry
            .note
            .note_model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid))
        else {
            continue;
        };
        if !model.flds.iter().any(|candidate| candidate.name == field) {
            continue;
        }
        notes_with_field += 1;
        let raw = field_value_by_name(entry.note, model, field)
            .map_or_else(String::new, crate::model::FieldValue::rendered);
        *counts.entry(raw).or_insert(0) += 1;
    }

    let distinct_values = counts.len();
    let mut buckets: Vec<CountEntry> = counts
        .into_iter()
        .map(|(key, count)| CountEntry { key, count })
        .collect();
    buckets.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.key.cmp(&right.key))
    });
    let truncated = buckets.len() > top;
    buckets.truncate(top);

    Ok(GroupByResult {
        field: field.to_string(),
        notes_with_field,
        notes_without_field: index.notes.len() - notes_with_field,
        distinct_values,
        buckets,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::index::ExportIndex;
    use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT, deck_node};

    fn query(group_by: Option<&str>, top: usize) -> StatsQuery {
        StatsQuery {
            group_by: group_by.map(ToString::to_string),
            top,
        }
    }

    #[test]
    fn structural_stats_count_notes_by_deck_model_and_config() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        let result = stats(Path::new("."), &index, &query(None, 20)).expect("stats");

        assert_eq!(result.notes_total, 3);
        assert_eq!(result.deck_nodes, 3);
        assert_eq!(result.unresolved_model_notes, 0);
        let decks: Vec<(String, usize)> = result
            .notes_by_deck
            .iter()
            .map(|entry| (entry.key.clone(), entry.count))
            .collect();
        assert_eq!(
            decks,
            vec![
                ("Root".to_string(), 1),
                ("Root::Child".to_string(), 1),
                ("Root::Child::Leaf".to_string(), 1),
            ]
        );
        assert_eq!(result.notes_by_model[0].count, 3);
        assert_eq!(result.config_usage[0].count, 3);
        assert!(result.group_by.is_none());
    }

    #[test]
    fn field_stats_split_empty_and_nonempty() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let result = stats(Path::new("."), &index, &query(None, 20)).expect("stats");

        let names: Vec<&str> = result
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, vec!["Слово", "Значение"]);
        let word = &result.fields[0];
        assert_eq!((word.total, word.nonempty, word.empty), (2, 2, 0));
        let meaning = &result.fields[1];
        assert_eq!((meaning.total, meaning.nonempty, meaning.empty), (2, 1, 1));
    }

    #[test]
    fn group_by_uses_raw_values_without_normalization() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let result = stats(Path::new("."), &index, &query(Some("Значение"), 20))
            .expect("stats")
            .group_by
            .expect("распределение должно быть посчитано");

        assert_eq!(result.field, "Значение");
        assert_eq!(result.notes_with_field, 2);
        assert_eq!(result.notes_without_field, 0);
        assert_eq!(result.distinct_values, 2);
        assert!(!result.truncated);
        assert_eq!(result.buckets[0].count, 1);
        assert_eq!(result.buckets[0].key, "");
        assert_eq!(result.buckets[1].key, "случайность");
    }

    #[test]
    fn distribution_is_ordered_by_count_then_value() {
        let mut value = serde_json::from_str::<serde_json::Value>(MINIMAL_EXPORT).expect("json");
        value["notes"][0]["fields"][1] = serde_json::json!("б");
        value["notes"][1]["fields"][1] = serde_json::json!("а");
        let node = deck_node(&serde_json::to_string(&value).expect("json"));
        let index = ExportIndex::build(&node);

        let buckets = stats(Path::new("."), &index, &query(Some("Значение"), 20))
            .expect("stats")
            .group_by
            .expect("распределение")
            .buckets;
        let pairs: Vec<(String, usize)> = buckets
            .into_iter()
            .map(|bucket| (bucket.key, bucket.count))
            .collect();
        assert_eq!(pairs, vec![("а".to_string(), 1), ("б".to_string(), 1)]);
    }

    #[test]
    fn top_truncates_distribution() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let group = stats(Path::new("."), &index, &query(Some("Значение"), 1))
            .expect("stats")
            .group_by
            .expect("распределение");
        assert!(group.truncated);
        assert_eq!(group.buckets.len(), 1);
        assert_eq!(group.distinct_values, 2);
    }

    #[test]
    fn unknown_group_by_field_is_rejected() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let error = stats(Path::new("."), &index, &query(Some("НетТакого"), 20))
            .expect_err("неизвестное поле");
        assert_eq!(error.code, ErrorCode::UnknownField);
    }
}
