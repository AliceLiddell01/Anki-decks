//! Обход дерева колод, in-memory индексы и централизованное разрешение полей.
//!
//! Здесь живёт единственная реализация критической связи CrowdAnki:
//!
//! ```text
//! Note.note_model_uuid
//! → NoteModel.crowdanki_uuid
//! → NoteModel.flds[].ord
//! → Note.fields[ord]
//! → named fields
//! ```
//!
//! Ни одна команда не должна реализовывать positional field resolution заново.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::model::{DeckConfig, DeckNode, FieldDef, FieldValue, Note, NoteModel, Ord};

/// Ссылка на узел колоды в preorder-обходе.
#[derive(Debug)]
pub struct NodeRef<'a> {
    /// Сам узел.
    pub node: &'a DeckNode,
    /// Путь колоды (`DeckNode.name`).
    pub path: &'a str,
    /// Глубина в дереве колод, начиная с нуля.
    pub depth: usize,
    /// Позиция в preorder-обходе.
    pub preorder: usize,
    /// Конец непрерывного диапазона поддерева в preorder-порядке.
    pub subtree_end: usize,
}

/// Ссылка на заметку вместе с её местом в экспорте.
#[derive(Debug)]
pub struct NoteRef<'a> {
    /// Сама заметка.
    pub note: &'a Note,
    /// Индекс узла колоды в [`ExportIndex::nodes`].
    pub node: usize,
    /// Порядок заметки внутри узла.
    pub order: usize,
}

/// Разрешённое поле заметки, сопоставленное с определением модели.
#[derive(Debug, Clone)]
pub struct ResolvedField<'a> {
    /// Имя поля из модели.
    pub name: &'a str,
    /// Позиция поля в модели.
    pub ord: Ord,
    /// Значение поля; `None`, если позиции нет в `Note.fields`.
    pub value: Option<&'a FieldValue>,
}

/// Индексы одного CrowdAnki-экспорта.
#[derive(Debug)]
pub struct ExportIndex<'a> {
    /// Узлы колод в preorder-порядке.
    pub nodes: Vec<NodeRef<'a>>,
    /// Все заметки в порядке экспорта: preorder узлов, затем порядок внутри узла.
    pub notes: Vec<NoteRef<'a>>,
    /// Модели заметок, объединённые по всему дереву (первое объявление побеждает).
    pub models: Vec<&'a NoteModel>,
    /// Конфигурации колод, объединённые по всему дереву.
    pub configs: Vec<&'a DeckConfig>,
    /// Индекс `guid` → позиции заметок в [`ExportIndex::notes`].
    pub guids: BTreeMap<&'a str, Vec<usize>>,
    models_by_uuid: BTreeMap<&'a str, usize>,
    configs_by_uuid: BTreeMap<&'a str, usize>,
    field_names: BTreeSet<&'a str>,
}

impl<'a> ExportIndex<'a> {
    /// Строит индексы по корню экспорта.
    pub fn build(root: &'a DeckNode) -> Self {
        let mut nodes: Vec<NodeRef<'a>> = Vec::new();
        collect_nodes(root, 0, &mut nodes);
        annotate_subtrees(&mut nodes);

        let mut notes: Vec<NoteRef<'a>> = Vec::new();
        for (node, entry) in nodes.iter().enumerate() {
            for (order, note) in entry.node.notes.iter().enumerate() {
                notes.push(NoteRef { note, node, order });
            }
        }

        let mut models: Vec<&'a NoteModel> = Vec::new();
        let mut models_by_uuid: BTreeMap<&'a str, usize> = BTreeMap::new();
        for entry in &nodes {
            for model in &entry.node.note_models {
                if let Some(uuid) = model.crowdanki_uuid.as_deref()
                    && !models_by_uuid.contains_key(uuid)
                {
                    models_by_uuid.insert(uuid, models.len());
                    models.push(model);
                }
            }
        }

        let mut configs: Vec<&'a DeckConfig> = Vec::new();
        let mut configs_by_uuid: BTreeMap<&'a str, usize> = BTreeMap::new();
        for entry in &nodes {
            for config in &entry.node.deck_configurations {
                if let Some(uuid) = config.crowdanki_uuid.as_deref()
                    && !configs_by_uuid.contains_key(uuid)
                {
                    configs_by_uuid.insert(uuid, configs.len());
                    configs.push(config);
                }
            }
        }

        let mut field_names: BTreeSet<&'a str> = BTreeSet::new();
        for &model in &models {
            for field in &model.flds {
                field_names.insert(field.name.as_str());
            }
        }

        let mut guids: BTreeMap<&'a str, Vec<usize>> = BTreeMap::new();
        for (position, entry) in notes.iter().enumerate() {
            if let Some(guid) = entry.note.guid.as_deref() {
                guids.entry(guid).or_default().push(position);
            }
        }

        Self {
            nodes,
            notes,
            models,
            configs,
            guids,
            models_by_uuid,
            configs_by_uuid,
            field_names,
        }
    }

