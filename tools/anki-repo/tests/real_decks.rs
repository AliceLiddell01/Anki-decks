//! Проверки на канонических колодах репозитория `decks/japanese/words/Words__N1..N5`.
//!
//! Ожидаемые значения не зашиты в код: counts и media пересчитываются напрямую
//! из сырого JSON и из содержимого каталогов, а результаты tool сверяются с ними.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use common::{
    collect_notes, raw_field_ord, raw_field_position, raw_json, raw_models, repo_root, words_deck,
};
use serde_json::Value;

use anki_repo::index::ExportIndex;
use anki_repo::loader::load_export;
use anki_repo::media::extract_media_references;
use anki_repo::ops::find::{FindCriteria, FindQuery, MatchMode};
use anki_repo::ops::inspect::inspect;
use anki_repo::ops::stats::{StatsQuery, stats};
use anki_repo::ops::validate::{Severity, validate};

const LEVELS: [u8; 5] = [1, 2, 3, 4, 5];

const EXPECTED_FIELDS: [&str; 6] = [
    "Слово",
    "Часть речи",
    "Значение",
    "Ударение",
    "Пример",
    "Похожие слова",
];

fn count_nodes(value: &Value) -> usize {
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .map_or(0, |children| children.iter().map(count_nodes).sum());
    1 + children
}

fn declared_media(value: &Value, out: &mut Vec<String>) {
    if let Some(media) = value.get("media_files").and_then(Value::as_array) {
        out.extend(
            media
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string),
        );
    }
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            declared_media(child, out);
        }
    }
}

fn physical_media(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir.join("media"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                // Фильтр совпадает с collect_media: каталоги внутри media/ не
                // считаются физическими media-файлами.
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn basename(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
}

/// Состав моделей из сырого JSON: `crowdanki_uuid` → (имя поля → `ord`).
/// Все уровни N1–N5 должны существовать и содержать `deck.json`.
#[test]
fn canonical_decks_exist() {
    for level in LEVELS {
        let dir = words_deck(level);
        assert!(
            dir.join("deck.json").is_file(),
            "нет {}",
            dir.join("deck.json").display()
        );
    }
}

#[test]
fn inspect_matches_independently_recomputed_counts() {
    for level in LEVELS {
        let value = raw_json(level);
        let mut notes = Vec::new();
        collect_notes(&value, &mut notes);

        let export_dir = words_deck(level);
        let loaded = load_export(&export_dir).unwrap_or_else(|error| {
            panic!("N{level}: deck.json не загрузился: {error}");
        });
        let index = ExportIndex::build(&loaded.root);
        let result = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, false);

        assert_eq!(
            result.notes_total,
            notes.len(),
            "N{level}: число заметок расходится с сырым JSON"
        );
        assert_eq!(
            result.deck_nodes,
            count_nodes(&value),
            "N{level}: число узлов"
        );
        assert_eq!(index.notes.len(), notes.len(), "N{level}: index notes");
        assert_eq!(result.models.len(), 1, "N{level}: ожидается одна модель");
        assert_eq!(
            result.configs.len(),
            1,
            "N{level}: ожидается одна конфигурация"
        );
        assert_eq!(result.notes_by_deck.len(), 1, "N{level}: одна колода");

        let fields: Vec<&str> = result.models[0]
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(fields, EXPECTED_FIELDS.to_vec(), "N{level}: порядок полей");
        assert_eq!(result.models[0].name, "Слова", "N{level}: имя модели");
        assert_eq!(
            result.models[0].used_notes, result.notes_total,
            "N{level}: модель должна использоваться всеми заметками"
        );
    }
}

#[test]
fn every_canonical_note_resolves_through_ord() {
    for level in LEVELS {
        let export_dir = words_deck(level);
        let loaded = load_export(&export_dir).expect("загрузка экспорта");
        let index = ExportIndex::build(&loaded.root);

        let models: BTreeSet<String> = loaded
            .root
            .note_models
            .iter()
            .filter_map(|model| model.crowdanki_uuid.clone())
            .collect();
        assert_eq!(models.len(), 1, "N{level}: одна модель у корня");

        for entry in &index.notes {
            let model_uuid = entry
                .note
                .note_model_uuid
                .as_deref()
                .unwrap_or_else(|| panic!("N{level}: у заметки нет note_model_uuid"));
            let model = index
                .model_by_uuid(model_uuid)
                .unwrap_or_else(|| panic!("N{level}: модель {model_uuid} не разрешается"));
            assert_eq!(
                model.flds.len(),
                entry.note.fields.len(),
                "N{level}: число fields расходится с моделью"
            );
        }
    }
}

#[test]
fn guids_are_unique_and_reported_accurately() {
    for level in LEVELS {
        let value = raw_json(level);
        let mut notes = Vec::new();
        collect_notes(&value, &mut notes);

        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for note in &notes {
            let guid = note
                .get("guid")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("N{level}: у заметки нет guid"))
                .to_string();
            *seen.entry(guid).or_insert(0) += 1;
        }
        let duplicates = seen.values().filter(|count| **count > 1).count();
        assert_eq!(duplicates, 0, "N{level}: найден повтор guid");

        let export_dir = words_deck(level);
        let loaded = load_export(&export_dir).expect("загрузка");
        let index = ExportIndex::build(&loaded.root);
        for guid in seen.keys() {
            assert_eq!(
                index.note_positions_by_guid(guid).len(),
                1,
                "N{level}: guid {guid} должен встречаться один раз"
            );
        }
    }
}

