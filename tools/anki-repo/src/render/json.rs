//! JSON renderer: стабильный machine-readable контракт schema_version 1.
//!
//! Human и JSON renderers получают один и тот же domain result. Имена
//! JSON-ключей и кодов стабильны и не зависят от Rust-имён типов.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

use crate::error::DomainError;
use crate::ops::NamedField;
use crate::ops::find::FindResult;
use crate::ops::inspect::{InspectResult, InspectVerbose, ModelSummary};
use crate::ops::stats::StatsResult;
use crate::ops::validate::ValidateResult;

/// Версия machine-readable контракта.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
struct SuccessEnvelope<T> {
    schema_version: u32,
    command: &'static str,
    result: T,
}

#[derive(Serialize)]
struct FailureEnvelope<'a> {
    schema_version: u32,
    command: &'a str,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'static str,
    message: &'a str,
    details: &'a serde_json::Value,
}

/// Поля заметки как JSON-объект в порядке `ord` модели.
///
/// Порядок ключей задаётся самим domain result'ом, а не сортировкой имён.
/// При дублирующихся именах полей (malformed модель) ключ получает суффикс
/// `#N`, чтобы JSON оставался однозначным.
struct OrderedFields<'a>(&'a [NamedField]);

impl Serialize for OrderedFields<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        let mut occurrences: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for field in self.0 {
            let seen = occurrences.entry(field.name.as_str()).or_insert(0);
            *seen += 1;
            let key = if *seen == 1 {
                field.name.clone()
            } else {
                format!("{}#{}", field.name, seen)
            };
            map.serialize_entry(&key, &field.value)?;
        }
        map.end()
    }
}

fn to_json<T: Serialize>(command: &'static str, result: T) -> String {
    let envelope = SuccessEnvelope {
        schema_version: SCHEMA_VERSION,
        command,
        result,
    };
    let mut text = serde_json::to_string_pretty(&envelope)
        .unwrap_or_else(|error| format!("{{\"internal_error\": \"{error}\"}}"));
    text.push('\n');
    text
}

/// JSON-представление доменной ошибки.
pub fn error_json(command: &str, error: &DomainError) -> String {
    let envelope = FailureEnvelope {
        schema_version: SCHEMA_VERSION,
        command,
        error: ErrorBody {
            code: error.code.as_str(),
            message: &error.message,
            details: &error.details,
        },
    };
    let mut text = serde_json::to_string_pretty(&envelope)
        .unwrap_or_else(|internal| format!("{{\"internal_error\": \"{internal}\"}}"));
    text.push('\n');
    text
}

/// JSON-представление результата `inspect`.
pub fn inspect_json(result: &InspectResult) -> String {
    to_json("inspect", InspectDto::from(result))
}

/// JSON-представление результата `find`.
pub fn find_json(result: &FindResult) -> String {
    to_json("find", FindDto::from(result))
}

/// JSON-представление результата `stats`.
pub fn stats_json(result: &StatsResult) -> String {
    to_json("stats", StatsDto::from(result))
}

/// JSON-представление результата `validate`.
pub fn validate_json(result: &ValidateResult) -> String {
    to_json("validate", ValidateDto::from(result))
}

#[derive(Serialize)]
struct InspectDto {
    export_dir: String,
    root_deck_name: String,
    deck_nodes: usize,
    notes_total: usize,
    note_models: Vec<ModelDto>,
    deck_configurations: Vec<ConfigDto>,
    media: MediaDto,
    notes_by_deck: Vec<DeckCountDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verbose: Option<VerboseDto>,
}

#[derive(Serialize)]
struct ModelDto {
    name: String,
    crowdanki_uuid: Option<String>,
    field_count: usize,
    template_count: usize,
    used_notes: usize,
    fields: Vec<FieldDefDto>,
}

#[derive(Serialize)]
struct FieldDefDto {
    ord: Option<i64>,
    name: String,
}

#[derive(Serialize)]
struct ConfigDto {
    name: String,
    crowdanki_uuid: Option<String>,
    nodes_using: usize,
}

#[derive(Serialize)]
struct MediaDto {
    declared_total: usize,
    declared_unique: usize,
    duplicate_declared: usize,
    dir_present: bool,
    physical_total: usize,
    missing_physical: usize,
    undeclared_physical: usize,
}

#[derive(Serialize)]
struct DeckCountDto {
    deck_path: String,
    notes: usize,
}

#[derive(Serialize)]
struct VerboseDto {
    deck_json: String,
    nodes: Vec<NodeDto>,
    guids_unique: usize,
    guid_duplicates: usize,
    model_templates: Vec<ModelTemplatesDto>,
    media_missing_physical_sample: Vec<String>,
    media_undeclared_physical_sample: Vec<String>,
    media_duplicate_declared_sample: Vec<String>,
    notes_sample: Vec<NoteSampleDto>,
}

