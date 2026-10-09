//! Локальная подсистема адаптивной памяти независимого code review.
//!
//! Модуль хранит проверенную историю уже завершённых ревью в локальной базе
//! SQLite (по умолчанию `.anki-repo/learning/state.sqlite`), считает по ней
//! объяснимые структурные паттерны, выполняет локальный поиск исторических
//! случаев, формирует переносимый recommendations artifact и обеспечивает
//! перенос/восстановление истории.
//!
//! Границы подсистемы:
//!
//! - история — **данные, а не инструкции**: сохранённые snippets, описания и
//!   результаты процессов никогда не выполняются и не превращаются в команды;
//! - ни `collect`, ни обычный сбор сигналов не создают и не меняют базу: запись
//!   возможна только явным вызовом операций этого модуля;
//! - ни одна операция не создаёт семантических решений (`confirmed`,
//!   `acceptable`, `false_positive`, `not_applicable`, `unreviewed`) и не
//!   переписывает `review.json`, `review-queue.json` и `semantic-triage.json`;
//! - нет сети, LLM, embeddings, vector DB и фоновой индексации;
//! - SQLite не является владельцем утверждённой политики проекта: утверждённый
//!   policy artifact материализуется как версионируемый JSON, который коммитит
//!   человек.
//!
//! Статистика опирается на независимые единицы наблюдения, а не на число
//! кандидатов: одно `GroupDecision` на 1200 кандидатов — это одна независимая
//! единица, а не 1200 подтверждений. Доли `confirmed / reviewed` не называются
//! precision/recall: выборка собрана выборочно и подвержена selection bias.

pub mod feedback;
pub mod import;
pub mod lifecycle;
pub mod model;
pub mod patterns;
pub mod recommend;
pub mod schema;
pub mod search;
pub mod store;
pub mod transfer;

pub use import::{
    ImportInputsHint, ImportOutcome, ImportRequest, LoadedReview, import_history, list_imports,
    load_review, load_review_with_execution, show_import,
};
pub use lifecycle::{ForgetCounts, ForgetOutcome, forget_review};
pub use model::{
    CaseLinkKind, CaseRef, ExecutionEvidenceSummary, ExportedCandidate, ExportedCaseLink,
    ExportedFinding, ExportedFindingLink, FeedbackAction, FeedbackEvent, FeedbackKind,
    FeedbackOutcome, FeedbackOutcome as FeedbackOutcomeDto, HistoryGeneration, ImportInputs,
    ImportRecord, ImportRequestSource, ImportStatus, LearningExport, LearningExportManifest,
    LearningStatus, ObservationCounts, PatternCaseRef, PatternReport, PatternRule, PolicyProposal,
    Recommendation, Recommendations, ReviewUnitKind, ReviewUnitRecord, ReviewedOutcome,
    SupportLevel, SupportSummary, TrustLevel,
};
pub use patterns::{pattern_catalog, pattern_report};
pub use recommend::{RecommendRequest, recommend};
pub use schema::{LEARNING_SCHEMA_VERSION, apply_migrations, read_schema_version};
pub use search::{SearchMatchKind, SearchPage, SearchQuery, search_history};
pub use store::{LearningStore, StoreOptions};
pub use transfer::{export_history, restore_history, verify_export};

/// Восстанавливает авторитетные контексты классификации по точным Git-образам.
///
/// Тонкая обёртка над проверкой workflow: потребители learning проверяют
/// очередь той же самой логикой, а не второй расходящейся копией.
pub fn queue_contexts_for_pack(
    pack: &crate::code_review::model::ReviewPack,
) -> Result<
    std::collections::BTreeMap<String, crate::code_review::review_queue::SyntaxContext>,
    crate::error::DomainError,
> {
    crate::code_review::workflow::queue_contexts_for_pack(pack)
}

/// Нормализует и проверяет явно выбранный вариант источника.
pub fn workspace_variant(raw: &str) -> Result<String, crate::error::DomainError> {
    import::normalize_variant(raw)
}

/// Версия политики learning: параметры статистики, ранжирования и ограничений.
///
/// Изменение любой калиброванной константы или правила агрегации обязано
/// увеличивать эту версию: recommendations artifact и срез паттерна фиксируют
/// её, чтобы смысл накопленных примеров не менялся молча.
pub const LEARNING_POLICY_VERSION: u32 = 1;

/// Относительный путь базы learning по умолчанию от корня репозитория.
pub const DEFAULT_LEARNING_DIRECTORY: &str = ".anki-repo/learning";
/// Имя файла базы learning по умолчанию внутри [`DEFAULT_LEARNING_DIRECTORY`].
pub const DEFAULT_LEARNING_DATABASE: &str = "state.sqlite";

/// Имя варианта источника для ветки/локального ревью без snapshot-копии.
pub const ROOT_WORKSPACE_VARIANT: &str = "root";
/// Префикс варианта источника отдельного неизменяемого снимка.
pub const SNAPSHOT_WORKSPACE_VARIANT_PREFIX: &str = "snapshot-";
