//! Версионируемые DTO learning-домена.
//!
//! Все структуры — контракт машинного вывода: они сериализуются в JSON с
//! `deny_unknown_fields` там, где документ переносится между клонами или
//! принимается от вызывающей стороны. Тексты описаний, сниппетов и объяснений
//! остаются данными и никогда не интерпретируются как команды.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Уровень доверия к импортированной истории.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Очередь подтверждена точными Git-образами и AST, источник проверен полностью.
    AstAuthenticated,
    /// Запрошена только структурная проверка: запись карантина, в обучении не участвует.
    StructureOnlyQuarantine,
}

impl TrustLevel {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AstAuthenticated => "ast_authenticated",
            Self::StructureOnlyQuarantine => "structure_only_quarantine",
        }
    }

    /// Участвует ли запись в обучении и статистике.
    #[must_use]
    pub const fn participates_in_learning(self) -> bool {
        matches!(self, Self::AstAuthenticated)
    }
}

/// Полнота рассмотрения истории одного импортированного ревью.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedOutcome {
    /// Все кандидаты получили решение, список нерассмотренных пуст.
    FullyReviewed,
    /// Часть кандидатов явно осталась без решения.
    PartiallyReviewed,
    /// Документ семантического разбора отсутствует: решения не заявлены вовсе.
    NoTriage,
    /// Импорт с ослабленной проверкой: запись исключена из обучения.
    Quarantined,
}

impl ReviewedOutcome {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FullyReviewed => "fully_reviewed",
            Self::PartiallyReviewed => "partially_reviewed",
            Self::NoTriage => "no_triage",
            Self::Quarantined => "quarantined",
        }
    }
}

/// Результат операции импорта: повтор отличается от новой записи и от ревизии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    /// Создана новая запись ревью.
    Imported,
    /// Точный повтор того же набора свидетельств: ничего не удвоено.
    NoopExisting,
    /// Другие байты при той же source identity: создана аудируемая ревизия.
    RevisionCreated,
    /// Ослабленная проверка: запись создана карантинной и исключена из обучения.
    Quarantined,
}

impl ImportStatus {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Imported => "imported",
            Self::NoopExisting => "noop_existing",
            Self::RevisionCreated => "revision_created",
            Self::Quarantined => "quarantined",
        }
    }
}

/// Идентичность набора входных документов импорта.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportInputs {
    /// SHA-256 точных байтов `review.json`.
    pub review_pack_sha256: String,
    /// SHA-256 точных байтов `review-queue.json`.
    pub queue_sha256: String,
    /// SHA-256 точных байтов `semantic-triage.json`, если документ передан.
    pub triage_sha256: Option<String>,
    /// SHA-256 точных байтов `result.json` завершённого задания, если он передан.
    pub execution_result_sha256: Option<String>,
    /// Версия схемы review-pack.
    pub review_schema_version: u32,
    /// Версия схемы структурной очереди.
    pub queue_schema_version: u32,
    /// Версия схемы семантического разбора, если документ передан.
    pub triage_schema_version: Option<u32>,
    /// Версия схемы результата изолированной проверки, если он передан.
    pub execution_schema_version: Option<u32>,
    /// Компактная сводка проверенного результата без текста журналов.
    #[serde(default)]
    pub execution_evidence: Option<ExecutionEvidenceSummary>,
    /// Digest набора анализаторов из `tool_runs` пакета.
    pub analyzer_digest: String,
    /// Digest версий правил классификации и семантики пакета.
    pub classifier_digest: String,
}

/// Минимизированное свидетельство результата изолированной проверки.
///
/// Сводка сохраняет машинный статус и признаки полноты, но не копирует stdout,
/// stderr, failure text, argv или содержимое worktree в историю и переносимые
/// архивы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvidenceSummary {
    /// Итог процесса: passed, failed, timed_out, cancelled, unavailable или incomplete.
    pub status: crate::code_review::execution::ExecutionStatus,
    /// Код завершения процесса, если доступен.
    pub exit_code: Option<i32>,
    /// Сигнал завершения процесса, если доступен.
    pub exit_signal: Option<i32>,
    /// Число байтов stdout без их содержания.
    pub stdout_total_bytes: u64,
    /// Число байтов stderr без их содержания.
    pub stderr_total_bytes: u64,
    /// Был ли текст stdout ограничен при формировании результата.
    pub stdout_truncated: bool,
    /// Был ли текст stderr ограничен при формировании результата.
    pub stderr_truncated: bool,
    /// Нормализованное состояние очистки процесса.
    pub process_cleanup: String,
    /// Нормализованное состояние security sandbox.
    pub security_sandbox: String,
    /// Сформированные из полей результата ограничения без произвольного текста.
    pub limitations: Vec<String>,
}