    /// Модель заметки по её CrowdAnki UUID.
    pub fn model_by_uuid(&self, uuid: &str) -> Option<&'a NoteModel> {
        self.models_by_uuid
            .get(uuid)
            .map(|index| self.models[*index])
    }

    /// Конфигурация колоды по её CrowdAnki UUID.
    pub fn config_by_uuid(&self, uuid: &str) -> Option<&'a DeckConfig> {
        self.configs_by_uuid
            .get(uuid)
            .map(|index| self.configs[*index])
    }

    /// Позиции заметок с данным `guid`.
    pub fn note_positions_by_guid(&self, guid: &str) -> &[usize] {
        self.guids.get(guid).map_or(&[][..], Vec::as_slice)
    }

    /// Объединение имён полей всех моделей экспорта.
    pub fn known_field_names(&self) -> &BTreeSet<&'a str> {
        &self.field_names
    }

    /// Индекс первого узла с точным путём колоды.
    pub fn node_index_by_path(&self, path: &str) -> Option<usize> {
        self.nodes.iter().position(|entry| entry.path == path)
    }

    /// Непрерывный диапазон preorder-индексов поддерева узла.
    pub fn subtree_range(&self, node: usize) -> Range<usize> {
        node..self.nodes[node].subtree_end
    }

    /// Путь колоды для заметки.
    pub fn note_deck_path(&self, note: &NoteRef<'a>) -> &'a str {
        self.nodes[note.node].path
    }
}

fn collect_nodes<'a>(node: &'a DeckNode, depth: usize, out: &mut Vec<NodeRef<'a>>) {
    let preorder = out.len();
    out.push(NodeRef {
        node,
        path: node.name.as_str(),
        depth,
        preorder,
        subtree_end: preorder + 1,
    });
    for child in &node.children {
        collect_nodes(child, depth + 1, out);
    }
}

/// Заполняет `subtree_end` после полного preorder-обхода.
///
/// Поддерево узла занимает непрерывный диапазон preorder-позиций: он
/// заканчивается на первом следующем узле с глубиной не больше собственной.
fn annotate_subtrees(nodes: &mut [NodeRef<'_>]) {
    let total = nodes.len();
    for position in 0..total {
        let depth = nodes[position].depth;
        let end = nodes[position + 1..]
            .iter()
            .position(|candidate| candidate.depth <= depth)
            .map_or(total, |offset| position + 1 + offset);
        nodes[position].subtree_end = end;
    }
}

/// Определения полей модели в порядке `ord`.
///
/// Для корректной модели это её собственный порядок объявления. Malformed
/// значения (`Ord::Invalid`) уходят в конец, сохраняя порядок объявления.
pub fn model_fields_in_ord_order(model: &NoteModel) -> Vec<&FieldDef> {
    let mut indexed: Vec<(usize, &FieldDef)> = model.flds.iter().enumerate().collect();
    indexed.sort_by_key(|(index, field)| (field.ord.value().is_none(), field.ord.value(), *index));
    indexed.into_iter().map(|(_, field)| field).collect()
}

/// Разрешает значение поля заметки по имени поля её модели.
///
/// Возвращает `None`, если модель не содержит такого поля либо позиции нет
/// в `Note.fields`.
pub fn field_value_by_name<'a>(
    note: &'a Note,
    model: &'a NoteModel,
    name: &str,
) -> Option<&'a FieldValue> {
    let field = model.flds.iter().find(|field| field.name == name)?;
    let ord = field.ord.value()?;
    let index = usize::try_from(ord).ok()?;
    note.fields.get(index)
}

