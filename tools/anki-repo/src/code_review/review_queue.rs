//! Детерминированная структурная очередь поверх полного raw evidence.
//!
//! Классификация, приоритет и группа задают маршрутизацию внешнего ревью.
//! Они не утверждают семантический результат или покрытие semantic review.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{DomainError, ErrorCode};

use super::language::TextContext;
use super::model::{CandidateEvidence, CandidateOrigin, ReviewFile, ReviewPack};
use super::scope::{FileCategory, FileSurface};
use super::semantic_triage::{self, TriageSource};

/// Версия самостоятельного контракта структурной очереди.
pub const QUEUE_SCHEMA_VERSION: u32 = 1;
/// Представители ограничены тремя разными местами; остальные IDs раскрываются отдельно.
pub const REPRESENTATIVE_LIMIT: usize = 3;

macro_rules! named_enum {
    ($name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }
        }
    };
}

named_enum!(ClassificationBasis {
    SyntaxContext => "syntax_context",
    FileContext => "file_context",
    FormatContext => "format_context",
    Detector => "detector",
    LiteralShape => "literal_shape",
    ParseFailure => "parse_failure",
    Unknown => "unknown",
});

named_enum!(StructuralRole {
    Text => "text",
    ErrorPath => "error_path",
    Security => "security",
    Suppression => "suppression",
    TestChange => "test_change",
    Dependency => "dependency",
    Configuration => "configuration",
    Generated => "generated",
    RepositoryContext => "repository_context",
    Path => "path",
    DevelopmentReference => "development_reference",
    Unknown => "unknown",
});

named_enum!(TextRole {
    HumanComment => "human_comment",
    HumanDocumentation => "human_documentation",
    HumanLog => "human_log",
    HumanDiagnostic => "human_diagnostic",
    HumanHelp => "human_help",
    HumanUi => "human_ui",
    TechnicalIdentifier => "technical_identifier",
    MachineContract => "machine_contract",
    ExternalLiteral => "external_literal",
    Path => "path",
    Url => "url",
    CliFlag => "cli_flag",
    CodeExample => "code_example",
    TestFixture => "test_fixture",
    Unknown => "unknown",
});

impl TextRole {
    #[must_use]
    pub const fn is_human(self) -> bool {
        matches!(
            self,
            Self::HumanComment
                | Self::HumanDocumentation
                | Self::HumanLog
                | Self::HumanDiagnostic
                | Self::HumanHelp
                | Self::HumanUi
        )
    }

    const fn is_machine(self) -> bool {
        matches!(
            self,
            Self::TechnicalIdentifier
                | Self::MachineContract
                | Self::ExternalLiteral
                | Self::Path
                | Self::Url
                | Self::CliFlag
                | Self::TestFixture
        )
    }
}

named_enum!(CodeRole {
    Runtime => "runtime",
    RuntimeBoundary => "runtime_boundary",
    TestSetup => "test_setup",
    TestAssertion => "test_assertion",
    TestHelper => "test_helper",
    Unknown => "unknown",
});

impl CodeRole {
    #[must_use]
    pub const fn is_test(self) -> bool {
        matches!(
            self,
            Self::TestSetup | Self::TestAssertion | Self::TestHelper
        )
    }
}

named_enum!(ReviewPriority {
    High => "high",
    Normal => "normal",
    Low => "low",
});

/// Адаптер доказанного синтаксического контекста; core не содержит парсер Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxContext {
    /// Исполняемая поверхность: только Production/Tests; None сохраняет unknown.
    pub execution: Option<FileSurface>,
    pub code_role: CodeRole,
    pub text_role: Option<TextRole>,
    /// Сигнатура ближайшего call/macro и роли аргумента, без локального имени функции.
    pub signature: Option<String>,
    pub basis: ClassificationBasis,
}

/// Независимые структурные измерения одного исходного candidate ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralClassification {
    /// Все релевантные поверхности; пустой список означает unknown.
    pub surfaces: Vec<FileSurface>,
    pub surface_basis: ClassificationBasis,
    pub file_category: Option<FileCategory>,
    /// Синтаксический контекст исполнения независим от файловых поверхностей.
    pub execution: Option<FileSurface>,
    pub execution_basis: ClassificationBasis,
    pub origin: CandidateOrigin,
    pub role: StructuralRole,
    pub role_basis: ClassificationBasis,
    pub text_role: Option<TextRole>,
    pub text_basis: ClassificationBasis,
    pub code_role: CodeRole,
    pub code_basis: ClassificationBasis,
    /// Доказанный call context, используемый для ограничения группировки.
    pub syntax_signature: Option<String>,
}

impl StructuralClassification {
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self.surfaces.is_empty()
            || matches!(
                self.surface_basis,
                ClassificationBasis::Unknown | ClassificationBasis::ParseFailure
            )
            || self.origin == CandidateOrigin::Unknown
            || self.execution_basis == ClassificationBasis::ParseFailure
            || self.code_basis == ClassificationBasis::ParseFailure
            || (self.file_category == Some(FileCategory::Rust) && self.execution.is_none())
            || self.role == StructuralRole::Unknown
            || self.text_role == Some(TextRole::Unknown)
            || (self.role == StructuralRole::ErrorPath && self.code_role == CodeRole::Unknown)
    }
}

/// Все существенные измерения однородности; сами по себе не semantic decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupingSignature {
    pub detector: String,
    pub source: String,
    pub classification: StructuralClassification,
    pub detector_signals: Vec<String>,
    pub text_context: Option<TextContext>,
    /// Родительский каталог ограничивает смешивание разных компонент.
    pub path_family: String,
    /// Для test code это call signature; для machine text — точное значение.
    pub structural_pattern: Option<String>,
}

