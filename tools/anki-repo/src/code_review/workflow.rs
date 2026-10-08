//! CLI-сценарии для неизменяемых свидетельств code-review и правил обработки языка.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{DomainError, ErrorCode};

use super::delta;
use super::detectors::{self, Candidate as DetectorCandidate, CandidateOrigin as DetectorOrigin};
use super::diagnostics::{self, CodeReviewDiagnostic, ToolRun, ToolRunStatus};
use super::language::{
    self, ApplyResult, LanguageDecisions, LanguageScan, SkippedFile, SourceFile,
};
use super::model::{
    CandidateEvidence, CandidateOrigin, REVIEW_SCHEMA_VERSION, ReviewDelta, ReviewFile, ReviewPack,
    ReviewScope, ToolRunEvidence,
};
use super::review_queue::{
    self, ClassificationBasis, CodeRole, QueueExecutionFilter, QueueListFilters, QueueListPage,
    QueueSummary, QueueSurfaceFilter, ReviewPriority, ReviewQueue, ReviewUnit,
    StructuralClassification, StructuralRole, TextRole,
};
use super::rust_context::{
    RustCallContext, RustCallKind, RustCodeRole, RustContext, RustContextBasis, RustContextIndex,
    RustExecutionContext,
};
use super::scope::{
    self, CollectedScope, FileStatus, GitTarget, ImageState, ScopeError, ScopedFile,
};
use super::semantic_triage::{self, SemanticTriage, TriageSummary};

/// Верхняя граница читаемого артефакта ревью.
pub const MAX_REVIEW_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
/// Верхняя граница языкового артефакта.
pub const MAX_LANGUAGE_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

/// Краткий результат создания пакета для CLI.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    pub artifact_dir: String,
    pub review_queue_artifact: String,
    pub target: GitTarget,
    pub files: usize,
    pub candidates: usize,
    pub review_queue: QueueSummary,
    pub diagnostics: usize,
    pub tool_runs: Vec<ToolRunEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<ReviewDelta>,
}

/// Краткий результат команд `language scan` и `language check`.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageSummary {
    pub artifact: String,
    pub schema_version: u32,
    pub files: usize,
    pub candidates: usize,
    pub skipped: usize,
}

/// Краткий результат `language apply` без копирования всего содержимого файлов в stdout.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageApplySummary {
    pub applied: bool,
    pub files: usize,
    pub replacements: usize,
    pub results: Vec<AppliedFileSummary>,
}

/// Результат инициализации JSON-документа семантических решений.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticTriageInitSummary {
    pub artifact: String,
    pub target: GitTarget,
    pub total_candidates: usize,
    pub unreviewed_candidates: usize,
}

/// Результат проверки JSON-документа семантических решений.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticTriageValidationSummary {
    pub canonical_artifact: Option<String>,
    pub summary: TriageSummary,
}

/// Результат сохранения Markdown-отчёта по семантическим решениям.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticTriageReportSummary {
    pub report: String,
    pub total_findings: usize,
    pub unreviewed_candidates: usize,
}

/// Результат валидации производного queue artifact.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewQueueValidationSummary {
    pub valid: bool,
    pub summary: QueueSummary,
}

/// Типизированные параметры CLI для фильтрации очереди.
#[derive(Debug, Clone, Default)]
pub struct ReviewQueueListOptions {
    pub priority: Option<ReviewPriority>,
    pub unknown: bool,
    pub detector: Option<String>,
    pub surface: Option<QueueSurfaceFilter>,
    pub execution: Option<QueueExecutionFilter>,
    pub role: Option<StructuralRole>,
    pub text_role: Option<TextRole>,
    pub code_role: Option<CodeRole>,
    pub offset: u64,
    pub limit: u64,
}

/// Полное свидетельство одного кандидата вместе с его маршрутизацией.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewQueueCandidateDetail {
    pub candidate: CandidateEvidence,
    pub classification: StructuralClassification,
    pub unit: ReviewUnit,
}

/// Полное исходное свидетельство представителя без повтора всей содержащей группы.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewQueueRepresentativeDetail {
    pub candidate: CandidateEvidence,
    pub classification: StructuralClassification,
}

/// Раскрытие группы: все исходные ID и полные evidence представителей.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewQueueGroupDetail {
    pub unit: ReviewUnit,
    pub representatives: Vec<ReviewQueueRepresentativeDetail>,
}

/// Результат публикации одного файла языкового процесса.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedFileSummary {
    pub path: String,
    pub before_sha256: String,
    pub after_sha256: String,
    pub replacements: usize,
}

/// Сопоставляет Git-снимки и создаёт неизменяемый каталог свидетельств.
pub fn collect(
    base: &str,
    head: &str,
    out_dir: Option<&Path>,
    run_clippy: bool,
) -> Result<SnapshotSummary, DomainError> {
    let root = repository_root(Path::new("."))?;
    let collected = scope::collect_scope(&root, base, head).map_err(scope_error)?;
    let (post_sources, base_sources) = rust_sources_from_scope(&collected);
    let post = RustImages::new(post_sources);
    let base = RustImages::new(base_sources);
    let pack = build_pack(&root, collected, run_clippy);
    let contexts = syntax_contexts(&pack, &post, &base);
    save_snapshot(&root, out_dir, &pack, None, &contexts)
}

/// Пересобирает свидетельства от зафиксированной базовой версии и создаёт дельту детекторов.
pub fn verify(
    baseline_path: &Path,
    head: &str,
    out_dir: Option<&Path>,
    run_clippy: bool,
) -> Result<SnapshotSummary, DomainError> {
    let root = repository_root(Path::new("."))?;
    let baseline: ReviewPack = read_json(
        baseline_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "исходный пакет ревью",
    )?;
    delta::validate_review_pack(&baseline)?;
    let collected =
        scope::collect_scope(&root, &baseline.target.base_sha, head).map_err(scope_error)?;
    if collected.target.repository_id != baseline.target.repository_id
        || collected.target.base_sha != baseline.target.base_sha
        || collected.target.merge_base_sha != baseline.target.merge_base_sha
    {
        // Проверяем до build_pack: он может явно запускать Clippy.
        return Err(DomainError::with_details(
            ErrorCode::BaselineMismatch,
            "новый снимок относится к другому репозиторию или отличается по merge-base от исходного пакета",
            crate::details! {
                "baseline_base_sha" => baseline.target.base_sha,
                "new_base_sha" => collected.target.base_sha,
                "baseline_merge_base_sha" => baseline.target.merge_base_sha,
                "new_merge_base_sha" => collected.target.merge_base_sha,
            },
        ));
    }
    let (post_sources, base_sources) = rust_sources_from_scope(&collected);
    let post = RustImages::new(post_sources);
    let base = RustImages::new(base_sources);
    let pack = build_pack(&root, collected, run_clippy);
    let changes = delta::compare(&baseline, &pack)?;
    let contexts = syntax_contexts(&pack, &post, &base);
    save_snapshot(&root, out_dir, &pack, Some(changes), &contexts)
}

/// Сравнивает сохранённые снимки; опциональный JSON пишется без перезаписи.
pub fn compare_files(
    before_path: &Path,
    after_path: &Path,
    output: Option<&Path>,
) -> Result<ReviewDelta, DomainError> {
    let before: ReviewPack = read_json(
        before_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "исходный пакет ревью",
    )?;
    let after: ReviewPack = read_json(after_path, MAX_REVIEW_ARTIFACT_BYTES, "новый пакет ревью")?;
    let changes = delta::compare(&before, &after)?;
    if let Some(path) = output {
        let bytes = json_bytes(&changes)?;
        let root = repository_root(Path::new("."))?;
        let path = output_path(&root, path)?;
        write_document_once(&path, &bytes)?;
    }
    Ok(changes)
}

/// Проверяет queue artifact относительно точных байтов переданного `review.json`.
pub fn validate_review_queue(
    pack_path: &Path,
    queue_path: &Path,
) -> Result<ReviewQueueValidationSummary, DomainError> {
    let (pack, pack_bytes) = read_review_pack(pack_path)?;
    let queue: ReviewQueue = read_json(
        queue_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "структурная очередь code-review",
    )?;
    let summary = review_queue::validate(&queue, &pack, &sha256_hex(&pack_bytes))?;
    Ok(ReviewQueueValidationSummary {
        valid: true,
        summary,
    })
}

/// Возвращает только проверенную агрегированную сводку queue artifact.
pub fn summarize_review_queue(
    pack_path: &Path,
    queue_path: &Path,
) -> Result<QueueSummary, DomainError> {
    Ok(load_validated_review_queue(pack_path, queue_path)?.2)
}

/// Возвращает страницу только после проверки точной привязки обоих артефактов к источнику.
pub fn list_review_queue(
    pack_path: &Path,
    queue_path: &Path,
    options: &ReviewQueueListOptions,
) -> Result<QueueListPage, DomainError> {
    let (_, queue, _) = load_validated_review_queue(pack_path, queue_path)?;
    if !(1..=review_queue::MAX_QUEUE_LIST_LIMIT).contains(&options.limit) {
        return Err(invalid_queue_list_filter(
            "limit",
            &options.limit.to_string(),
        ));
    }
    let detector = options
        .detector
        .as_deref()
        .map(|value| {
            if is_canonical_filter_label(value) {
                Ok(value.to_owned())
            } else {
                Err(invalid_queue_list_filter("detector", value))
            }
        })
        .transpose()?;
    let filters = QueueListFilters {
        priority: options.priority,
        unknown_only: options.unknown,
        detector,
        surface: options.surface,
        execution: options.execution,
        role: options.role,
        text_role: options.text_role,
        code_role: options.code_role,
    };
    let offset = usize::try_from(options.offset).map_err(|_| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "Смещение очереди слишком велико для этой платформы",
        )
    })?;
    let limit = usize::try_from(options.limit).map_err(|_| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "Размер страницы очереди слишком велик для этой платформы",
        )
    })?;
    Ok(review_queue::list_units(&queue, &filters, offset, limit))
}

fn is_canonical_filter_label(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
}

fn invalid_queue_list_filter(flag: &str, value: &str) -> DomainError {
    DomainError::new(
        ErrorCode::InvalidRequest,
        format!("Некорректное значение --{flag}: «{value}»"),
    )
}