impl ExecutionEvidenceSummary {
    /// Строит минимизированную сводку из уже проверенного результата.
    #[must_use]
    pub fn from_result(result: &crate::code_review::execution::ExecutionResult) -> Self {
        let process_cleanup = match result.enforcement.process_cleanup.as_str() {
            "not_started"
            | "process_group_killed_partial"
            | "process_group_kill_failed"
            | "direct_child_only"
            | "direct_child_reaped_descendants_unverified" => {
                result.enforcement.process_cleanup.clone()
            }
            _ => "unknown".to_owned(),
        };
        let security_sandbox = match result.enforcement.security_sandbox.as_str() {
            "absent" => "absent".to_owned(),
            _ => "unknown".to_owned(),
        };
        let mut limitations = vec![
            "Результат команды не является semantic-решением и не доказывает отсутствие дефекта."
                .to_owned(),
        ];
        if security_sandbox != "absent" {
            limitations.push(
                "Состояние security sandbox не подтверждено этой версией формата.".to_owned(),
            );
        } else {
            limitations.push(
                "Проверка исполнялась без security sandbox, с правами пользователя.".to_owned(),
            );
        }
        if result.stdout.truncated || result.stderr.truncated {
            limitations
                .push("Текст вывода был ограничен; полные логи не импортируются.".to_owned());
        }
        if result.status == crate::code_review::execution::ExecutionStatus::Incomplete {
            limitations.push("Результат исполнения неполон.".to_owned());
        }
        if process_cleanup != "not_started" {
            limitations.push(format!(
                "Состояние очистки процесса: {process_cleanup}; завершение всех потомков не гарантировано."
            ));
        }
        Self {
            status: result.status,
            exit_code: result.exit.as_ref().and_then(|exit| exit.code),
            exit_signal: result.exit.as_ref().and_then(|exit| exit.signal),
            stdout_total_bytes: result.stdout.total_bytes,
            stderr_total_bytes: result.stderr.total_bytes,
            stdout_truncated: result.stdout.truncated,
            stderr_truncated: result.stderr.truncated,
            process_cleanup,
            security_sandbox,
            limitations,
        }
    }
}

/// Явно выбранный вызывающей стороной источник импорта.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequestSource {
    /// Явный вариант источника: `root` либо `snapshot-<32 hex>`.
    pub workspace_variant: String,
    /// Идентификатор каталога ревью (номер PR либо локальная метка), если он известен.
    pub workspace_label: Option<String>,
}

/// Полная идентичность импортированного ревью.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRecord {
    /// Стабильный идентификатор записи ревью в локальной истории.
    pub review_id: String,
    /// Переносимая идентичность Git без пути к локальному клону.
    pub repository_id: String,
    /// Полный base commit.
    pub base_sha: String,
    /// Полный head commit.
    pub head_sha: String,
    /// Общий предок base/head.
    pub merge_base_sha: String,
    /// Вариант источника: `root` либо `snapshot-<32 hex>`.
    pub workspace_variant: String,
    /// Идентификатор каталога ревью, если он был известен при импорте.
    pub workspace_label: Option<String>,
    /// Идентичность входных документов и версий схем.
    pub inputs: ImportInputs,
    /// Уровень доверия к записи.
    pub trust: TrustLevel,
    /// Полнота рассмотрения истории.
    pub outcome: ReviewedOutcome,
    /// Явные ограничения доверия и полноты записи.
    pub limitations: Vec<String>,
    /// Запись, ревизией которой является эта (если это ревизия).
    pub revision_of: Option<String>,
    /// Запись, которая вытеснена этой ревизией.
    pub superseded_by: Option<String>,
    /// Номер, назначенный исходной историей; локально может повториться после удаления
    /// записи или восстановления архива.
    pub revision: u64,
    /// Детерминированная метка времени импорта в секундах Unix.
    pub imported_at: u64,
    /// Раздельные единицы наблюдения записи.
    pub observations: ObservationCounts,
}

