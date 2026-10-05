//! Версионируемый публичный формат пакета свидетельств для независимого code review.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::diagnostics::{CodeReviewDiagnostic, ExitStatusSummary};
use super::language::LanguageScan;
use super::scope::{FileCategory, FileStatus, FileSurface, GitTarget, ImageState, LineRange};

/// Текущая версия JSON-контракта review-pack и delta.
pub const REVIEW_SCHEMA_VERSION: u32 = 1;

/// Неизменяемая цель собранного пакета свидетельств ревью.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPack {
    /// Версия внешнего JSON-контракта.
    pub schema_version: u32,
    /// Разрешённые ссылки Git и идентификаторы объектов.
    pub target: GitTarget,
    /// Фактическая область изменений и машинные диапазоны изменённых строк.
    pub scope: ReviewScope,
    /// Машинные диагностики Cargo/rustc/Clippy; сами по себе они не подтверждённые замечания.
    pub diagnostics: Vec<CodeReviewDiagnostic>,
    /// Эвристические кандидаты; ни один не является подтверждённым замечанием.
    pub candidates: Vec<CandidateEvidence>,
    /// Кандидаты на остаточный иноязычный человеческий текст, найденные с учётом формата.
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

/// Машинное описание изменённых файлов без полного текста.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewScope {
    /// Общий предок разрешённых ссылок base/head.
    pub merge_base_sha: String,
    /// Предел размера текста, используемый сборщиком.
    pub text_image_limit_bytes: u64,
    /// Сортированный список путей из base...head.
    pub files: Vec<ReviewFile>,
}

/// Краткая информация об исходной и новой версиях одного пути.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    /// Путь в снимке HEAD.
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
    /// Есть ли среди версий файла хотя бы один бинарный образ.
    pub binary: bool,
    /// Состояние файла в снимке базового коммита.
    pub base_state: ImageState,
    /// Размер исходного образа в байтах.
    pub base_size: u64,
    /// Идентификатор Git-объекта исходного образа, если он есть.
    pub base_object_id: Option<String>,
    /// Строки исходного образа, изменённые в диапазоне (начало включено, конец нет).
    pub base_changed_lines: Vec<LineRange>,
    /// Состояние файла в снимке HEAD.
    pub post_state: ImageState,
    /// Размер нового образа в байтах.
    pub post_size: u64,
    /// Идентификатор Git-объекта нового образа, если он есть.
    pub post_object_id: Option<String>,
    /// Строки нового образа, изменённые в диапазоне (начало включено, конец нет).
    pub post_changed_lines: Vec<LineRange>,
}

/// Свидетельство о кандидате одной детерминированной эвристики.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvidence {
    /// Стабильный идентификатор детектора.
    pub id: String,
    /// Стабильный тип эвристики.
    pub detector: String,
    /// Путь в снимке; для удалённого места это base-путь.
    pub path: String,
    /// Строка, если детектор может её локализовать.
    pub line: Option<usize>,
    /// Колонка, если детектор может её локализовать.
    pub column: Option<usize>,
    /// Короткий фрагмент, достаточный для перехода к исходнику.
    pub snippet: Option<String>,
    /// Происхождение сигнала относительно рассматриваемого диапазона.
    pub origin: CandidateOrigin,
    /// Краткие машинные причины, по которым сформирован кандидат.
    pub signals: Vec<String>,
    /// Источник: имя детектора или подсистемы правил.
    pub source: String,
    /// Типизированные дополнительные сведения, стабильные для schema version.
    pub metadata: BTreeMap<String, Value>,
}

/// Происхождение кандидата в изменённом файле.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrigin {
    /// Сигнал появился или его строка/значение изменились в base...head.
    IntroducedOrChanged,
    /// Сигнал уже присутствовал в затронутом файле до рассматриваемого диапазона.
    PreExisting,
    /// Детектор не смог установить происхождение.
    Unknown,
}

