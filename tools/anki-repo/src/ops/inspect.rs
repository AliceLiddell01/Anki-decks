//! Операция `inspect`: компактное описание структуры одного экспорта.

use std::path::Path;

use crate::index::ExportIndex;
use crate::media::{MediaReport, collect_media};
use crate::model::{DeckConfig, DeckNode, NoteModel};
use crate::ops::{CountEntry, MediaCounters, SAMPLE_LIMIT};

/// Компактное описание CrowdAnki-экспорта.
#[derive(Debug, PartialEq)]
pub struct InspectResult {
    /// Каталог экспорта.
    pub export_dir: String,
    /// Имя корневой колоды.
    pub root_deck_name: String,
    /// Число узлов дерева колод.
    pub deck_nodes: usize,
    /// Общее число заметок.
    pub notes_total: usize,
    /// Модели заметок экспорта.
    pub models: Vec<ModelSummary>,
    /// Конфигурации колод экспорта.
    pub configs: Vec<ConfigSummary>,
    /// Media-счётчики.
    pub media: MediaCounters,
    /// Заметки по узлам колоды в preorder-порядке.
    pub notes_by_deck: Vec<CountEntry>,
    /// Дополнительные детали режима `--verbose`.
    pub verbose: Option<InspectVerbose>,
}

/// Сводка по одной модели заметок.
#[derive(Debug, PartialEq)]
pub struct ModelSummary {
    /// Имя модели.
    pub name: String,
    /// Идентичность CrowdAnki.
    pub crowdanki_uuid: Option<String>,
    /// Число объявленных полей.
    pub field_count: usize,
    /// Число объявленных шаблонов.
    pub template_count: usize,
    /// Сколько заметок экспорта ссылается на эту модель.
    pub used_notes: usize,
    /// Поля модели в порядке `ord`.
    pub fields: Vec<FieldSummary>,
}

/// Поле модели в порядке `ord`.
#[derive(Debug, PartialEq)]
pub struct FieldSummary {
    /// Позиция поля.
    pub ord: Option<i64>,
    /// Имя поля.
    pub name: String,
}

/// Сводка по одной конфигурации колоды.
#[derive(Debug, PartialEq)]
pub struct ConfigSummary {
    /// Имя конфигурации.
    pub name: String,
    /// Идентичность CrowdAnki.
    pub crowdanki_uuid: Option<String>,
    /// Сколько узлов колод ссылается на эту конфигурацию.
    pub nodes_using: usize,
}

/// Детали, доступные только в режиме `--verbose`.
#[derive(Debug, PartialEq)]
pub struct InspectVerbose {
    /// Путь к разобранному `deck.json`.
    pub deck_json: String,
    /// Узлы колод в preorder-порядке.
    pub nodes: Vec<NodeSummary>,
    /// Шаблоны по моделям в порядке объявления моделей.
    pub model_templates: Vec<ModelTemplates>,
    /// Сколько заметок имеют уникальный `guid`.
    pub guids_unique: usize,
    /// Сколько заметок имеют `guid`, встречающийся более одного раза.
    pub guid_duplicates: usize,
    /// Выборка объявленного media без физического файла.
    pub media_missing_physical_sample: Vec<String>,
    /// Выборка физического media без объявления.
    pub media_undeclared_physical_sample: Vec<String>,
    /// Выборка повторяющихся объявленных media-имён.
    pub media_duplicate_declared_sample: Vec<String>,
    /// Ограниченная выборка заметок.
    pub notes_sample: Vec<NoteSample>,
}

/// Узел колоды в preorder-порядке.
#[derive(Debug, PartialEq)]
pub struct NodeSummary {
    /// Позиция в preorder-обходе.
    pub preorder: usize,
    /// Глубина в дереве.
    pub depth: usize,
    /// Путь колоды.
    pub path: String,
    /// Идентичность CrowdAnki узла.
    pub crowdanki_uuid: Option<String>,
    /// Число заметок непосредственно в узле.
    pub notes: usize,
}

/// Шаблоны одной модели.
#[derive(Debug, PartialEq)]
pub struct ModelTemplates {
    /// Имя модели.
    pub model_name: String,
    /// Идентичность модели.
    pub crowdanki_uuid: Option<String>,
    /// Шаблоны в порядке объявления.
    pub templates: Vec<TemplateSummary>,
}

/// Шаблон карточки в компактном виде.
#[derive(Debug, PartialEq)]
pub struct TemplateSummary {
    /// Позиция шаблона.
    pub ord: Option<i64>,
    /// Имя шаблона.
    pub name: Option<String>,
}

/// Заметка в ограниченной диагностической выборке.
#[derive(Debug, PartialEq)]
pub struct NoteSample {
    /// Идентификатор заметки.
    pub guid: Option<String>,
    /// Путь колоды.
    pub deck_path: String,
    /// Число значений в `fields`.
    pub field_count: usize,
}