/// Раздельные единицы наблюдения; ничего не смешивает между уровнями.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCounts {
    /// Число исходных кандидатов пакета.
    pub raw_candidates: usize,
    /// Число кандидатов, получивших индивидуальное или групповое решение.
    pub covered_candidates: usize,
    /// Число самостоятельных индивидуальных решений.
    pub individual_decisions: usize,
    /// Число групповых решений.
    pub group_decisions: usize,
    /// Число независимых единиц очереди, реально рассмотренных ревьюером.
    pub reviewed_units: usize,
    /// Число независимых единиц очереди, оставшихся без решения.
    pub unresolved_units: usize,
    /// Число замечаний.
    pub findings: usize,
    /// Число замечаний с происхождением `direct_candidate`.
    pub findings_direct_candidate: usize,
    /// Число замечаний с происхождением `candidate_assisted`.
    pub findings_candidate_assisted: usize,
    /// Число замечаний с происхождением `independent`.
    pub findings_independent: usize,
    /// Число явно нерассмотренных кандидатов.
    pub unreviewed_candidates: usize,
    /// Число покрытых кандидатов с решением `uncertain`.
    pub uncertain_candidates: usize,
    /// Число покрытых кандидатов с решением `not_applicable`.
    pub not_applicable_candidates: usize,
    /// Число покрытых кандидатов с решением `confirmed`.
    pub confirmed_candidates: usize,
    /// Число покрытых кандидатов с решением `acceptable`.
    pub acceptable_candidates: usize,
    /// Число покрытых кандидатов с решением `false_positive`.
    pub false_positive_candidates: usize,
}

/// Вид независимой единицы очереди.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewUnitKind {
    /// Единица из одного кандидата.
    Individual,
    /// Единица из структурно однородной группы кандидатов.
    Group,
}

impl ReviewUnitKind {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Individual => "individual",
            Self::Group => "group",
        }
    }
}

/// Одна независимая единица очереди, сохранённая в истории.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewUnitRecord {
    /// Идентификатор единицы очереди.
    pub unit_id: String,
    /// Запись ревью, которой принадлежит единица.
    pub review_id: String,
    /// Вид единицы.
    pub kind: ReviewUnitKind,
    /// Число кандидатов в единице.
    pub candidate_count: usize,
    /// Детерминированный приоритет очереди; learning его не меняет.
    pub priority: String,
    /// Подтверждённые представители единицы.
    pub representative_candidate_ids: Vec<String>,
    /// Решение ревьюера по единице, если оно было заявлено.
    pub disposition: Option<String>,
    /// Код причины решения, если он был заявлен.
    pub reason_code: Option<String>,
    /// Связь единицы со структурной идентичностью.
    pub detector: String,
    /// Источник сигнала.
    pub source: String,
    /// Структурная роль единицы.
    pub role: String,
    /// Роль кода единицы.
    pub code_role: String,
    /// Поверхности исполнения единицы.
    pub surfaces: Vec<String>,
    /// Точный ключ признаков единицы; заполняется при чтении истории и архива.
    #[serde(default)]
    pub feature_map: BTreeMap<String, String>,
}

impl ReviewUnitRecord {
    /// Точный ключ признаков единицы.
    #[must_use]
    pub fn feature_map(&self) -> &BTreeMap<String, String> {
        &self.feature_map
    }

    /// Точная подпись ключа признаков.
    #[must_use]
    pub fn signature(&self) -> String {
        super::patterns::feature_signature(&self.feature_map)
    }

    /// Присоединяет точный ключ признаков к записи.
    #[must_use]
    pub fn with_feature_map(mut self, features: BTreeMap<String, String>) -> Self {
        self.feature_map = features;
        self
    }
}