/// Раскрывает группу до полного списка IDs и полных свидетельств представителей.
pub fn expand_review_queue_group(
    pack_path: &Path,
    queue_path: &Path,
    id: &str,
) -> Result<ReviewQueueGroupDetail, DomainError> {
    let (pack, queue, _) = load_validated_review_queue(pack_path, queue_path)?;
    let unit = queue
        .units
        .iter()
        .find(|unit| unit.id == id)
        .filter(|unit| unit.is_group())
        .cloned()
        .ok_or_else(|| {
            DomainError::new(
                ErrorCode::NotFound,
                format!("Группа очереди ревью не найдена: {id}"),
            )
        })?;
    let candidates: BTreeMap<_, _> = pack
        .all_candidates()
        .into_iter()
        .map(|candidate| (candidate.id.clone(), candidate))
        .collect();
    let representatives = unit
        .representative_candidate_ids()
        .iter()
        .map(|candidate_id| queue_representative_detail(&queue, &candidates, candidate_id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ReviewQueueGroupDetail {
        unit,
        representatives,
    })
}

/// Возвращает raw evidence кандидата, его классификацию и containing unit.
pub fn inspect_review_queue_candidate(
    pack_path: &Path,
    queue_path: &Path,
    id: &str,
) -> Result<ReviewQueueCandidateDetail, DomainError> {
    let (pack, queue, _) = load_validated_review_queue(pack_path, queue_path)?;
    let unit = queue
        .units
        .iter()
        .find(|unit| {
            unit.candidate_ids()
                .iter()
                .any(|candidate_id| candidate_id == id)
        })
        .cloned()
        .ok_or_else(|| {
            DomainError::new(
                ErrorCode::NotFound,
                format!("Кандидат не найден в структурной очереди: {id}"),
            )
        })?;
    let candidates: BTreeMap<_, _> = pack
        .all_candidates()
        .into_iter()
        .map(|candidate| (candidate.id.clone(), candidate))
        .collect();
    queue_candidate_detail(&queue, &candidates, &unit, id)
}

fn load_validated_review_queue(
    pack_path: &Path,
    queue_path: &Path,
) -> Result<(ReviewPack, ReviewQueue, QueueSummary), DomainError> {
    let (pack, pack_bytes) = read_review_pack(pack_path)?;
    let queue = read_json(
        queue_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "структурная очередь code-review",
    )?;
    let summary = review_queue::validate(&queue, &pack, &sha256_hex(&pack_bytes))?;
    Ok((pack, queue, summary))
}

fn queue_candidate_detail(
    queue: &ReviewQueue,
    candidates: &BTreeMap<String, CandidateEvidence>,
    unit: &ReviewUnit,
    id: &str,
) -> Result<ReviewQueueCandidateDetail, DomainError> {
    let candidate = candidates.get(id).cloned().ok_or_else(|| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("Очередь ссылается на неизвестный ID кандидата: {id}"),
        )
    })?;
    let classification = queue.classifications.get(id).cloned().ok_or_else(|| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("В очереди отсутствует классификация кандидата: {id}"),
        )
    })?;
    Ok(ReviewQueueCandidateDetail {
        candidate,
        classification,
        unit: unit.clone(),
    })
}

fn queue_representative_detail(
    queue: &ReviewQueue,
    candidates: &BTreeMap<String, CandidateEvidence>,
    id: &str,
) -> Result<ReviewQueueRepresentativeDetail, DomainError> {
    let candidate = candidates.get(id).cloned().ok_or_else(|| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("Очередь ссылается на неизвестный ID кандидата-представителя: {id}"),
        )
    })?;
    let classification = queue.classifications.get(id).cloned().ok_or_else(|| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("В очереди отсутствует классификация кандидата-представителя: {id}"),
        )
    })?;
    Ok(ReviewQueueRepresentativeDetail {
        candidate,
        classification,
    })
}

/// Создаёт версионируемый JSON-документ с явным списком нерассмотренных кандидатов.
pub fn init_semantic_triage(
    pack_path: &Path,
    output: &Path,
) -> Result<SemanticTriageInitSummary, DomainError> {
    let root = repository_root(Path::new("."))?;
    let (pack, source_bytes) = read_review_pack(pack_path)?;
    let source_hash = sha256_hex(&source_bytes);
    let mut triage = semantic_triage::initialize(&pack, &source_hash);
    semantic_triage::canonicalize(&mut triage);
    let summary = semantic_triage::validate(&triage, &pack, &source_hash)?;
    let bytes = json_bytes(&triage)?;
    let output = output_path(&root, output)?;
    write_document_once(&output, &bytes)?;
    Ok(SemanticTriageInitSummary {
        artifact: output.display().to_string(),
        target: pack.target,
        total_candidates: summary.total_candidates,
        unreviewed_candidates: summary.unreviewed_candidates,
    })
}

/// Проверяет решения относительно точного источника и при запросе сохраняет нормализованный JSON.
pub fn validate_semantic_triage(
    pack_path: &Path,
    triage_path: &Path,
    canonical_output: Option<&Path>,
) -> Result<SemanticTriageValidationSummary, DomainError> {
    let (pack, source_bytes) = read_review_pack(pack_path)?;
    let source_hash = sha256_hex(&source_bytes);
    let (mut triage, _) = read_json_with_bytes::<SemanticTriage>(
        triage_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "JSON-документ семантических решений",
    )?;
    let summary = semantic_triage::validate(&triage, &pack, &source_hash)?;
    let canonical_artifact = if let Some(canonical_output) = canonical_output {
        semantic_triage::canonicalize(&mut triage);
        let bytes = json_bytes(&triage)?;
        let root = repository_root(Path::new("."))?;
        let output =
            replace_derived_document(&root, canonical_output, &bytes, &[pack_path, triage_path])?;
        Some(output.display().to_string())
    } else {
        None
    };
    Ok(SemanticTriageValidationSummary {
        canonical_artifact,
        summary,
    })
}

/// Возвращает только воспроизводимую сводку проверенного документа решений.
pub fn summarize_semantic_triage(
    pack_path: &Path,
    triage_path: &Path,
) -> Result<TriageSummary, DomainError> {
    let (pack, source_bytes) = read_review_pack(pack_path)?;
    let source_hash = sha256_hex(&source_bytes);
    let (triage, _) = read_json_with_bytes::<SemanticTriage>(
        triage_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "JSON-документ семантических решений",
    )?;
    semantic_triage::validate(&triage, &pack, &source_hash)
}

/// Создаёт компактный Markdown-отчёт по проверенному документу решений.
pub fn report_semantic_triage(
    pack_path: &Path,
    triage_path: &Path,
    output: &Path,
) -> Result<SemanticTriageReportSummary, DomainError> {
    let root = repository_root(Path::new("."))?;
    let (pack, source_bytes) = read_review_pack(pack_path)?;
    let source_hash = sha256_hex(&source_bytes);
    let (mut triage, _) = read_json_with_bytes::<SemanticTriage>(
        triage_path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "JSON-документ семантических решений",
    )?;
    let summary = semantic_triage::validate(&triage, &pack, &source_hash)?;
    semantic_triage::canonicalize(&mut triage);
    let report = semantic_triage::render_markdown(&triage, &pack);
    let output =
        replace_derived_document(&root, output, report.as_bytes(), &[pack_path, triage_path])?;
    Ok(SemanticTriageReportSummary {
        report: output.display().to_string(),
        total_findings: triage.findings.len(),
        unreviewed_candidates: summary.unreviewed_candidates,
    })
}

/// Сканирует явные пути относительно репозитория или полные новые версии файлов пакета ревью.
pub fn scan_language(
    root_arg: &Path,
    paths: &[PathBuf],
    pack_path: Option<&Path>,
    output: &Path,
) -> Result<LanguageSummary, DomainError> {
    let root = repository_root(root_arg)?;
    let (sources, mut skipped) = if let Some(pack_path) = pack_path {
        let pack: ReviewPack = read_json(pack_path, MAX_REVIEW_ARTIFACT_BYTES, "пакет ревью")?;
        delta::validate_review_pack(&pack)?;
        let collected = scope::collect_scope(&root, &pack.target.base_sha, &pack.target.head_sha)
            .map_err(scope_error)?;
        if collected.target.repository_id != pack.target.repository_id
            || collected.target.base_sha != pack.target.base_sha
            || collected.target.head_sha != pack.target.head_sha
        {
            return Err(DomainError::new(
                ErrorCode::BaselineMismatch,
                "пакет ревью относится к другому репозиторию или снимку Git",
            ));
        }
        sources_from_scope(&collected)
    } else {
        if paths.is_empty() {
            return Err(DomainError::new(
                ErrorCode::Usage,
                "для `language scan` укажите хотя бы один `--path` или `--pack`",
            ));
        }
        read_language_paths(&root, paths)?
    };
    let mut scan = scan_sources(sources);
    scan.skipped.append(&mut skipped);
    normalize_skipped(&mut scan.skipped);
    let bytes = json_bytes(&scan)?;
    let output = output_path(&root, output)?;
    write_document_once(&output, &bytes)?;
    Ok(language_summary(&output, &scan))
}

/// Повторно сканирует тот же список исходников из языкового артефакта.
pub fn check_language(
    root_arg: &Path,
    scan_path: &Path,
    output: Option<&Path>,
) -> Result<(LanguageScan, Option<LanguageSummary>), DomainError> {
    let root = repository_root(root_arg)?;
    let previous: LanguageScan = read_json(
        scan_path,
        MAX_LANGUAGE_ARTIFACT_BYTES,
        "артефакт `language scan`",
    )?;
    if previous.schema_version != language::LANGUAGE_SCHEMA_VERSION {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!(
                "неподдерживаемая версия артефакта `language scan`: {}",
                previous.schema_version
            ),
        ));
    }
    let paths: Vec<_> = previous
        .files
        .iter()
        .map(|file| PathBuf::from(&file.path))
        .collect();
    let (sources, mut skipped) = read_language_paths(&root, &paths)?;
    let mut scan = scan_sources(sources);
    scan.skipped.append(&mut skipped);
    scan.skipped.extend(previous.skipped);
    normalize_skipped(&mut scan.skipped);
    let summary = if let Some(output) = output {
        let bytes = json_bytes(&scan)?;
        let output = output_path(&root, output)?;
        write_document_once(&output, &bytes)?;
        Some(language_summary(&output, &scan))
    } else {
        None
    };
    Ok((scan, summary))
}

/// Проверяет решения без записи и применяет только явные `replace` через `apply_changes`.
pub fn apply_language(
    root_arg: &Path,
    decisions_path: &Path,
    apply_changes: bool,
) -> Result<LanguageApplySummary, DomainError> {
    let root = repository_root(root_arg)?;
    let decisions: LanguageDecisions = read_json(
        decisions_path,
        MAX_LANGUAGE_ARTIFACT_BYTES,
        "файл решений для `language apply`",
    )?;
    let result = language::apply(&root, &decisions, apply_changes).map_err(language_error)?;
    Ok(apply_summary(result))
}

/// Готовит сводку и локальные артефакты пакета и, при режиме `verify`, дельту.
fn save_snapshot(
    root: &Path,
    out_dir: Option<&Path>,
    pack: &ReviewPack,
    changes: Option<ReviewDelta>,
    contexts: &BTreeMap<String, review_queue::SyntaxContext>,
) -> Result<SnapshotSummary, DomainError> {
    let default_name = match changes.as_ref() {
        Some(delta) => format!(
            "{}-{}-{}",
            delta.before.head_sha, pack.target.base_sha, pack.target.head_sha
        ),
        None => format!("{}-{}", pack.target.base_sha, pack.target.head_sha),
    };
    let directory = out_dir.map_or_else(
        || root.join(".anki-repo").join("review").join(default_name),
        Path::to_path_buf,
    );
    let directory = safe_output_path(root, &directory, true)?;
    let mut documents = BTreeMap::new();
    let pack_bytes = json_bytes(pack)?;
    let queue = review_queue::build(pack, &sha256_hex(&pack_bytes), contexts)?;
    let review_queue_bytes = json_bytes(&queue)?;
    documents.insert("review.json", pack_bytes);
    documents.insert("review-queue.json", review_queue_bytes);
    documents.insert("review.txt", human_summary(pack, &queue).into_bytes());
    if let Some(delta) = &changes {
        documents.insert("delta.json", json_bytes(delta)?);
    }
    write_directory_once(&directory, &documents)?;
    let tool_runs = pack.tool_runs.clone();
    Ok(SnapshotSummary {
        artifact_dir: display_path(&directory, root),
        review_queue_artifact: display_path(&directory.join("review-queue.json"), root),
        target: pack.target.clone(),
        files: pack.scope.files.len(),
        candidates: pack.all_candidates().len(),
        review_queue: queue.summary,
        diagnostics: pack.diagnostics.len(),
        tool_runs,
        delta: changes,
    })
}

struct RustImages {
    index: RustContextIndex,
    source: BTreeMap<String, String>,
    lines: BTreeMap<String, SourceLines>,
}

impl RustImages {
    fn new(sources: Vec<SourceFile>) -> Self {
        let source = sources
            .iter()
            .map(|file| (file.path.clone(), file.content.clone()))
            .collect();
        let lines = sources
            .iter()
            .map(|file| (file.path.clone(), SourceLines::new(&file.content)))
            .collect();
        let index = RustContextIndex::from_sources(&sources);
        Self {
            index,
            source,
            lines,
        }
    }

