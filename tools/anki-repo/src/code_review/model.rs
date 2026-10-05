//! Версионируемый публичный формат evidence-pack для независимого code review.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::diagnostics::{CodeReviewDiagnostic, ExitStatusSummary};
use super::language::LanguageScan;
use super::scope::{FileCategory, FileStatus, FileSurface, GitTarget, ImageState, LineRange};

/// Текущая версия JSON-контракта review-pack и delta.
pub const REVIEW_SCHEMA_VERSION: u32 = 1;

/// Неизменяемая цель собранного review evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPack {
    /// Версия внешнего JSON-контракта.
    pub schema_version: u32,
    /// Разрешённые Git refs и object ids.
    pub target: GitTarget,
    /// Фактический scope и машинные диапазоны изменённых строк.
    pub scope: ReviewScope,
    /// Машинные диагностики Cargo/rustc/Clippy; сами по себе они не findings.
    pub diagnostics: Vec<CodeReviewDiagnostic>,
    /// Эвристические кандидаты; ни один не является подтверждённым finding.
    pub candidates: Vec<CandidateEvidence>,
    /// Format-aware кандидаты остаточного иностранного человеческого текста.
    pub language: LanguageScan,
    /// Ссылки на кандидаты соответствующих направлений.
    pub dependencies: Vec<String>,
    /// Ссылки на кандидаты об изменениях тестов.
    pub tests: Vec<String>,
    /// Ссылки на кандидаты об ослаблениях и suppressions.
    pub suppressions: Vec<String>,
    /// Ссылки на кандидаты о конфигурационных и security-sensitive поверхностях.
    pub risk_surfaces: Vec<String>,
    /// Итог и доступность каждого запущенного внешнего анализатора.
    pub tool_runs: Vec<ToolRunEvidence>,
}

/// Машинное описание изменённых файлов без полного содержимого.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewScope {
    /// Общий предок resolved base/head.
    pub merge_base_sha: String,
    /// Предел текстового образа, используемый collector.
    pub text_image_limit_bytes: u64,
    /// Сортированный список путей из base...head.
    pub files: Vec<ReviewFile>,
}

/// Краткая информация о post-image и base-image одного пути.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    /// Путь в head snapshot.
    pub path: String,
    /// Старый путь при rename/copy.
    pub previous_path: Option<String>,
    /// Git-статус изменения.
    pub status: FileStatus,
    /// Число добавленных текстовых строк, если Git может его посчитать.
    pub additions: Option<u64>,
    /// Число удалённых текстовых строк, если Git может его посчитать.
    pub deletions: Option<u64>,
    /// Формат пути по расширению.
    pub category: FileCategory,
    /// Релевантные поверхности для маршрутизации ревью.
    pub surfaces: Vec<FileSurface>,
    /// Является ли хотя бы один image бинарным.
    pub binary: bool,
    /// Состояние файла в base snapshot.
    pub base_state: ImageState,
    /// Размер base-image в байтах.
    pub base_size: u64,
    /// Git object id base-image, если есть.
    pub base_object_id: Option<String>,
    /// Изменённые в диапазоне строки base-image (начало включительно, конец нет).
    pub base_changed_lines: Vec<LineRange>,
    /// Состояние файла в head snapshot.
    pub post_state: ImageState,
    /// Размер post-image в байтах.
    pub post_size: u64,
    /// Git object id post-image, если есть.
    pub post_object_id: Option<String>,
    /// Изменённые в диапазоне строки post-image (начало включительно, конец нет).
    pub post_changed_lines: Vec<LineRange>,
}

/// Candidate/evidence одной детерминированной эвристики.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvidence {
    /// Стабильный id detector-а.
    pub id: String,
    /// Стабильный тип эвристики.
    pub detector: String,
    /// Путь в снимке; для удалённого места это base-путь.
    pub path: String,
    /// Строка, если detector может её локализовать.
    pub line: Option<usize>,
    /// Колонка, если detector может её локализовать.
    pub column: Option<usize>,
    /// Короткий фрагмент, достаточный для перехода к исходнику.
    pub snippet: Option<String>,
    /// Происхождение сигнала относительно рассматриваемого диапазона.
    pub origin: CandidateOrigin,
    /// Короткие машинные причины, сформировавшие candidate.
    pub signals: Vec<String>,
    /// Источник: имя detector-а или policy subsystem.
    pub source: String,
    /// Типизированные дополнительные сведения, стабильные для schema version.
    pub metadata: BTreeMap<String, Value>,
}

/// Происхождение candidate в изменённом файле.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrigin {
    /// Сигнал появился или его строка/значение изменились в base...head.
    IntroducedOrChanged,
    /// Сигнал уже присутствовал в затронутом файле до рассматриваемого диапазона.
    PreExisting,
    /// Detector не смог доказать происхождение.
    Unknown,
}