impl ImportRecord {
    /// Digest точных байтов `review.json`, к которому привязана запись.
    #[must_use]
    pub fn review_pack_sha256(&self) -> &str {
        &self.inputs.review_pack_sha256
    }
}

/// Уровень доверия к срезу поддержки паттерна.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportLevel {
    /// Поддержки достаточно для наблюдения.
    Supported,
    /// Поддержки недостаточно, вывод явно воздержался.
    InsufficientEvidence,
    /// Независимые единицы противоречат друг другу.
    Contradictory,
}

impl SupportLevel {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::Contradictory => "contradictory",
        }
    }
}

/// Прозрачная поддержка вывода по паттерну.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportSummary {
    /// Число независимых линий наблюдения, поддерживающих паттерн.
    ///
    /// Одна линия — это один случай в одной базовой ревизии Git: повторные
    /// наблюдения того же случая в других версиях того же диапазона сюда не
    /// входят и показываются отдельно в `revised_units`.
    pub support_units: usize,
    /// Число записей ревью, наблюдавших паттерн: сырые запуски с повторами.
    pub support_reviews: usize,
    /// Единицы с решением `confirmed`.
    pub confirmed_units: usize,
    /// Единицы с решением `acceptable`.
    pub acceptable_units: usize,
    /// Единицы с решением `false_positive`.
    pub false_positive_units: usize,
    /// Единицы с решением `not_applicable`.
    pub not_applicable_units: usize,
    /// Единицы с решением `uncertain`.
    pub uncertain_units: usize,
    /// Единицы без решения.
    pub unresolved_units: usize,
    /// Связанные замечания с решением `confirmed`.
    pub confirmed_findings: usize,
    /// Распределение происхождения связанных замечаний.
    pub findings_by_provenance: BTreeMap<String, usize>,
    /// Возраст самой свежей поддерживающей единицы в днях от опорного времени.
    pub freshest_age_days: u64,
    /// Возраст самой старой поддерживающей единицы в днях от опорного времени.
    pub oldest_age_days: u64,
    /// Сколько единиц среза оказались повторными наблюдениями той же линии.
    ///
    /// Инвариант: `support_units + revised_units` равно числу единиц среза.
    pub revised_units: usize,
    /// Уровень доверия к срезу.
    pub level: SupportLevel,
    /// Нижняя граница доли единиц с решением `confirmed` по Wilson для 95 % уровня.
    ///
    /// Это мера согласованности наблюдений на выборочной истории, а не
    /// вероятность безопасности нового кода и не precision детектора.
    pub confirmed_share_lower_bound: Option<f64>,
    /// Объяснение выбранного уровня доверия.
    pub explanation: String,
    /// Противоречащие случаи: единицы с решениями, спорящими с большинством.
    pub contradicting_unit_ids: Vec<String>,
}

/// Ссылка на исходное свидетельство исторического случая.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseRef {
    /// Идентификатор записи ревью.
    pub review_id: String,
    /// Идентификатор единицы очереди.
    pub unit_id: String,
    /// Идентификаторы кандидатов из состава единицы.
    pub candidate_ids: Vec<String>,
    /// Безопасный относительный путь источника, если он известен.
    pub path: Option<String>,
    /// Компактный ограниченный сниппет, если он был сохранён.
    pub snippet: Option<String>,
    /// Решение ревьюера по случаю, если оно было заявлено.
    pub disposition: Option<String>,
    /// Уровень доверия записи.
    pub trust: TrustLevel,
    /// Возраст записи в днях от опорного времени.
    pub age_days: u64,
}

/// Ссылка на случай в срезе поддержки паттерна.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternCaseRef {
    /// Ссылка на исходное свидетельство.
    pub case: CaseRef,
    /// Связь с текущим ревью, если она установлена.
    pub link: CaseLinkKind,
}

/// Вид связи повторяющегося случая.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseLinkKind {
    /// Тот же структурный дефект в другой версии того же ревью.
    StructuralRepeat,
    /// Повторное чтение того же неизменного случая в той же итерации.
    SameIteration,
    /// Связь не установлена доказанно.
    Unknown,
}