    fn line(&self, path: &str, line: usize) -> Option<&str> {
        let source = self.source.get(path)?;
        self.lines.get(path)?.get(source, line)
    }
}

/// Смещения строк одного Git image. Диапазоны исключают LF и необязательный CR.
/// Индекс строится один раз на файл и сохраняет поведение `str::lines()`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceLines {
    ranges: Vec<(usize, usize)>,
}

impl SourceLines {
    fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut ranges = Vec::new();
        let mut start = 0;
        for (end, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                let content_end = if end > start && bytes[end - 1] == b'\r' {
                    end - 1
                } else {
                    end
                };
                ranges.push((start, content_end));
                start = end + 1;
            }
        }
        if start < bytes.len() {
            ranges.push((start, bytes.len()));
        }
        Self { ranges }
    }

    fn get<'a>(&self, source: &'a str, line: usize) -> Option<&'a str> {
        let (start, end) = *self.ranges.get(line.checked_sub(1)?)?;
        source.get(start..end)
    }
}

/// Сохраняет исходные образы base и post отдельно, чтобы разбирать удалённые
/// и переименованные свидетельства по той версии файла, где они находились.
fn rust_sources_from_scope(collected: &CollectedScope) -> (Vec<SourceFile>, Vec<SourceFile>) {
    let mut post_sources = Vec::new();
    let mut base_sources = Vec::new();
    for file in &collected.files {
        if file.path.ends_with(".rs")
            && let (ImageState::Text, Some(content)) = (&file.post.state, &file.post.text)
        {
            post_sources.push(SourceFile {
                path: file.path.clone(),
                content: content.clone(),
            });
        }
        let base_path = file.previous_path.as_deref().unwrap_or(&file.path);
        if base_path.ends_with(".rs")
            && let (ImageState::Text, Some(content)) = (&file.base.state, &file.base.text)
        {
            base_sources.push(SourceFile {
                path: base_path.to_owned(),
                content: content.clone(),
            });
        }
    }
    (post_sources, base_sources)
}

fn syntax_contexts(
    pack: &ReviewPack,
    post: &RustImages,
    base: &RustImages,
) -> BTreeMap<String, review_queue::SyntaxContext> {
    let mut result = BTreeMap::new();
    for candidate in pack.all_candidates() {
        if !candidate.path.ends_with(".rs") {
            continue;
        }
        let context = if candidate.detector == "residual_foreign_human_text" {
            let Some(source) = post.source.get(&candidate.path) else {
                continue;
            };
            let Some(expected_digest) = candidate
                .metadata
                .get("source_sha256")
                .and_then(Value::as_str)
            else {
                continue;
            };
            if sha256_hex(source.as_bytes()) != expected_digest {
                continue;
            }
            let (Some(start), Some(end)) = (
                candidate.metadata.get("start").and_then(Value::as_u64),
                candidate.metadata.get("end").and_then(Value::as_u64),
            ) else {
                continue;
            };
            let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
                continue;
            };
            let context = post.index.lookup_range(&candidate.path, start, end);
            let line = candidate
                .line
                .and_then(|line| post.line(&candidate.path, line));
            let text_role = classify_rust_text(&candidate, source, &context, line);
            rust_syntax_context(context, Some(text_role))
        } else {
            let Some(line) = candidate.line else {
                continue;
            };
            let matches_evidence = |images: &RustImages| {
                images
                    .line(&candidate.path, line)
                    .zip(candidate.snippet.as_deref())
                    .is_some_and(|(source_line, snippet)| source_line.trim() == snippet.trim())
            };
            let images = if candidate.snippet.is_some() && matches_evidence(post) {
                post
            } else if candidate.snippet.is_some() && matches_evidence(base) {
                base
            } else if candidate.snippet.is_none() {
                if post.source.contains_key(&candidate.path) {
                    post
                } else if base.source.contains_key(&candidate.path) {
                    base
                } else {
                    continue;
                }
            } else {
                // Устаревший или непозиционный фрагмент не должен наследовать
                // синтаксический контекст несвязанной строки из образа Git.
                continue;
            };
            let column = candidate.column.or_else(|| {
                images
                    .line(&candidate.path, line)
                    .and_then(|line| detector_column(line, &candidate.signals))
            });
            rust_syntax_context(images.index.lookup(&candidate.path, line, column), None)
        };
        result.insert(candidate.id, context);
    }
    result
}

fn rust_syntax_context(
    context: RustContext,
    text_role: Option<TextRole>,
) -> review_queue::SyntaxContext {
    let execution = match context.execution {
        RustExecutionContext::Runtime => Some(super::scope::FileSurface::Production),
        RustExecutionContext::Test => Some(super::scope::FileSurface::Tests),
        RustExecutionContext::Unknown => None,
    };
    let mut code_role = match context.code_role {
        RustCodeRole::Runtime => CodeRole::Runtime,
        RustCodeRole::Item if execution == Some(super::scope::FileSurface::Tests) => {
            CodeRole::TestHelper
        }
        RustCodeRole::Item => CodeRole::Runtime,
        RustCodeRole::RuntimeBoundary => CodeRole::RuntimeBoundary,
        RustCodeRole::TestSetup => CodeRole::TestSetup,
        RustCodeRole::TestAssertion => CodeRole::TestAssertion,
        RustCodeRole::TestHelper => CodeRole::TestHelper,
        RustCodeRole::Unknown => CodeRole::Unknown,
    };
    let basis = match context.basis {
        RustContextBasis::RustSyntax => ClassificationBasis::SyntaxContext,
        RustContextBasis::AmbiguousLocation if execution.is_some() => {
            ClassificationBasis::SyntaxContext
        }
        RustContextBasis::ParseFailure => ClassificationBasis::ParseFailure,
        RustContextBasis::AmbiguousLocation | RustContextBasis::Unavailable => {
            ClassificationBasis::Unknown
        }
    };
    let (execution, text_role, signature) =
        if basis == ClassificationBasis::SyntaxContext && execution.is_some() {
            (execution, text_role, context.call_signature())
        } else {
            code_role = CodeRole::Unknown;
            (None, None, None)
        };
    review_queue::SyntaxContext {
        execution,
        code_role,
        text_role,
        signature,
        basis,
    }
}

fn detector_column(line_text: &str, signals: &[String]) -> Option<usize> {
    let tokens: BTreeSet<&str> = signals
        .iter()
        .filter_map(|signal| match signal.as_str() {
            "unwrap_call" => Some("unwrap"),
            "unwrap_unchecked_call" => Some("unwrap_unchecked"),
            "expect_call" => Some("expect"),
            "panic_macro" => Some("panic"),
            "todo_macro" => Some("todo"),
            "unimplemented_macro" => Some("unimplemented"),
            "explicit_process_exit" => Some("exit"),
            "rust_unsafe_construct" => Some("unsafe"),
            _ => None,
        })
        .collect();
    let locations: Vec<_> = tokens
        .iter()
        .flat_map(|token| {
            line_text
                .match_indices(token)
                .map(move |(offset, _)| offset)
        })
        .collect();
    if locations.len() != 1 {
        return None;
    }
    Some(line_text[..locations[0]].chars().count() + 1)
}

fn classify_rust_text(
    candidate: &CandidateEvidence,
    source: &str,
    context: &RustContext,
    line: Option<&str>,
) -> TextRole {
    let text_context = candidate
        .metadata
        .get("context")
        .cloned()
        .and_then(|value| serde_json::from_value::<super::language::TextContext>(value).ok());
    if text_context != Some(super::language::TextContext::StringLiteral) {
        return TextRole::Unknown;
    }
    let line = line.unwrap_or_default();
    let Some(call) = context.call_context.as_ref() else {
        return TextRole::Unknown;
    };
    let name = call.path.rsplit("::").next().unwrap_or(&call.path);
    let kind = call.kind;
    if kind == RustCallKind::Macro
        && matches!(
            name,
            "print"
                | "println"
                | "eprint"
                | "eprintln"
                | "trace"
                | "debug"
                | "info"
                | "warn"
                | "error"
        )
    {
        return TextRole::HumanLog;
    }
    if (kind == RustCallKind::Macro && matches!(name, "panic" | "bail" | "ensure" | "anyhow"))
        || (kind == RustCallKind::Method
            && matches!(
                name,
                "expect" | "ok_or" | "ok_or_else" | "map_err" | "context" | "with_context"
            ))
        || (kind == RustCallKind::Call
            && (name == "Err"
                || call.path.ends_with("Error::new")
                || call.path.ends_with("Error::other")))
    {
        return TextRole::HumanDiagnostic;
    }
    if kind == RustCallKind::Attribute && matches!(name, "arg" | "command") {
        let start = candidate
            .metadata
            .get("start")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        let prefix = start
            .filter(|start| *start <= source.len() && source.is_char_boundary(*start))
            .and_then(|start| {
                source[..start]
                    .rfind("#[")
                    .map(|attribute_start| &source[attribute_start..start])
            })
            .unwrap_or(line);
        if ["help", "about", "long_about", "value_name"]
            .iter()
            .any(|key| prefix.contains(&format!("{key} =")))
        {
            return TextRole::HumanHelp;
        }
    }
    if (call.kind == RustCallKind::Attribute && matches!(name, "serde" | "serde_as"))
        || (call.kind == RustCallKind::Macro && name == "json")
    {
        return TextRole::MachineContract;
    }
    if (call.kind == RustCallKind::Method
        && matches!(
            name,
            "header" | "query" | "route" | "content_type" | "user_agent" | "accept"
        ))
        || (call.kind == RustCallKind::Call
            && (call.path.ends_with("Command::new")
                || call.path.ends_with("TcpStream::connect")
                || call.path.ends_with("TcpListener::bind")
                || call.path.ends_with("UdpSocket::bind")
                || call.path.ends_with("Client::get")
                || call.path.ends_with("Client::post")))
    {
        return TextRole::ExternalLiteral;
    }
    if call.kind == RustCallKind::Method
        && matches!(
            name,
            "set_text"
                | "set_label"
                | "set_title"
                | "set_message"
                | "set_placeholder"
                | "show_message"
        )
    {
        return TextRole::HumanUi;
    }
    if context.execution == RustExecutionContext::Test && is_test_fixture_context(call) {
        return TextRole::TestFixture;
    }
    TextRole::Unknown
}

fn is_test_fixture_context(call: &RustCallContext) -> bool {
    let name = call.path.rsplit("::").next().unwrap_or(&call.path);
    match call.kind {
        RustCallKind::Macro => matches!(
            name,
            "assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert"
                | "debug_assert_eq"
                | "debug_assert_ne"
                | "assert_snapshot"
                | "assert_json_snapshot"
                | "assert_yaml_snapshot"
                | "assert_toml_snapshot"
                | "expect_file"
                | "include"
                | "include_bytes"
                | "include_str"
        ),
        RustCallKind::Call | RustCallKind::Method => call
            .path
            .split("::")
            .any(|part| matches!(part, "fixture" | "fixtures" | "snapshot" | "snapshots")),
        RustCallKind::Attribute => false,
    }
}