/// Разрешает все поля заметки в именованный вид в порядке `ord`.
pub fn resolve_named_fields<'a>(note: &'a Note, model: &'a NoteModel) -> Vec<ResolvedField<'a>> {
    model_fields_in_ord_order(model)
        .into_iter()
        .map(|field| {
            let value = field
                .ord
                .value()
                .and_then(|ord| usize::try_from(ord).ok())
                .and_then(|index| note.fields.get(index));
            ResolvedField {
                name: field.name.as_str(),
                ord: field.ord,
                value,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MINIMAL_EXPORT, NESTED_EXPORT, deck_node};

    #[test]
    fn resolves_note_model_by_uuid() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let model = index
            .model_by_uuid("model-1")
            .expect("модель должна разрешаться");
        assert_eq!(model.name.as_deref(), Some("Слова"));
        assert!(index.model_by_uuid("нет-такой").is_none());
    }

    #[test]
    fn resolves_field_names_through_ord() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let model = index.model_by_uuid("model-1").expect("модель");
        let note = &index.notes[0].note;

        let word = field_value_by_name(note, model, "Слово").expect("поле Слово");
        assert_eq!(word.as_text(), Some("[sound:a.mp3]偶然"));
        let meaning = field_value_by_name(note, model, "Значение").expect("поле Значение");
        assert_eq!(meaning.as_text(), Some("случайность"));
        assert!(field_value_by_name(note, model, "НетТакого").is_none());
    }

    #[test]
    fn named_fields_follow_model_ord() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let model = index.model_by_uuid("model-1").expect("модель");
        let fields = resolve_named_fields(index.notes[0].note, model);
        let names: Vec<&str> = fields.iter().map(|field| field.name).collect();
        assert_eq!(names, vec!["Слово", "Значение"]);
        assert_eq!(fields[0].ord, Ord::Int(0));
        assert_eq!(fields[1].ord, Ord::Int(1));

        let second = resolve_named_fields(index.notes[1].note, model);
        assert_eq!(second[1].value.and_then(FieldValue::as_text), Some(""));
    }

    #[test]
    fn field_order_is_deterministic_for_malformed_ords() {
        let node = deck_node(
            r#"{
                "__type__": "Deck",
                "name": "D",
                "note_models": [{
                    "crowdanki_uuid": "m",
                    "name": "M",
                    "flds": [
                        {"name": "Первый", "ord": "строка"},
                        {"name": "Второй", "ord": 1},
                        {"name": "Третий", "ord": 0}
                    ],
                    "tmpls": []
                }],
                "notes": [],
                "children": []
            }"#,
        );
        let model = &node.note_models[0];
        let names: Vec<&str> = model_fields_in_ord_order(model)
            .iter()
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(names, vec!["Третий", "Второй", "Первый"]);
    }

    #[test]
    fn traversal_is_preorder_with_deck_paths() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        let paths: Vec<&str> = index.nodes.iter().map(|entry| entry.path).collect();
        assert_eq!(paths, vec!["Root", "Root::Child", "Root::Child::Leaf"]);
        let depths: Vec<usize> = index.nodes.iter().map(|entry| entry.depth).collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn subtree_ranges_cover_descendants() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        assert_eq!(index.subtree_range(0), 0..3);
        assert_eq!(index.subtree_range(1), 1..3);
        assert_eq!(index.subtree_range(2), 2..3);
    }

    #[test]
    fn notes_keep_export_order_across_nodes() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        let guids: Vec<&str> = index
            .notes
            .iter()
            .map(|entry| entry.note.guid.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(guids, vec!["guid-root", "guid-child", "guid-leaf"]);
    }

    #[test]
    fn guid_index_groups_repeated_guids() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        assert_eq!(index.note_positions_by_guid("guid-1"), &[0]);
        assert!(index.note_positions_by_guid("нет-такого").is_empty());

        let duplicated = crate::test_support::export_with(MINIMAL_EXPORT, |value| {
            value["notes"][1]["guid"] = serde_json::json!("guid-1");
        });
        let node = deck_node(&duplicated);
        let index = ExportIndex::build(&node);
        assert_eq!(index.note_positions_by_guid("guid-1"), &[0, 1]);
    }

    #[test]
    fn models_are_merged_across_nodes_without_duplicates() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        assert_eq!(index.models.len(), 1);
        assert_eq!(index.configs.len(), 1);
        assert_eq!(index.known_field_names().len(), 2);
    }

    #[test]
    fn deck_path_lookup_is_exact() {
        let node = deck_node(NESTED_EXPORT);
        let index = ExportIndex::build(&node);
        assert_eq!(index.node_index_by_path("Root::Child"), Some(1));
        assert_eq!(index.node_index_by_path("Child"), None);
    }
}