impl CaseLinkKind {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StructuralRepeat => "structural_repeat",
            Self::SameIteration => "same_iteration",
            Self::Unknown => "unknown",
        }
    }
}

/// Версия генерации накопленной истории, на которой построен производный документ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryGeneration {
    /// Монотонная ревизия накопления истории.
    pub revision: u64,
    /// Число записей ревью с доверием `ast_authenticated`.
    pub trusted_reviews: usize,
    /// Число карантинных записей.
    pub quarantined_reviews: usize,
    /// Число независимых единиц, доступных обучению.
    pub trusted_units: usize,
}

/// Одно правило-наблюдение, выведенное из истории.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternRule {
    /// Точный ключ признаков; приблизительная схожесть сюда не попадает.
    pub key: BTreeMap<String, String>,
    /// Числовое представление ключа признаков для индексации и группировки.
    pub signature: String,
    /// Полный срез поддержки и противоречий.
    pub support: SupportSummary,
}

/// Отчёт по структурным паттернам с явным ограничением вывода.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternReport {
    /// Версия схемы документа.
    pub schema_version: u32,
    /// Версия политики learning.
    pub policy_version: u32,
    /// Генерация истории, на которой построен отчёт.
    pub generation: HistoryGeneration,
    /// Минимум поддержки в независимых единицах для вывода.
    pub min_support_units: usize,
    /// Общее описание применённого правила агрегации.
    pub policy: String,
    /// Чистое наблюдение без вывода: в срезе не нашлось ни одной поддержки.
    pub abstained: bool,
    /// Общее число правил до применения ограничения выдачи.
    pub rules_total: usize,
    /// Ограничения доверия ко всему отчёту.
    pub limitations: Vec<String>,
    /// Отобранные правила в детерминированном порядке.
    pub rules: Vec<PatternRule>,
}

/// Предложение постоянного правила политики; оно никогда не применяется само.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProposal {
    /// Версия схемы документа.
    pub schema_version: u32,
    /// Версия политики learning.
    pub policy_version: u32,
    /// Генерация истории, на которой построено предложение.
    pub generation: HistoryGeneration,
    /// Устойчивый идентификатор предложения.
    pub proposal_id: String,
    /// Идентификатор правила, которое предлагается рассмотреть.
    pub rule_id: String,
    /// Признаки, к которым относится предложение.
    pub key: BTreeMap<String, String>,
    /// Случаи, поддерживающие предложение.
    pub supporting_cases: Vec<CaseRef>,
    /// Случаи, противоречащие предложению.
    pub contradicting_cases: Vec<CaseRef>,
    /// Точные ограничения, почему предложение нельзя применить автоматически.
    pub cautions: Vec<String>,
    /// Имя файла, в который человек может материализовать утверждённую политику.
    pub artifact_path: String,
    /// Признак автоматического применения; всегда `false`.
    pub auto_applied: bool,
}

/// Вид содержательной обратной связи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    /// Оценка полезности сгенерированной рекомендации.
    RecommendationUsefulness,
    /// Содержательная правка исхода семантического рассмотрения.
    SemanticOutcomeRevision,
}

impl FeedbackKind {
    /// Стабильная машиночитаемая метка.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecommendationUsefulness => "recommendation_usefulness",
            Self::SemanticOutcomeRevision => "semantic_outcome_revision",
        }
    }
}

/// Действие обратной связи по отношению к уже сохранённым утверждениям.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackAction {
    /// Новое утверждение; при конфликте импорт отклоняется с `learning_conflict`.
    Append,
    /// Отзыв ранее сохранённого утверждения по его идентификатору.
    Retract,
    /// Явная замена ранее сохранённого утверждения.
    Supersede,
}

/// Действующий исход семантического рассмотрения случая.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackOutcome {
    /// Решение, которое заявил семантический разбор при импорте.
    pub original_disposition: Option<String>,
    /// Действующее решение с учётом не отозванных правок.
    pub effective_disposition: Option<String>,
    /// Идентификаторы утверждений, влияющих на действующий исход.
    pub effective_event_ids: Vec<String>,
    /// Идентификаторы отозванных утверждений по тому же случаю.
    pub retracted_event_ids: Vec<String>,
    /// Есть ли в истории этого случая противоречащие утверждения.
    pub has_conflict: bool,
}

