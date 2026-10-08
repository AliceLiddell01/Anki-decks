//! Детерминированная структурная очередь поверх полного пакета исходных свидетельств.
//!
//! Классификация, приоритет и группа задают маршрутизацию внешнего ревью.
//! Они не утверждают семантический результат или полноту семантического ревью.

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
/// Представители ограничены тремя разными местами; остальные идентификаторы раскрываются отдельно.
pub const REPRESENTATIVE_LIMIT: usize = 3;

macro_rules! named_enum {
    ($name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        named_enum!(@define $name [] [] { $($variant => $label),+ });
    };
    ($name:ident, value_enum { $($variant:ident => $label:literal),+ $(,)? }) => {
        named_enum!(@define $name [clap::ValueEnum] [value(rename_all = "snake_case")] {
            $($variant => $label),+
        });
    };
    (@define $name:ident [$($derive:path)?] [$($attribute:meta)?] {
        $($variant:ident => $label:literal),+ $(,)?
    }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize $(, $derive)?)]
        #[serde(rename_all = "snake_case")]
        $(#[$attribute])?
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

named_enum!(StructuralRole, value_enum {
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

named_enum!(TextRole, value_enum {
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

named_enum!(CodeRole, value_enum {
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

named_enum!(ReviewPriority, value_enum {
    High => "high",
    Normal => "normal",
    Low => "low",
});

named_enum!(QueueSurfaceFilter, value_enum {
    Production => "production",
    Tests => "tests",
    Ci => "ci",
    Config => "config",
    Docs => "docs",
    Generated => "generated",
    Data => "data",
    AgentContext => "agent_context",
    Dependencies => "dependencies",
    Unknown => "unknown",
});

named_enum!(QueueExecutionFilter, value_enum {
    Production => "production",
    Tests => "tests",
    Unknown => "unknown",
});

/// Адаптер доказанного синтаксического контекста; основная логика не содержит парсер Rust.
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

/// Независимые структурные измерения одного исходного идентификатора кандидата.
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

/// Все существенные измерения однородности; сами по себе они не являются семантическим решением.
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

/// Тип элемента явно задаёт гранулярность навигации, но не результат рассмотрения.
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

/// Сводка нагрузки учитывает свидетельства и элементы очереди, а не точность или полноту семантической проверки.
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

/// Самостоятельный версионируемый артефакт; исходный пакет остаётся полным и неизменным.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueue {
    pub schema_version: u32,
    pub source: TriageSource,
    pub classifications: BTreeMap<String, StructuralClassification>,
    pub units: Vec<ReviewUnit>,
    pub summary: QueueSummary,
}

/// Параметры точечной выборки элементов очереди для API списка.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueListFilters {
    pub priority: Option<ReviewPriority>,
    pub unknown_only: bool,
    pub detector: Option<String>,
    pub surface: Option<QueueSurfaceFilter>,
    pub execution: Option<QueueExecutionFilter>,
    pub role: Option<StructuralRole>,
    pub text_role: Option<TextRole>,
    pub code_role: Option<CodeRole>,
}

/// Гранулярность строки очереди.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueUnitKind {
    Individual,
    Group,
}

/// Строка списка с компактной навигационной информацией по элементу очереди.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueListItem {
    pub id: String,
    pub kind: QueueUnitKind,
    pub candidate_id: Option<String>,
    pub priority: ReviewPriority,
    pub classification: StructuralClassification,
    pub detector: String,
    pub candidate_count: usize,
    pub representative_candidate_ids: Vec<String>,
}

/// Одна страница отфильтрованной очереди.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueListPage {
    pub total_units: usize,
    pub matched_units: usize,
    pub offset: usize,
    pub limit: usize,
    pub returned_units: usize,
    pub has_more: bool,
    pub units: Vec<QueueListItem>,
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

/// Классифицирует кандидата, явно сохраняя неизвестные измерения.
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
            "Не удалось сериализовать структурную подпись: {error}"
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
            "Структурная классификация противоречит исходному свидетельству: {}",
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

/// Строит очередь без изменения исходного пакета и без семантических решений.
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
        return Err(invalid(
            "Контекст синтаксиса содержит неизвестный идентификатор кандидата",
        ));
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

fn matches_list_filters(unit: &ReviewUnit, filters: &QueueListFilters) -> bool {
    let classification = &unit.signature.classification;
    filters
        .priority
        .is_none_or(|priority| unit.priority == priority)
        && (!filters.unknown_only || classification.is_unknown())
        && filters
            .detector
            .as_ref()
            .is_none_or(|detector| unit.signature.detector == detector.as_str())
        && filters.surface.is_none_or(|surface| match surface {
            QueueSurfaceFilter::Unknown => {
                classification.surfaces.is_empty()
                    || (classification.file_category == Some(FileCategory::Rust)
                        && classification.execution.is_none())
            }
            _ => classification
                .surfaces
                .iter()
                .any(|item| surface_name(item) == surface.as_str()),
        })
        && filters.execution.is_none_or(|execution| match execution {
            QueueExecutionFilter::Production => {
                classification.execution == Some(FileSurface::Production)
            }
            QueueExecutionFilter::Tests => classification.execution == Some(FileSurface::Tests),
            QueueExecutionFilter::Unknown => classification.execution.is_none(),
        })
        && filters
            .role
            .is_none_or(|role| classification.role.as_str() == role.as_str())
        && filters.text_role.is_none_or(|role| {
            classification
                .text_role
                .map_or(TextRole::Unknown.as_str(), TextRole::as_str)
                == role.as_str()
        })
        && filters
            .code_role
            .is_none_or(|role| classification.code_role.as_str() == role.as_str())
}

fn list_item(unit: &ReviewUnit) -> QueueListItem {
    QueueListItem {
        id: unit.id.clone(),
        kind: if unit.is_group() {
            QueueUnitKind::Group
        } else {
            QueueUnitKind::Individual
        },
        candidate_id: match &unit.members {
            UnitMembers::Individual { candidate_id } => Some(candidate_id.clone()),
            UnitMembers::Group { .. } => None,
        },
        priority: unit.priority,
        classification: unit.signature.classification.clone(),
        detector: unit.signature.detector.clone(),
        candidate_count: unit.candidate_ids().len(),
        representative_candidate_ids: unit.representative_candidate_ids().to_vec(),
    }
}

/// Возвращает страницу очереди, сохраняя установленный при построении порядок элементов.
#[must_use]
pub fn list_units(
    queue: &ReviewQueue,
    filters: &QueueListFilters,
    offset: usize,
    limit: usize,
) -> QueueListPage {
    let matched: Vec<_> = queue
        .units
        .iter()
        .filter(|unit| matches_list_filters(unit, filters))
        .collect();
    let matched_units = matched.len();
    let start = offset.min(matched_units);
    let end = offset.saturating_add(limit).min(matched_units);
    let units: Vec<_> = matched[start..end]
        .iter()
        .map(|unit| list_item(unit))
        .collect();
    QueueListPage {
        total_units: queue.units.len(),
        matched_units,
        offset,
        limit,
        returned_units: units.len(),
        has_more: end < matched_units,
        units,
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

/// Проверяет точную границу источника, принадлежность кандидатов, однородность подписей и сводку.
pub fn validate(
    queue: &ReviewQueue,
    pack: &ReviewPack,
    source_pack_sha256: &str,
) -> Result<QueueSummary, DomainError> {
    if queue.schema_version != QUEUE_SCHEMA_VERSION {
        return Err(invalid(
            "Значение поля `schema_version` структурной очереди не поддерживается",
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
            "Классификации не покрывают точное множество исходных идентификаторов кандидатов",
        ));
    }
    let mut unit_ids = BTreeSet::new();
    let mut coverage = BTreeSet::new();
    for unit in &queue.units {
        if unit.id.is_empty() || !unit_ids.insert(&unit.id) {
            return Err(invalid(
                "Идентификатор единицы очереди пуст или повторяется",
            ));
        }
        let ids = unit.candidate_ids();
        if ids.is_empty() || ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid(
                "Идентификаторы кандидатов в единице очереди должны образовывать непустое отсортированное множество",
            ));
        }
        let mut members = Vec::new();
        for id in ids {
            let candidate = candidates
                .get(id.as_str())
                .ok_or_else(|| invalid(format!("Неизвестный идентификатор кандидата: {id}")))?;
            if !coverage.insert(id.as_str()) {
                return Err(invalid(format!("Кандидат учтён повторно: {id}")));
            }
            let class = &queue.classifications[id];
            validate_classification(class, &files_by_candidate_path, candidate)?;
            if class.origin != candidate.origin
                || class.role != structural_role(&candidate.detector)
                || class.surfaces.windows(2).any(|pair| pair[0] >= pair[1])
                || signature(candidate, class) != unit.signature
            {
                return Err(invalid(format!(
                    "Структурная подпись неоднородна или неверна: {id}"
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
                "Идентификатор, статистика или приоритет единицы не соответствуют её свидетельствам",
            ));
        }
    }
    if coverage.len() != candidates.len() {
        return Err(invalid(
            "Каждый исходный кандидат должен присутствовать ровно в одной единице очереди",
        ));
    }
    let summary = summarize(queue);
    if summary != queue.summary {
        return Err(invalid(
            "Сводка очереди не соответствует классификациям и единицам",
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

    fn queue_for_list() -> ReviewQueue {
        let pack = pack(vec![
            candidate("group-a", "src/tests/a.rs", 1),
            candidate("group-b", "src/tests/b.rs", 2),
            candidate("group-c", "src/tests/c.rs", 3),
            {
                let mut candidate = candidate("high-security", "src/security.rs", 4);
                candidate.detector = "security_surface".into();
                candidate
            },
            candidate("unknown", "src/unknown.rs", 5),
            {
                let mut candidate = candidate("config", ".github/workflows/check.yml", 6);
                candidate.detector = "config_surface".into();
                candidate
            },
            {
                let mut candidate =
                    text_candidate("human-doc", "Release notes", TextContext::MarkdownProse);
                candidate.path = "docs/release.md".into();
                candidate
            },
        ]);
        let mut contexts = contexts(&pack);
        contexts.remove("unknown");
        contexts.insert(
            "high-security".into(),
            SyntaxContext {
                execution: Some(FileSurface::Production),
                code_role: CodeRole::Runtime,
                text_role: None,
                signature: Some("function:authorize".into()),
                basis: ClassificationBasis::SyntaxContext,
            },
        );
        contexts.insert(
            "config".into(),
            SyntaxContext {
                execution: None,
                code_role: CodeRole::Unknown,
                text_role: None,
                signature: None,
                basis: ClassificationBasis::SyntaxContext,
            },
        );
        contexts.insert(
            "human-doc".into(),
            SyntaxContext {
                execution: None,
                code_role: CodeRole::Unknown,
                text_role: None,
                signature: None,
                basis: ClassificationBasis::SyntaxContext,
            },
        );
        build(&pack, DIGEST, &contexts).unwrap()
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
    fn list_paginates_without_changing_queue_order() {
        let queue = queue_for_list();
        let all_ids: Vec<_> = queue.units.iter().map(|unit| unit.id.clone()).collect();
        let first = list_units(&queue, &QueueListFilters::default(), 0, 3);
        let second = list_units(&queue, &QueueListFilters::default(), 3, 20);
        let returned_ids: Vec<_> = first
            .units
            .iter()
            .chain(&second.units)
            .map(|unit| unit.id.clone())
            .collect();

        assert_eq!(first.total_units, queue.units.len());
        assert_eq!(first.matched_units, queue.units.len());
        assert_eq!(first.offset, 0);
        assert_eq!(first.limit, 3);
        assert_eq!(first.returned_units, 3);
        assert!(first.has_more);
        assert_eq!(second.offset, 3);
        assert_eq!(second.returned_units, queue.units.len() - 3);
        assert!(!second.has_more);
        assert_eq!(returned_ids, all_ids);

        let empty_page = list_units(&queue, &QueueListFilters::default(), 0, 0);
        assert!(empty_page.units.is_empty());
        assert!(empty_page.has_more);
        let past_end = list_units(&queue, &QueueListFilters::default(), usize::MAX, 4);
        assert!(past_end.units.is_empty());
        assert!(!past_end.has_more);
    }

    #[test]
    fn list_filters_match_canonical_classification_dimensions() {
        let queue = queue_for_list();
        let get_ids = |filters: QueueListFilters| {
            list_units(&queue, &filters, 0, usize::MAX)
                .units
                .into_iter()
                .map(|item| item.id)
                .collect::<BTreeSet<_>>()
        };
        let ids_for_candidate = |candidate_id: &str| {
            queue
                .units
                .iter()
                .find(|unit| unit.candidate_ids().iter().any(|id| id == candidate_id))
                .unwrap()
                .id
                .clone()
        };
        let group_id = ids_for_candidate("group-a");
        let high_id = ids_for_candidate("high-security");
        let unknown_id = ids_for_candidate("unknown");
        let config_id = ids_for_candidate("config");
        let doc_id = ids_for_candidate("human-doc");

        assert_eq!(
            get_ids(QueueListFilters {
                priority: Some(ReviewPriority::High),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([high_id.clone(), doc_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                unknown_only: true,
                ..QueueListFilters::default()
            }),
            BTreeSet::from([unknown_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                detector: Some("security_surface".into()),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([high_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                surface: Some(QueueSurfaceFilter::Docs),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([doc_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                surface: Some(QueueSurfaceFilter::Tests),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([group_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                execution: Some(QueueExecutionFilter::Tests),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([group_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                execution: Some(QueueExecutionFilter::Unknown),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([unknown_id.clone(), config_id.clone(), doc_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                role: Some(StructuralRole::Security),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([high_id])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                role: Some(StructuralRole::Text),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([doc_id.clone()])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                text_role: Some(TextRole::HumanDocumentation),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([doc_id])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                text_role: Some(TextRole::Unknown),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([
                group_id.clone(),
                unknown_id,
                config_id,
                ids_for_candidate("high-security"),
            ])
        );
        assert_eq!(
            get_ids(QueueListFilters {
                code_role: Some(CodeRole::TestSetup),
                ..QueueListFilters::default()
            }),
            BTreeSet::from([group_id])
        );
    }

    #[test]
    fn list_unknown_surface_and_group_representatives_follow_unit_membership() {
        let queue = queue_for_list();
        let unknown_id = queue
            .units
            .iter()
            .find(|unit| unit.candidate_ids() == ["unknown"])
            .unwrap()
            .id
            .clone();
        let page = list_units(
            &queue,
            &QueueListFilters {
                surface: Some(QueueSurfaceFilter::Unknown),
                ..QueueListFilters::default()
            },
            0,
            20,
        );
        assert_eq!(page.matched_units, 1);
        assert_eq!(page.units[0].id, unknown_id);

        let mut empty_surface_queue = queue.clone();
        let empty_surface_id = {
            let config = empty_surface_queue
                .units
                .iter_mut()
                .find(|unit| unit.candidate_ids() == ["config"])
                .unwrap();
            config.signature.classification.surfaces.clear();
            config.id.clone()
        };
        empty_surface_queue
            .classifications
            .get_mut("config")
            .unwrap()
            .surfaces
            .clear();
        let empty_surface_page = list_units(
            &empty_surface_queue,
            &QueueListFilters {
                surface: Some(QueueSurfaceFilter::Unknown),
                ..QueueListFilters::default()
            },
            0,
            20,
        );
        assert_eq!(
            empty_surface_page
                .units
                .iter()
                .map(|item| item.id.clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([unknown_id, empty_surface_id])
        );

        let group = list_units(
            &queue,
            &QueueListFilters {
                priority: Some(ReviewPriority::Low),
                ..QueueListFilters::default()
            },
            0,
            20,
        )
        .units
        .into_iter()
        .find(|item| item.kind == QueueUnitKind::Group)
        .unwrap();
        let source_group = queue.units.iter().find(|unit| unit.id == group.id).unwrap();
        assert_eq!(group.candidate_count, 3);
        assert_eq!(group.candidate_id, None);
        assert!(serde_json::to_value(&group).unwrap()["candidate_id"].is_null());
        assert_eq!(
            group.representative_candidate_ids,
            source_group.representative_candidate_ids().to_vec()
        );
        assert!(group.representative_candidate_ids.len() <= REPRESENTATIVE_LIMIT);
        assert_eq!(
            group.representative_candidate_ids,
            vec![
                "group-a".to_owned(),
                "group-b".to_owned(),
                "group-c".to_owned()
            ]
        );
        let individual = list_units(
            &queue,
            &QueueListFilters {
                detector: Some("security_surface".into()),
                ..QueueListFilters::default()
            },
            0,
            1,
        )
        .units
        .pop()
        .unwrap();
        assert_eq!(individual.kind, QueueUnitKind::Individual);
        assert_eq!(individual.candidate_count, 1);
        assert_eq!(individual.candidate_id.as_deref(), Some("high-security"));
        assert!(individual.representative_candidate_ids.is_empty());
    }

    #[test]
    fn list_dtos_roundtrip_and_reject_unknown_fields() {
        let queue = queue_for_list();
        let filters = QueueListFilters {
            priority: Some(ReviewPriority::High),
            unknown_only: true,
            detector: Some("security_surface".into()),
            surface: Some(QueueSurfaceFilter::Production),
            execution: Some(QueueExecutionFilter::Production),
            role: Some(StructuralRole::Security),
            text_role: Some(TextRole::Unknown),
            code_role: Some(CodeRole::Runtime),
        };
        assert_eq!(
            serde_json::from_value::<QueueListFilters>(serde_json::to_value(&filters).unwrap())
                .unwrap(),
            filters
        );
        let mut filters_with_extra = serde_json::to_value(&filters).unwrap();
        filters_with_extra["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<QueueListFilters>(filters_with_extra).is_err());

        let page = list_units(&queue, &QueueListFilters::default(), 0, 1);
        assert_eq!(
            serde_json::from_value::<QueueListPage>(serde_json::to_value(&page).unwrap()).unwrap(),
            page
        );
        let mut page_with_extra = serde_json::to_value(&page).unwrap();
        page_with_extra["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<QueueListPage>(page_with_extra).is_err());

        let item = &page.units[0];
        assert_eq!(
            serde_json::to_value(item).unwrap()["candidate_id"],
            "high-security"
        );
        assert_eq!(
            serde_json::from_value::<QueueListItem>(serde_json::to_value(item).unwrap()).unwrap(),
            item.clone()
        );
        let mut item_with_extra = serde_json::to_value(item).unwrap();
        item_with_extra["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<QueueListItem>(item_with_extra).is_err());
        assert_eq!(serde_json::to_value(QueueUnitKind::Group).unwrap(), "group");
        assert_eq!(
            serde_json::from_value::<QueueUnitKind>(serde_json::json!("individual")).unwrap(),
            QueueUnitKind::Individual
        );
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