/// Итог анализатора без дублирования диагностик, которые хранятся рядом в review-pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunEvidence {
    /// Имя анализатора.
    pub tool: String,
    /// success, diagnostics, skipped, unavailable или failed.
    pub status: String,
    /// Состояние завершения процесса, если его запускали.
    pub exit_status: Option<ExitStatusSummary>,
    /// Количество структурированных диагностик в общем разделе review-pack.
    pub diagnostic_count: usize,
    /// Число непонятных JSON-строк.
    pub malformed_lines: usize,
    /// Число корректных записей Cargo, не относящихся к диагностикам.
    pub ignored_records: usize,
    /// Ограниченная сводка stderr, если она есть.
    pub stderr_summary: Option<String>,
    /// Причина unavailable/skipped/error, если она есть.
    pub message: Option<String>,
}

/// Область изменений и состояния детекторов для сравнения двух пакетов свидетельств.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDelta {
    /// Версия JSON-контракта.
    pub schema_version: u32,
    /// Идентичность исходного пакета.
    pub before: SnapshotIdentity,
    /// Идентичность нового pack.
    pub after: SnapshotIdentity,
    /// Статусы сигналов; сами по себе они не подтверждают, что замечание исправлено.
    pub candidates: Vec<CandidateChange>,
    /// Статусы структурированных диагностик по всему запуску инструмента.
    pub diagnostics: Vec<DiagnosticChange>,
    /// Изменение доступности/результата внешних анализаторов.
    pub tool_runs: Vec<ToolRunChange>,
}

/// Минимальная идентичность снимка, необходимая для проверки совместимости с исходным пакетом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    /// Переносимый идентификатор совместимости точной базовой Git-ревизии.
    pub repository_id: String,
    /// Полный base commit.
    pub base_sha: String,
    /// Полный head commit.
    pub head_sha: String,
}

/// Статус одного кандидата на уровне детектора.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateChange {
    /// Статус кандидата в новом снимке.
    pub status: CandidateStatus,
    /// Кандидат в исходном пакете, если есть.
    pub before: Option<CandidateEvidence>,
    /// Кандидат в новом снимке, если есть.
    pub after: Option<CandidateEvidence>,
}

/// Состояние эвристического сигнала между снимками.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    StillPresent,
    Gone,
    Changed,
    New,
}

/// Изменение структурированной диагностики между снимками.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticChange {
    /// Статус диагностики в новом снимке.
    pub status: CandidateStatus,
    /// Старая диагностика, если есть.
    pub before: Option<CodeReviewDiagnostic>,
    /// Новая диагностика, если есть.
    pub after: Option<CodeReviewDiagnostic>,
}

/// Изменение состояния отдельного анализатора.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunChange {
    /// Стабильное имя внешнего инструмента.
    pub tool: String,
    /// Состояние инструмента в исходном пакете.
    pub before_status: String,
    /// Состояние инструмента в новом прогоне.
    pub after_status: String,
    /// Отличается ли исход диагностики без оценки самого замечания.
    pub status_changed: bool,
    /// Число диагностик до и после.
    pub before_diagnostics: usize,
    /// Число диагностик после.
    pub after_diagnostics: usize,
    /// Причина отказа/пропуска нового запуска, если есть.
    pub after_message: Option<String>,
}