/// Типизированное событие обратной связи с аудитом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackEvent {
    /// Версия схемы документа.
    pub schema_version: u32,
    /// Устойчивый идентификатор события.
    pub event_id: String,
    /// Запись ревью, к которой относится утверждение.
    pub review_id: String,
    /// Единица очереди, к которой относится утверждение.
    pub unit_id: String,
    /// Кандидат, к которому относится утверждение, если оно пошло от кандидата.
    pub candidate_id: Option<String>,
    /// Вид обратной связи.
    pub kind: FeedbackKind,
    /// Действие по отношению к прежним утверждениям.
    pub action: FeedbackAction,
    /// Явно заменяемое утверждение при действии `supersede`.
    pub supersedes_event_id: Option<String>,
    /// Новое действующее решение для содержательной правки.
    pub effective_disposition: Option<String>,
    /// Оценка полезности рекомендации, если это оценка.
    pub usefulness: Option<String>,
    /// Объяснение от автора утверждения.
    pub explanation: String,
    /// Источник утверждения: например `reviewer`, `operator`, `automation`.
    pub provenance: String,
    /// Детерминированная метка авторитетного времени утверждения.
    pub recorded_at: u64,
}

/// Сравнение текущего входа с историей для одной единицы очереди.
///
/// `Eq` реализуется вручную: документ содержит нижнюю границу Уилсона, но при
/// этом обязан оставаться детерминированным и пригодным для точного сравнения
/// идентичных входов (значение `NaN` в расчёте невозможно).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recommendation {
    /// Идентификатор единицы очереди текущего снимка.
    pub unit_id: String,
    /// Ограниченный набор candidate IDs: кандидат для individual или представители группы.
    pub candidate_ids: Vec<String>,
    /// Полный размер единицы.
    pub candidate_count: usize,
    /// Не все candidate IDs перечислены в `candidate_ids`.
    pub candidate_ids_truncated: bool,
    /// Представители единицы в детерминированном порядке очереди.
    pub representative_candidate_ids: Vec<String>,
    /// Детерминированный приоритет очереди; learning его не изменяет.
    pub queue_priority: String,
    /// Отдельная подсказка порядка; baseline priority не меняется скрытно.
    pub suggested_position: String,
    /// Гранулярность проверки.
    pub granularity: String,
    /// Прозрачная причина подсказки.
    pub reason: String,
    /// Точный ключ признаков, по которому искалась история.
    pub pattern_signature: Option<String>,
    /// Срез поддержки истории по этому ключу.
    pub support: Option<SupportSummary>,
    /// Релевантные независимые исторические случаи.
    pub historical_cases: Vec<CaseRef>,
    /// Явные ограничения доверия к этой подсказке.
    pub limitations: Vec<String>,
    /// Обязательный указатель на первичный контекст текущего снимка.
    pub source_reference: String,
}

impl Eq for Recommendation {}

/// Версионируемый recommendations artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recommendations {
    /// Версия схемы документа.
    pub schema_version: u32,
    /// Версия политики learning.
    pub policy_version: u32,
    /// Генерация истории, на которой построен документ.
    pub generation: HistoryGeneration,
    /// Переносимая идентичность Git текущего снимка.
    pub repository_id: String,
    /// Полный base commit текущего снимка.
    pub base_sha: String,
    /// Полный head commit текущего снимка.
    pub head_sha: String,
    /// Общий предок base/head текущего снимка.
    pub merge_base_sha: String,
    /// Вариант источника текущего снимка.
    pub workspace_variant: String,
    /// Digest точных байтов `review.json` текущего снимка.
    pub review_pack_sha256: String,
    /// Digest точных байтов `review-queue.json` текущего снимка.
    pub queue_sha256: String,
    /// Источник входа: `ast_authenticated` либо карантин.
    pub input_trust: TrustLevel,
    /// Признак штатного режима без learning.
    pub learning_disabled: bool,
    /// Порядок: идентификаторы единиц в предложенной последовательности.
    pub suggested_order: Vec<String>,
    /// Отдельные подсказки по единицам.
    pub recommendations: Vec<Recommendation>,
    /// Ограничения доверия ко всему документу.
    pub limitations: Vec<String>,
}

