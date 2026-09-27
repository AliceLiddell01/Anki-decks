//! JSON renderer: стабильный machine-readable контракт schema_version 1.
//!
//! Human и JSON renderers получают один и тот же domain result. Имена
//! JSON-ключей и кодов стабильны и не зависят от Rust-имён типов.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

use crate::error::DomainError;
use crate::ops::NamedField;
use crate::ops::edit::EditResult;
use crate::ops::find::FindResult;
use crate::ops::inspect::{InspectResult, InspectVerbose, ModelSummary};
use crate::ops::qa::QaResult;
use crate::ops::review::ReviewResult;
use crate::ops::review_check::ReviewCheckResult;
use crate::ops::stats::StatsResult;
use crate::ops::validate::{SeverityCounts, ValidateResult};

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

/// JSON-представление результата `edit`.
pub fn edit_json(result: &EditResult) -> String {
    to_json("edit", EditDto::from(result))
}

#[derive(Serialize)]
struct EditDto {
    export_dir: String,
    deck_json: String,
    dry_run: bool,
    applied: bool,
    source_bytes: usize,
    candidate_bytes: usize,
    byte_delta: i64,
    changed_lines: usize,
    first_changed_line: Option<usize>,
    edits_total: usize,
    effective_edits: usize,
    summaries: EditSummariesDto,
    outcomes: Vec<EditOutcomeDto>,
    outcomes_truncated: bool,
    validation: ValidationDeltaDto,
    checks: EditChecksDto,
}

#[derive(Serialize)]
struct EditSummariesDto {
    applied: usize,
    dry_run: usize,
    noop_identical: usize,
    already_applied: usize,
}

#[derive(Serialize)]
struct EditOutcomeDto {
    edit_index: usize,
    edit_id: Option<String>,
    guid: String,
    field: String,
    field_ord: usize,
    note_index: usize,
    deck_path: String,
    status: &'static str,
    old_len: usize,
    new_len: usize,
    old_sample: String,
    new_sample: String,
}

#[derive(Serialize)]
struct ValidationDeltaDto {
    before: SeverityCountsDto,
    after: SeverityCountsDto,
    new_error_codes: Vec<String>,
    new_warning_codes: Vec<String>,
}

#[derive(Serialize)]
struct SeverityCountsDto {
    errors: usize,
    warnings: usize,
    info: usize,
}

#[derive(Serialize)]
struct EditChecksDto {
    source_canonical: bool,
    candidate_reparsed: bool,
    semantic_targets_verified: bool,
    diff_shape_is_exactly_requested: bool,
    byte_delta_matches_token_delta: bool,
}

impl From<&EditResult> for EditDto {
    fn from(result: &EditResult) -> Self {
        Self {
            export_dir: result.export_dir.display().to_string(),
            deck_json: result.deck_json.display().to_string(),
            dry_run: result.dry_run,
            applied: result.applied,
            source_bytes: result.source_bytes,
            candidate_bytes: result.candidate_bytes,
            byte_delta: result.byte_delta,
            changed_lines: result.changed_lines,
            first_changed_line: result.first_changed_line,
            edits_total: result.edits_total,
            effective_edits: result.effective_edits,
            summaries: EditSummariesDto {
                applied: result.summaries.applied,
                dry_run: result.summaries.dry_run,
                noop_identical: result.summaries.noop_identical,
                already_applied: result.summaries.already_applied,
            },
            outcomes: result
                .outcomes
                .iter()
                .map(|outcome| EditOutcomeDto {
                    edit_index: outcome.edit_index,
                    edit_id: outcome.edit_id.clone(),
                    guid: outcome.guid.clone(),
                    field: outcome.field.clone(),
                    field_ord: outcome.field_ord,
                    note_index: outcome.note_index,
                    deck_path: outcome.deck_path.clone(),
                    status: outcome.status.as_str(),
                    old_len: outcome.old_len,
                    new_len: outcome.new_len,
                    old_sample: outcome.old_sample.clone(),
                    new_sample: outcome.new_sample.clone(),
                })
                .collect(),
            outcomes_truncated: result.outcomes_truncated,
            validation: ValidationDeltaDto {
                before: SeverityCountsDto::from(result.validation.before),
                after: SeverityCountsDto::from(result.validation.after),
                new_error_codes: result.validation.new_error_codes.clone(),
                new_warning_codes: result.validation.new_warning_codes.clone(),
            },
            checks: EditChecksDto {
                source_canonical: result.checks.source_canonical,
                candidate_reparsed: result.checks.candidate_reparsed,
                semantic_targets_verified: result.checks.semantic_targets_verified,
                diff_shape_is_exactly_requested: result.checks.diff_shape_is_exactly_requested,
                byte_delta_matches_token_delta: result.checks.byte_delta_matches_token_delta,
            },
        }
    }
}