#[derive(Serialize)]
struct NodeDto {
    preorder: usize,
    depth: usize,
    deck_path: String,
    crowdanki_uuid: Option<String>,
    notes: usize,
}

#[derive(Serialize)]
struct ModelTemplatesDto {
    model_name: String,
    crowdanki_uuid: Option<String>,
    templates: Vec<TemplateDto>,
}

#[derive(Serialize)]
struct TemplateDto {
    ord: Option<i64>,
    name: Option<String>,
}

#[derive(Serialize)]
struct NoteSampleDto {
    guid: Option<String>,
    deck_path: String,
    fields: usize,
}

impl From<&InspectResult> for InspectDto {
    fn from(result: &InspectResult) -> Self {
        Self {
            export_dir: result.export_dir.clone(),
            root_deck_name: result.root_deck_name.clone(),
            deck_nodes: result.deck_nodes,
            notes_total: result.notes_total,
            note_models: result.models.iter().map(ModelDto::from).collect(),
            deck_configurations: result
                .configs
                .iter()
                .map(|config| ConfigDto {
                    name: config.name.clone(),
                    crowdanki_uuid: config.crowdanki_uuid.clone(),
                    nodes_using: config.nodes_using,
                })
                .collect(),
            media: MediaDto::from(&result.media),
            notes_by_deck: result
                .notes_by_deck
                .iter()
                .map(|entry| DeckCountDto {
                    deck_path: entry.key.clone(),
                    notes: entry.count,
                })
                .collect(),
            verbose: result.verbose.as_ref().map(VerboseDto::from),
        }
    }
}

impl From<&ModelSummary> for ModelDto {
    fn from(model: &ModelSummary) -> Self {
        Self {
            name: model.name.clone(),
            crowdanki_uuid: model.crowdanki_uuid.clone(),
            field_count: model.field_count,
            template_count: model.template_count,
            used_notes: model.used_notes,
            fields: model
                .fields
                .iter()
                .map(|field| FieldDefDto {
                    ord: field.ord,
                    name: field.name.clone(),
                })
                .collect(),
        }
    }
}

impl From<&crate::ops::MediaCounters> for MediaDto {
    fn from(media: &crate::ops::MediaCounters) -> Self {
        Self {
            declared_total: media.declared_total,
            declared_unique: media.declared_unique,
            duplicate_declared: media.duplicate_declared,
            dir_present: media.dir_present,
            physical_total: media.physical_total,
            missing_physical: media.missing_physical,
            undeclared_physical: media.undeclared_physical,
        }
    }
}

