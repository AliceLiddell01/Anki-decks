//! Свойства, обязанные выполняться для любого корректного CrowdAnki-экспорта.
//!
//! Ожидаемые значения не зашиты в код и не берутся из `decks/`: counts, порядок
//! полей и media пересчитываются независимо — из сырого JSON самого экспорта и
//! из содержимого его каталога. Проверки строят экспорт сами, поэтому они
//! остаются корректными, когда состав пользовательских колод изменится или
//! колоды временно отсутствуют.
//!
//! Форма экспорта здесь намеренно не совпадает с реальными колодами: два
//! дочерних узла, две note models, разное число полей и `flds`, объявленные не в
//! порядке `ord`. Поведение, которое случайно опиралось бы на форму конкретного
//! экспорта, обязано разойтись именно на таких данных.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use common::{
    TempDir, canonical_export, collect_notes, mixed_export, parse_json, raw_field_position,
    raw_models, run_cli, run_cli_in,
};
use serde_json::{Value, json};

use anki_repo::index::ExportIndex;
use anki_repo::loader::load_export;
use anki_repo::media::extract_media_references;
use anki_repo::ops::find::{FindCriteria, FindQuery, MatchMode, find};
use anki_repo::ops::inspect::inspect;
use anki_repo::ops::stats::{StatsQuery, stats};
use anki_repo::ops::validate::{Severity, validate};

/// Экспорт с media: ссылки в заметках, объявленные и физические файлы.
///
/// `media_files` объявляет два файла, а в каталоге `media/` лежит один из них —
/// так проверки media видят и отсутствующий физически, и неиспользуемый файл,
/// не полагаясь на конкретное содержимое репозитория.
fn media_export() -> Value {
    let mut value = mixed_export();
    value["children"][0]["media_files"] = json!(["a.mp3", "нет-такого.mp3"]);
    value["children"][0]["notes"][0]["fields"] = json!([
        "значение гамма",
        "[sound:a.mp3] значение альфа",
        "значение бета"
    ]);
    value["children"][0]["notes"][1]["fields"] = json!(["[sound:a.mp3] одно поле"]);
    value
}

/// Готовит временный каталог: канонический `deck.json` плюс физический `media/`.
fn prepared_export(label: &str, export: &Value, media: &[&str]) -> TempDir {
    let dir = canonical_export(label, export);
    if !media.is_empty() {
        dir.write_media(media);
    }
    dir
}

/// Загружает экспорт; `ExportIndex` строится вызывающим тестом, потому что
/// индекс ссылается на корневой узел и не может быть возвращён вместе с ним.
fn load(dir: &Path) -> anki_repo::loader::LoadedExport {
    load_export(dir)
        .unwrap_or_else(|error| panic!("{}: deck.json не загрузился: {error}", dir.display()))
}

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