impl From<SeverityCounts> for SeverityCountsDto {
    fn from(counts: SeverityCounts) -> Self {
        Self {
            errors: counts.errors,
            warnings: counts.warnings,
            info: counts.info,
        }
    }
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

/// JSON-представление результата `qa`.
pub fn qa_json(result: &QaResult) -> String {
    to_json("qa", QaDto::from(result))
}

/// JSON-представление результата `review`.
pub fn review_json(result: &ReviewResult) -> String {
    to_json("review", ReviewDto::from(result))
}

/// JSON-представление результата `review-check`.
pub fn review_check_json(result: &ReviewCheckResult) -> String {
    to_json("review-check", ReviewCheckDto::from(result))
}

#[derive(Serialize)]
struct QaDto<'a> {
    export_dir: String,
    notes_total: usize,
    findings_total: usize,
    findings_returned: usize,
    truncated: bool,
    max_per_code: usize,
    codes: Vec<String>,
    by_code: Vec<CodeCountDto>,
    rules: Vec<RuleDto>,
    unaddressable_findings: usize,
    findings: Vec<QaFindingDto<'a>>,
}

#[derive(Serialize)]
struct CodeCountDto {
    code: &'static str,
    severity: &'static str,
    count: usize,
}

#[derive(Serialize)]
struct RuleDto {
    code: &'static str,
    severity: &'static str,
    description: &'static str,
    applicable: bool,
    findings: usize,
}

#[derive(Serialize)]
struct QaFindingDto<'a> {
    code: &'static str,
    severity: &'static str,
    note_index: usize,
    guid: Option<String>,
    deck_path: String,
    note_model_name: Option<String>,
    note_model_uuid: Option<String>,
    field: Option<String>,
    field_ord: Option<i64>,
    message: String,
    evidence: &'a serde_json::Value,
    addressable: bool,
    related_guids: Vec<String>,
    related_note_indices: Vec<usize>,
    related_truncated: bool,
    group_size: Option<usize>,
}