impl From<&InspectVerbose> for VerboseDto {
    fn from(verbose: &InspectVerbose) -> Self {
        Self {
            deck_json: verbose.deck_json.clone(),
            nodes: verbose
                .nodes
                .iter()
                .map(|node| NodeDto {
                    preorder: node.preorder,
                    depth: node.depth,
                    deck_path: node.path.clone(),
                    crowdanki_uuid: node.crowdanki_uuid.clone(),
                    notes: node.notes,
                })
                .collect(),
            guids_unique: verbose.guids_unique,
            guid_duplicates: verbose.guid_duplicates,
            model_templates: verbose
                .model_templates
                .iter()
                .map(|model| ModelTemplatesDto {
                    model_name: model.model_name.clone(),
                    crowdanki_uuid: model.crowdanki_uuid.clone(),
                    templates: model
                        .templates
                        .iter()
                        .map(|template| TemplateDto {
                            ord: template.ord,
                            name: template.name.clone(),
                        })
                        .collect(),
                })
                .collect(),
            media_missing_physical_sample: verbose.media_missing_physical_sample.clone(),
            media_undeclared_physical_sample: verbose.media_undeclared_physical_sample.clone(),
            media_duplicate_declared_sample: verbose.media_duplicate_declared_sample.clone(),
            notes_sample: verbose
                .notes_sample
                .iter()
                .map(|sample| NoteSampleDto {
                    guid: sample.guid.clone(),
                    deck_path: sample.deck_path.clone(),
                    fields: sample.field_count,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct FindDto<'a> {
    export_dir: String,
    criteria: CriteriaDto,
    matched_total: usize,
    returned: usize,
    truncated: bool,
    notes: Vec<FoundNoteDto<'a>>,
}

#[derive(Serialize)]
struct CriteriaDto {
    kind: &'static str,
    guid: Option<String>,
    field: Option<String>,
    value: Option<String>,
    match_mode: Option<&'static str>,
    deck: Option<String>,
    limit: usize,
}

#[derive(Serialize)]
struct FoundNoteDto<'a> {
    guid: Option<String>,
    deck_path: String,
    note_model_name: Option<String>,
    note_model_uuid: Option<String>,
    tags: Vec<String>,
    fields: OrderedFields<'a>,
}

impl<'a> From<&'a FindResult> for FindDto<'a> {
    fn from(result: &'a FindResult) -> Self {
        Self {
            export_dir: result.export_dir.clone(),
            criteria: CriteriaDto {
                kind: result.criteria.kind,
                guid: result.criteria.guid.clone(),
                field: result.criteria.field.clone(),
                value: result.criteria.value.clone(),
                match_mode: result.criteria.match_mode,
                deck: result.criteria.deck.clone(),
                limit: result.criteria.limit,
            },
            matched_total: result.matched_total,
            returned: result.returned,
            truncated: result.truncated,
            notes: result
                .notes
                .iter()
                .map(|note| FoundNoteDto {
                    guid: note.guid.clone(),
                    deck_path: note.deck_path.clone(),
                    note_model_name: note.note_model_name.clone(),
                    note_model_uuid: note.note_model_uuid.clone(),
                    tags: note.tags.clone(),
                    fields: OrderedFields(&note.fields),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct StatsDto {
    export_dir: String,
    deck_nodes: usize,
    notes_total: usize,
    unresolved_model_notes: usize,
    notes_by_deck: Vec<DeckCountDto>,
    notes_by_model: Vec<ModelCountDto>,
    config_usage: Vec<ConfigUsageDto>,
    fields: Vec<FieldStatsDto>,
    media: MediaDto,
    group_by: Option<GroupByDto>,
}

#[derive(Serialize)]
struct ModelCountDto {
    note_model: String,
    notes: usize,
}

#[derive(Serialize)]
struct ConfigUsageDto {
    deck_config: String,
    deck_nodes: usize,
}

#[derive(Serialize)]
struct FieldStatsDto {
    name: String,
    total: usize,
    nonempty: usize,
    empty: usize,
}

#[derive(Serialize)]
struct GroupByDto {
    field: String,
    notes_with_field: usize,
    notes_without_field: usize,
    distinct_values: usize,
    truncated: bool,
    values: Vec<ValueCountDto>,
}

#[derive(Serialize)]
struct ValueCountDto {
    value: String,
    notes: usize,
}

impl From<&StatsResult> for StatsDto {
    fn from(result: &StatsResult) -> Self {
        Self {
            export_dir: result.export_dir.clone(),
            deck_nodes: result.deck_nodes,
            notes_total: result.notes_total,
            unresolved_model_notes: result.unresolved_model_notes,
            notes_by_deck: result
                .notes_by_deck
                .iter()
                .map(|entry| DeckCountDto {
                    deck_path: entry.key.clone(),
                    notes: entry.count,
                })
                .collect(),
            notes_by_model: result
                .notes_by_model
                .iter()
                .map(|entry| ModelCountDto {
                    note_model: entry.key.clone(),
                    notes: entry.count,
                })
                .collect(),
            config_usage: result
                .config_usage
                .iter()
                .map(|entry| ConfigUsageDto {
                    deck_config: entry.key.clone(),
                    deck_nodes: entry.count,
                })
                .collect(),
            fields: result
                .fields
                .iter()
                .map(|field| FieldStatsDto {
                    name: field.name.clone(),
                    total: field.total,
                    nonempty: field.nonempty,
                    empty: field.empty,
                })
                .collect(),
            media: MediaDto::from(&result.media),
            group_by: result.group_by.as_ref().map(|group| GroupByDto {
                field: group.field.clone(),
                notes_with_field: group.notes_with_field,
                notes_without_field: group.notes_without_field,
                distinct_values: group.distinct_values,
                truncated: group.truncated,
                values: group
                    .buckets
                    .iter()
                    .map(|bucket| ValueCountDto {
                        value: bucket.key.clone(),
                        notes: bucket.count,
                    })
                    .collect(),
            }),
        }
    }
}

#[derive(Serialize)]
struct ValidateDto {
    valid: bool,
    summary: SummaryDto,
    issues: Vec<IssueDto>,
}

#[derive(Serialize)]
struct SummaryDto {
    errors: usize,
    warnings: usize,
    info: usize,
}

#[derive(Serialize)]
struct IssueDto {
    severity: &'static str,
    code: &'static str,
    deck_path: Option<String>,
    location: String,
    message: String,
    details: serde_json::Value,
}

impl From<&ValidateResult> for ValidateDto {
    fn from(result: &ValidateResult) -> Self {
        Self {
            valid: result.valid,
            summary: SummaryDto {
                errors: result.summary.errors,
                warnings: result.summary.warnings,
                info: result.summary.info,
            },
            issues: result
                .issues
                .iter()
                .map(|issue| IssueDto {
                    severity: issue.severity.as_str(),
                    code: issue.code,
                    deck_path: issue.deck_path.clone(),
                    location: issue.location.clone(),
                    message: issue.message.clone(),
                    details: issue.details.clone(),
                })
                .collect(),
        }
    }
}