fn build_pack(root: &Path, collected: CollectedScope, run_clippy: bool) -> ReviewPack {
    let inputs: Vec<_> = collected
        .files
        .iter()
        .map(|file| detectors::FileInput {
            path: file.path.clone(),
            previous_path: file.previous_path.clone(),
            status: detector_status(&file.status),
            base_text: file.base.text.clone(),
            post_text: file.post.text.clone(),
            post_changed_lines: (!file.binary).then(|| file.post_changed_lines.clone()),
        })
        .collect();
    let static_candidates = detectors::detect(&inputs);
    let (diagnostics, tool_runs) = if !run_clippy {
        (
            Vec::new(),
            vec![ToolRunEvidence {
                tool: "clippy".into(),
                status: "skipped".into(),
                exit_status: None,
                diagnostic_count: 0,
                malformed_lines: 0,
                ignored_records: 0,
                stderr_summary: None,
                message: Some(
                    "Clippy не запускался: исполнение кода проекта требует явного --run-clippy."
                        .into(),
                ),
            }],
        )
    } else if !working_tree_matches(&collected.target, root) {
        (
            Vec::new(),
            vec![ToolRunEvidence {
                tool: "clippy".into(),
                status: "skipped".into(),
                exit_status: None,
                diagnostic_count: 0,
                malformed_lines: 0,
                ignored_records: 0,
                stderr_summary: None,
                message: Some(
                    "Clippy пропущен: рабочая копия отличается от зафиксированного снимка head."
                        .into(),
                ),
            }],
        )
    } else {
        evidence_for_run(root, diagnostics::run_local_clippy(root))
    };
    let mut candidates = static_candidates
        .into_iter()
        .map(map_candidate)
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        (&a.path, &a.detector, a.line, &a.id).cmp(&(&b.path, &b.detector, b.line, &b.id))
    });
    let language = language_from_scope(&collected);
    let mut dependencies = Vec::new();
    let mut tests = Vec::new();
    let mut suppressions = Vec::new();
    let mut risk_surfaces = Vec::new();
    for candidate in &candidates {
        match candidate.detector.as_str() {
            "dependency_change" => dependencies.push(candidate.id.clone()),
            "test_added" | "test_removed" | "test_ignored" | "assertion_removed" => {
                tests.push(candidate.id.clone());
            }
            "rust_suppression" | "ci_suppression" => suppressions.push(candidate.id.clone()),
            "config_surface" | "generated_surface" | "skill_surface" | "security_surface"
            | "absolute_path" | "local_endpoint" => risk_surfaces.push(candidate.id.clone()),
            _ => {}
        }
    }
    let target = collected.target;
    let scope = ReviewScope {
        merge_base_sha: target.merge_base_sha.clone(),
        text_image_limit_bytes: collected.text_image_limit_bytes,
        files: collected.files.iter().map(review_file).collect(),
    };
    ReviewPack {
        schema_version: REVIEW_SCHEMA_VERSION,
        target,
        scope,
        diagnostics,
        candidates,
        language,
        dependencies,
        tests,
        suppressions,
        risk_surfaces,
        tool_runs,
    }
}

fn evidence_for_run(
    root: &Path,
    mut run: ToolRun,
) -> (Vec<CodeReviewDiagnostic>, Vec<ToolRunEvidence>) {
    normalize_tool_run(root, &mut run);
    sort_diagnostics(&mut run.diagnostics);
    let status = match run.status {
        ToolRunStatus::Unavailable => "unavailable",
        ToolRunStatus::Failed => "failed",
        ToolRunStatus::SuccessWithoutDiagnostics => "success_without_diagnostics",
        ToolRunStatus::Diagnostics => "diagnostics",
    };
    let count = run.diagnostics.len();
    let evidence = ToolRunEvidence {
        tool: "clippy".into(),
        status: status.into(),
        exit_status: run.exit_status,
        diagnostic_count: count,
        malformed_lines: run.malformed_lines,
        ignored_records: run.ignored_records,
        stderr_summary: run.stderr_summary,
        message: run.message,
    };
    (run.diagnostics, vec![evidence])
}

fn normalize_tool_run(root: &Path, run: &mut ToolRun) {
    for diagnostic in &mut run.diagnostics {
        normalize_diagnostic(root, diagnostic);
    }
    if let Some(summary) = &mut run.stderr_summary {
        *summary = sanitize_root_text(summary, root);
    }
    if let Some(message) = &mut run.message {
        *message = sanitize_root_text(message, root);
    }
}

fn normalize_diagnostic(root: &Path, diagnostic: &mut CodeReviewDiagnostic) {
    diagnostic.message = sanitize_root_text(&diagnostic.message, root);
    if let Some(code) = &mut diagnostic.code
        && let Some(explanation) = &mut code.explanation
    {
        *explanation = sanitize_root_text(explanation, root);
    }
    for span in &mut diagnostic.spans {
        span.file_name = normalize_diagnostic_path(&span.file_name, root);
        if let Some(label) = &mut span.label {
            *label = sanitize_root_text(label, root);
        }
        if let Some(replacement) = &mut span.suggested_replacement {
            *replacement = sanitize_root_text(replacement, root);
        }
        for line in &mut span.source_lines {
            line.text = sanitize_root_text(&line.text, root);
        }
    }
    if let Some(package_id) = &mut diagnostic.source.package_id {
        *package_id = sanitize_root_text(package_id, root);
    }
    for child in &mut diagnostic.children {
        normalize_diagnostic(root, child);
    }
}

fn normalize_diagnostic_path(raw: &str, root: &Path) -> String {
    let path = Path::new(raw);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(root) {
            return path_string(relative);
        }
        return path
            .file_name()
            .and_then(|name| name.to_str())
            .map_or_else(|| "<external>".into(), |name| format!("<external>/{name}"));
    }
    raw.strip_prefix("./").unwrap_or(raw).replace('\\', "/")
}

fn sanitize_root_text(text: &str, root: &Path) -> String {
    let root = root.to_string_lossy();
    text.replace(root.as_ref(), "<repository>")
}

fn sort_diagnostics(diagnostics: &mut [CodeReviewDiagnostic]) {
    diagnostics.sort_by_key(diagnostic_sort_key);
    for diagnostic in diagnostics {
        sort_diagnostics(&mut diagnostic.children);
    }
}

fn diagnostic_sort_key(diagnostic: &CodeReviewDiagnostic) -> (String, u64, String, String) {
    let span = diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or(diagnostic.spans.first());
    (
        span.map_or_else(String::new, |span| span.file_name.clone()),
        span.map_or(0, |span| span.line_start),
        diagnostic
            .code
            .as_ref()
            .map_or_else(String::new, |code| code.code.clone()),
        diagnostic.message.clone(),
    )
}

fn working_tree_matches(target: &GitTarget, root: &Path) -> bool {
    let head = git_output(root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|text| text.trim().to_owned());
    if head.as_deref() != Some(target.head_sha.as_str()) {
        return false;
    }
    let tracked_tree_clean = Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["diff", "--quiet", "HEAD", "--"])
        .status()
        .is_ok_and(|status| status.success());
    tracked_tree_clean && !has_untracked_workspace_sources(root)
}

fn has_untracked_workspace_sources(root: &Path) -> bool {
    let Ok(output) = git_output(root, &["ls-files", "--others", "--exclude-standard", "-z"]) else {
        return true;
    };
    if output
        .split(|byte| *byte == 0)
        .any(is_untracked_workspace_source)
    {
        return true;
    }
    has_ignored_untracked_workspace_inputs(root)
}

fn is_untracked_workspace_source(path: &[u8]) -> bool {
    if path.is_empty() || is_local_data_path(path) {
        return false;
    }
    // Это проверка известных входов Cargo, а не изоляция произвольного
    // build.rs: он может читать любые файлы после явного --run-clippy.
    let Ok(path) = std::str::from_utf8(path) else {
        return true;
    };
    let name = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".rs")
        || matches!(
            name,
            "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
        )
        || path == ".cargo/config"
        || path == ".cargo/config.toml"
        || path.ends_with("/.cargo/config")
        || path.ends_with("/.cargo/config.toml")
}

fn has_ignored_untracked_workspace_inputs(root: &Path) -> bool {
    let mut args = vec![
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "-z",
        "--",
    ];
    args.extend([
        ":(glob)**/*.rs",
        ":(glob)**/Cargo.toml",
        ":(glob)**/Cargo.lock",
        ":(glob)**/rust-toolchain",
        ":(glob)**/rust-toolchain.toml",
        ":(glob)**/.cargo/config",
        ":(glob)**/.cargo/config.toml",
        ":(exclude,glob)**/target/**",
    ]);
    let Ok(output) = git_output(root, &args) else {
        return true;
    };
    output.split(|byte| *byte == 0).any(|path| {
        !path.is_empty()
            && !is_local_data_path(path)
            && !path
                .split(|byte| *byte == b'/')
                .any(|part| part == b"target")
    })
}

fn is_local_data_path(path: &[u8]) -> bool {
    [
        b"decks/".as_slice(),
        b".asset-store/".as_slice(),
        b".anki-repo/review/".as_slice(),
        b".codex/local/".as_slice(),
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

fn detector_status(status: &FileStatus) -> detectors::FileStatus {
    match status {
        FileStatus::Added => detectors::FileStatus::Added,
        FileStatus::Modified => detectors::FileStatus::Modified,
        FileStatus::Deleted => detectors::FileStatus::Deleted,
        FileStatus::Renamed => detectors::FileStatus::Renamed,
        FileStatus::Copied => detectors::FileStatus::Copied,
        FileStatus::TypeChanged => detectors::FileStatus::TypeChanged,
    }
}

fn map_candidate(candidate: DetectorCandidate) -> CandidateEvidence {
    let mut metadata = BTreeMap::new();
    for (key, value) in candidate.metadata {
        metadata.insert(key, Value::String(value));
    }
    CandidateEvidence {
        id: candidate.id,
        detector: candidate.candidate_type.as_str().to_owned(),
        path: candidate.path,
        line: candidate.line,
        column: None,
        snippet: candidate.snippet,
        origin: match candidate.origin {
            DetectorOrigin::IntroducedOrChanged => CandidateOrigin::IntroducedOrChanged,
            DetectorOrigin::PreExisting => CandidateOrigin::PreExisting,
            DetectorOrigin::Unknown => CandidateOrigin::Unknown,
        },
        signals: candidate.signals,
        source: candidate.source,
        metadata,
    }
}

fn review_file(file: &ScopedFile) -> ReviewFile {
    ReviewFile {
        path: file.path.clone(),
        previous_path: file.previous_path.clone(),
        status: file.status.clone(),
        additions: file.additions,
        deletions: file.deletions,
        category: file.category.clone(),
        surfaces: file.surfaces.clone(),
        binary: file.binary,
        base_state: file.base.state.clone(),
        base_size: file.base.size,
        base_object_id: file.base.object_id.clone(),
        base_changed_lines: file.base_changed_lines.clone(),
        post_state: file.post.state.clone(),
        post_size: file.post.size,
        post_object_id: file.post.object_id.clone(),
        post_changed_lines: file.post_changed_lines.clone(),
    }
}

fn language_from_scope(collected: &CollectedScope) -> LanguageScan {
    let (sources, mut skipped) = sources_from_scope(collected);
    let mut scan = scan_sources(sources);
    scan.skipped.append(&mut skipped);
    normalize_skipped(&mut scan.skipped);
    scan
}

fn sources_from_scope(collected: &CollectedScope) -> (Vec<SourceFile>, Vec<SkippedFile>) {
    let mut sources = Vec::new();
    let mut skipped = Vec::new();
    for file in &collected.files {
        match (&file.post.state, file.post.text.as_ref()) {
            (ImageState::Text, Some(content)) => {
                sources.push(SourceFile {
                    path: file.path.clone(),
                    content: if language::is_eligible_path(&file.path) {
                        content.clone()
                    } else {
                        String::new()
                    },
                });
            }
            (ImageState::Missing, _) => skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "версия после изменений отсутствует (файл удалён)".into(),
            }),
            (ImageState::Oversized, _) => skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "версия после изменений превышает лимит полного текстового образа".into(),
            }),
            (ImageState::Binary, _) => skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "бинарная версия после изменений".into(),
            }),
            (ImageState::InvalidUtf8, _) => skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "версия после изменений не является текстом UTF-8".into(),
            }),
            (ImageState::Gitlink, _) => skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "gitlink не является текстовым файлом".into(),
            }),
            (ImageState::Text, None) => skipped.push(SkippedFile {
                path: file.path.clone(),
                reason: "полный текст версии после изменений недоступен".into(),
            }),
        }
    }
    (sources, skipped)
}