/// Один импортированный кортеж переносимого архива.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningExportManifest {
    /// Версия схемы архива.
    pub export_schema_version: u32,
    /// Версия схемы базы.
    pub schema_version: u32,
    /// Версия политики learning на момент экспорта.
    pub policy_version: u32,
    /// Генерация истории на момент экспорта.
    pub generation: HistoryGeneration,
    /// Число перенесённых записей ревью.
    pub reviews: usize,
    /// Число перенесённых единиц наблюдения.
    pub units: usize,
    /// Число перенесённых замечаний.
    pub findings: usize,
    /// Число перенесённых событий обратной связи.
    pub feedback_events: usize,
    /// Число перенесённых предложений политики.
    pub policy_proposals: usize,
    /// Число перенесённых поисковых случаев.
    ///
    /// Отсутствует в архивах первой версии: такие архивы переносят только
    /// исходные записи, а поисковый индекс в них не входит.
    #[serde(default)]
    pub search_cases: usize,
    /// SHA-256 канонического JSON тела архива.
    pub payload_sha256: String,
}

/// Переносимый архив проверенной истории learning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningExport {
    /// Manifest архива.
    pub manifest: LearningExportManifest,
    /// Записи ревью в детерминированном порядке.
    pub reviews: Vec<ImportRecord>,
    /// Единицы наблюдения в детерминированном порядке.
    pub units: Vec<ReviewUnitRecord>,
    /// Кандидаты с сохранённой структурной классификацией.
    pub candidates: Vec<ExportedCandidate>,
    /// Решения ревьюера с исходными покрытыми ID (с версии архива 4).
    /// Старые архивы решений не сохраняли; они не восстанавливаются из эвристик.
    #[serde(default)]
    pub decisions: Vec<ExportedDecision>,
    /// Замечания в детерминированном порядке.
    pub findings: Vec<ExportedFinding>,
    /// Ссылки «кандидат — замечание».
    pub finding_links: Vec<ExportedFindingLink>,
    /// Связи повторяющихся случаев.
    pub case_links: Vec<ExportedCaseLink>,
    /// События обратной связи.
    pub feedback_events: Vec<FeedbackEvent>,
    /// Предложения политики.
    pub policy_proposals: Vec<PolicyProposal>,
    /// Поисковые случаи: единственная сохранённая копия текстов ревьюера.
    ///
    /// Переносятся как есть, чтобы после восстановления локальный поиск
    /// находил те же случаи. Архивы первой версии поля не содержат.
    #[serde(default)]
    pub search: Vec<ExportedSearchCase>,
}

/// Исходное семантическое решение ревьюера в переносимом архиве.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedDecision {
    /// Ревью, которому принадлежит решение.
    pub review_id: String,
    /// Исходный ID решения, без изменения при переносе.
    pub decision_id: String,
    /// Вид: `individual` либо `group`.
    pub kind: String,
    /// Исходный семантический результат.
    pub disposition: String,
    /// Причина семантического решения.
    pub reason_code: String,
    /// Объяснение ревьюера.
    pub explanation: String,
    /// Число покрытых кандидатов.
    pub candidate_count: usize,
    /// Полное покрытие решения; порядок сохраняется при переносе.
    pub covered_candidate_ids: Vec<String>,
}

/// Поисковый случай, перенесённый вместе с историей.
///
/// Поля повторяют строку `learning_search`: путь в них не хранится, поэтому
/// перенос не может вынести абсолютный путь за пределы клона.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedSearchCase {
    /// Идентификатор случая поиска.
    pub case_id: String,
    /// Запись ревью, к которой относится случай.
    pub review_id: String,
    /// Единица наблюдения, если случай к ней привязан.
    pub unit_id: String,
    /// Кандидат, если случай описывает решение по кандидату.
    pub candidate_id: Option<String>,
    /// Замечание, если случай описывает замечание.
    pub finding_id: Option<String>,
    /// Вид случая: `finding`, `decision` или `group`.
    pub kind: String,
    /// Действующий исход решения, если он есть.
    pub disposition: Option<String>,
    /// Наиболее серьёзная связанная оценка, если она есть.
    pub severity: Option<String>,
    /// Происхождение связанной оценки, если оно есть.
    pub provenance: Option<String>,
    /// Санитизированный текст случая.
    pub text: String,
}