impl<'a> From<&'a QaResult> for QaDto<'a> {
    fn from(result: &'a QaResult) -> Self {
        Self {
            export_dir: result.export_dir.clone(),
            notes_total: result.notes_total,
            findings_total: result.findings_total,
            findings_returned: result.findings_returned,
            truncated: result.truncated,
            max_per_code: result.max_per_code,
            codes: result.codes.clone(),
            by_code: result
                .by_code
                .iter()
                .map(|entry| CodeCountDto {
                    code: entry.code,
                    severity: entry.severity.as_str(),
                    count: entry.count,
                })
                .collect(),
            rules: result
                .rules
                .iter()
                .map(|rule| RuleDto {
                    code: rule.code,
                    severity: rule.severity.as_str(),
                    description: rule.description,
                    applicable: rule.applicable,
                    findings: rule.findings,
                })
                .collect(),
            unaddressable_findings: result.unaddressable_findings,
            findings: result
                .findings
                .iter()
                .map(|finding| QaFindingDto {
                    code: finding.code,
                    severity: finding.severity.as_str(),
                    note_index: finding.note_index,
                    guid: finding.guid.clone(),
                    deck_path: finding.deck_path.clone(),
                    note_model_name: finding.note_model.clone(),
                    note_model_uuid: finding.note_model_uuid.clone(),
                    field: finding.field.clone(),
                    field_ord: finding.field_ord,
                    message: finding.message.clone(),
                    evidence: &finding.evidence,
                    addressable: finding.addressable,
                    related_guids: finding.related_guids.clone(),
                    related_note_indices: finding.related_note_indices.clone(),
                    related_truncated: finding.related_truncated,
                    group_size: finding.group_size,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct ReviewDto<'a> {
    export_dir: String,
    selection: ReviewSelectionDto,
    notes_total: usize,
    total_selected: usize,
    offset: usize,
    limit: usize,
    returned: usize,
    truncated: bool,
    next_offset: Option<usize>,
    excluded_unaddressable: usize,
    items: Vec<ReviewItemDto<'a>>,
}

#[derive(Serialize)]
struct ReviewSelectionDto {
    kind: &'static str,
    guid: Option<String>,
    field: Option<String>,
    value: Option<String>,
    match_mode: Option<&'static str>,
    qa_code: Option<String>,
    deck: Option<String>,
}

#[derive(Serialize)]
struct ReviewItemDto<'a> {
    note_index: usize,
    guid: Option<String>,
    deck_path: String,
    note_model_name: Option<String>,
    note_model_uuid: Option<String>,
    tags: Vec<String>,
    fields: OrderedFields<'a>,
    qa_findings: Vec<FindingSummaryDto>,
    qa_findings_truncated: bool,
    group_membership: Vec<GroupMembershipDto>,
}

#[derive(Serialize)]
struct FindingSummaryDto {
    code: &'static str,
    severity: &'static str,
    field: Option<String>,
    message: String,
    group_size: Option<usize>,
    related: Vec<RelatedNoteDto>,
    related_truncated: bool,
}

#[derive(Serialize)]
struct RelatedNoteDto {
    note_index: usize,
    guid: Option<String>,
}

#[derive(Serialize)]
struct GroupMembershipDto {
    code: &'static str,
    owner_note_index: usize,
    owner_guid: Option<String>,
    group_size: usize,
}

impl<'a> From<&'a ReviewResult> for ReviewDto<'a> {
    fn from(result: &'a ReviewResult) -> Self {
        Self {
            export_dir: result.export_dir.clone(),
            selection: ReviewSelectionDto {
                kind: result.selection.kind,
                guid: result.selection.guid.clone(),
                field: result.selection.field.clone(),
                value: result.selection.value.clone(),
                match_mode: result.selection.match_mode,
                qa_code: result.selection.qa_code.clone(),
                deck: result.selection.deck.clone(),
            },
            notes_total: result.notes_total,
            total_selected: result.total_selected,
            offset: result.offset,
            limit: result.limit,
            returned: result.returned,
            truncated: result.truncated,
            next_offset: result.next_offset,
            excluded_unaddressable: result.excluded_unaddressable,
            items: result
                .items
                .iter()
                .map(|item| ReviewItemDto {
                    note_index: item.note_index,
                    guid: item.note.guid.clone(),
                    deck_path: item.note.deck_path.clone(),
                    note_model_name: item.note.note_model_name.clone(),
                    note_model_uuid: item.note.note_model_uuid.clone(),
                    tags: item.note.tags.clone(),
                    fields: OrderedFields(&item.note.fields),
                    qa_findings: item
                        .qa_findings
                        .iter()
                        .map(|finding| FindingSummaryDto {
                            code: finding.code,
                            severity: finding.severity.as_str(),
                            field: finding.field.clone(),
                            message: finding.message.clone(),
                            group_size: finding.group_size,
                            related: finding
                                .related
                                .iter()
                                .map(|related| RelatedNoteDto {
                                    note_index: related.note_index,
                                    guid: related.guid.clone(),
                                })
                                .collect(),
                            related_truncated: finding.related_truncated,
                        })
                        .collect(),
                    qa_findings_truncated: item.qa_findings_truncated,
                    group_membership: item
                        .group_membership
                        .iter()
                        .map(|membership| GroupMembershipDto {
                            code: membership.code,
                            owner_note_index: membership.owner_note_index,
                            owner_guid: membership.owner_guid.clone(),
                            group_size: membership.group_size,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct ReviewCheckDto<'a> {
    export_dir: String,
    deck_json: String,
    outcome: &'static str,
    proposals_total: usize,
    counts: CheckCountsDto,
    effective_proposals: usize,
    proposals_truncated: bool,
    source_editable: bool,
    source_blockers: Vec<SourceBlockerDto>,
    proposals: Vec<CheckedProposalDto<'a>>,
    edit_request: Option<EditRequestDto>,
}

/// Причина, по которой текущий исходник нельзя править.
#[derive(Serialize)]
struct SourceBlockerDto {
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct CheckCountsDto {
    valid: usize,
    already_correct: usize,
    already_applied: usize,
    conflict: usize,
    invalid: usize,
}

#[derive(Serialize)]
struct CheckedProposalDto<'a> {
    proposal_index: usize,
    proposal_id: Option<String>,
    guid: String,
    field: String,
    status: &'static str,
    note_index: Option<usize>,
    deck_path: Option<String>,
    field_ord: Option<usize>,
    current_len: Option<usize>,
    expected_len: usize,
    replacement_len: usize,
    current_sample: Option<String>,
    expected_sample: &'a str,
    replacement_sample: &'a str,
    reason: Option<String>,
    problem: Option<&'static str>,
    message: Option<String>,
}

#[derive(Serialize)]
struct EditRequestDto {
    schema_version: u32,
    edits: Vec<EditSpecDto>,
}

#[derive(Serialize)]
struct EditSpecDto {
    edit_id: Option<String>,
    guid: String,
    field: String,
    expected: String,
    replacement: String,
}

impl<'a> From<&'a ReviewCheckResult> for ReviewCheckDto<'a> {
    fn from(result: &'a ReviewCheckResult) -> Self {
        Self {
            export_dir: result.export_dir.clone(),
            deck_json: result.deck_json.clone(),
            outcome: result.outcome.as_str(),
            proposals_total: result.proposals_total,
            counts: CheckCountsDto {
                valid: result.counts.valid,
                already_correct: result.counts.already_correct,
                already_applied: result.counts.already_applied,
                conflict: result.counts.conflict,
                invalid: result.counts.invalid,
            },
            effective_proposals: result.effective_proposals,
            proposals_truncated: result.proposals_truncated,
            source_editable: result.source_editable,
            source_blockers: result
                .source_blockers
                .iter()
                .map(|blocker| SourceBlockerDto {
                    code: blocker.code,
                    message: blocker.message.clone(),
                })
                .collect(),
            proposals: result
                .proposals
                .iter()
                .map(|proposal| CheckedProposalDto {
                    proposal_index: proposal.proposal_index,
                    proposal_id: proposal.proposal_id.clone(),
                    guid: proposal.guid.clone(),
                    field: proposal.field.clone(),
                    status: proposal.status.as_str(),
                    note_index: proposal.note_index,
                    deck_path: proposal.deck_path.clone(),
                    field_ord: proposal.field_ord,
                    current_len: proposal.current_len,
                    expected_len: proposal.expected_len,
                    replacement_len: proposal.replacement_len,
                    current_sample: proposal.current_sample.clone(),
                    expected_sample: proposal.expected_sample.as_str(),
                    replacement_sample: proposal.replacement_sample.as_str(),
                    reason: proposal.reason.clone(),
                    problem: proposal.problem,
                    message: proposal.message.clone(),
                })
                .collect(),
            edit_request: result.edit_request.as_ref().map(|request| EditRequestDto {
                schema_version: crate::ops::edit::SUPPORTED_REQUEST_SCHEMA_VERSION,
                edits: request
                    .edits
                    .iter()
                    .map(|edit| EditSpecDto {
                        edit_id: edit.edit_id.clone(),
                        guid: edit.guid.clone(),
                        field: edit.field.clone(),
                        expected: edit.expected.clone(),
                        replacement: edit.replacement.clone(),
                    })
                    .collect(),
            }),
        }
    }
}