#[test]
fn stats_agree_with_inspect() {
    for level in LEVELS {
        let export_dir = words_deck(level);
        let loaded = load_export(&export_dir).expect("загрузка");
        let index = ExportIndex::build(&loaded.root);
        let inspected = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, false);
        let computed = stats(
            &loaded.export_dir,
            &index,
            &StatsQuery {
                group_by: None,
                top: 20,
            },
        )
        .expect("stats должны считаться");

        assert_eq!(computed.notes_total, inspected.notes_total, "N{level}");
        assert_eq!(computed.deck_nodes, inspected.deck_nodes, "N{level}");
        assert_eq!(
            computed.notes_by_deck[0].count, inspected.notes_total,
            "N{level}"
        );
        assert_eq!(computed.notes_by_model[0].count, inspected.notes_total);
        assert_eq!(computed.unresolved_model_notes, 0, "N{level}");

        let names: Vec<&str> = computed
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(
            names,
            EXPECTED_FIELDS.to_vec(),
            "N{level}: порядок полей stats"
        );

        let word = &computed.fields[0];
        assert_eq!(word.total, inspected.notes_total, "N{level}");
        assert_eq!(
            word.empty, 0,
            "N{level}: поле «Слово» не должно быть пустым"
        );
    }
}

#[test]
fn group_by_matches_independently_recomputed_distribution() {
    for level in LEVELS {
        let value = raw_json(level);
        let models = raw_models(&value);
        let mut notes = Vec::new();
        collect_notes(&value, &mut notes);

        let mut counted: BTreeMap<String, usize> = BTreeMap::new();
        for note in &notes {
            let raw = note
                .get("fields")
                .and_then(Value::as_array)
                .and_then(|fields| fields.get(raw_field_position(&models, note, "Часть речи")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            *counted.entry(raw).or_insert(0) += 1;
        }

        let export_dir = words_deck(level);
        let loaded = load_export(&export_dir).expect("загрузка");
        let index = ExportIndex::build(&loaded.root);
        let group = stats(
            &loaded.export_dir,
            &index,
            &StatsQuery {
                group_by: Some("Часть речи".to_string()),
                top: 500,
            },
        )
        .expect("stats")
        .group_by
        .expect("распределение");

        assert_eq!(
            group.distinct_values,
            counted.len(),
            "N{level}: число различных значений"
        );
        let expected_total: usize = counted.values().sum();
        assert_eq!(group.notes_with_field, expected_total, "N{level}");

        for bucket in &group.buckets {
            assert_eq!(
                counted.get(&bucket.key),
                Some(&bucket.count),
                "N{level}: значение {:?}",
                bucket.key
            );
        }
    }
}

#[test]
fn canonical_decks_have_no_errors_and_only_expected_media_warnings() {
    for level in LEVELS {
        let export_dir = words_deck(level);

        let mut declared = Vec::new();
        declared_media(&raw_json(level), &mut declared);
        let declared_set: BTreeSet<String> = declared.iter().map(|name| basename(name)).collect();
        let physical = physical_media(&export_dir);
        let expected_missing = declared_set.difference(&physical).count();
        let expected_undeclared = physical.difference(&declared_set).count();

        let result = validate(&export_dir).expect("validate");
        assert!(result.valid, "N{level}: экспорт должен быть валидным");
        assert_eq!(
            result.summary.errors, 0,
            "N{level}: issues {:?}",
            result.issues
        );

        for issue in &result.issues {
            if issue.severity == Severity::Error {
                panic!("N{level}: неожиданный ERROR {:?}", issue.message);
            }
            if issue.severity == Severity::Warning {
                assert_eq!(
                    issue.code, "media_physical_missing",
                    "N{level}: неожиданный WARNING {}",
                    issue.code
                );
            }
        }

        let missing = result
            .issues
            .iter()
            .filter(|issue| issue.code == "media_physical_missing")
            .count();
        assert!(
            !result
                .issues
                .iter()
                .any(|issue| issue.code == "issues_truncated"),
            "N{level}: issues усечены, сравнение количества некорректно"
        );
        assert_eq!(
            missing, expected_missing,
            "N{level}: число отсутствующих физически media"
        );

        let unused = result
            .issues
            .iter()
            .filter(|issue| issue.code == "media_physical_unused")
            .count();
        assert_eq!(unused, expected_undeclared, "N{level}: необъявленные media");
    }
}

#[test]
fn every_media_reference_in_notes_is_declared() {
    for level in LEVELS {
        let value = raw_json(level);
        let mut declared = Vec::new();
        declared_media(&value, &mut declared);
        let declared_set: BTreeSet<String> = declared.iter().map(|name| basename(name)).collect();

        let mut notes = Vec::new();
        collect_notes(&value, &mut notes);

        let mut references: BTreeSet<String> = BTreeSet::new();
        for note in &notes {
            if let Some(fields) = note.get("fields").and_then(Value::as_array) {
                for field in fields {
                    if let Some(text) = field.as_str() {
                        references.extend(extract_media_references(text));
                    }
                }
            }
        }

        assert!(
            !references.is_empty(),
            "N{level}: ожидались ссылки на media в полях заметок"
        );
        let undeclared: Vec<&String> = references.difference(&declared_set).collect();
        assert!(
            undeclared.is_empty(),
            "N{level}: ссылки на необъявленный media: {undeclared:?}"
        );
    }
}

#[test]
fn find_by_guid_returns_all_six_named_fields_in_order() {
    for level in LEVELS {
        let value = raw_json(level);
        let mut notes = Vec::new();
        collect_notes(&value, &mut notes);
        let first = notes.first().expect("в колоде есть заметки");
        let guid = first.get("guid").and_then(Value::as_str).expect("guid");

        let export_dir = words_deck(level);
        let loaded = load_export(&export_dir).expect("загрузка");
        let index = ExportIndex::build(&loaded.root);
        let result = anki_repo::ops::find::find(
            &loaded.export_dir,
            &index,
            &FindQuery {
                criteria: FindCriteria::Guid {
                    guid: guid.to_string(),
                },
                deck: None,
                limit: 20,
            },
        )
        .unwrap_or_else(|error| panic!("N{level}: guid {guid} не найден: {error}"));

        assert_eq!(result.matched_total, 1, "N{level}");
        assert_eq!(result.returned, 1, "N{level}");
        let note = &result.notes[0];
        assert_eq!(note.guid.as_deref(), Some(guid), "N{level}");

        let names: Vec<&str> = note
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, EXPECTED_FIELDS.to_vec(), "N{level}: порядок полей");
        let values: Vec<Option<&str>> = note
            .fields
            .iter()
            .map(|field| field.value.as_deref())
            .collect();
        let raw_fields = first
            .get("fields")
            .and_then(Value::as_array)
            .expect("fields");
        for (position, raw) in raw_fields.iter().enumerate() {
            assert_eq!(
                values[position],
                raw.as_str(),
                "N{level}: поле {} расходится с сырым JSON",
                EXPECTED_FIELDS[position]
            );
        }
    }
}

#[test]
fn find_word_search_reports_bounded_results() {
    let export_dir = words_deck(3);
    let loaded = load_export(&export_dir).expect("загрузка");
    let index = ExportIndex::build(&loaded.root);

    let result = anki_repo::ops::find::find(
        &loaded.export_dir,
        &index,
        &FindQuery {
            criteria: FindCriteria::Field {
                field: "Часть речи".to_string(),
                value: "Существительное".to_string(),
                mode: MatchMode::Contains,
            },
            deck: Some("Words::N3".to_string()),
            limit: 5,
        },
    )
    .expect("поиск должен что-то найти");

    assert!(result.matched_total > result.returned);
    assert_eq!(result.returned, 5);
    assert!(result.truncated);
    assert_eq!(result.notes.len(), 5);
}

#[test]
fn output_is_deterministic_across_runs() {
    let export_dir = words_deck(3);
    let loaded = load_export(&export_dir).expect("загрузка");
    let index = ExportIndex::build(&loaded.root);

    let first = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, true);
    let second = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, true);
    let first_json = anki_repo::render::json::inspect_json(&first);
    let second_json = anki_repo::render::json::inspect_json(&second);
    assert_eq!(first_json, second_json, "JSON должен быть стабильным");

    let first_stats = stats(
        &loaded.export_dir,
        &index,
        &StatsQuery {
            group_by: Some("Значение".to_string()),
            top: 50,
        },
    )
    .expect("stats");
    let second_stats = stats(
        &loaded.export_dir,
        &index,
        &StatsQuery {
            group_by: Some("Значение".to_string()),
            top: 50,
        },
    )
    .expect("stats");
    assert_eq!(first_stats.group_by, second_stats.group_by);
}