/// Физические файлы каталога `media/`.
///
/// Каталоги внутри `media/` не считаются media-файлами — как и в самом
/// toolkit'е.
fn physical_media(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir.join("media"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Физические файлы каталога `media/` фикстуры.
///
/// `лишний.mp3` лежит физически, но не объявлен в `media_files`: так проверка
/// видит оба media-warning'а, не полагаясь на содержимое репозитория.
const MEDIA: &[&str] = &["a.mp3", "лишний.mp3"];

fn basename(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
}

/// Ожидаемые поля модели в порядке `ord`: (ord, имя).
///
/// Порядок берётся из модели, а не из порядка массива `flds`: эти порядки в
/// фикстуре намеренно разные.
fn fields_by_ord(value: &Value, note: &Value, uuid: &str) -> Vec<(i64, String)> {
    let models = raw_models(value);
    let model = models
        .get(uuid)
        .unwrap_or_else(|| panic!("модель должна быть в модели экспорта"));
    let mut by_ord: Vec<(i64, String)> = model
        .iter()
        .map(|(name, ord)| (*ord, name.clone()))
        .collect();
    by_ord.sort_by_key(|(ord, _)| *ord);

    // Число полей заметки должно совпадать с моделью: иначе «порядок полей»
    // неопределён и сверять его было бы не с чем.
    let fields = note
        .get("fields")
        .and_then(Value::as_array)
        .expect("у заметки есть fields");
    assert_eq!(
        fields.len(),
        by_ord.len(),
        "число fields расходится с моделью"
    );
    by_ord
}

#[test]
fn inspect_matches_independently_recomputed_counts() {
    for (label, export) in [("base", mixed_export()), ("media", media_export())] {
        let dir = prepared_export(&format!("export-inspect-{label}"), &export, MEDIA);
        let mut notes = Vec::new();
        collect_notes(&export, &mut notes);

        let loaded = load(dir.path());
        let index = ExportIndex::build(&loaded.root);
        let result = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, false);

        assert_eq!(result.notes_total, notes.len(), "{label}: число заметок");
        assert_eq!(result.deck_nodes, count_nodes(&export), "{label}: узлы");
        assert_eq!(index.notes.len(), notes.len(), "{label}: index notes");
        assert_eq!(
            result
                .notes_by_deck
                .iter()
                .map(|deck| deck.count)
                .sum::<usize>(),
            result.notes_total,
            "{label}: сумма заметок по узлам"
        );

        // Состав моделей и конфигураций берётся из самого экспорта.
        let expected_models: Vec<&str> = export["note_models"]
            .as_array()
            .expect("note_models")
            .iter()
            .filter_map(|model| model["name"].as_str())
            .collect();
        let actual_models: Vec<&str> = result
            .models
            .iter()
            .map(|model| model.name.as_str())
            .collect();
        assert_eq!(actual_models, expected_models, "{label}: состав моделей");

        let expected_configs = export["deck_configurations"]
            .as_array()
            .expect("deck_configurations")
            .len();
        assert_eq!(
            result.configs.len(),
            expected_configs,
            "{label}: конфигурации"
        );

        for model in &result.models {
            let fields: Vec<&str> = model
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect();
            let declared = export["note_models"]
                .as_array()
                .expect("note_models")
                .iter()
                .find(|candidate| candidate["name"] == model.name)
                .and_then(|candidate| candidate["flds"].as_array())
                .expect("flds");
            let mut by_ord: Vec<(i64, &str)> = declared
                .iter()
                .map(|field| {
                    (
                        field["ord"].as_i64().expect("ord"),
                        field["name"].as_str().expect("имя поля"),
                    )
                })
                .collect();
            by_ord.sort_by_key(|(ord, _)| *ord);
            let expected: Vec<&str> = by_ord.iter().map(|(_, name)| *name).collect();
            assert_eq!(fields, expected, "{label}: порядок полей модели");
        }

        let used: usize = result.models.iter().map(|model| model.used_notes).sum();
        assert_eq!(used, result.notes_total, "{label}: заметки по моделям");
    }
}

#[test]
fn every_note_resolves_through_ord() {
    let export = media_export();
    let dir = prepared_export("export-ord", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let mut notes = Vec::new();
    collect_notes(&export, &mut notes);

    for entry in &index.notes {
        let uuid = entry
            .note
            .note_model_uuid
            .as_deref()
            .expect("у заметки есть note_model_uuid");
        let model = index
            .model_by_uuid(uuid)
            .unwrap_or_else(|| panic!("модель не разрешается"));
        assert_eq!(
            model.flds.len(),
            entry.note.fields.len(),
            "число fields заметки расходится с моделью"
        );
    }

    // Значения замечаний сверяются с сырым JSON через ord, а не через позицию
    // массива `flds`.
    for note in &notes {
        let uuid = note["note_model_uuid"].as_str().expect("uuid");
        let by_ord = fields_by_ord(&export, note, uuid);
        assert_eq!(
            raw_field_position(&raw_models(&export), note, &by_ord[0].1),
            usize::try_from(by_ord[0].0).expect("ord неотрицателен"),
            "первое по ord поле должно читаться из своей позиции"
        );
    }

    assert_eq!(index.notes.len(), notes.len());
}

#[test]
fn guids_are_reported_accurately() {
    let export = media_export();
    let dir = prepared_export("export-guid", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let mut notes = Vec::new();
    collect_notes(&export, &mut notes);
    assert_eq!(index.notes.len(), notes.len());

    for note in &notes {
        let guid = note["guid"].as_str().expect("у заметки есть guid");
        assert_eq!(
            index.note_positions_by_guid(guid).len(),
            1,
            "guid {guid} должен встречаться один раз"
        );
    }

    let root_name = loaded.root.name.clone();
    assert_eq!(root_name, "Группа", "корневой узел читается из экспорта");
}

#[test]
fn stats_agree_with_inspect() {
    let export = media_export();
    let dir = prepared_export("export-stats", &export, MEDIA);
    let loaded = load(dir.path());
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

    assert_eq!(computed.notes_total, inspected.notes_total);
    assert_eq!(computed.deck_nodes, inspected.deck_nodes);
    assert_eq!(
        computed
            .notes_by_deck
            .iter()
            .map(|deck| deck.count)
            .sum::<usize>(),
        inspected.notes_total
    );
    assert_eq!(
        computed
            .notes_by_model
            .iter()
            .map(|model| model.count)
            .sum::<usize>(),
        inspected.notes_total
    );
    assert_eq!(computed.unresolved_model_notes, 0);

    let model_names: Vec<&str> = computed
        .notes_by_model
        .iter()
        .map(|model| model.key.as_str())
        .collect();
    let inspected_names: Vec<&str> = inspected
        .models
        .iter()
        .map(|model| model.name.as_str())
        .collect();
    assert_eq!(model_names, inspected_names);
}

#[test]
fn group_by_matches_independently_recomputed_distribution() {
    let export = media_export();
    let dir = prepared_export("export-group", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let models = raw_models(&export);
    let mut notes = Vec::new();
    collect_notes(&export, &mut notes);

    let group_field = "Альфа";
    let mut counted: BTreeMap<String, usize> = BTreeMap::new();
    for note in &notes {
        let uuid = note["note_model_uuid"].as_str().expect("uuid");
        // Поле есть не у каждой модели: заметки без него в распределение не
        // попадают, как и в самом `stats`.
        if !models
            .get(uuid)
            .is_some_and(|model| model.contains_key(group_field))
        {
            continue;
        }
        let raw = note
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|fields| fields.get(raw_field_position(&models, note, group_field)))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        *counted.entry(raw).or_insert(0) += 1;
    }

    let group = stats(
        &loaded.export_dir,
        &index,
        &StatsQuery {
            group_by: Some(group_field.to_string()),
            top: 500,
        },
    )
    .expect("stats")
    .group_by
    .expect("распределение");

    assert_eq!(
        group.distinct_values,
        counted.len(),
        "число различных значений"
    );
    assert_eq!(group.notes_with_field, counted.values().sum::<usize>());

    for bucket in &group.buckets {
        assert_eq!(
            counted.get(&bucket.key),
            Some(&bucket.count),
            "значение {:?}",
            bucket.key
        );
    }
}

#[test]
fn valid_export_has_no_errors_and_only_recomputed_media_warnings() {
    let export = media_export();
    let dir = prepared_export("export-validate", &export, MEDIA);

    let mut declared = Vec::new();
    declared_media(&export, &mut declared);
    let declared_set: BTreeSet<String> = declared.iter().map(|name| basename(name)).collect();
    let physical = physical_media(dir.path());
    let expected_missing = declared_set.difference(&physical).count();
    let expected_undeclared = physical.difference(&declared_set).count();

    let result = validate(dir.path()).expect("validate");
    assert!(result.valid, "экспорт должен быть валидным");
    assert_eq!(result.summary.errors, 0, "issues {:?}", result.issues);

    for issue in &result.issues {
        if issue.severity == Severity::Error {
            panic!("неожиданный ERROR {:?}", issue.message);
        }
        if issue.severity == Severity::Warning {
            assert_eq!(
                issue.code, "media_physical_missing",
                "неожиданный WARNING {}",
                issue.code
            );
        }
    }

    assert!(
        !result
            .issues
            .iter()
            .any(|issue| issue.code == "issues_truncated"),
        "issues усечены, сравнение количества некорректно"
    );
    let missing = result
        .issues
        .iter()
        .filter(|issue| issue.code == "media_physical_missing")
        .count();
    assert_eq!(
        missing, expected_missing,
        "число отсутствующих физически media"
    );

    let unused = result
        .issues
        .iter()
        .filter(|issue| issue.code == "media_physical_unused")
        .count();
    assert_eq!(unused, expected_undeclared, "число необъявленных media");
}

#[test]
fn every_media_reference_in_notes_is_declared() {
    let export = media_export();
    let mut declared = Vec::new();
    declared_media(&export, &mut declared);
    let declared_set: BTreeSet<String> = declared.iter().map(|name| basename(name)).collect();

    let mut notes = Vec::new();
    collect_notes(&export, &mut notes);

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

    assert!(!references.is_empty(), "ожидались ссылки на media в полях");
    let undeclared: Vec<&String> = references.difference(&declared_set).collect();
    assert!(
        undeclared.is_empty(),
        "ссылки на необъявленный media: {undeclared:?}"
    );
}

#[test]
fn find_by_guid_returns_fields_in_model_ord_order() {
    let export = media_export();
    let dir = prepared_export("export-find-guid", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let mut notes = Vec::new();
    collect_notes(&export, &mut notes);

    for note in &notes {
        let guid = note["guid"].as_str().expect("guid");
        let uuid = note["note_model_uuid"].as_str().expect("uuid");
        let by_ord = fields_by_ord(&export, note, uuid);
        let expected_names: Vec<&str> = by_ord.iter().map(|(_, name)| name.as_str()).collect();
        let raw_fields = note["fields"].as_array().expect("fields");

        let result = find(
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
        .unwrap_or_else(|error| panic!("guid {guid} не найден: {error}"));

        assert_eq!(result.matched_total, 1);
        assert_eq!(result.returned, 1);
        let found = &result.notes[0];
        assert_eq!(found.guid.as_deref(), Some(guid));

        let names: Vec<&str> = found
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, expected_names, "порядок полей должен быть по ord");

        let values: Vec<Option<&str>> = found
            .fields
            .iter()
            .map(|field| field.value.as_deref())
            .collect();
        for (position, (ord, name)) in by_ord.iter().enumerate() {
            let raw_position = usize::try_from(*ord).expect("ord неотрицателен");
            assert_eq!(
                values[position],
                raw_fields[raw_position].as_str(),
                "поле {name} расходится с сырым JSON"
            );
        }
    }
}

#[test]
fn find_by_field_value_reports_bounded_results() {
    let export = media_export();
    let dir = prepared_export("export-find-field", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let result = find(
        &loaded.export_dir,
        &index,
        &FindQuery {
            criteria: FindCriteria::Field {
                field: "Альфа".to_string(),
                value: "значение".to_string(),
                mode: MatchMode::Contains,
            },
            deck: None,
            limit: 1,
        },
    )
    .expect("поиск должен что-то найти");

    assert!(result.matched_total > result.returned);
    assert_eq!(result.returned, 1);
    assert!(result.truncated);
    assert_eq!(result.notes.len(), 1);
}

#[test]
fn output_is_deterministic_across_runs() {
    let export = media_export();
    let dir = prepared_export("export-determinism", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let first = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, true);
    let second = inspect(&loaded.export_dir, &loaded.deck_json, &loaded.root, true);
    assert_eq!(
        anki_repo::render::json::inspect_json(&first),
        anki_repo::render::json::inspect_json(&second),
        "JSON должен быть стабильным"
    );

    let compute = || {
        stats(
            &loaded.export_dir,
            &index,
            &StatsQuery {
                group_by: Some("Альфа".to_string()),
                top: 50,
            },
        )
        .expect("stats")
    };
    assert_eq!(compute().group_by, compute().group_by);
}

#[test]
fn json_render_keeps_ordered_fields() {
    let export = media_export();
    let dir = prepared_export("export-json-order", &export, MEDIA);
    let loaded = load(dir.path());
    let index = ExportIndex::build(&loaded.root);

    let mut notes = Vec::new();
    collect_notes(&export, &mut notes);
    let note = &notes[0];
    let guid = note["guid"].as_str().expect("guid");
    let uuid = note["note_model_uuid"].as_str().expect("uuid");
    let by_ord = fields_by_ord(&export, note, uuid);

    let result = find(
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
    // в serde_json::Value порядок объекта нормализуется.
    let positions: Vec<usize> = by_ord
        .iter()
        .map(|(_, name)| {
            text.find(&format!("\"{name}\":"))
                .unwrap_or_else(|| panic!("в выводе нет ключа {name}"))
        })
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(positions, sorted, "ключи fields должны идти в порядке ord");

    let fields = parsed["result"]["notes"][0]["fields"]
        .as_object()
        .expect("fields — объект");
    assert_eq!(fields.len(), by_ord.len());

    let raw_fields = note["fields"].as_array().expect("fields");
    for (ord, name) in &by_ord {
        let position = usize::try_from(*ord).expect("ord неотрицателен");
        assert_eq!(
            fields[name.as_str()],
            raw_fields[position],
            "поле {name} должно совпадать с сырым JSON"
        );
    }

    // Порядок, который видит пользователь, — тот же: `find` печатает поля по ord.
    let (code, stdout, stderr) = run_cli(&[
        "find",
        &dir.path().to_string_lossy(),
        "--guid",
        guid,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let cli = parse_json(&stdout);
    let cli_fields = cli["result"]["notes"][0]["fields"]
        .as_object()
        .expect("fields — объект");
    let cli_names: Vec<&str> = cli_fields.keys().map(String::as_str).collect();
    let expected_names: Vec<&str> = by_ord.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(cli_names, expected_names);
}

#[test]
fn relative_and_absolute_export_paths_are_equivalent() {
    let export = mixed_export();
    let dir = canonical_export("export-paths", &export);

    let absolute = dir.path().to_string_lossy().to_string();
    let name = dir
        .path()
        .file_name()
        .expect("у временного каталога есть имя")
        .to_string_lossy()
        .to_string();

    let (code_abs, stdout_abs, stderr_abs) = run_cli(&["inspect", &absolute, "--json"]);
    assert_eq!(code_abs, 0, "stderr: {stderr_abs}");

    // Относительный путь проверяется из родительского каталога: имя каталога
    // одно и то же, а рабочий каталог процесса другой.
    let (code_rel, stdout_rel, stderr_rel) = run_cli_in(
        Some(dir.path().parent().expect("родительский каталог")),
        &["inspect", &name, "--json"],
    );
    assert_eq!(code_rel, 0, "stderr: {stderr_rel}");

    let absolute_json = parse_json(&stdout_abs);
    let relative_json = parse_json(&stdout_rel);
    let mut absolute_result = absolute_json["result"].clone();
    let mut relative_result = relative_json["result"].clone();
    // `export_dir` — единственное поле, которое обязано различаться.
    absolute_result["export_dir"] = Value::from("");
    relative_result["export_dir"] = Value::from("");
    assert_eq!(
        absolute_result, relative_result,
        "результат не должен зависеть от формы пути"
    );
}

#[test]
fn missing_export_directory_is_reported_as_usage_of_input() {
    let (code, stdout, stderr) = run_cli(&["inspect", "/нет-такого-каталога-anki-repo"]);
    assert_ne!(code, 0);
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("нет-такого-каталога-anki-repo"),
        "stderr: {stderr}"
    );
}