/// Кандидат, перенесённый с сохранённой структурной классификацией.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedCandidate {
    /// Запись ревью.
    pub review_id: String,
    /// Идентификатор кандидата внутри записи.
    pub candidate_id: String,
    /// Единица очереди, содержащая кандидата.
    pub unit_id: String,
    /// Стабильный тип эвристики.
    pub detector: String,
    /// Источник сигнала.
    pub source: String,
    /// Относительный путь источника.
    pub path: String,
    /// Семейство пути без абсолютных компонентов.
    pub path_family: String,
    /// Происхождение сигнала.
    pub origin: String,
    /// Компактный ограниченный сниппет.
    pub snippet: Option<String>,
    /// Сериализованная структурная классификация очереди.
    pub classification_json: String,
}

/// Замечание, перенесённое вместе со структурной сигнатурой.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedFinding {
    /// Запись ревью.
    pub review_id: String,
    /// Идентификатор замечания внутри записи.
    pub finding_id: String,
    /// Серьёзность.
    pub severity: String,
    /// Происхождение замечания.
    pub provenance: String,
    /// Заголовок замечания.
    pub title: String,
    /// Описание замечания.
    pub description: String,
    /// Точный ключ признаков для точного структурного сопоставления.
    pub signature: String,
}

/// Связь кандидата с замечанием.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedFindingLink {
    /// Запись ревью.
    pub review_id: String,
    /// Идентификатор кандидата.
    pub candidate_id: String,
    /// Идентификатор замечания.
    pub finding_id: String,
}

/// Связь повторяющегося случая, перенесённая без абсолютных путей.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedCaseLink {
    /// Запись ревью.
    pub review_id: String,
    /// Идентификатор замечания в этой записи.
    pub finding_id: String,
    /// Связанная запись ревью.
    pub linked_review_id: String,
    /// Связанное замечание.
    pub linked_finding_id: String,
    /// Вид связи.
    pub kind: CaseLinkKind,
    /// Основание связи.
    pub basis: String,
}

/// Состояние локального хранилища learning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningStatus {
    /// Версия схемы документа.
    pub schema_version: u32,
    /// Версия политики learning.
    pub policy_version: u32,
    /// Существует ли файл базы.
    pub present: bool,
    /// Относительный путь базы от корня репозитория.
    pub database_path: String,
    /// Применённая версия схемы базы.
    pub user_version: u32,
    /// Фактически выбранный режим журнала SQLite.
    pub journal_mode: String,
    /// Объяснение выбора режима журнала.
    pub journal_mode_reason: String,
    /// Значение `busy_timeout` в миллисекундах.
    pub busy_timeout_ms: u64,
    /// Доступен ли FTS5 в текущей сборке SQLite.
    pub fts5_available: bool,
    /// Включены ли внешние ключи.
    pub foreign_keys: bool,
    /// Действующая генерация истории.
    pub generation: HistoryGeneration,
    /// Проверка целостности базы.
    pub integrity_ok: bool,
    /// Причина недоступности хранилища, если она есть.
    pub unavailable_reason: Option<String>,
    /// Допустимые пути восстановления без удаления исходных данных.
    pub recovery_paths: Vec<String>,
}

/// Действие обратной связи, принятое хранилищем.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackResult {
    /// Версия схемы документа.
    pub schema_version: u32,
    /// Идентификатор принятого события.
    pub event_id: String,
    /// Идентификатор заменённого события, если замена состоялась.
    pub superseded_event_id: Option<String>,
    /// Идентификатор отозванного события, если был отзыв.
    pub retracted_event_id: Option<String>,
    /// Действующий исход случая после операции.
    pub outcome: FeedbackOutcome,
    /// Ограничения доверия к записи.
    pub limitations: Vec<String>,
}
