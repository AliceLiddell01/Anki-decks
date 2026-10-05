//! CLI-сценарии для неизменяемых свидетельств code-review и языковой политики.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

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
use super::scope::{
    self, CollectedScope, FileStatus, GitTarget, ImageState, ScopeError, ScopedFile,
};

/// Верхняя граница читаемого артефакта ревью.
pub const MAX_REVIEW_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
/// Верхняя граница языкового артефакта.
pub const MAX_LANGUAGE_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

/// Краткий результат создания пакета для CLI.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    pub artifact_dir: String,
    pub target: GitTarget,
    pub files: usize,
    pub candidates: usize,
    pub diagnostics: usize,
    pub tool_runs: Vec<ToolRunEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<ReviewDelta>,
}

/// Краткий результат language scan/check.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageSummary {
    pub artifact: String,
    pub schema_version: u32,
    pub files: usize,
    pub candidates: usize,
    pub skipped: usize,
}

/// Краткий результат language apply без копирования всего содержимого файлов в stdout.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageApplySummary {
    pub applied: bool,
    pub files: usize,
    pub replacements: usize,
    pub results: Vec<AppliedFileSummary>,
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
    let pack = build_pack(&root, collected, run_clippy);
    save_snapshot(&root, out_dir, &pack, None)
}

/// Пересобирает свидетельства от зафиксированной base и создаёт дельту детекторов.
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
        "исходный review-pack",
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
            "новый снимок относится к другому репозиторию или merge-base относительно исходного пакета",
            crate::details! {
                "baseline_base_sha" => baseline.target.base_sha,
                "new_base_sha" => collected.target.base_sha,
                "baseline_merge_base_sha" => baseline.target.merge_base_sha,
                "new_merge_base_sha" => collected.target.merge_base_sha,
            },
        ));
    }
    let pack = build_pack(&root, collected, run_clippy);
    let changes = delta::compare(&baseline, &pack)?;
    save_snapshot(&root, out_dir, &pack, Some(changes))
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
        "исходный review-pack",
    )?;
    let after: ReviewPack = read_json(after_path, MAX_REVIEW_ARTIFACT_BYTES, "новый review-pack")?;
    let changes = delta::compare(&before, &after)?;
    if let Some(path) = output {
        let bytes = json_bytes(&changes)?;
        let root = repository_root(Path::new("."))?;
        let path = output_path(&root, path)?;
        write_document_once(&path, &bytes)?;
    }
    Ok(changes)
}