#[test]
fn json_render_is_valid_and_keeps_ordered_fields() {
    let export_dir = words_deck(1);
    let loaded = load_export(&export_dir).expect("загрузка");
    let index = ExportIndex::build(&loaded.root);

    let value = raw_json(1);
    let mut notes = Vec::new();
    collect_notes(&value, &mut notes);
    let guid = notes[0].get("guid").and_then(Value::as_str).expect("guid");

    let result = anki_repo::ops::find::find(
        &loaded.export_dir,
        &index,
        &FindQuery {
            criteria: FindCriteria::Guid {
                guid: guid.to_string(),
            },
            deck: None,
            limit: 20,
        },
    )
    .expect("find");

    let text = anki_repo::render::json::find_json(&result);
    let parsed: Value = serde_json::from_str(&text).expect("вывод должен быть JSON");
    assert_eq!(parsed["schema_version"], Value::from(1));
    assert_eq!(parsed["command"], Value::from("find"));

    // Порядок ключей проверяется по фактическому тексту: при повторном разборе
    // в serde_json::Value порядок объекта нормализуется. Ожидаемый порядок
    // берётся из ord модели, а не из порядка массива `fields`.
    let models = raw_models(&value);
    let mut by_ord: Vec<(i64, &str)> = EXPECTED_FIELDS
        .iter()
        .map(|name| (raw_field_ord(&models, notes[0], name), *name))
        .collect();
    by_ord.sort_by_key(|(ord, _)| *ord);

    let positions: Vec<usize> = by_ord
        .iter()
        .map(|(_, name)| {
            text.find(&format!("\"{name}\":"))
                .unwrap_or_else(|| panic!("в выводе нет ключа {name}"))
        })
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "ключи fields должны идти в порядке ord модели"
    );

    let fields = parsed["result"]["notes"][0]["fields"]
        .as_object()
        .expect("fields — объект");
    assert_eq!(fields.len(), by_ord.len());

    let raw_fields = notes[0]
        .get("fields")
        .and_then(Value::as_array)
        .expect("fields");
    for (ord, name) in &by_ord {
        let position = usize::try_from(*ord).expect("ord должен быть неотрицательным");
        assert_eq!(
            fields[*name], raw_fields[position],
            "поле {name} должно совпадать с сырым JSON"
        );
    }
}

#[test]
fn repo_root_and_deck_paths_are_resolved() {
    let root = repo_root();
    assert!(
        root.join("AGENTS.md").is_file(),
        "корень репозитория найден неверно"
    );
    assert!(words_deck(1).join("media").is_dir());
}