fn scan_sources(sources: Vec<SourceFile>) -> LanguageScan {
    let mut scan = language::scan(&sources);
    scan.files.sort_by(|a, b| a.path.cmp(&b.path));
    scan
}

fn read_language_paths(
    root: &Path,
    paths: &[PathBuf],
) -> Result<(Vec<SourceFile>, Vec<SkippedFile>), DomainError> {
    let mut ordered = BTreeSet::new();
    let mut sources = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let relative = normalize_relative_path(path)?;
        if !ordered.insert(relative.clone()) {
            continue;
        }
        if !language::is_eligible_path(&relative) {
            sources.push(SourceFile {
                path: relative,
                content: String::new(),
            });
            continue;
        }
        let absolute = checked_source_path(root, &relative)?;
        let metadata = fs::metadata(&absolute).map_err(|error| {
            DomainError::with_details(
                ErrorCode::InputUnreadable,
                format!(
                    "не удалось прочитать исходник для языковой проверки «{relative}»: {error}"
                ),
                crate::details! { "path" => relative },
            )
        })?;
        if !metadata.is_file() {
            return Err(DomainError::new(
                ErrorCode::InputUnreadable,
                format!("исходник для языковой проверки «{relative}» не является обычным файлом"),
            ));
        }
        if metadata.len() > scope::MAX_TEXT_BYTES {
            skipped.push(SkippedFile {
                path: relative,
                reason: "файл превышает лимит полного текстового образа".into(),
            });
            continue;
        }
        let bytes = fs::read(&absolute).map_err(|error| {
            DomainError::new(
                ErrorCode::InputUnreadable,
                format!("не удалось прочитать исходник для языковой проверки: {error}"),
            )
        })?;
        match String::from_utf8(bytes) {
            Ok(content) => sources.push(SourceFile {
                path: relative,
                content,
            }),
            Err(_) => skipped.push(SkippedFile {
                path: relative,
                reason: "файл не является UTF-8".into(),
            }),
        }
    }
    Ok((sources, skipped))
}

fn normalize_skipped(skipped: &mut Vec<SkippedFile>) {
    skipped.sort_by(|left, right| (&left.path, &left.reason).cmp(&(&right.path, &right.reason)));
    skipped.dedup();
}

fn language_summary(path: &Path, scan: &LanguageScan) -> LanguageSummary {
    LanguageSummary {
        artifact: path.display().to_string(),
        schema_version: scan.schema_version,
        files: scan.files.len(),
        candidates: scan.candidates.len(),
        skipped: scan.skipped.len(),
    }
}

fn apply_summary(result: ApplyResult) -> LanguageApplySummary {
    LanguageApplySummary {
        applied: result.applied,
        files: result.files.len(),
        replacements: result.files.iter().map(|file| file.replacements).sum(),
        results: result
            .files
            .into_iter()
            .map(|file| AppliedFileSummary {
                path: file.path,
                before_sha256: file.before_sha256,
                after_sha256: file.after_sha256,
                replacements: file.replacements,
            })
            .collect(),
    }
}

fn human_summary(pack: &ReviewPack, queue: &ReviewQueue) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Навигация code-review · review.json v{}",
        pack.schema_version
    );
    let _ = writeln!(text, "База: {}", pack.target.base_sha);
    let _ = writeln!(text, "HEAD: {}", pack.target.head_sha);
    let _ = writeln!(
        text,
        "Общий предок (merge-base): {}",
        pack.target.merge_base_sha
    );
    let _ = writeln!(text, "Файлов: {}", pack.scope.files.len());
    let mut file_statuses = BTreeMap::new();
    let mut file_surfaces = BTreeMap::new();
    for file in &pack.scope.files {
        *file_statuses
            .entry(file_status_label(&file.status))
            .or_insert(0usize) += 1;
        for surface in &file.surfaces {
            *file_surfaces
                .entry(review_queue::surface_name(surface))
                .or_insert(0usize) += 1;
        }
    }
    let _ = writeln!(
        text,
        "Статусы файлов: {}",
        display_counts(&file_statuses, |key| (*key).to_owned())
    );
    let _ = writeln!(
        text,
        "Поверхности файлов: {}",
        display_counts(&file_surfaces, |key| (*key).to_owned())
    );
    let summary = &queue.summary;
    let _ = writeln!(
        text,
        "Сырых кандидатов: {}; единиц ревью: {} (отдельных: {}, групп: {})",
        summary.raw_candidates, summary.review_units, summary.individual_units, summary.group_units,
    );
    let _ = writeln!(
        text,
        "Кандидатов в группах: {}; представителей: {}; с неизвестной классификацией: {}",
        summary.grouped_candidates, summary.representative_candidates, summary.unknown_candidates,
    );
    let _ = writeln!(
        text,
        "Приоритет единиц ревью: {}",
        display_counts(
            &summary
                .units_by_priority
                .iter()
                .map(|(priority, count)| (priority.as_str(), *count))
                .collect(),
            |key| (*key).to_owned(),
        )
    );
    let _ = writeln!(
        text,
        "Число кандидатов по детекторам: {}",
        display_counts(&summary.by_detector, |key| key.clone())
    );
    let _ = writeln!(
        text,
        "Число кандидатов по поверхностям: {}",
        display_counts(&summary.by_surface, |key| key.clone())
    );
    let _ = writeln!(
        text,
        "Исполнение: {}; структурные роли: {}; роли текста: {}; роли кода: {}",
        display_counts(&summary.by_execution, |key| key.clone()),
        display_counts(&summary.by_structural_role, |key| key.as_str().to_owned()),
        display_counts(&summary.by_text_role, |key| key.as_str().to_owned()),
        display_counts(&summary.by_code_role, |key| key.as_str().to_owned()),
    );
    if !summary.largest_group_sizes.is_empty() {
        let _ = writeln!(
            text,
            "Самые крупные группы (число кандидатов): {}",
            summary
                .largest_group_sizes
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let candidates: BTreeMap<_, _> = pack
        .all_candidates()
        .into_iter()
        .map(|candidate| (candidate.id.clone(), candidate))
        .collect();
    let important: Vec<_> = queue
        .units
        .iter()
        .filter(|unit| {
            !unit.is_group()
                && (unit.priority == ReviewPriority::High
                    || queue.classifications[&unit.candidate_ids()[0]].is_unknown())
        })
        .take(20)
        .collect();
    let _ = writeln!(
        text,
        "Важные отдельные единицы ({} показано, максимум 20):",
        important.len()
    );
    for unit in &important {
        let id = &unit.candidate_ids()[0];
        if let Some(candidate) = candidates.get(id) {
            let location = candidate.line.map_or_else(
                || candidate.path.clone(),
                |line| format!("{}:{line}", candidate.path),
            );
            let snippet = candidate
                .snippet
                .as_deref()
                .map(crate::text::bounded_sample)
                .map(|value| format!(" — {}", single_line(&value)))
                .unwrap_or_default();
            let _ = writeln!(
                text,
                "  [{}] {} {} {}{}",
                unit.priority.as_str(),
                id,
                candidate.detector,
                location,
                snippet
            );
        }
    }
    let important_total = queue
        .units
        .iter()
        .filter(|unit| {
            !unit.is_group()
                && (unit.priority == ReviewPriority::High
                    || queue.classifications[&unit.candidate_ids()[0]].is_unknown())
        })
        .count();
    if important_total > important.len() {
        let _ = writeln!(
            text,
            "  … ещё {} отдельных единиц",
            important_total - important.len()
        );
    }

    let groups: Vec<_> = queue.units.iter().filter(|unit| unit.is_group()).collect();
    let _ = writeln!(
        text,
        "Группы ({} показано, максимум 20):",
        groups.len().min(20)
    );
    for unit in groups.iter().take(20) {
        let representatives = unit
            .representative_candidate_ids()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            text,
            "  [{}] {} · кандидатов: {} · детектор: {} · семейство путей: {} · представители: {}",
            unit.priority.as_str(),
            unit.id,
            unit.candidate_ids().len(),
            unit.signature.detector,
            unit.signature.path_family,
            representatives,
        );
    }
    if groups.len() > 20 {
        let _ = writeln!(text, "  … ещё {} групп", groups.len() - 20);
    }
    let _ = writeln!(text, "Очередь: review-queue.json");
    let _ = writeln!(text, "Полные свидетельства: review.json");
    let _ = writeln!(
        text,
        "Все единицы доступны постранично через `code-review queue list`."
    );
    let _ = writeln!(
        text,
        "Приоритет и группа задают навигацию, но не являются семантическим решением."
    );
    let _ = writeln!(
        text,
        "Диагностик: {} (сами по себе не являются подтверждёнными замечаниями)",
        pack.diagnostics.len()
    );
    let mut diagnostic_severities = BTreeMap::new();
    for diagnostic in &pack.diagnostics {
        *diagnostic_severities
            .entry(format!("{:?}", diagnostic.severity).to_ascii_lowercase())
            .or_insert(0usize) += 1;
    }
    if !diagnostic_severities.is_empty() {
        let _ = writeln!(
            text,
            "Серьёзность диагностик: {}",
            display_counts(&diagnostic_severities, |key| key.to_string())
        );
    }
    for diagnostic in pack.diagnostics.iter().take(12) {
        let code = diagnostic
            .code
            .as_ref()
            .map_or("".into(), |code| format!(" [{}]", code.code));
        let location = diagnostic
            .spans
            .iter()
            .find(|span| span.is_primary)
            .or(diagnostic.spans.first())
            .map_or_else(String::new, |span| {
                format!(" {}:{}", span.file_name, span.line_start)
            });
        let _ = writeln!(
            text,
            "  {:?}{}{} {}",
            diagnostic.severity,
            code,
            location,
            single_line(&crate::text::bounded_sample(&diagnostic.message))
        );
    }
    if pack.diagnostics.len() > 12 {
        let _ = writeln!(text, "  … ещё {} диагностик", pack.diagnostics.len() - 12);
    }
    for run in &pack.tool_runs {
        let _ = writeln!(
            text,
            "Анализатор {}: {} (диагностик: {})",
            run.tool, run.status, run.diagnostic_count
        );
        if let Some(message) = &run.message {
            let _ = writeln!(
                text,
                "  {}",
                single_line(&crate::text::bounded_sample(message))
            );
        }
    }
    text
}

pub(crate) fn display_counts<K, F>(counts: &BTreeMap<K, usize>, label: F) -> String
where
    K: Ord,
    F: Fn(&K) -> String,
{
    if counts.is_empty() {
        return "—".into();
    }
    counts
        .iter()
        .map(|(key, count)| format!("{}: {count}", label(key)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn file_status_label(status: &FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "добавлен",
        FileStatus::Modified => "изменён",
        FileStatus::Deleted => "удалён",
        FileStatus::Renamed => "переименован",
        FileStatus::Copied => "скопирован",
        FileStatus::TypeChanged => "изменён тип",
    }
}

fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], "↵")
}

fn read_json<T: DeserializeOwned>(path: &Path, limit: u64, what: &str) -> Result<T, DomainError> {
    read_json_with_bytes(path, limit, what).map(|(document, _)| document)
}

fn read_review_pack(path: &Path) -> Result<(ReviewPack, Vec<u8>), DomainError> {
    let (pack, bytes) = read_json_with_bytes(
        path,
        MAX_REVIEW_ARTIFACT_BYTES,
        "пакет свидетельств code-review",
    )?;
    delta::validate_review_pack(&pack)?;
    Ok((pack, bytes))
}