/// Итог анализатора без дублирования его diagnostics, которые лежат рядом в pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunEvidence {
    /// Имя analyzer-а.
    pub tool: String,
    /// success, diagnostics, skipped, unavailable или failed.
    pub status: String,
    /// Process exit status, если процесс запускался.
    pub exit_status: Option<ExitStatusSummary>,
    /// Количество структурированных diagnostics в общем разделе pack.
    pub diagnostic_count: usize,
    /// Число непонятных JSON-строк.
    pub malformed_lines: usize,
    /// Число валидных, но не относящихся к diagnostics Cargo records.
    pub ignored_records: usize,
    /// Ограниченная сводка stderr, если она есть.
    pub stderr_summary: Option<String>,
    /// Причина unavailable/skipped/error, если она есть.
    pub message: Option<String>,
}

/// Scope и detector-level status для сравнения двух evidence snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDelta {
    /// Версия JSON-контракта.
    pub schema_version: u32,
    /// Идентичность baseline pack.
    pub before: SnapshotIdentity,
    /// Идентичность нового pack.
    pub after: SnapshotIdentity,
    /// Статусы сигналов; здесь не утверждается, что semantic finding исправлен.
    pub candidates: Vec<CandidateChange>,
    /// Статусы structured diagnostics по всему tool run.
    pub diagnostics: Vec<DiagnosticChange>,
    /// Изменение доступности/результата внешних анализаторов.
    pub tool_runs: Vec<ToolRunChange>,
}

/// Минимальная identity snapshot, нужная для проверки совместимости baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    /// Локальная identity общего Git object store без пути и remote URL.
    pub repository_id: String,
    /// Полный base commit.
    pub base_sha: String,
    /// Полный head commit.
    pub head_sha: String,
}

/// Detector-level status одного candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateChange {
    /// Статус candidate в новом снимке.
    pub status: CandidateStatus,
    /// Candidate в baseline, если есть.
    pub before: Option<CandidateEvidence>,
    /// Candidate в новом snapshot, если есть.
    pub after: Option<CandidateEvidence>,
}

/// Состояние эвристического сигнала между snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    StillPresent,
    Gone,
    Changed,
    New,
}

/// Изменение структурированной диагностики между snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticChange {
    /// Статус диагностики в новом снимке.
    pub status: CandidateStatus,
    /// Старая диагностика, если есть.
    pub before: Option<CodeReviewDiagnostic>,
    /// Новая диагностика, если есть.
    pub after: Option<CodeReviewDiagnostic>,
}

/// Изменение состояния отдельного analyzer-а.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunChange {
    /// Стабильное имя внешнего инструмента.
    pub tool: String,
    /// Состояние инструмента в baseline.
    pub before_status: String,
    /// Состояние инструмента в новом прогоне.
    pub after_status: String,
    /// Отличается ли исход диагностики без сравнения с semantic finding.
    pub status_changed: bool,
    /// Число диагностик до и после.
    pub before_diagnostics: usize,
    /// Число диагностик после.
    pub after_diagnostics: usize,
    /// Причина отказа/пропуска нового запуска, если есть.
    pub after_message: Option<String>,
}

impl ReviewPack {
    /// Возвращает все candidates в одном детерминированном списке.
    #[must_use]
    pub fn all_candidates(&self) -> Vec<CandidateEvidence> {
        let mut candidates = self.candidates.clone();
        candidates.extend(self.language.candidates.iter().map(|candidate| {
            let mut metadata = BTreeMap::new();
            metadata.insert(
                "source_sha256".to_owned(),
                Value::String(candidate.source_sha256.clone()),
            );
            metadata.insert("start".to_owned(), Value::from(candidate.start));
            metadata.insert("end".to_owned(), Value::from(candidate.end));
            metadata.insert(
                "context".to_owned(),
                serde_json::to_value(candidate.context).unwrap_or(Value::Null),
            );
            let origin = self
                .scope
                .files
                .iter()
                .find(|file| file.path == candidate.path)
                .map_or(CandidateOrigin::Unknown, |file| {
                    if file.post_changed_lines.iter().any(|range| {
                        u64::try_from(candidate.line)
                            .is_ok_and(|line| range.start <= line && line < range.end)
                    }) {
                        CandidateOrigin::IntroducedOrChanged
                    } else {
                        CandidateOrigin::PreExisting
                    }
                });
            CandidateEvidence {
                id: candidate.id.clone(),
                detector: "residual_foreign_human_text".to_owned(),
                path: candidate.path.clone(),
                line: Some(candidate.line),
                column: Some(candidate.column),
                snippet: Some(candidate.text.clone()),
                origin,
                signals: candidate.signals.clone(),
                source: "language_policy".to_owned(),
                metadata,
            }
        }));
        candidates.sort_by(|a, b| {
            (&a.path, &a.detector, a.line, &a.id).cmp(&(&b.path, &b.detector, b.line, &b.id))
        });
        candidates
    }
}