/// Сканирует явные пути относительно репозитория или полные новые версии файлов review-pack.
pub fn scan_language(
    root_arg: &Path,
    paths: &[PathBuf],
    pack_path: Option<&Path>,
    output: &Path,
) -> Result<LanguageSummary, DomainError> {
    let root = repository_root(root_arg)?;
    let (sources, mut skipped) = if let Some(pack_path) = pack_path {
        let pack: ReviewPack = read_json(pack_path, MAX_REVIEW_ARTIFACT_BYTES, "review-pack")?;
        delta::validate_review_pack(&pack)?;
        let collected = scope::collect_scope(&root, &pack.target.base_sha, &pack.target.head_sha)
            .map_err(scope_error)?;
        if collected.target.repository_id != pack.target.repository_id
            || collected.target.base_sha != pack.target.base_sha
            || collected.target.head_sha != pack.target.head_sha
        {
            return Err(DomainError::new(
                ErrorCode::BaselineMismatch,
                "review-pack относится к другому репозиторию или Git-снимку",
            ));
        }
        sources_from_scope(&collected)
    } else {
        if paths.is_empty() {
            return Err(DomainError::new(
                ErrorCode::Usage,
                "language scan требует хотя бы один --path или --pack",
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
    let previous: LanguageScan =
        read_json(scan_path, MAX_LANGUAGE_ARTIFACT_BYTES, "language scan")?;
    if previous.schema_version != language::LANGUAGE_SCHEMA_VERSION {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!(
                "неподдерживаемая версия language scan: {}",
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

/// Проверяет решения в dry-run и применяет только явные `replace` с `apply_changes`.
pub fn apply_language(
    root_arg: &Path,
    decisions_path: &Path,
    apply_changes: bool,
) -> Result<LanguageApplySummary, DomainError> {
    let root = repository_root(root_arg)?;
    let decisions: LanguageDecisions = read_json(
        decisions_path,
        MAX_LANGUAGE_ARTIFACT_BYTES,
        "language decisions",
    )?;
    let result = language::apply(&root, &decisions, apply_changes).map_err(language_error)?;
    Ok(apply_summary(result))
}

/// Готовит сводку и локальные артефакты пакета и, при verify, дельту.
fn save_snapshot(
    root: &Path,
    out_dir: Option<&Path>,
    pack: &ReviewPack,
    changes: Option<ReviewDelta>,
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
    documents.insert("review.json", json_bytes(pack)?);
    documents.insert("review.txt", human_summary(pack).into_bytes());
    if let Some(delta) = &changes {
        documents.insert("delta.json", json_bytes(delta)?);
    }
    write_directory_once(&directory, &documents)?;
    let tool_runs = pack.tool_runs.clone();
    Ok(SnapshotSummary {
        artifact_dir: display_path(&directory, root),
        target: pack.target.clone(),
        files: pack.scope.files.len(),
        candidates: pack.all_candidates().len(),
        diagnostics: pack.diagnostics.len(),
        tool_runs,
        delta: changes,
    })
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
    output.split(|byte| *byte == 0).any(|path| {
        if path.is_empty() {
            return false;
        }
        if [
            b"decks/".as_slice(),
            b".asset-store/".as_slice(),
            b".anki-repo/review/".as_slice(),
            b".codex/local/".as_slice(),
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
        {
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
    })
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

fn human_summary(pack: &ReviewPack) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(text, "Снимок пакета свидетельств v{}", pack.schema_version);
    let _ = writeln!(text, "База: {}", pack.target.base_sha);
    let _ = writeln!(text, "HEAD: {}", pack.target.head_sha);
    let _ = writeln!(
        text,
        "Общий предок (merge-base): {}",
        pack.target.merge_base_sha
    );
    let _ = writeln!(text, "Файлов: {}", pack.scope.files.len());
    for file in &pack.scope.files {
        let _ = writeln!(
            text,
            "  {} {} +{} -{} [{}]",
            file.path,
            file_status_label(&file.status),
            file.additions.map_or_else(|| "?".into(), |n| n.to_string()),
            file.deletions.map_or_else(|| "?".into(), |n| n.to_string()),
            file.surfaces
                .iter()
                .map(|surface| format!("{surface:?}").to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    let candidates = pack.all_candidates();
    let _ = writeln!(
        text,
        "Кандидатов: {} (требуют семантической проверки)",
        candidates.len()
    );
    for candidate in candidates {
        let location = candidate.line.map_or_else(
            || candidate.path.clone(),
            |line| format!("{}:{line}", candidate.path),
        );
        let snippet = candidate
            .snippet
            .as_deref()
            .map(single_line)
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "  {} {} {}: {}{}",
            candidate.detector,
            location,
            candidate_origin_label(candidate.origin),
            candidate.signals.join(","),
            if snippet.is_empty() {
                String::new()
            } else {
                format!(" — {snippet}")
            }
        );
    }
    let _ = writeln!(
        text,
        "Диагностик: {} (сами по себе не являются подтверждёнными замечаниями)",
        pack.diagnostics.len()
    );
    for diagnostic in &pack.diagnostics {
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
            single_line(&diagnostic.message)
        );
    }
    let _ = writeln!(
        text,
        "Кандидатов текста: {}",
        pack.language.candidates.len()
    );
    for run in &pack.tool_runs {
        let _ = writeln!(
            text,
            "Анализатор {}: {} (диагностик: {})",
            run.tool, run.status, run.diagnostic_count
        );
        if let Some(message) = &run.message {
            let _ = writeln!(text, "  {}", single_line(message));
        }
    }
    text
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

fn candidate_origin_label(origin: CandidateOrigin) -> &'static str {
    match origin {
        CandidateOrigin::IntroducedOrChanged => "внесён или изменён диапазоном",
        CandidateOrigin::PreExisting => "существовал в base",
        CandidateOrigin::Unknown => "происхождение неизвестно",
    }
}

fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], "↵")
}

fn read_json<T: DeserializeOwned>(path: &Path, limit: u64, what: &str) -> Result<T, DomainError> {
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
    serde_json::from_slice(&bytes).map_err(|error| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("некорректный JSON в {what}: {error}"),
        )
    })
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
    if directory.exists() {
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
        return Err(artifact_conflict(directory));
    }
    let parent = directory.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| artifact_write_error(parent, &error))?;
    fs::create_dir(directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            artifact_conflict(directory)
        } else {
            artifact_write_error(directory, &error)
        }
    })?;
    let mut written = Vec::new();
    for (name, bytes) in documents {
        let path = directory.join(name);
        if let Err(error) = write_new_file(&path, bytes) {
            for written_path in written {
                let _ = fs::remove_file(written_path);
            }
            let _ = fs::remove_dir(directory);
            return Err(artifact_write_error(&path, &error));
        }
        written.push(path);
    }
    Ok(())
}

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
    if requested.exists()
        && fs::symlink_metadata(&requested).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
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
    let resolved = if resolved.exists() {
        if resolved.is_symlink() {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "символьная ссылка как путь артефакта запрещена",
            ));
        }
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
            "language --path должен быть относительным",
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
                    "language --path не должен выходить за корень репозитория",
                ));
            }
        }
    }
    if segments.is_empty() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "language --path должен указывать на файл",
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
                format!("символьная ссылка в language path запрещена: {relative}"),
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
            "language path вышел за корень репозитория",
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
    use std::time::{SystemTime, UNIX_EPOCH};

    struct GitFixture(PathBuf);

    impl GitFixture {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "anki-review-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            git_ok(&path, &["init", "-q"]);
            git_ok(&path, &["config", "user.email", "test@example.invalid"]);
            git_ok(&path, &["config", "user.name", "Test"]);
            git_ok(&path, &["config", "commit.gpgsign", "false"]);
            git_ok(&path, &["config", "core.autocrlf", "false"]);
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(path.join("src/lib.rs"), "pub fn run() { let _ = 1; }\n").unwrap();
            git_ok(&path, &["add", "."]);
            git_ok(&path, &["commit", "-qm", "base"]);
            Self(path)
        }

        fn add_head(&self, content: &str) {
            fs::write(self.0.join("src/lib.rs"), content).unwrap();
            git_ok(&self.0, &["add", "."]);
            git_ok(&self.0, &["commit", "-qm", "head"]);
        }
    }

    impl Drop for GitFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
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
        assert_eq!(human_summary(&pack_a), human_summary(&pack_b));
        assert!(
            pack_a
                .candidates
                .iter()
                .any(|candidate| candidate.detector == "error_path")
        );
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
        let result_a = save_snapshot(&repo.0, None, &pack, Some(changes_a)).unwrap();

        let mut baseline_b = pack.clone();
        baseline_b.target.head_sha = head.clone();
        let changes_b = delta::compare(&baseline_b, &pack).unwrap();
        let result_b = save_snapshot(&repo.0, None, &pack, Some(changes_b)).unwrap();

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
        save_snapshot(&repo.0, Some(&output), &pack_a, None).unwrap();
        let second = scope::collect_scope(&repo.0, "HEAD~1", "HEAD").unwrap();
        let pack_b = build_pack(&repo.0, second, true);
        assert!(pack_b.tool_runs[0].exit_status.unwrap().success);
        assert_eq!(json_bytes(&pack_a).unwrap(), json_bytes(&pack_b).unwrap());
        assert_eq!(human_summary(&pack_a), human_summary(&pack_b));
        save_snapshot(&repo.0, Some(&output), &pack_b, None).unwrap();
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
        let temp = std::env::temp_dir().join(format!("anki-artifact-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let first = BTreeMap::from([("review.json", b"{}\n".to_vec())]);
        let second = BTreeMap::from([("review.json", b"{\"different\":true}\n".to_vec())]);
        let directory = temp.join("snapshot");
        write_directory_once(&directory, &first).unwrap();
        write_directory_once(&directory, &first).unwrap();
        assert_eq!(
            write_directory_once(&directory, &second).unwrap_err().code,
            ErrorCode::ReviewArtifactConflict
        );
        fs::remove_dir_all(temp).unwrap();
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