fn read_json_with_bytes<T: DeserializeOwned>(
    path: &Path,
    limit: u64,
    what: &str,
) -> Result<(T, Vec<u8>), DomainError> {
    let metadata = fs::metadata(path).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось открыть {what}: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("{what} не является обычным файлом или превышает лимит {limit} байт"),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|error| {
            DomainError::new(
                ErrorCode::InputUnreadable,
                format!("не удалось прочитать {what}: {error}"),
            )
        })?;
    if bytes.len() as u64 > limit {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("{what} превышает лимит {limit} байт"),
        ));
    }
    let document = serde_json::from_slice(&bytes).map_err(|error| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("некорректный JSON в {what}: {error}"),
        )
    })?;
    Ok((document, bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, DomainError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        DomainError::new(
            ErrorCode::Internal,
            format!("не удалось сериализовать артефакт: {error}"),
        )
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_directory_once(
    directory: &Path,
    documents: &BTreeMap<&str, Vec<u8>>,
) -> Result<(), DomainError> {
    let created_directory = if directory.exists() {
        if directory.is_symlink() || !directory.is_dir() {
            return Err(artifact_conflict(directory));
        }
        let entries =
            fs::read_dir(directory).map_err(|error| artifact_write_error(directory, &error))?;
        let mut names = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|error| artifact_write_error(directory, &error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(expected) = documents.get(name.as_str()) else {
                return Err(artifact_conflict(directory));
            };
            if !entry
                .file_type()
                .map_err(|error| artifact_write_error(&entry.path(), &error))?
                .is_file()
                || fs::read(entry.path())
                    .map_err(|error| artifact_write_error(&entry.path(), &error))?
                    != *expected
            {
                return Err(artifact_conflict(directory));
            }
            names.insert(name);
        }
        if names.len() == documents.len() {
            return Ok(());
        }
        if !names.is_empty() {
            return Err(artifact_conflict(directory));
        }
        false
    } else {
        let parent = directory.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| artifact_write_error(parent, &error))?;
        fs::create_dir(directory).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                artifact_conflict(directory)
            } else {
                artifact_write_error(directory, &error)
            }
        })?;
        true
    };
    let mut written = Vec::new();
    for (name, bytes) in documents {
        let path = directory.join(name);
        if let Err(error) = write_new_file(&path, bytes) {
            for written_path in written {
                let _ = fs::remove_file(written_path);
            }
            if created_directory {
                let _ = fs::remove_dir(directory);
            }
            return Err(artifact_write_error(&path, &error));
        }
        written.push(path);
    }
    Ok(())
}

/// Сохраняет исходный артефакт однократно; повтор допустим только с теми же байтами.
fn write_document_once(path: &Path, bytes: &[u8]) -> Result<(), DomainError> {
    if path.exists() {
        if path.is_symlink() || !path.is_file() || fs::read(path).ok().as_deref() != Some(bytes) {
            return Err(artifact_conflict(path));
        }
        return Ok(());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| artifact_write_error(parent, &error))?;
    write_new_file(path, bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            artifact_conflict(path)
        } else {
            artifact_write_error(path, &error)
        }
    })
}

/// Обновляет производный документ, сохраняя входные артефакты и ограничения путей.
fn replace_derived_document(
    root: &Path,
    requested: &Path,
    bytes: &[u8],
    source_paths: &[&Path],
) -> Result<PathBuf, DomainError> {
    let output = output_path(root, requested)?;
    for source in source_paths {
        let source = fs::canonicalize(source).map_err(|error| {
            DomainError::new(
                ErrorCode::InputUnreadable,
                format!("не удалось разрешить путь исходного артефакта: {error}"),
            )
        })?;
        if output == source {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                "производный артефакт нельзя сохранять вместо исходного документа",
                crate::details! { "path" => output.display().to_string() },
            ));
        }
    }
    if output.exists() {
        let existing = fs::read(&output).map_err(|error| artifact_write_error(&output, &error))?;
        if existing == bytes {
            return Ok(output);
        }
    }
    crate::write::replace_document_atomically(&output, bytes)?;
    Ok(output)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn safe_output_path(
    root: &Path,
    requested: &Path,
    directory: bool,
) -> Result<PathBuf, DomainError> {
    if requested.as_os_str().is_empty() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "путь артефакта не может быть пустым",
        ));
    }
    let requested = lexical_absolute(requested);
    reject_decks_path(root, &requested)?;
    if requested.is_symlink() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "символьная ссылка как путь артефакта запрещена",
        ));
    }
    let future_path = resolve_future_path(&requested)?;
    reject_decks_path(root, &future_path)?;
    reject_git_metadata_path(root, &future_path)?;
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| artifact_write_error(parent, &error))?;
    let parent = fs::canonicalize(parent).map_err(|error| artifact_write_error(parent, &error))?;
    let name = requested.file_name().ok_or_else(|| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "путь артефакта должен включать имя",
        )
    })?;
    let resolved = parent.join(name);
    if resolved.is_symlink() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "символьная ссылка как путь артефакта запрещена",
        ));
    }
    let resolved = if resolved.exists() {
        if directory && !resolved.is_dir() || !directory && !resolved.is_file() {
            return Err(DomainError::new(
                ErrorCode::ReviewArtifactConflict,
                "тип существующего пути артефакта не совпадает",
            ));
        }
        fs::canonicalize(&resolved).map_err(|error| artifact_write_error(&resolved, &error))?
    } else {
        resolved
    };
    let decks = root.join("decks");
    let decks = fs::canonicalize(&decks).unwrap_or(decks);
    if resolved.starts_with(decks) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "артефакты code-review нельзя сохранять под decks/**",
        ));
    }
    Ok(resolved)
}

fn reject_git_metadata_path(root: &Path, candidate: &Path) -> Result<(), DomainError> {
    for metadata_arg in ["--git-dir", "--git-common-dir"] {
        let output = git_output(root, &["rev-parse", "--path-format=absolute", metadata_arg])
            .map_err(|error| {
                DomainError::new(
                    ErrorCode::GitEvidenceFailed,
                    format!("не удалось определить каталог служебных данных Git: {error}"),
                )
            })?;
        let metadata_path = String::from_utf8_lossy(&output);
        let metadata_path = fs::canonicalize(metadata_path.trim()).map_err(|error| {
            DomainError::new(
                ErrorCode::GitEvidenceFailed,
                format!("не удалось разрешить каталог служебных данных Git: {error}"),
            )
        })?;
        if candidate.starts_with(&metadata_path) {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "артефакты code-review нельзя сохранять внутри служебных каталогов Git",
            ));
        }
    }
    Ok(())
}

fn resolve_future_path(path: &Path) -> Result<PathBuf, DomainError> {
    let mut ancestor = path.to_path_buf();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "не удалось разрешить путь артефакта относительно существующего каталога",
            ));
        };
        missing.push(name.to_os_string());
        if !ancestor.pop() {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "не удалось разрешить путь артефакта относительно существующего каталога",
            ));
        }
    }
    let mut resolved =
        fs::canonicalize(&ancestor).map_err(|error| artifact_write_error(&ancestor, &error))?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn reject_decks_path(root: &Path, candidate: &Path) -> Result<(), DomainError> {
    let decks = root.join("decks");
    let decks = fs::canonicalize(&decks).unwrap_or(decks);
    let candidate = if candidate.exists() {
        fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf())
    } else {
        candidate.to_path_buf()
    };
    if candidate.starts_with(decks) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "артефакты code-review нельзя сохранять под decks/**",
        ));
    }
    Ok(())
}

fn output_path(root: &Path, path: &Path) -> Result<PathBuf, DomainError> {
    safe_output_path(root, path, false)
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

fn normalize_relative_path(path: &Path) -> Result<String, DomainError> {
    if path.is_absolute() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "путь в аргументе `language --path` должен быть относительным",
        ));
    }
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                let segment = segment.to_str().ok_or_else(|| {
                    DomainError::new(ErrorCode::InvalidRequest, "путь должен быть UTF-8")
                })?;
                if segment.contains(['\\', ':']) {
                    return Err(DomainError::new(
                        ErrorCode::InvalidRequest,
                        "путь содержит запрещённый разделитель",
                    ));
                }
                segments.push(segment);
            }
            Component::CurDir => {}
            _ => {
                return Err(DomainError::new(
                    ErrorCode::InvalidRequest,
                    "путь из `language --path` не должен выходить за корень репозитория",
                ));
            }
        }
    }
    if segments.is_empty() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "аргумент `language --path` должен указывать на файл",
        ));
    }
    Ok(segments.join("/"))
}

fn checked_source_path(root: &Path, relative: &str) -> Result<PathBuf, DomainError> {
    let mut current = root.to_path_buf();
    for segment in relative.split('/') {
        current.push(segment);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            DomainError::new(
                ErrorCode::InputUnreadable,
                format!("не удалось прочитать «{relative}»: {error}"),
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                format!("символьная ссылка в пути `language --path` запрещена: {relative}"),
            ));
        }
    }
    let resolved = fs::canonicalize(&current).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось разрешить «{relative}»: {error}"),
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "путь из `language --path` вышел за корень репозитория",
        ));
    }
    Ok(resolved)
}

fn repository_root(start: &Path) -> Result<PathBuf, DomainError> {
    let start = fs::canonicalize(start).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось открыть корень репозитория: {error}"),
        )
    })?;
    let output = Command::new("git")
        .current_dir(&start)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| {
            DomainError::new(
                ErrorCode::GitEvidenceFailed,
                format!("не удалось запустить Git: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::GitEvidenceFailed,
            "текущий каталог не находится внутри Git-репозитория",
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| {
        DomainError::new(
            ErrorCode::GitEvidenceFailed,
            "Git вернул некорректный путь корня репозитория",
        )
    })?;
    fs::canonicalize(text.trim()).map_err(|error| {
        DomainError::new(
            ErrorCode::GitEvidenceFailed,
            format!("не удалось разрешить корень репозитория: {error}"),
        )
    })
}

fn git_output(root: &Path, args: &[&str]) -> Result<Vec<u8>, std::io::Error> {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(path_string)
        .unwrap_or_else(|_| path.display().to_string())
}

fn scope_error(error: ScopeError) -> DomainError {
    match error {
        ScopeError::InvalidRef { reference, detail } => DomainError::with_details(
            ErrorCode::InvalidGitRef,
            format!("недоступная Git-ссылка «{reference}»: {detail}"),
            crate::details! { "reference" => reference },
        ),
        other => DomainError::new(ErrorCode::GitEvidenceFailed, other.to_string()),
    }
}

fn language_error(error: language::LanguageError) -> DomainError {
    match error {
        language::LanguageError::Preconditions(message) => {
            DomainError::new(ErrorCode::LanguageDecisionInvalid, message)
        }
        language::LanguageError::StaleDigest { path } => DomainError::new(
            ErrorCode::SourceChanged,
            format!("устаревший SHA-256: {path}"),
        ),
        language::LanguageError::StaleAnchor { path } => DomainError::new(
            ErrorCode::SourceChanged,
            format!("исходный текст изменился: {path}"),
        ),
        language::LanguageError::Read { path, source } => DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось прочитать исходник для языковой проверки «{path}»: {source}"),
        ),
        language::LanguageError::SourceChanged { source, written }
        | language::LanguageError::Publication { source, written } => {
            language_publication_error(source, written)
        }
    }
}

fn language_publication_error(source: DomainError, written: Vec<String>) -> DomainError {
    let DomainError {
        code,
        message,
        mut details,
    } = source;
    if let Value::Object(object) = &mut details {
        object.insert("written".into(), serde_json::json!(written));
    } else {
        details = serde_json::json!({"source_details": details, "written": written});
    }
    DomainError::with_details(code, message, details)
}

fn artifact_conflict(path: &Path) -> DomainError {
    DomainError::with_details(
        ErrorCode::ReviewArtifactConflict,
        "путь уже содержит другой артефакт; выберите новое имя, чтобы сохранить оба снимка",
        crate::details! { "path" => path.display().to_string() },
    )
}