impl ReviewPack {
    /// Возвращает всех кандидатов одним детерминированным списком.
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
                    if file.binary {
                        return CandidateOrigin::Unknown;
                    }
                    let Ok(start) = u64::try_from(candidate.line) else {
                        return CandidateOrigin::Unknown;
                    };
                    let Ok(newlines) =
                        u64::try_from(candidate.text.bytes().filter(|byte| *byte == b'\n').count())
                    else {
                        return CandidateOrigin::Unknown;
                    };
                    let Some(end) = start
                        .checked_add(newlines)
                        .and_then(|line| line.checked_add(1))
                    else {
                        return CandidateOrigin::Unknown;
                    };
                    // Байтовый span может занимать несколько строк: изменение
                    // внутри комментария/литерала относится к тому же вхождению.
                    if file
                        .post_changed_lines
                        .iter()
                        .any(|range| range.start < end && start < range.end)
                    {
                        CandidateOrigin::IntroducedOrChanged
                    } else if file.base_state == ImageState::Text {
                        CandidateOrigin::PreExisting
                    } else {
                        CandidateOrigin::Unknown
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_review::language::{self, SourceFile};
    use crate::code_review::scope::FileStatus;

    fn language_pack(text: &str, changed: Vec<LineRange>) -> ReviewPack {
        let language = language::scan(&[SourceFile {
            path: "src/lib.rs".into(),
            content: text.into(),
        }]);
        assert!(
            !language.candidates.is_empty(),
            "fixture содержит человеческий текст"
        );
        ReviewPack {
            schema_version: REVIEW_SCHEMA_VERSION,
            target: GitTarget {
                repository_id: "fixture-repo".into(),
                base_sha: "base".into(),
                head_sha: "head".into(),
                merge_base_sha: "base".into(),
            },
            scope: ReviewScope {
                merge_base_sha: "base".into(),
                text_image_limit_bytes: 1024,
                files: vec![ReviewFile {
                    path: "src/lib.rs".into(),
                    previous_path: None,
                    status: FileStatus::Modified,
                    additions: None,
                    deletions: None,
                    category: FileCategory::Rust,
                    surfaces: Vec::new(),
                    binary: false,
                    base_state: ImageState::Text,
                    base_size: 0,
                    base_object_id: None,
                    base_changed_lines: Vec::new(),
                    post_state: ImageState::Text,
                    post_size: text.len() as u64,
                    post_object_id: None,
                    post_changed_lines: changed,
                }],
            },
            diagnostics: Vec::new(),
            candidates: Vec::new(),
            language,
            dependencies: Vec::new(),
            tests: Vec::new(),
            suppressions: Vec::new(),
            risk_surfaces: Vec::new(),
            tool_runs: Vec::new(),
        }
    }

    #[test]
    fn language_origin_covers_middle_and_last_lines_of_multiline_spans() {
        for text in [
            "/* Existing English opening\nChanged English middle\nExisting English closing */\n",
            "const MESSAGE: &str = r#\"Existing English opening\nChanged English middle\nExisting English closing\"#;\n",
        ] {
            for changed_line in [2, 3] {
                let pack = language_pack(
                    text,
                    vec![LineRange {
                        start: changed_line,
                        end: changed_line + 1,
                    }],
                );
                assert_eq!(pack.language.candidates.len(), 1);
                assert_eq!(pack.language.candidates[0].line, 1);
                let candidates = pack.all_candidates();
                assert_eq!(candidates[0].origin, CandidateOrigin::IntroducedOrChanged);
            }
        }
    }

    #[test]
    fn language_origin_uses_each_occurrence_and_preserves_unchanged_text() {
        let pack = language_pack(
            "// Existing English explanation\n// Existing English explanation\n",
            vec![LineRange { start: 2, end: 3 }],
        );
        let candidates = pack.all_candidates();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].origin, CandidateOrigin::PreExisting);
        assert_eq!(candidates[1].origin, CandidateOrigin::IntroducedOrChanged);
        assert_eq!(candidates[0].snippet, candidates[1].snippet);
        assert_ne!(candidates[0].id, candidates[1].id);

        let unchanged = language_pack(
            "/* Existing English opening\nExisting English middle\nExisting English closing */\nfn changed() {}\n",
            vec![LineRange { start: 4, end: 5 }],
        );
        assert_eq!(
            unchanged.all_candidates()[0].origin,
            CandidateOrigin::PreExisting
        );
    }

    #[test]
    fn language_origin_is_unknown_without_textual_baseline_or_matching_scope() {
        let mut pack = language_pack("// Existing English explanation\n", Vec::new());
        pack.scope.files[0].base_state = ImageState::InvalidUtf8;
        assert_eq!(pack.all_candidates()[0].origin, CandidateOrigin::Unknown);
        pack.scope.files.clear();
        assert_eq!(pack.all_candidates()[0].origin, CandidateOrigin::Unknown);
    }

    #[test]
    fn language_origin_is_unknown_for_binary_scope_files() {
        let mut pack = language_pack("// Existing English explanation\n", Vec::new());
        let file = &mut pack.scope.files[0];
        file.binary = true;
        file.post_state = ImageState::Binary;
        assert_eq!(pack.all_candidates()[0].origin, CandidateOrigin::Unknown);
    }
}