/// Тип unit явно определяет гранулярность навигации, не результат рассмотрения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnitMembers {
    Individual {
        candidate_id: String,
    },
    Group {
        candidate_ids: Vec<String>,
        representative_candidate_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewUnit {
    pub id: String,
    pub members: UnitMembers,
    pub signature: GroupingSignature,
    pub priority: ReviewPriority,
    pub priority_signals: Vec<String>,
    pub statistics: UnitStatistics,
}

impl ReviewUnit {
    #[must_use]
    pub fn candidate_ids(&self) -> &[String] {
        match &self.members {
            UnitMembers::Individual { candidate_id } => std::slice::from_ref(candidate_id),
            UnitMembers::Group { candidate_ids, .. } => candidate_ids,
        }
    }

    #[must_use]
    pub fn representative_candidate_ids(&self) -> &[String] {
        match &self.members {
            UnitMembers::Individual { .. } => &[],
            UnitMembers::Group {
                representative_candidate_ids,
                ..
            } => representative_candidate_ids,
        }
    }

    #[must_use]
    pub const fn is_group(&self) -> bool {
        matches!(&self.members, UnitMembers::Group { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitStatistics {
    pub candidates: usize,
    pub paths: BTreeMap<String, usize>,
}

/// Workload summary считает evidence и units, не precision или semantic coverage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueSummary {
    pub raw_candidates: usize,
    pub review_units: usize,
    pub individual_units: usize,
    pub group_units: usize,
    pub grouped_candidates: usize,
    pub representative_candidates: usize,
    pub unknown_candidates: usize,
    /// Число units каждого приоритета.
    pub units_by_priority: BTreeMap<ReviewPriority, usize>,
    /// Остальные распределения считают исходные candidates.
    pub by_detector: BTreeMap<String, usize>,
    pub by_surface: BTreeMap<String, usize>,
    pub by_execution: BTreeMap<String, usize>,
    pub by_structural_role: BTreeMap<StructuralRole, usize>,
    pub by_text_role: BTreeMap<TextRole, usize>,
    pub by_code_role: BTreeMap<CodeRole, usize>,
    /// До десяти размеров самых больших групп, по убыванию.
    pub largest_group_sizes: Vec<usize>,
}

/// Самостоятельный versioned artifact; исходный pack остаётся полным evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueue {
    pub schema_version: u32,
    pub source: TriageSource,
    pub classifications: BTreeMap<String, StructuralClassification>,
    pub units: Vec<ReviewUnit>,
    pub summary: QueueSummary,
}

fn invalid(message: impl Into<String>) -> DomainError {
    DomainError::new(ErrorCode::ReviewArtifactInvalid, message)
}

fn text_context(candidate: &CandidateEvidence) -> Option<TextContext> {
    candidate
        .metadata
        .get("context")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn structural_role(detector: &str) -> StructuralRole {
    match detector {
        "residual_foreign_human_text" => StructuralRole::Text,
        "error_path" => StructuralRole::ErrorPath,
        "security_surface" | "unsafe_path" | "local_endpoint" => StructuralRole::Security,
        "rust_suppression" | "ci_suppression" => StructuralRole::Suppression,
        "test_added" | "test_removed" | "test_ignored" | "assertion_removed" => {
            StructuralRole::TestChange
        }
        "dependency_change" => StructuralRole::Dependency,
        "config_surface" => StructuralRole::Configuration,
        "generated_surface" => StructuralRole::Generated,
        "skill_surface" => StructuralRole::RepositoryContext,
        "absolute_path" => StructuralRole::Path,
        "development_reference" => StructuralRole::DevelopmentReference,
        _ => StructuralRole::Unknown,
    }
}

fn literal_shape(text: &str) -> TextRole {
    // Эти формы доказывают только форму literal. Английская лексика не участвует.
    let text = text.trim();
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return TextRole::Unknown;
    }
    if text.contains("://")
        && text.split("://").next().is_some_and(|scheme| {
            !scheme.is_empty()
                && scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
        })
    {
        return TextRole::Url;
    }
    if text.starts_with("--")
        && text.len() > 2
        && text[2..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return TextRole::CliFlag;
    }
    if text.starts_with('/')
        || text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with("~/")
        || text.as_bytes().get(1) == Some(&b':')
            && text
                .as_bytes()
                .get(2)
                .is_some_and(|b| matches!(b, b'/' | b'\\'))
        || text.rsplit_once('.').is_some_and(|(name, extension)| {
            !name.is_empty()
                && !extension.is_empty()
                && extension.len() <= 8
                && extension.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    {
        return TextRole::Path;
    }
    if text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
        && text.bytes().any(|b| matches!(b, b'_' | b'-' | b':'))
    {
        return TextRole::TechnicalIdentifier;
    }
    TextRole::Unknown
}

fn classify_text(
    candidate: &CandidateEvidence,
    syntax: Option<&SyntaxContext>,
) -> (Option<TextRole>, ClassificationBasis) {
    if candidate.detector != "residual_foreign_human_text" {
        return (None, ClassificationBasis::Unknown);
    }
    let context = text_context(candidate);
    match context {
        Some(TextContext::Comment) => {
            return (
                Some(TextRole::HumanComment),
                ClassificationBasis::FormatContext,
            );
        }
        Some(TextContext::DocComment | TextContext::MarkdownProse) => {
            return (
                Some(TextRole::HumanDocumentation),
                ClassificationBasis::FormatContext,
            );
        }
        _ => {}
    }
    let shape = literal_shape(candidate.snippet.as_deref().unwrap_or_default());
    if matches!(shape, TextRole::Url | TextRole::Path | TextRole::CliFlag) {
        return (Some(shape), ClassificationBasis::LiteralShape);
    }
    if matches!(context, Some(TextContext::ScriptOutput)) {
        return (Some(TextRole::HumanLog), ClassificationBasis::FormatContext);
    }
    if let Some(syntax) = syntax
        && let Some(role) = syntax.text_role.filter(|role| role.is_machine())
    {
        return (Some(role), syntax.basis);
    }
    if let Some(syntax) = syntax
        && let Some(role) = syntax.text_role.filter(|role| role.is_human())
    {
        return (Some(role), syntax.basis);
    }
    if shape != TextRole::Unknown {
        return (Some(shape), ClassificationBasis::LiteralShape);
    }
    (Some(TextRole::Unknown), ClassificationBasis::Unknown)
}

/// Классифицирует candidate, сохраняя неизвестные измерения явно.
#[must_use]
pub fn classify(
    pack: &ReviewPack,
    candidate: &CandidateEvidence,
    syntax: Option<&SyntaxContext>,
) -> StructuralClassification {
    let files_by_candidate_path = pack.scope_files_by_candidate_path();
    classify_indexed(candidate, &files_by_candidate_path, syntax)
}

fn classify_indexed(
    candidate: &CandidateEvidence,
    files_by_candidate_path: &BTreeMap<&str, &ReviewFile>,
    syntax: Option<&SyntaxContext>,
) -> StructuralClassification {
    let file = files_by_candidate_path
        .get(candidate.path.as_str())
        .copied();
    let (file_category, mut surfaces) = file.map_or_else(
        || (None, Vec::new()),
        |file| {
            if file.path == candidate.path {
                (Some(file.category.clone()), file.surfaces.clone())
            } else {
                let (category, surfaces) = super::scope::classify_path(&candidate.path);
                (Some(category), surfaces)
            }
        },
    );
    let surface_basis = if surfaces.is_empty() {
        ClassificationBasis::Unknown
    } else {
        ClassificationBasis::FileContext
    };
    let (execution, execution_basis, code_role, code_basis, syntax_signature) =
        if let Some(syntax) = syntax {
            let execution = syntax
                .execution
                .as_ref()
                .filter(|surface| matches!(surface, FileSurface::Production | FileSurface::Tests))
                .cloned();
            (
                execution,
                syntax.basis,
                syntax.code_role,
                syntax.basis,
                syntax.signature.clone(),
            )
        } else {
            (
                None,
                ClassificationBasis::Unknown,
                CodeRole::Unknown,
                ClassificationBasis::Unknown,
                None,
            )
        };
    surfaces.sort();
    surfaces.dedup();
    let role = structural_role(&candidate.detector);
    let (text_role, text_basis) = classify_text(candidate, syntax);
    StructuralClassification {
        surfaces,
        surface_basis,
        file_category,
        execution,
        execution_basis,
        origin: candidate.origin,
        role,
        role_basis: if role == StructuralRole::Unknown {
            ClassificationBasis::Unknown
        } else {
            ClassificationBasis::Detector
        },
        text_role,
        text_basis,
        code_role,
        code_basis,
        syntax_signature,
    }
}

fn priority(classification: &StructuralClassification) -> (ReviewPriority, Vec<String>) {
    let mut reasons = Vec::new();
    if matches!(
        classification.role,
        StructuralRole::Security | StructuralRole::Suppression
    ) {
        reasons.push("security_or_suppression_surface".to_owned());
    }
    if classification.code_role == CodeRole::RuntimeBoundary {
        reasons.push("runtime_boundary".to_owned());
    }
    if classification.origin == CandidateOrigin::IntroducedOrChanged
        && classification.execution == Some(FileSurface::Production)
        && classification.role == StructuralRole::ErrorPath
    {
        reasons.push("changed_runtime_error_path".to_owned());
    }
    if classification.origin == CandidateOrigin::IntroducedOrChanged
        && classification.text_role.is_some_and(TextRole::is_human)
    {
        reasons.push("changed_human_text".to_owned());
    }
    if !reasons.is_empty() {
        return (ReviewPriority::High, reasons);
    }
    if classification.is_unknown() {
        return (
            ReviewPriority::Normal,
            vec!["unknown_structural_context".to_owned()],
        );
    }
    if classification.code_role.is_test() || classification.text_role == Some(TextRole::TestFixture)
    {
        reasons.push("proven_test_or_fixture_context".to_owned());
        if classification.origin == CandidateOrigin::PreExisting {
            reasons.push("pre_existing".to_owned());
        }
        return (ReviewPriority::Low, reasons);
    }
    (ReviewPriority::Normal, vec!["structural_review".to_owned()])
}

fn path_family(path: &str) -> String {
    path.rsplit_once('/')
        .map_or(".", |(parent, _)| parent)
        .to_owned()
}

fn signature(
    candidate: &CandidateEvidence,
    classification: &StructuralClassification,
) -> GroupingSignature {
    let mut signals = candidate.signals.clone();
    signals.sort();
    signals.dedup();
    let structural_pattern = if classification.role == StructuralRole::Text {
        candidate.snippet.clone()
    } else {
        classification.syntax_signature.clone()
    };
    GroupingSignature {
        detector: candidate.detector.clone(),
        source: candidate.source.clone(),
        classification: classification.clone(),
        detector_signals: signals,
        text_context: text_context(candidate),
        path_family: path_family(&candidate.path),
        structural_pattern,
    }
}

fn can_group(signature: &GroupingSignature) -> bool {
    let class = &signature.classification;
    if class.is_unknown()
        || class.code_role == CodeRole::RuntimeBoundary
        || signature
            .structural_pattern
            .as_ref()
            .is_none_or(|value| value.is_empty())
    {
        return false;
    }
    match class.role {
        StructuralRole::ErrorPath => {
            class.code_role.is_test()
                && class.code_basis == ClassificationBasis::SyntaxContext
                && class.execution == Some(FileSurface::Tests)
                && !signature.detector_signals.is_empty()
        }
        StructuralRole::Text => {
            class.text_role.is_some_and(TextRole::is_machine)
                && !matches!(
                    signature.text_context,
                    Some(
                        TextContext::Comment
                            | TextContext::DocComment
                            | TextContext::MarkdownProse
                            | TextContext::ScriptOutput
                    )
                )
        }
        _ => false,
    }
}

fn serialized_signature(signature: &GroupingSignature) -> Result<String, DomainError> {
    serde_json::to_string(signature).map_err(|error| {
        invalid(format!(
            "Не удалось сериализовать structural signature: {error}"
        ))
    })
}

fn group_id(signature: &GroupingSignature) -> Result<String, DomainError> {
    Ok(format!(
        "group-{:x}",
        Sha256::digest(serialized_signature(signature)?.as_bytes())
    ))
}

fn representatives(candidates: &[&CandidateEvidence]) -> Vec<String> {
    let mut ordered = candidates.to_vec();
    ordered.sort_by(|a, b| {
        (&a.path, a.line, a.column, &a.id).cmp(&(&b.path, b.line, b.column, &b.id))
    });
    let mut ids = Vec::new();
    let mut paths = BTreeSet::new();
    // Сначала разные пути; внутри каждого пути одинаковая signature уже доказана.
    for candidate in &ordered {
        if paths.insert(&candidate.path) {
            ids.push(candidate.id.clone());
            if ids.len() == REPRESENTATIVE_LIMIT {
                break;
            }
        }
    }
    for candidate in ordered.iter().rev() {
        if ids.len() == REPRESENTATIVE_LIMIT {
            break;
        }
        if !ids.contains(&candidate.id) {
            ids.push(candidate.id.clone());
        }
    }
    ids.sort();
    ids
}

fn statistics(candidates: &[&CandidateEvidence]) -> UnitStatistics {
    let mut paths = BTreeMap::new();
    for candidate in candidates {
        *paths.entry(candidate.path.clone()).or_default() += 1;
    }
    UnitStatistics {
        candidates: candidates.len(),
        paths,
    }
}

fn validate_classification(
    class: &StructuralClassification,
    files_by_candidate_path: &BTreeMap<&str, &ReviewFile>,
    candidate: &CandidateEvidence,
) -> Result<(), DomainError> {
    let fallback = classify_indexed(candidate, files_by_candidate_path, None);
    if class.surfaces != fallback.surfaces
        || class.surface_basis != fallback.surface_basis
        || class.file_category != fallback.file_category
        || class.origin != fallback.origin
        || class.role != fallback.role
        || class.role_basis != fallback.role_basis
        || (class.text_basis != ClassificationBasis::SyntaxContext
            && (class.text_role != fallback.text_role || class.text_basis != fallback.text_basis))
        || (class.role != StructuralRole::Text && class.text_role.is_some())
        || class
            .execution
            .as_ref()
            .is_some_and(|surface| !matches!(surface, FileSurface::Production | FileSurface::Tests))
        || (class.execution.is_some()
            && class.execution_basis != ClassificationBasis::SyntaxContext)
        || (class.code_role != CodeRole::Unknown
            && class.code_basis != ClassificationBasis::SyntaxContext)
        || (class.code_role.is_test() && class.execution != Some(FileSurface::Tests))
        || (matches!(
            class.code_role,
            CodeRole::Runtime | CodeRole::RuntimeBoundary
        ) && class.execution != Some(FileSurface::Production))
        || class
            .syntax_signature
            .as_ref()
            .is_some_and(|value| value.is_empty())
    {
        return Err(invalid(format!(
            "Structural classification противоречит исходному evidence: {}",
            candidate.id
        )));
    }
    Ok(())
}

fn make_unit(
    signature: GroupingSignature,
    candidates: &[&CandidateEvidence],
) -> Result<ReviewUnit, DomainError> {
    let (priority, priority_signals) = priority(&signature.classification);
    let (id, members) = if candidates.len() == 1 {
        (
            format!("individual-{}", candidates[0].id),
            UnitMembers::Individual {
                candidate_id: candidates[0].id.clone(),
            },
        )
    } else {
        let mut ids: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect();
        ids.sort();
        (
            group_id(&signature)?,
            UnitMembers::Group {
                candidate_ids: ids,
                representative_candidate_ids: representatives(candidates),
            },
        )
    };
    Ok(ReviewUnit {
        id,
        members,
        signature,
        priority,
        priority_signals,
        statistics: statistics(candidates),
    })
}

/// Строит queue без изменения raw pack и без семантических решений.
pub fn build(
    pack: &ReviewPack,
    source_pack_sha256: &str,
    contexts: &BTreeMap<String, SyntaxContext>,
) -> Result<ReviewQueue, DomainError> {
    let source = semantic_triage::source_identity(pack, source_pack_sha256);
    semantic_triage::validate_source(&source, pack, source_pack_sha256)?;
    let candidates = pack.all_candidates();
    let candidate_ids: BTreeSet<_> = candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    if contexts
        .keys()
        .any(|id| !candidate_ids.contains(id.as_str()))
    {
        return Err(invalid("Syntax context содержит неизвестный candidate ID"));
    }
    let files_by_candidate_path = pack.scope_files_by_candidate_path();
    let mut classifications = BTreeMap::new();
    let mut grouped: BTreeMap<String, (GroupingSignature, Vec<&CandidateEvidence>)> =
        BTreeMap::new();
    let mut units = Vec::new();
    for candidate in &candidates {
        let classification = classify_indexed(
            candidate,
            &files_by_candidate_path,
            contexts.get(&candidate.id),
        );
        let signature = signature(candidate, &classification);
        classifications.insert(candidate.id.clone(), classification);
        if can_group(&signature) {
            grouped
                .entry(serialized_signature(&signature)?)
                .or_insert_with(|| (signature, Vec::new()))
                .1
                .push(candidate);
        } else {
            units.push(make_unit(signature, &[candidate])?);
        }
    }
    for (signature, members) in grouped.into_values() {
        units.push(make_unit(signature, &members)?);
    }
    units.sort_by(|a, b| (a.priority, &a.id).cmp(&(b.priority, &b.id)));
    let mut queue = ReviewQueue {
        schema_version: QUEUE_SCHEMA_VERSION,
        source,
        classifications,
        units,
        summary: QueueSummary::default(),
    };
    queue.summary = summarize(&queue);
    validate(&queue, pack, source_pack_sha256)?;
    Ok(queue)
}

pub(crate) fn surface_name(surface: &FileSurface) -> &'static str {
    match surface {
        FileSurface::Production => "production",
        FileSurface::Tests => "tests",
        FileSurface::Ci => "ci",
        FileSurface::Config => "config",
        FileSurface::Docs => "docs",
        FileSurface::Generated => "generated",
        FileSurface::Data => "data",
        FileSurface::AgentContext => "agent_context",
        FileSurface::Dependencies => "dependencies",
    }
}

fn summarize(queue: &ReviewQueue) -> QueueSummary {
    let mut summary = QueueSummary {
        raw_candidates: queue.classifications.len(),
        review_units: queue.units.len(),
        ..QueueSummary::default()
    };
    for class in queue.classifications.values() {
        summary.unknown_candidates += usize::from(class.is_unknown());
        *summary.by_structural_role.entry(class.role).or_default() += 1;
        *summary.by_code_role.entry(class.code_role).or_default() += 1;
        let execution = class.execution.as_ref().map_or("unknown", surface_name);
        *summary.by_execution.entry(execution.into()).or_default() += 1;
        if let Some(role) = class.text_role {
            *summary.by_text_role.entry(role).or_default() += 1;
        }
        if class.surfaces.is_empty() {
            *summary.by_surface.entry("unknown".into()).or_default() += 1;
        } else {
            for surface in &class.surfaces {
                *summary
                    .by_surface
                    .entry(surface_name(surface).into())
                    .or_default() += 1;
            }
            if class.file_category == Some(FileCategory::Rust) && class.execution.is_none() {
                *summary.by_surface.entry("unknown".into()).or_default() += 1;
            }
        }
    }
    for unit in &queue.units {
        *summary.units_by_priority.entry(unit.priority).or_default() += 1;
        *summary
            .by_detector
            .entry(unit.signature.detector.clone())
            .or_default() += unit.candidate_ids().len();
        summary.representative_candidates += unit.representative_candidate_ids().len();
        if unit.is_group() {
            summary.group_units += 1;
            summary.grouped_candidates += unit.candidate_ids().len();
            summary.largest_group_sizes.push(unit.candidate_ids().len());
        } else {
            summary.individual_units += 1;
        }
    }
    summary.largest_group_sizes.sort_by(|a, b| b.cmp(a));
    summary.largest_group_sizes.truncate(10);
    summary
}

/// Проверяет точную source boundary, membership, homogeneous signatures и summary.
pub fn validate(
    queue: &ReviewQueue,
    pack: &ReviewPack,
    source_pack_sha256: &str,
) -> Result<QueueSummary, DomainError> {
    if queue.schema_version != QUEUE_SCHEMA_VERSION {
        return Err(invalid(
            "Неподдерживаемая schema_version структурной очереди",
        ));
    }
    semantic_triage::validate_source(&queue.source, pack, source_pack_sha256)?;
    let candidates = pack.all_candidates();
    let files_by_candidate_path = pack.scope_files_by_candidate_path();
    let candidates: BTreeMap<_, _> = candidates
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate))
        .collect();
    if queue.classifications.len() != candidates.len()
        || queue
            .classifications
            .keys()
            .any(|id| !candidates.contains_key(id.as_str()))
    {
        return Err(invalid(
            "Classifications не покрывают точное множество raw candidate IDs",
        ));
    }
    let mut unit_ids = BTreeSet::new();
    let mut coverage = BTreeSet::new();
    for unit in &queue.units {
        if unit.id.is_empty() || !unit_ids.insert(&unit.id) {
            return Err(invalid("Пустой или повторяющийся review unit ID"));
        }
        let ids = unit.candidate_ids();
        if ids.is_empty() || ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid(
                "Candidate IDs unit должны быть непустым сортированным множеством",
            ));
        }
        let mut members = Vec::new();
        for id in ids {
            let candidate = candidates
                .get(id.as_str())
                .ok_or_else(|| invalid(format!("Неизвестный candidate ID: {id}")))?;
            if !coverage.insert(id.as_str()) {
                return Err(invalid(format!("Candidate покрыт повторно: {id}")));
            }
            let class = &queue.classifications[id];
            validate_classification(class, &files_by_candidate_path, candidate)?;
            if class.origin != candidate.origin
                || class.role != structural_role(&candidate.detector)
                || class.surfaces.windows(2).any(|pair| pair[0] >= pair[1])
                || signature(candidate, class) != unit.signature
            {
                return Err(invalid(format!(
                    "Неоднородная либо неверная structural signature: {id}"
                )));
            }
            members.push(*candidate);
        }
        let expected_id = if unit.is_group() {
            if ids.len() < 2 || !can_group(&unit.signature) {
                return Err(invalid(
                    "Группа не имеет доказанной структурной однородности",
                ));
            }
            if unit.representative_candidate_ids() != representatives(&members) {
                return Err(invalid(
                    "Представители группы не соответствуют детерминированному выбору",
                ));
            }
            group_id(&unit.signature)?
        } else {
            format!("individual-{}", ids[0])
        };
        if unit.id != expected_id
            || unit.statistics != statistics(&members)
            || (unit.priority, unit.priority_signals.clone())
                != priority(&unit.signature.classification)
        {
            return Err(invalid(
                "ID, статистика или приоритет unit не соответствуют её evidence",
            ));
        }
    }
    if coverage.len() != candidates.len() {
        return Err(invalid(
            "Каждый raw candidate должен присутствовать ровно в одной review unit",
        ));
    }
    let summary = summarize(queue);
    if summary != queue.summary {
        return Err(invalid(
            "Queue summary не соответствует classifications и units",
        ));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_review::language::{LANGUAGE_SCHEMA_VERSION, LanguageScan};
    use crate::code_review::model::{REVIEW_SCHEMA_VERSION, ReviewFile, ReviewScope};
    use crate::code_review::scope::{FileStatus, GitTarget, ImageState, LineRange};

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn candidate(id: &str, path: &str, line: usize) -> CandidateEvidence {
        CandidateEvidence {
            id: id.into(),
            detector: "error_path".into(),
            path: path.into(),
            line: Some(line),
            column: None,
            snippet: Some("result.unwrap()".into()),
            origin: CandidateOrigin::IntroducedOrChanged,
            signals: vec!["unwrap_call".into()],
            source: "synthetic_detector".into(),
            metadata: BTreeMap::new(),
        }
    }

    fn pack(candidates: Vec<CandidateEvidence>) -> ReviewPack {
        let paths: BTreeSet<_> = candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect();
        let files = paths
            .into_iter()
            .map(|path| {
                let (category, surfaces) = super::super::scope::classify_path(&path);
                ReviewFile {
                    path,
                    previous_path: None,
                    status: FileStatus::Modified,
                    additions: None,
                    deletions: None,
                    category,
                    surfaces,
                    binary: false,
                    base_state: ImageState::Text,
                    base_size: 1,
                    base_object_id: None,
                    base_changed_lines: Vec::new(),
                    post_state: ImageState::Text,
                    post_size: 1,
                    post_object_id: None,
                    post_changed_lines: vec![LineRange {
                        start: 1,
                        end: 10_000,
                    }],
                }
            })
            .collect();
        ReviewPack {
            schema_version: REVIEW_SCHEMA_VERSION,
            target: GitTarget {
                repository_id: "synthetic-repository".into(),
                base_sha: "base".into(),
                head_sha: "head".into(),
                merge_base_sha: "base".into(),
            },
            scope: ReviewScope {
                merge_base_sha: "base".into(),
                text_image_limit_bytes: 1_000_000,
                files,
            },
            diagnostics: Vec::new(),
            candidates,
            language: LanguageScan {
                schema_version: LANGUAGE_SCHEMA_VERSION,
                files: Vec::new(),
                candidates: Vec::new(),
                skipped: Vec::new(),
            },
            dependencies: Vec::new(),
            tests: Vec::new(),
            suppressions: Vec::new(),
            risk_surfaces: Vec::new(),
            tool_runs: Vec::new(),
        }
    }

    fn test_context() -> SyntaxContext {
        SyntaxContext {
            execution: Some(FileSurface::Tests),
            code_role: CodeRole::TestSetup,
            text_role: None,
            signature: Some("method:unwrap".into()),
            basis: ClassificationBasis::SyntaxContext,
        }
    }

    fn contexts(pack: &ReviewPack) -> BTreeMap<String, SyntaxContext> {
        pack.all_candidates()
            .iter()
            .map(|candidate| (candidate.id.clone(), test_context()))
            .collect()
    }

    #[test]
    fn thousands_of_test_calls_compress_with_bounded_deterministic_representatives() {
        let pack = pack(
            (0..1_200)
                .map(|index| {
                    candidate(
                        &format!("candidate-{index:04}"),
                        &format!("src/test_{}.rs", index % 4),
                        index + 1,
                    )
                })
                .collect(),
        );
        let before = pack.clone();
        let queue = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        assert_eq!(pack, before);
        assert_eq!(queue.summary.raw_candidates, 1_200);
        assert_eq!(queue.summary.review_units, 1);
        assert_eq!(queue.summary.group_units, 1);
        assert_eq!(queue.summary.grouped_candidates, 1_200);
        assert_eq!(
            queue.summary.representative_candidates,
            REPRESENTATIVE_LIMIT
        );
        assert_eq!(queue.summary.by_execution["tests"], 1_200);
        assert_eq!(queue.summary.largest_group_sizes, vec![1_200]);
        let unit = &queue.units[0];
        assert_eq!(unit.statistics.paths.len(), 4);
        let candidates: BTreeMap<_, _> = pack
            .all_candidates()
            .into_iter()
            .map(|candidate| (candidate.id.clone(), candidate))
            .collect();
        let paths: BTreeSet<_> = unit
            .representative_candidate_ids()
            .iter()
            .map(|id| &candidates[id].path)
            .collect();
        assert_eq!(paths.len(), REPRESENTATIVE_LIMIT);
        for id in unit.representative_candidate_ids() {
            assert!(unit.candidate_ids().contains(id));
        }
        assert_eq!(queue, build(&pack, DIGEST, &contexts(&pack)).unwrap());
    }

    #[test]
    fn origins_runtime_unknown_and_component_families_remain_distinct() {
        let mut candidates: Vec<_> = (0..8)
            .map(|index| candidate(&format!("candidate-{index}"), "src/lib.rs", index + 1))
            .collect();
        candidates[2].origin = CandidateOrigin::PreExisting;
        candidates[3].origin = CandidateOrigin::PreExisting;
        candidates[6].path = "other/src/lib.rs".into();
        candidates[7].path = "other/src/lib.rs".into();
        let pack = pack(candidates);
        let mut contexts = contexts(&pack);
        contexts.insert(
            "candidate-4".into(),
            SyntaxContext {
                execution: Some(FileSurface::Production),
                code_role: CodeRole::RuntimeBoundary,
                ..test_context()
            },
        );
        contexts.remove("candidate-5");
        let queue = build(&pack, DIGEST, &contexts).unwrap();
        assert_eq!(queue.summary.group_units, 3);
        assert_eq!(queue.summary.individual_units, 2);
        assert_eq!(queue.summary.unknown_candidates, 1);
        let runtime = queue
            .units
            .iter()
            .find(|unit| unit.candidate_ids() == ["candidate-4"])
            .unwrap();
        assert!(!runtime.is_group());
        assert_eq!(runtime.priority, ReviewPriority::High);
        let unknown = queue
            .units
            .iter()
            .find(|unit| unit.candidate_ids() == ["candidate-5"])
            .unwrap();
        assert!(!unknown.is_group());
        assert_eq!(unknown.priority, ReviewPriority::Normal);
        for unit in queue.units.iter().filter(|unit| unit.is_group()) {
            let origins: BTreeSet<_> = unit
                .candidate_ids()
                .iter()
                .map(|id| queue.classifications[id].origin)
                .collect();
            assert_eq!(origins.len(), 1);
        }
    }

    #[test]
    fn exact_raw_language_ids_share_coverage_with_static_candidates() {
        let mut pack = pack(vec![candidate("static", "src/lib.rs", 1)]);
        pack.language = super::super::language::scan(&[super::super::language::SourceFile {
            path: "src/lib.rs".into(),
            content: "const VALUE: &str = \"invalid_request\";\n// Human explanation\n".into(),
        }]);
        assert!(!pack.language.candidates.is_empty());
        let queue = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        let raw: BTreeSet<_> = pack
            .all_candidates()
            .into_iter()
            .map(|candidate| candidate.id)
            .collect();
        let covered: BTreeSet<_> = queue
            .units
            .iter()
            .flat_map(|unit| unit.candidate_ids().iter().cloned())
            .collect();
        assert_eq!(covered, raw);
        assert_eq!(queue.summary.raw_candidates, raw.len());
        for candidate in &pack.language.candidates {
            assert!(queue.classifications.contains_key(&candidate.id));
        }
    }

    fn text_candidate(id: &str, text: &str, context: TextContext) -> CandidateEvidence {
        let mut candidate = candidate(id, "src/lib.rs", 1);
        candidate.detector = "residual_foreign_human_text".into();
        candidate.snippet = Some(text.into());
        candidate
            .metadata
            .insert("context".into(), serde_json::to_value(context).unwrap());
        candidate
    }

    #[test]
    fn machine_literals_and_human_diagnostics_never_share_units() {
        let pack = pack(vec![
            text_candidate("machine-a", "invalid_request", TextContext::StringLiteral),
            text_candidate("machine-b", "invalid_request", TextContext::StringLiteral),
            text_candidate(
                "human",
                "Failed to read configuration file",
                TextContext::StringLiteral,
            ),
            text_candidate("comment", "invalid_request", TextContext::Comment),
        ]);
        let mut contexts = contexts(&pack);
        for id in ["machine-a", "machine-b"] {
            contexts.insert(
                id.into(),
                SyntaxContext {
                    execution: Some(FileSurface::Production),
                    code_role: CodeRole::Runtime,
                    text_role: None,
                    signature: Some("method:header:argument:0".into()),
                    basis: ClassificationBasis::SyntaxContext,
                },
            );
        }
        contexts.get_mut("human").unwrap().text_role = Some(TextRole::HumanDiagnostic);
        let queue = build(&pack, DIGEST, &contexts).unwrap();
        assert_eq!(queue.summary.group_units, 1);
        assert_eq!(queue.summary.individual_units, 2);
        assert_eq!(queue.summary.representative_candidates, 2);
        assert_eq!(
            queue.classifications["machine-a"].text_role,
            Some(TextRole::TechnicalIdentifier)
        );
        assert_eq!(
            queue
                .units
                .iter()
                .find(|unit| unit.candidate_ids().contains(&"machine-a".into()))
                .unwrap()
                .priority,
            ReviewPriority::Normal,
            "production machine/API literals should remain reviewable at normal priority"
        );
        assert_eq!(
            queue.classifications["comment"].text_role,
            Some(TextRole::HumanComment)
        );
        let human = queue
            .units
            .iter()
            .find(|unit| unit.candidate_ids().contains(&"human".into()))
            .unwrap();
        assert!(!human.is_group());
        assert_eq!(human.priority, ReviewPriority::High);
    }

    #[test]
    fn syntax_proven_human_log_overrides_identifier_shaped_literal() {
        let pack = pack(vec![text_candidate(
            "usage-log",
            "Usage:",
            TextContext::StringLiteral,
        )]);
        let mut contexts = contexts(&pack);
        contexts.insert(
            "usage-log".into(),
            SyntaxContext {
                execution: Some(FileSurface::Production),
                code_role: CodeRole::Runtime,
                text_role: Some(TextRole::HumanLog),
                signature: Some("macro:println".into()),
                basis: ClassificationBasis::SyntaxContext,
            },
        );

        let queue = build(&pack, DIGEST, &contexts).unwrap();
        assert_eq!(
            queue.classifications["usage-log"].text_role,
            Some(TextRole::HumanLog)
        );
        assert_eq!(
            queue.classifications["usage-log"].text_basis,
            ClassificationBasis::SyntaxContext
        );
        assert_eq!(queue.units[0].priority, ReviewPriority::High);
    }

    #[test]
    fn multiword_text_without_structural_hint_remains_unknown() {
        let pack = pack(vec![text_candidate(
            "message",
            "Failed to read configuration file",
            TextContext::StringLiteral,
        )]);
        let queue = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        assert_eq!(
            queue.classifications["message"].text_role,
            Some(TextRole::Unknown)
        );
        assert_eq!(queue.summary.unknown_candidates, 1);
        assert_eq!(queue.units[0].priority, ReviewPriority::Normal);
        assert!(!queue.units[0].is_group());
        for (text, expected) in [
            ("--run-clippy", TextRole::CliFlag),
            ("review.json", TextRole::Path),
            ("https://example.invalid/path", TextRole::Url),
            ("Content-Type", TextRole::TechnicalIdentifier),
        ] {
            assert_eq!(literal_shape(text), expected);
        }
    }

    #[test]
    fn reordered_source_and_additional_occurrence_keep_group_identity() {
        let mut pack = pack(vec![
            candidate("a", "src/a.rs", 1),
            candidate("b", "src/b.rs", 2),
        ]);
        let original = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        pack.candidates.reverse();
        pack.scope.files.reverse();
        assert_eq!(original, build(&pack, DIGEST, &contexts(&pack)).unwrap());
        pack.candidates.push(candidate("c", "src/a.rs", 3));
        let additional = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        assert_eq!(original.units[0].id, additional.units[0].id);
        assert_eq!(additional.units[0].candidate_ids(), ["a", "b", "c"]);
        let mut context = contexts(&pack);
        context.get_mut("c").unwrap().signature = Some("method:expect:argument:0".into());
        let variant = build(&pack, DIGEST, &context).unwrap();
        assert_eq!(variant.summary.review_units, 2);
    }

    #[test]
    fn parser_failure_preserves_file_context_and_candidate_visibility() {
        let mut pack = pack(vec![
            candidate("a", "src/lib.rs", 1),
            candidate("b", "src/lib.rs", 2),
        ]);
        pack.scope.files[0]
            .surfaces
            .extend([FileSurface::Config, FileSurface::Ci]);
        let contexts = pack
            .all_candidates()
            .into_iter()
            .map(|candidate| {
                (
                    candidate.id,
                    SyntaxContext {
                        execution: None,
                        code_role: CodeRole::Unknown,
                        text_role: None,
                        signature: None,
                        basis: ClassificationBasis::ParseFailure,
                    },
                )
            })
            .collect();
        let queue = build(&pack, DIGEST, &contexts).unwrap();
        assert_eq!(queue.summary.individual_units, 2);
        assert_eq!(queue.summary.unknown_candidates, 2);
        assert_eq!(
            queue.classifications["a"].execution_basis,
            ClassificationBasis::ParseFailure
        );
        assert!(
            queue.classifications["a"]
                .surfaces
                .contains(&FileSurface::Production)
        );
        assert!(
            queue.classifications["a"]
                .surfaces
                .contains(&FileSurface::Ci)
        );
        assert!(
            queue.classifications["a"]
                .surfaces
                .contains(&FileSurface::Config)
        );
        assert!(
            queue
                .units
                .iter()
                .all(|unit| unit.priority == ReviewPriority::Normal)
        );
    }

    #[test]
    fn candidate_surface_uses_its_source_path_when_a_file_was_renamed() {
        let mut pack = pack(vec![candidate("old-path", "docs/guide.md", 1)]);
        let file = &mut pack.scope.files[0];
        file.path = "src/guide.rs".into();
        file.previous_path = Some("docs/guide.md".into());
        file.category = FileCategory::Rust;
        file.surfaces = vec![FileSurface::Production];

        let class = classify(&pack, &pack.candidates[0], None);
        assert_eq!(class.file_category, Some(FileCategory::Markdown));
        assert_eq!(class.surfaces, vec![FileSurface::Docs]);
    }

    #[test]
    fn coverage_membership_source_representatives_and_summary_are_checked() {
        let pack = pack(vec![
            candidate("a", "src/a.rs", 1),
            candidate("b", "src/a.rs", 2),
        ]);
        let original = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        for variant in 0..8 {
            let mut queue = original.clone();
            match variant {
                0 => queue.source.review_pack_sha256.replace_range(0..1, "b"),
                1 => queue.source.snapshot.head_sha.push_str("-different"),
                2 => queue.units.clear(),
                3 => queue.units.push(queue.units[0].clone()),
                4 => queue.classifications.remove("a").map(|_| ()).unwrap(),
                5 => queue.summary.raw_candidates += 1,
                6 => {
                    let UnitMembers::Group {
                        representative_candidate_ids,
                        ..
                    } = &mut queue.units[0].members
                    else {
                        panic!("group fixture")
                    };
                    *representative_candidate_ids = vec!["missing".into()];
                }
                _ => queue.units[0].priority = ReviewPriority::High,
            }
            assert!(
                validate(&queue, &pack, DIGEST).is_err(),
                "variant {variant}"
            );
        }
        let mut duplicate = pack.clone();
        duplicate.candidates.push(duplicate.candidates[0].clone());
        assert!(build(&duplicate, DIGEST, &contexts(&duplicate)).is_err());
    }

    #[test]
    fn queue_roundtrips_and_rejects_semantic_fields() {
        let pack = pack(vec![candidate("a", "src/lib.rs", 1)]);
        let queue = build(&pack, DIGEST, &contexts(&pack)).unwrap();
        let value = serde_json::to_value(&queue).unwrap();
        assert_eq!(
            serde_json::from_value::<ReviewQueue>(value.clone()).unwrap(),
            queue
        );
        assert!(
            !serde_json::to_string(&queue)
                .unwrap()
                .contains("disposition")
        );
        assert!(!serde_json::to_string(&queue).unwrap().contains("findings"));
        let mut with_findings = value.clone();
        with_findings["findings"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ReviewQueue>(with_findings).is_err());
        let mut with_disposition = value;
        with_disposition["units"][0]["disposition"] = serde_json::json!("acceptable");
        assert!(serde_json::from_value::<ReviewQueue>(with_disposition).is_err());
        assert!(
            semantic_triage::validate(&semantic_triage::initialize(&pack, DIGEST), &pack, DIGEST)
                .is_ok()
        );
    }
}