fn artifact_write_error(path: &Path, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::WriteFailed,
        format!("не удалось сохранить артефакт code-review: {error}"),
        crate::details! { "path" => path.display().to_string() },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;

    struct GitFixture(PathBuf, #[allow(dead_code)] TempWorkspace);

    impl GitFixture {
        fn new(label: &str) -> Self {
            let workspace = TempWorkspace::create(&format!("anki-review-{label}"))
                .expect("временная рабочая область проекта должна создаваться");
            let path = workspace.path().join("repo");
            fs::create_dir(&path).unwrap();
            git_ok(&path, &["init", "-q"]);
            git_ok(&path, &["config", "user.email", "test@example.invalid"]);
            git_ok(&path, &["config", "user.name", "Test"]);
            git_ok(&path, &["config", "commit.gpgsign", "false"]);
            git_ok(&path, &["config", "core.autocrlf", "false"]);
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(path.join("src/lib.rs"), "pub fn run() { let _ = 1; }\n").unwrap();
            git_ok(&path, &["add", "."]);
            git_ok(&path, &["commit", "-qm", "base"]);
            let tracked = String::from_utf8(
                git_output(&path, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap(),
            )
            .unwrap();
            assert_eq!(tracked, "src/lib.rs\n");
            Self(path, workspace)
        }

        fn add_head(&self, content: &str) {
            fs::write(self.0.join("src/lib.rs"), content).unwrap();
            git_ok(&self.0, &["add", "."]);
            git_ok(&self.0, &["commit", "-qm", "head"]);
        }
    }

    fn git_ok(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} завершился с ошибкой");
    }

    #[test]
    fn rust_source_images_include_only_rust_effective_paths() {
        let image = |text: Option<&str>| match text {
            Some(text) => scope::FileImage {
                state: ImageState::Text,
                size: text.len() as u64,
                object_id: None,
                mode: None,
                text: Some(text.to_owned()),
            },
            None => scope::FileImage {
                state: ImageState::Missing,
                size: 0,
                object_id: None,
                mode: None,
                text: None,
            },
        };
        let file = |path: &str,
                    previous_path: Option<&str>,
                    base_text: Option<&str>,
                    post_text: Option<&str>| ScopedFile {
            path: path.to_owned(),
            previous_path: previous_path.map(str::to_owned),
            status: FileStatus::Modified,
            additions: None,
            deletions: None,
            category: scope::FileCategory::Other,
            surfaces: Vec::new(),
            binary: false,
            base_changed_lines: Vec::new(),
            post_changed_lines: Vec::new(),
            base: image(base_text),
            post: image(post_text),
        };
        let collected = CollectedScope {
            target: GitTarget {
                repository_id: "repo".into(),
                base_sha: "base".into(),
                head_sha: "head".into(),
                merge_base_sha: "base".into(),
            },
            text_image_limit_bytes: 1024,
            files: vec![
                file(
                    "README.md",
                    None,
                    Some("pub fn markdown_base() {}"),
                    Some("pub fn markdown_post() {}"),
                ),
                file(
                    "src/current.rs",
                    Some("docs/old.md"),
                    Some("pub fn old_markdown() {}"),
                    Some("pub fn current_rust() {}"),
                ),
                file(
                    "docs/current.md",
                    Some("src/old.rs"),
                    Some("pub fn old_rust() {}"),
                    Some("pub fn current_markdown() {}"),
                ),
                file(
                    "src/unchanged.rs",
                    None,
                    Some("pub fn rust_base() {}"),
                    Some("pub fn rust_post() {}"),
                ),
            ],
        };

        let (post_sources, base_sources) = rust_sources_from_scope(&collected);
        assert_eq!(
            post_sources
                .iter()
                .map(|source| source.path.as_str())
                .collect::<Vec<_>>(),
            ["src/current.rs", "src/unchanged.rs"]
        );
        assert_eq!(
            base_sources
                .iter()
                .map(|source| source.path.as_str())
                .collect::<Vec<_>>(),
            ["src/old.rs", "src/unchanged.rs"]
        );
    }

    fn test_review_queue(pack: &ReviewPack) -> ReviewQueue {
        let digest = sha256_hex(&json_bytes(pack).unwrap());
        review_queue::build(pack, &digest, &BTreeMap::new()).unwrap()
    }

    fn rust_text_candidate(source: &str, text: &str) -> CandidateEvidence {
        rust_text_candidate_at(source, text, source.find(text).unwrap())
    }

    fn rust_text_candidate_at(source: &str, text: &str, start: usize) -> CandidateEvidence {
        let end = start + text.len();
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "context".into(),
            serde_json::to_value(super::super::language::TextContext::StringLiteral).unwrap(),
        );
        metadata.insert("start".into(), Value::from(start));
        metadata.insert("end".into(), Value::from(end));
        CandidateEvidence {
            id: format!("text-{text}"),
            detector: "residual_foreign_human_text".into(),
            path: "src/context.rs".into(),
            line: Some(
                source[..start]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1,
            ),
            column: Some(1),
            snippet: Some(text.into()),
            origin: CandidateOrigin::IntroducedOrChanged,
            signals: vec!["foreign_text".into()],
            source: "language_policy".into(),
            metadata,
        }
    }

    #[test]
    fn language_stale_and_publication_errors_keep_domain_codes() {
        for error in [
            language::LanguageError::StaleDigest {
                path: "src/lib.rs".into(),
            },
            language::LanguageError::StaleAnchor {
                path: "src/lib.rs".into(),
            },
        ] {
            assert_eq!(language_error(error).code, ErrorCode::SourceChanged);
        }

        let source = DomainError::with_details(
            ErrorCode::SourceChanged,
            "исходник изменился перед записью",
            serde_json::json!({"reason": "source_modified"}),
        );
        let mapped = language_error(language::LanguageError::SourceChanged {
            source,
            written: vec!["src/lib.rs".into()],
        });
        assert_eq!(mapped.code, ErrorCode::SourceChanged);
        assert_eq!(
            mapped.details["reason"],
            serde_json::json!("source_modified")
        );
        assert_eq!(mapped.details["written"], serde_json::json!(["src/lib.rs"]));

        let source = DomainError::with_details(
            ErrorCode::WriteFailed,
            "не удалось сохранить исходник",
            serde_json::json!({"operation": "replace"}),
        );
        let mapped = language_error(language::LanguageError::Publication {
            source,
            written: vec!["src/lib.rs".into()],
        });
        assert_eq!(mapped.code, ErrorCode::WriteFailed);
        assert_eq!(mapped.details["operation"], serde_json::json!("replace"));
        assert_eq!(mapped.details["written"], serde_json::json!(["src/lib.rs"]));
    }

    #[test]
    fn pack_bytes_and_summary_are_deterministic_for_same_snapshot() {
        let repo = GitFixture::new("stable");
        let base = git_output(&repo.0, &["rev-parse", "HEAD"]).unwrap();
        let base = String::from_utf8(base).unwrap().trim().to_owned();
        repo.add_head("pub fn run() { panic!(\"example\"); }\n");
        let head = String::from_utf8(git_output(&repo.0, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        let first = scope::collect_scope(&repo.0, &base, &head).unwrap();
        let pack_a = build_pack(&repo.0, first, false);
        let second = scope::collect_scope(&repo.0, &base, &head).unwrap();
        let pack_b = build_pack(&repo.0, second, false);
        assert_eq!(json_bytes(&pack_a).unwrap(), json_bytes(&pack_b).unwrap());
        assert_eq!(
            human_summary(&pack_a, &test_review_queue(&pack_a)),
            human_summary(&pack_b, &test_review_queue(&pack_b))
        );
        assert!(
            pack_a
                .candidates
                .iter()
                .any(|candidate| candidate.detector == "error_path")
        );
    }

    #[test]
    fn rust_text_roles_follow_calls_attributes_and_test_context() {
        let source = r#"
fn emit() { println!("Log message"); }
fn fail() { panic!("Error message"); }
#[arg(
    help = "Help message"
)]
fn cli() {}
fn ui(view: &mut View) { view.set_text("UI message"); }
fn request(builder: Builder) { builder.header("Content-Type", "application/json"); }
#[serde(rename = "machine_name")]
fn field() {}
#[test]
fn fixture() { assert_eq!(1, 1, "fixture message"); }
"#;
        let index = RustContextIndex::from_sources(&[SourceFile {
            path: "src/context.rs".into(),
            content: source.into(),
        }]);
        for (text, expected) in [
            ("Log message", TextRole::HumanLog),
            ("Error message", TextRole::HumanDiagnostic),
            ("Help message", TextRole::HumanHelp),
            ("UI message", TextRole::HumanUi),
            ("Content-Type", TextRole::ExternalLiteral),
            ("machine_name", TextRole::MachineContract),
            ("fixture message", TextRole::TestFixture),
        ] {
            let candidate = rust_text_candidate(source, text);
            let start = candidate.metadata["start"].as_u64().unwrap() as usize;
            let end = candidate.metadata["end"].as_u64().unwrap() as usize;
            let context = index.lookup_range("src/context.rs", start, end);
            let line = candidate
                .line
                .and_then(|line| SourceLines::new(source).get(source, line));
            assert_eq!(
                classify_rust_text(&candidate, source, &context, line),
                expected,
                "unexpected role for {text}"
            );
        }
    }

    #[test]
    fn rust_test_text_roles_do_not_follow_execution_alone() {
        let source = r#"
fn production_log() { println!("Shared message"); }
#[test]
fn test_log() { eprintln!("Shared message"); }
#[test]
fn fixture() { let _ = include_str!("fixtures/input.json"); }
#[test]
fn json_contract() { let _ = serde_json::json!({"message": "JSON contract"}); }
#[test]
fn unknown_context() { custom_test_macro!("Unknown macro text"); }
"#;
        let index = RustContextIndex::from_sources(&[SourceFile {
            path: "src/context.rs".into(),
            content: source.into(),
        }]);
        let lines = SourceLines::new(source);
        let shared: Vec<_> = source.match_indices("Shared message").collect();
        assert_eq!(shared.len(), 2);
        for (index_in_source, (start, _)) in shared.iter().enumerate() {
            let candidate = rust_text_candidate_at(source, "Shared message", *start);
            let end = start + "Shared message".len();
            let context = index.lookup_range("src/context.rs", *start, end);
            let line = candidate.line.and_then(|line| lines.get(source, line));
            assert_eq!(
                classify_rust_text(&candidate, source, &context, line),
                TextRole::HumanLog
            );
            assert_eq!(
                context.execution,
                if index_in_source == 0 {
                    RustExecutionContext::Runtime
                } else {
                    RustExecutionContext::Test
                }
            );
        }

        for (text, expected) in [
            ("fixtures/input.json", TextRole::TestFixture),
            ("JSON contract", TextRole::MachineContract),
            ("Unknown macro text", TextRole::Unknown),
        ] {
            let candidate = rust_text_candidate(source, text);
            let start = candidate.metadata["start"].as_u64().unwrap() as usize;
            let end = candidate.metadata["end"].as_u64().unwrap() as usize;
            let context = index.lookup_range("src/context.rs", start, end);
            let line = candidate.line.and_then(|line| lines.get(source, line));
            assert_eq!(
                classify_rust_text(&candidate, source, &context, line),
                expected,
                "unexpected role for {text}"
            );
            assert_eq!(context.execution, RustExecutionContext::Test);
        }
    }

    #[test]
    fn source_line_index_matches_lf_crlf_utf8_and_str_lines_edges() {
        let source = "первая\r\n二行\nпоследняя";
        let lines = SourceLines::new(source);
        assert_eq!(lines.get(source, 1), Some("первая"));
        assert_eq!(lines.get(source, 2), Some("二行"));
        assert_eq!(lines.get(source, 3), Some("последняя"));
        assert_eq!(lines.get(source, 0), None);
        assert_eq!(lines.get(source, 4), None);

        let trailing_newline = "one\r\n\n";
        let lines = SourceLines::new(trailing_newline);
        assert_eq!(lines.get(trailing_newline, 1), Some("one"));
        assert_eq!(lines.get(trailing_newline, 2), Some(""));
        assert_eq!(lines.get(trailing_newline, 3), None);
        assert_eq!(SourceLines::new("").get("", 1), None);
    }

    #[test]
    fn rust_images_keep_line_indexes_for_each_git_image() {
        let base = RustImages::new(vec![SourceFile {
            path: "src/image.rs".into(),
            content: "fn base() {}\nsecond base line\n".into(),
        }]);
        let post = RustImages::new(vec![SourceFile {
            path: "src/image.rs".into(),
            content: "первая post строка\r\nпоследняя post строка".into(),
        }]);
        assert_eq!(base.line("src/image.rs", 2), Some("second base line"));
        assert_eq!(post.line("src/image.rs", 1), Some("первая post строка"));
        assert_eq!(post.line("src/image.rs", 2), Some("последняя post строка"));
        assert_eq!(base.line("src/image.rs", 3), None);
        assert_eq!(post.line("src/image.rs", 3), None);
    }

    #[test]
    fn verify_snapshot_path_includes_baseline_head() {
        let repo = GitFixture::new("verify-baseline-path");
        let base = String::from_utf8(git_output(&repo.0, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        repo.add_head("pub fn run() { panic!(\"example\"); }\n");
        let head = String::from_utf8(git_output(&repo.0, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        let collected = scope::collect_scope(&repo.0, &base, &head).unwrap();
        let pack = build_pack(&repo.0, collected, false);

        let mut baseline_a = pack.clone();
        baseline_a.target.head_sha = base.clone();
        let changes_a = delta::compare(&baseline_a, &pack).unwrap();
        let result_a =
            save_snapshot(&repo.0, None, &pack, Some(changes_a), &BTreeMap::new()).unwrap();

        let mut baseline_b = pack.clone();
        baseline_b.target.head_sha = head.clone();
        let changes_b = delta::compare(&baseline_b, &pack).unwrap();
        let result_b =
            save_snapshot(&repo.0, None, &pack, Some(changes_b), &BTreeMap::new()).unwrap();

        assert_eq!(
            result_a.artifact_dir,
            format!(".anki-repo/review/{base}-{base}-{head}")
        );
        assert_eq!(
            result_b.artifact_dir,
            format!(".anki-repo/review/{head}-{base}-{head}")
        );
        assert_ne!(result_a.artifact_dir, result_b.artifact_dir);
        assert!(
            repo.0
                .join(&result_a.artifact_dir)
                .join("delta.json")
                .exists()
        );
        assert!(
            repo.0
                .join(&result_b.artifact_dir)
                .join("delta.json")
                .exists()
        );
    }

    #[test]
    fn local_data_is_ignored_but_untracked_workspace_sources_skip_clippy() {
        let repo = GitFixture::new("untracked-clippy");
        let head = String::from_utf8(git_output(&repo.0, &["rev-parse", "HEAD"]).unwrap())
            .unwrap()
            .trim()
            .to_owned();
        let target = GitTarget {
            repository_id: "test".into(),
            base_sha: head.clone(),
            head_sha: head,
            merge_base_sha: "merge-base".into(),
        };
        assert!(working_tree_matches(&target, &repo.0));

        for path in [
            "decks/local-media/image.png",
            ".asset-store/cache.json",
            ".anki-repo/review/run/review.json",
            ".codex/local/run.json",
            "notes.txt",
            "custom-output/review.json",
        ] {
            let path = repo.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"local data").unwrap();
        }
        assert!(working_tree_matches(&target, &repo.0));

        fs::write(repo.0.join("src/untracked.rs"), "pub fn new_source() {}\n").unwrap();
        assert!(!working_tree_matches(&target, &repo.0));
        fs::remove_file(repo.0.join("src/untracked.rs")).unwrap();
        fs::create_dir_all(repo.0.join(".cargo")).unwrap();
        fs::write(repo.0.join(".cargo/config.toml"), "[build]\n").unwrap();
        assert!(!working_tree_matches(&target, &repo.0));
        fs::remove_file(repo.0.join(".cargo/config.toml")).unwrap();

        fs::write(
            repo.0.join(".gitignore"),
            "Cargo.toml\nCargo.lock\nrust-toolchain\nrust-toolchain.toml\n.cargo/\ntarget/\n*.rs\nignored.txt\n",
        )
        .unwrap();
        assert!(working_tree_matches(&target, &repo.0));
        for relative in [
            "Cargo.toml",
            "nested/Cargo.toml",
            "Cargo.lock",
            "nested/rust-toolchain",
            "rust-toolchain.toml",
            ".cargo/config",
            "workspace/.cargo/config.toml",
            "src/ignored.rs",
        ] {
            let path = repo.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"ignored Cargo input").unwrap();
            assert!(
                !working_tree_matches(&target, &repo.0),
                "изменение входного файла Cargo «{relative}» должно блокировать запуск Clippy"
            );
            fs::remove_file(path).unwrap();
        }
        for relative in [
            "ignored.txt",
            "target/generated.rs",
            "target/Cargo.toml",
            "decks/module/Cargo.toml",
            ".asset-store/module/Cargo.toml",
            ".anki-repo/review/run/Cargo.toml",
            ".codex/local/run/Cargo.toml",
        ] {
            let path = repo.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"ignored local data").unwrap();
            assert!(
                working_tree_matches(&target, &repo.0),
                "изменение локальных данных «{relative}» не должно блокировать запуск Clippy"
            );
            fs::remove_file(path).unwrap();
        }
    }

    fn synthetic_cargo_repo(label: &str) -> GitFixture {
        let repo = GitFixture::new(label);
        fs::write(
            repo.0.join("Cargo.toml"),
            "[package]\nname = \"review_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            repo.0.join("Cargo.lock"),
            "version = 3\n\n[[package]]\nname = \"review_fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(repo.0.join(".gitignore"), "/target/\n").unwrap();
        repo.add_head("pub fn run() {}\n");
        repo
    }

    #[test]
    fn ordinary_collection_never_executes_reviewed_build_script() {
        let repo = synthetic_cargo_repo("execution-boundary");
        let marker = repo.0.parent().unwrap().join(format!(
            "{}-execution-marker",
            repo.0.file_name().unwrap().to_string_lossy()
        ));
        assert!(!marker.exists());
        fs::write(
            repo.0.join("build.rs"),
            format!(
                "fn main() {{ std::fs::write({:?}, b\"executed\").unwrap(); }}\n",
                marker.to_str().unwrap()
            ),
        )
        .unwrap();
        repo.add_head("pub fn run() { let _ = 2; }\n");
        let scope = scope::collect_scope(&repo.0, "HEAD~1", "HEAD").unwrap();
        assert!(working_tree_matches(&scope.target, &repo.0));
        let pack = build_pack(&repo.0, scope, false);
        assert_eq!(pack.tool_runs[0].status, "skipped");
        assert!(
            pack.tool_runs[0]
                .message
                .as_deref()
                .unwrap()
                .contains("--run-clippy")
        );
        assert!(
            !marker.exists(),
            "обычный сбор не должен исполнять build.rs"
        );
        assert!(!repo.0.join("target").exists());
    }

    #[test]
    fn real_clippy_pack_is_stable_and_republication_is_idempotent() {
        // Только доверенная синтетическая программа без зависимостей и build.rs.
        let repo = synthetic_cargo_repo("clippy-stability");
        repo.add_head("pub fn run() { let _ = vec![1].len() == 0; }\n");
        let first = scope::collect_scope(&repo.0, "HEAD~1", "HEAD").unwrap();
        let pack_a = build_pack(&repo.0, first, true);
        assert!(pack_a.tool_runs[0].exit_status.unwrap().success);
        assert_eq!(pack_a.tool_runs[0].status, "diagnostics");
        assert!(!pack_a.diagnostics.is_empty());
        let output = repo.0.join(".anki-repo/review/stable");
        save_snapshot(&repo.0, Some(&output), &pack_a, None, &BTreeMap::new()).unwrap();
        let second = scope::collect_scope(&repo.0, "HEAD~1", "HEAD").unwrap();
        let pack_b = build_pack(&repo.0, second, true);
        assert!(pack_b.tool_runs[0].exit_status.unwrap().success);
        assert_eq!(json_bytes(&pack_a).unwrap(), json_bytes(&pack_b).unwrap());
        assert_eq!(
            human_summary(&pack_a, &test_review_queue(&pack_a)),
            human_summary(&pack_b, &test_review_queue(&pack_b))
        );
        save_snapshot(&repo.0, Some(&output), &pack_b, None, &BTreeMap::new()).unwrap();
        assert!(working_tree_matches(&pack_b.target, &repo.0));
    }

    #[test]
    fn artifact_paths_cannot_write_into_git_metadata() {
        let repo = GitFixture::new("git-metadata-output");
        let requested = repo.0.join(".git/review-artifacts");
        let error = safe_output_path(&repo.0, &requested, true)
            .expect_err("каталог Git не принимает локальные артефакты ревью");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(!requested.exists());
    }

    #[test]
    fn empty_artifact_path_is_rejected_before_path_resolution() {
        let repo = GitFixture::new("empty-artifact-path");
        let error = safe_output_path(&repo.0, Path::new(""), true).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("не может быть пустым"));
        assert!(!repo.0.join("review.json").exists());
    }

    #[test]
    fn artifacts_are_idempotent_but_never_silently_overwritten() {
        let owner = TempWorkspace::create("anki-artifact-write-test")
            .expect("временная рабочая область проекта должна создаваться");
        let temp = owner.path().to_path_buf();
        let first = BTreeMap::from([("review.json", b"{}\n".to_vec())]);
        let second = BTreeMap::from([("review.json", b"{\"different\":true}\n".to_vec())]);
        let directory = temp.join("snapshot");
        fs::create_dir(&directory).unwrap();
        write_directory_once(&directory, &first).unwrap();
        write_directory_once(&directory, &first).unwrap();
        assert_eq!(
            write_directory_once(&directory, &second).unwrap_err().code,
            ErrorCode::ReviewArtifactConflict
        );

        let failing_documents = BTreeMap::from([
            ("a", b"written before failure".to_vec()),
            ("nested/file", b"write failure".to_vec()),
        ]);
        let existing_empty = temp.join("existing-empty");
        fs::create_dir(&existing_empty).unwrap();
        assert_eq!(
            write_directory_once(&existing_empty, &failing_documents)
                .unwrap_err()
                .code,
            ErrorCode::WriteFailed
        );
        assert!(existing_empty.is_dir());
        assert_eq!(fs::read_dir(&existing_empty).unwrap().count(), 0);

        let newly_created = temp.join("newly-created");
        assert_eq!(
            write_directory_once(&newly_created, &failing_documents)
                .unwrap_err()
                .code,
            ErrorCode::WriteFailed
        );
        assert!(!newly_created.exists());
    }

    #[test]
    fn language_scope_refuses_data_and_symlink_sources() {
        let temp = GitFixture::new("language-path");
        fs::create_dir_all(temp.0.join("decks")).unwrap();
        fs::write(temp.0.join("decks/card.json"), "{}\n").unwrap();
        let (sources, _) =
            read_language_paths(&temp.0, &[PathBuf::from("decks/card.json")]).unwrap();
        assert!(sources[0].content.is_empty());
        assert!(language::scan(&sources).candidates.is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.0.join("src/lib.rs"), temp.0.join("src/link.rs"))
                .unwrap();
            assert!(read_language_paths(&temp.0, &[PathBuf::from("src/link.rs")]).is_err());
        }
    }
}