/// Выполняет `inspect`.
pub fn inspect(
    export_dir: &Path,
    deck_json: &Path,
    root: &DeckNode,
    verbose: bool,
) -> InspectResult {
    let index = ExportIndex::build(root);
    let report = collect_media(export_dir, &index);

    let models = index
        .models
        .iter()
        .map(|model| summarize_model(model, &index))
        .collect();

    let configs = index
        .configs
        .iter()
        .map(|config| summarize_config(config, &index))
        .collect();

    let notes_by_deck = index
        .nodes
        .iter()
        .map(|entry| CountEntry {
            key: entry.path.to_string(),
            count: entry.node.notes.len(),
        })
        .collect();

    InspectResult {
        export_dir: export_dir.display().to_string(),
        root_deck_name: root.display_name().to_string(),
        deck_nodes: index.nodes.len(),
        notes_total: index.notes.len(),
        models,
        configs,
        media: MediaCounters::from_report(&report),
        notes_by_deck,
        verbose: verbose.then(|| build_verbose(deck_json, &index, &report)),
    }
}

fn summarize_model(model: &NoteModel, index: &ExportIndex<'_>) -> ModelSummary {
    let used_notes = model.crowdanki_uuid.as_deref().map_or(0, |uuid| {
        index
            .notes
            .iter()
            .filter(|entry| entry.note.note_model_uuid.as_deref() == Some(uuid))
            .count()
    });

    let fields = crate::index::model_fields_in_ord_order(model)
        .into_iter()
        .map(|field| FieldSummary {
            ord: field.ord.value(),
            name: field.name.clone(),
        })
        .collect();

    ModelSummary {
        name: model
            .name
            .clone()
            .unwrap_or_else(|| "(без имени)".to_string()),
        crowdanki_uuid: model.crowdanki_uuid.clone(),
        field_count: model.flds.len(),
        template_count: model.tmpls.len(),
        used_notes,
        fields,
    }
}

fn summarize_config(config: &DeckConfig, index: &ExportIndex<'_>) -> ConfigSummary {
    let nodes_using = config.crowdanki_uuid.as_deref().map_or(0, |uuid| {
        index
            .nodes
            .iter()
            .filter(|entry| entry.node.deck_config_uuid.as_deref() == Some(uuid))
            .count()
    });

    ConfigSummary {
        name: config
            .name
            .clone()
            .unwrap_or_else(|| "(без имени)".to_string()),
        crowdanki_uuid: config.crowdanki_uuid.clone(),
        nodes_using,
    }
}

fn build_verbose(
    deck_json: &Path,
    index: &ExportIndex<'_>,
    report: &MediaReport,
) -> InspectVerbose {
    let nodes = index
        .nodes
        .iter()
        .map(|entry| NodeSummary {
            preorder: entry.preorder,
            depth: entry.depth,
            path: entry.path.to_string(),
            crowdanki_uuid: entry.node.crowdanki_uuid.clone(),
            notes: entry.node.notes.len(),
        })
        .collect();

    let model_templates = index
        .models
        .iter()
        .map(|model| ModelTemplates {
            model_name: model
                .name
                .clone()
                .unwrap_or_else(|| "(без имени)".to_string()),
            crowdanki_uuid: model.crowdanki_uuid.clone(),
            templates: model
                .tmpls
                .iter()
                .map(|template| TemplateSummary {
                    ord: template.ord.value(),
                    name: template.name.clone(),
                })
                .collect(),
        })
        .collect();

    let guid_duplicates = index
        .guids
        .values()
        .filter(|positions| positions.len() > 1)
        .count();

    let notes_sample = index
        .notes
        .iter()
        .take(SAMPLE_LIMIT)
        .map(|entry| NoteSample {
            guid: entry.note.guid.clone(),
            deck_path: index.note_deck_path(entry).to_string(),
            field_count: entry.note.fields.len(),
        })
        .collect();

    let mut missing_physical = report.missing_physical();
    missing_physical.truncate(SAMPLE_LIMIT);
    let mut undeclared_physical = report.undeclared_physical();
    undeclared_physical.truncate(SAMPLE_LIMIT);
    let duplicate_declared: Vec<String> = report
        .duplicate_declared
        .iter()
        .take(SAMPLE_LIMIT)
        .cloned()
        .collect();

    InspectVerbose {
        deck_json: deck_json.display().to_string(),
        nodes,
        model_templates,
        guids_unique: index.guids.len(),
        guid_duplicates,
        media_missing_physical_sample: missing_physical,
        media_undeclared_physical_sample: undeclared_physical,
        media_duplicate_declared_sample: duplicate_declared,
        notes_sample,
    }
}
