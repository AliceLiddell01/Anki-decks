//! Самостоятельная командная строка для получения и публикации японского ударения.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::batch_runtime::SafeBatchRuntime;
use crate::browser_runtime::BrowserSession;
use crate::diagnostics::{OutputMode as DiagnosticOutputMode, RunLogGuard, safe_message};
use crate::domain::AssetDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::jpdb::{
    JpdbPitchAcquisitionReport, JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchProvider,
    JpdbPitchQuery, JpdbPitchRequest, JpdbPitchSelection, JpdbPitchStage,
    pitch_browser_runtime_config,
};
use crate::model::{
    AssetIdentity, AssetRecord, HumanDecision, LifecycleState, Provenance, SemanticStatus,
};
use crate::pitch_accent::{PitchAccentDomainPolicy, PitchAccentImageValidator};
use crate::pitch_batch::{
    MAX_PITCH_BATCH_REASON_BYTES, PITCH_PLAN_SCHEMA_VERSION, PitchAccentBatch,
    PitchAccentBatchRuntime, PitchBatchItemStatus, PitchBatchItemToken, PitchBatchOutcome,
    PitchBatchOwnerSnapshot, PitchBatchPlanIdentity, PitchBatchPublicationStatus,
    is_retryable_failure,
};
use crate::store::{AssetStore, HumanAttestationRequest, StoreOptions, VerifiedIngestRequest};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

const MAX_PLAN_BYTES: u64 = 8 * 1024 * 1024;

/// Командная строка предметной области `pitch_accent`.
#[derive(Debug, Parser)]
#[command(
    name = "pitch-assets",
    version,
    about = "Получение и verified-only публикация изображений pitch accent"
)]
pub struct PitchCli {
    /// Корень локального хранилища ударений; по умолчанию `.asset-store/pitch-accent`.
    #[arg(long, global = true)]
    pub store: Option<PathBuf>,
    /// Корень рабочей копии, относительно которого защищается дерево `decks/`.
    #[arg(long, global = true, default_value = ".")]
    pub repository_root: PathBuf,
    /// Формат вывода.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub output: OutputFormat,
    #[command(subcommand)]
    pub command: PitchCommand,
}

#[derive(Debug, Subcommand)]
pub enum PitchCommand {
    /// Просмотр и проверка канонического корпуса ударений без записи.
    Corpus {
        #[command(subcommand)]
        command: CorpusCommand,
    },
    /// Одноразовое получение простого запроса или набора запросов с версией схемы.
    Ensure {
        /// Строгий JSON-план с версией схемы. Несовместим с аргументами одного запроса.
        #[arg(long, conflicts_with_all = ["surface", "reading", "vocabulary_id", "detail_url"])]
        plan: Option<PathBuf>,
        /// Точная словоформа (`surface`) одного запроса.
        #[arg(long)]
        surface: Option<String>,
        /// Необязательное точное чтение запроса.
        #[arg(long, requires = "surface")]
        reading: Option<String>,
        /// Явный ID словарной записи JPDB для разрешения неоднозначности.
        #[arg(long, requires_all = ["surface", "detail_url"])]
        vocabulary_id: Option<u64>,
        /// Точный маршрут страницы словарной записи JPDB для явного выбора.
        #[arg(long, requires_all = ["surface", "vocabulary_id"])]
        detail_url: Option<String>,
        /// Разрешает адресное повторное получение существующей идентичности с CAS по текущему SHA.
        #[arg(long)]
        refresh: bool,
    },
    /// Возобновляемый цикл обработки сохранённого пакета.
    Batch {
        #[command(subcommand)]
        command: PitchBatchCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CorpusCommand {
    /// Выводит проверенные канонические записи без чтения временных кандидатов.
    List,
    /// Проверяет весь присутствующий корпус без создания временных данных или lock-файлов.
    Check,
}

#[derive(Debug, Subcommand)]
pub enum PitchBatchCommand {
    /// Создаёт пакет из JSON-плана с версией схемы.
    Start {
        #[arg(long)]
        batch_id: Option<String>,
        #[arg(long, required = true)]
        plan: PathBuf,
    },
    /// Продолжает пакет; полученные результаты сохраняются после каждого элемента.
    Run {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Возобновляет пакет тем же путём обработки, что и `run`.
    Resume {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Сверяет сохранённое состояние с каноническим хранилищем без сетевого получения.
    Status {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Создаёт локальный HTML-отчёт в каталоге временных данных пакета.
    Review {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Сохраняет точный выбор пользователя; следующий `run` заново проверит его поиском JPDB.
    Select {
        #[arg(long, required = true)]
        batch_id: String,
        #[arg(long, required = true)]
        surface: String,
        #[arg(long, required = true)]
        vocabulary_id: u64,
        #[arg(long, required = true)]
        detail_url: String,
    },
    /// Повторяет только указанную словоформу; соседние завершённые элементы не сбрасываются.
    Retry {
        #[arg(long, required = true)]
        batch_id: String,
        #[arg(long, required = true)]
        surface: String,
        #[arg(long, required = true)]
        reason: String,
    },
    /// Повторно получает точную идентичность после явного решения пользователя.
    Reacquire {
        #[arg(long, required = true)]
        batch_id: String,
        #[arg(long, required = true)]
        surface: String,
        #[arg(long, required = true)]
        reason: String,
    },
    /// Отмечает точно указанные визуально отклонённые байты; новые SHA не наследуют отказ.
    Reject {
        #[arg(long, required = true)]
        batch_id: String,
        #[arg(long, required = true)]
        surface: String,
        #[arg(long, required = true)]
        sha256: String,
        #[arg(long, required = true)]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

/// Строгая внешняя схема плана. Версия сохраняется вместе с запросами пакета.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchPlan {
    pub schema_version: u32,
    pub items: Vec<PitchPlanItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchPlanItem {
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<PitchPlanSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchPlanSelection {
    pub vocabulary_id: u64,
    pub detail_url: String,
}

impl PitchPlanItem {
    fn query(&self) -> JpdbPitchQuery {
        JpdbPitchQuery::new(self.surface.clone(), self.reading.clone())
    }

    fn request(&self) -> Result<JpdbPitchRequest, AssetError> {
        let query = self.query();
        match &self.selection {
            Some(selection) => {
                let selection =
                    JpdbPitchSelection::new(selection.vocabulary_id, selection.detail_url.clone())
                        .map_err(invalid_plan)?;
                Ok(JpdbPitchRequest::with_selection(query, selection))
            }
            None => Ok(JpdbPitchRequest::new(query)),
        }
    }

    fn identity(&self) -> AssetIdentity {
        AssetIdentity {
            namespace: "pitch_accent".into(),
            key: self.surface.clone(),
        }
    }
}

/// Результат CLI с теми же полями оболочки, что и у `kanji-assets`.
pub struct PitchCliOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
}

#[derive(Debug, Clone, Serialize)]
struct StoreSummary {
    path: String,
    store_id: String,
}

#[derive(Debug, Serialize)]
struct ItemSummary {
    surface: String,
    reading: Option<String>,
    status: PitchBatchItemStatus,
    current_candidate_sha256: Option<String>,
    canonical_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_outcome: Option<PitchBatchOutcome>,
    #[serde(skip)]
    selection: Option<JpdbPitchSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<JpdbPitchFailure>,
    #[serde(skip)]
    failure_retryable: Option<bool>,
    #[serde(skip)]
    ambiguity: Vec<crate::jpdb::JpdbVocabularyCandidate>,
    #[serde(skip)]
    owner_conflict: Option<crate::pitch_batch::PitchBatchConflict>,
}

#[derive(Debug, Serialize)]
struct Response {
    schema_version: u32,
    operation: String,
    outcome: String,
    /// Изменилось сохранённое состояние хранилища/пакета либо записан отчёт проверки.
    /// Служебные блокировки и создание пустых каталогов не учитываются.
    changed: bool,
    store: Option<StoreSummary>,
    batch_id: Option<String>,
    batch: Option<serde_json::Value>,
    items: Vec<ItemSummary>,
    records: Vec<AssetRecord>,
    blockers: Vec<String>,
    artifact: Option<String>,
    error: Option<ErrorSummary>,
}

#[derive(Debug, Serialize)]
struct ErrorSummary {
    code: String,
    message: String,
    details: serde_json::Value,
}

const CLI_SCHEMA_VERSION: u32 = 1;

/// Выполняет команду; в JSON-режиме машиночитаемый ответ идёт в stdout.
pub async fn execute(cli: PitchCli) -> PitchCliOutput {
    let operation = operation_name(&cli.command).to_owned();
    let store_path = store_path(cli.store.as_deref(), &cli.repository_root);
    let prevalidated = prevalidate_command(&cli.command);
    let (plan, direct_items) = match prevalidated {
        Ok(value) => value,
        Err(error) => {
            return render_error(operation, None, error, cli.output, false);
        }
    };

    if matches!(
        &cli.command,
        PitchCommand::Corpus {
            command: CorpusCommand::Check
        }
    ) {
        return check_corpus(&store_path, &cli.repository_root, operation, cli.output);
    }

    let create = matches!(
        &cli.command,
        PitchCommand::Ensure { .. }
            | PitchCommand::Batch {
                command: PitchBatchCommand::Start { .. }
            }
    );
    let store = match open_store(&store_path, &cli.repository_root, create) {
        Ok(store) => store,
        Err(error) => return render_error(operation, Some(&store_path), error, cli.output, false),
    };
    let summary = StoreSummary {
        path: store.root().display().to_string(),
        store_id: store.store_id().to_owned(),
    };
    let store_changed = store.did_mutate_on_open();

    match cli.command {
        PitchCommand::Corpus {
            command: CorpusCommand::List,
        } => list_corpus(&store, operation, summary, cli.output, store_changed),
        PitchCommand::Corpus {
            command: CorpusCommand::Check,
        } => unreachable!("read-only corpus gate возвращается до открытия store"),
        PitchCommand::Ensure { refresh, .. } => {
            ensure(
                &store,
                summary,
                direct_items
                    .or_else(|| plan.map(|plan| plan.items))
                    .unwrap_or_default(),
                refresh,
                cli.output,
                store_changed,
            )
            .await
        }
        PitchCommand::Batch { command } => {
            execute_batch(&store, summary, command, plan, cli.output, store_changed).await
        }
    }
}

fn prevalidate_command(
    command: &PitchCommand,
) -> Result<(Option<PitchPlan>, Option<Vec<PitchPlanItem>>), AssetError> {
    match command {
        PitchCommand::Corpus { .. } => Ok((None, None)),
        PitchCommand::Ensure {
            plan,
            surface,
            reading,
            vocabulary_id,
            detail_url,
            ..
        } => {
            let items = if let Some(path) = plan {
                let plan = read_plan(path)?;
                Some(plan.items)
            } else if let Some(surface) = surface {
                Some(vec![PitchPlanItem {
                    surface: surface.clone(),
                    reading: reading.clone(),
                    selection: vocabulary_id.zip(detail_url.clone()).map(
                        |(vocabulary_id, detail_url)| PitchPlanSelection {
                            vocabulary_id,
                            detail_url,
                        },
                    ),
                }])
            } else {
                return Err(invalid_plan(
                    "ensure требует `--plan` либо `--surface` с необязательным чтением",
                ));
            };
            let items = validate_and_deduplicate_items(items.unwrap_or_default())?;
            Ok((None, Some(items)))
        }
        PitchCommand::Batch { command } => match command {
            PitchBatchCommand::Start { batch_id, plan } => {
                if let Some(batch_id) = batch_id {
                    crate::batch_runtime::validate_batch_id(batch_id)?;
                }
                let parsed = read_plan(plan)?;
                let items = validate_and_deduplicate_items(parsed.items)?;
                Ok((
                    Some(PitchPlan {
                        schema_version: PITCH_PLAN_SCHEMA_VERSION,
                        items,
                    }),
                    None,
                ))
            }
            PitchBatchCommand::Select {
                batch_id,
                surface,
                vocabulary_id,
                detail_url,
                ..
            } => {
                crate::batch_runtime::validate_batch_id(batch_id)?;
                let item = PitchPlanItem {
                    surface: surface.clone(),
                    reading: None,
                    selection: Some(PitchPlanSelection {
                        vocabulary_id: *vocabulary_id,
                        detail_url: detail_url.clone(),
                    }),
                };
                validate_and_deduplicate_items(vec![item])?;
                Ok((None, None))
            }
            PitchBatchCommand::Retry {
                batch_id,
                surface,
                reason,
            }
            | PitchBatchCommand::Reacquire {
                batch_id,
                surface,
                reason,
            } => {
                crate::batch_runtime::validate_batch_id(batch_id)?;
                validate_and_deduplicate_items(vec![PitchPlanItem {
                    surface: surface.clone(),
                    reading: None,
                    selection: None,
                }])?;
                validate_reason(reason)?;
                Ok((None, None))
            }
            PitchBatchCommand::Reject {
                batch_id,
                surface,
                sha256,
                reason,
            } => {
                crate::batch_runtime::validate_batch_id(batch_id)?;
                validate_and_deduplicate_items(vec![PitchPlanItem {
                    surface: surface.clone(),
                    reading: None,
                    selection: None,
                }])?;
                crate::batch_runtime::validate_hash(sha256)?;
                validate_reason(reason)?;
                Ok((None, None))
            }
            PitchBatchCommand::Run { batch_id }
            | PitchBatchCommand::Resume { batch_id }
            | PitchBatchCommand::Status { batch_id }
            | PitchBatchCommand::Review { batch_id } => {
                crate::batch_runtime::validate_batch_id(batch_id)?;
                Ok((None, None))
            }
        },
    }
}

fn validate_reason(reason: &str) -> Result<(), AssetError> {
    if reason.trim().is_empty() || reason.len() > MAX_PITCH_BATCH_REASON_BYTES {
        return Err(invalid_plan(format!(
            "причина должна быть непустой и не длиннее {MAX_PITCH_BATCH_REASON_BYTES} байт"
        )));
    }
    Ok(())
}

fn read_plan(path: &Path) -> Result<PitchPlan, AssetError> {
    let metadata = fs::metadata(path)
        .map_err(|error| AssetError::io("не удалось прочитать метаданные JSON-плана", error))?;
    if !metadata.is_file() || metadata.len() > MAX_PLAN_BYTES {
        return Err(invalid_plan(format!(
            "JSON-план должен быть обычным файлом размером не более {MAX_PLAN_BYTES} байт"
        )));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| AssetError::io("не удалось открыть JSON-план", error))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("не удалось прочитать JSON-план", error))?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err(invalid_plan(format!(
            "JSON-план превысил ограничение {MAX_PLAN_BYTES} байт при чтении"
        )));
    }
    let plan: PitchPlan = serde_json::from_slice(&bytes).map_err(|error| {
        invalid_plan(format!("JSON-план не соответствует строгой схеме: {error}"))
    })?;
    if plan.schema_version != PITCH_PLAN_SCHEMA_VERSION {
        return Err(invalid_plan(format!(
            "неподдерживаемая версия JSON-плана {}; ожидается {PITCH_PLAN_SCHEMA_VERSION}",
            plan.schema_version
        )));
    }
    Ok(plan)
}

fn validate_and_deduplicate_items(
    items: Vec<PitchPlanItem>,
) -> Result<Vec<PitchPlanItem>, AssetError> {
    if items.is_empty() {
        return Err(invalid_plan("план не может быть пустым"));
    }
    let policy = PitchAccentDomainPolicy;
    let mut seen = BTreeMap::<String, PitchPlanItem>::new();
    let mut validated = Vec::with_capacity(items.len());
    for item in items {
        if item.surface.trim() != item.surface || item.surface.is_empty() {
            return Err(invalid_plan(
                "surface должна быть непустой и не содержать краевых пробелов",
            ));
        }
        if item
            .reading
            .as_deref()
            .is_some_and(|reading| reading.trim().is_empty() || reading.trim() != reading)
        {
            return Err(invalid_plan(
                "reading должна отсутствовать либо быть непустой без краевых пробелов",
            ));
        }
        policy.validate_identity(&item.identity())?;
        if let Some(selection) = &item.selection {
            JpdbPitchSelection::new(selection.vocabulary_id, selection.detail_url.clone())
                .map_err(invalid_plan)?;
        }
        if let Some(previous) = seen.get(&item.surface) {
            if previous != &item {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    format!(
                        "написание `{}` повторяется с несовместимым чтением или выбором записи JPDB",
                        item.surface
                    ),
                    json!({"surface": item.surface}),
                ));
            }
            continue;
        }
        seen.insert(item.surface.clone(), item.clone());
        validated.push(item);
    }
    Ok(validated)
}

fn operation_name(command: &PitchCommand) -> &'static str {
    match command {
        PitchCommand::Corpus { command } => match command {
            CorpusCommand::List => "corpus_list",
            CorpusCommand::Check => "corpus_check",
        },
        PitchCommand::Ensure { .. } => "ensure",
        PitchCommand::Batch { command } => match command {
            PitchBatchCommand::Start { .. } => "batch_start",
            PitchBatchCommand::Run { .. } => "batch_run",
            PitchBatchCommand::Resume { .. } => "batch_resume",
            PitchBatchCommand::Status { .. } => "batch_status",
            PitchBatchCommand::Review { .. } => "batch_review",
            PitchBatchCommand::Select { .. } => "batch_select",
            PitchBatchCommand::Retry { .. } => "batch_retry",
            PitchBatchCommand::Reacquire { .. } => "batch_reacquire",
            PitchBatchCommand::Reject { .. } => "batch_reject",
        },
    }
}

fn store_path(store: Option<&Path>, repository_root: &Path) -> PathBuf {
    if let Some(path) = store {
        return path.to_path_buf();
    }
    find_workspace_root(repository_root)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| find_workspace_root(&cwd))
        })
        .or_else(|| std::fs::canonicalize(repository_root).ok())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".asset-store/pitch-accent")
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(start).ok()?;
    canonical.ancestors().find_map(|ancestor| {
        (ancestor.join("Cargo.toml").is_file() && ancestor.join(".git").exists())
            .then(|| ancestor.to_path_buf())
    })
}

fn open_store(path: &Path, repository_root: &Path, create: bool) -> Result<AssetStore, AssetError> {
    validate_store_boundary(path, repository_root)?;
    let mut protected = BTreeSet::new();
    protected.insert(repository_root.join("decks"));
    protected.extend(discover_decks_roots(path));
    protected.extend(discover_decks_roots(repository_root));
    if let Ok(cwd) = std::env::current_dir() {
        protected.extend(discover_decks_roots(&cwd));
    }

    let mut options = StoreOptions::new(path);
    for root in protected {
        options = options.protect_from(root);
    }
    if create {
        AssetStore::open_with_policy(options, PitchAccentDomainPolicy)
    } else {
        AssetStore::open_existing_with_policy(options, PitchAccentDomainPolicy)
    }
}

fn discover_decks_roots(path: &Path) -> BTreeSet<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    absolute
        .ancestors()
        .map(|ancestor| ancestor.join("decks"))
        .filter(|candidate| {
            std::fs::symlink_metadata(candidate).is_ok_and(|metadata| {
                metadata.is_dir()
                    || (metadata.file_type().is_symlink()
                        && std::fs::metadata(candidate).is_ok_and(|target| target.is_dir()))
            })
        })
        .collect()
}

fn validate_store_boundary(path: &Path, repository_root: &Path) -> Result<(), AssetError> {
    let repository_absolute = absolute_path(repository_root)?;
    let repository_metadata = std::fs::metadata(&repository_absolute).map_err(|error| {
        AssetError::io("не удалось проверить каталог `--repository-root`", error)
    })?;
    if !repository_metadata.is_dir() {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "`--repository-root` должен указывать на существующий каталог checkout",
        ));
    }
    let canonical_repository = std::fs::canonicalize(&repository_absolute)
        .map_err(|error| AssetError::io("не удалось разрешить `--repository-root`", error))?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить текущий каталог", error))?
            .join(path)
    };
    let lexical = normalize_path(&absolute);
    let mut decks = discover_decks_roots(&absolute);
    decks.extend(discover_decks_roots(&repository_absolute));
    if let Ok(cwd) = std::env::current_dir() {
        decks.extend(discover_decks_roots(&cwd));
    }
    for root in decks {
        let lexical_root = normalize_path(&root);
        if lexical.starts_with(&lexical_root) || lexical_root.starts_with(&lexical) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "pitch store пересекается с защищённым деревом `decks/`",
            ));
        }
        let canonical_root = std::fs::canonicalize(&root).map_err(|error| {
            AssetError::io("не удалось разрешить защищённое дерево `decks/`", error)
        })?;
        let canonical_path = canonicalize_future_path(&lexical)
            .map_err(|error| AssetError::io("не удалось проверить путь pitch store", error))?;
        if canonical_path.starts_with(&canonical_root)
            || canonical_root.starts_with(&canonical_path)
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "pitch store пересекается с защищённым деревом `decks/` через alias",
            ));
        }
        if canonical_repository.starts_with(&canonical_root) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "`--repository-root` указывает внутрь защищённого дерева `decks/`",
            ));
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, AssetError> {
    if path.is_absolute() {
        Ok(normalize_path(path))
    } else {
        let cwd = std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить текущий каталог", error))?;
        Ok(normalize_path(&cwd.join(path)))
    }
}

fn canonicalize_future_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name() else {
                    return Err(error);
                };
                missing.push(name.to_os_string());
                if !existing.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    let mut canonical = std::fs::canonicalize(existing)?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn check_corpus(
    path: &Path,
    repository_root: &Path,
    operation: String,
    output: OutputFormat,
) -> PitchCliOutput {
    let result = validate_store_boundary(path, repository_root).and_then(|()| {
        AssetStore::verify_publishable_corpus_with_policy(
            path,
            &PitchAccentDomainPolicy,
            &PitchAccentImageValidator::validator_identity(),
        )
    });
    match result {
        Ok(()) => render_response(
            Response {
                schema_version: CLI_SCHEMA_VERSION,
                operation,
                outcome: "verified".into(),
                changed: false,
                store: None,
                batch_id: None,
                batch: None,
                items: Vec::new(),
                records: Vec::new(),
                blockers: Vec::new(),
                artifact: None,
                error: None,
            },
            output,
            0,
        ),
        Err(error) => render_error(operation, Some(path), error, output, false),
    }
}

fn list_corpus(
    store: &AssetStore,
    operation: String,
    summary: StoreSummary,
    output: OutputFormat,
    store_changed: bool,
) -> PitchCliOutput {
    let result = store.verify_integrity().map(|records| {
        records
            .into_iter()
            .filter(|record| {
                record.identity.namespace == "pitch_accent"
                    && record.lifecycle == LifecycleState::Verified
            })
            .collect::<Vec<_>>()
    });
    match result {
        Ok(records) => render_response(
            Response {
                schema_version: CLI_SCHEMA_VERSION,
                operation,
                outcome: "listed".into(),
                changed: store_changed,
                store: Some(summary),
                batch_id: None,
                batch: None,
                items: Vec::new(),
                records,
                blockers: Vec::new(),
                artifact: None,
                error: None,
            },
            output,
            0,
        ),
        Err(error) => render_error(operation, None, error, output, store_changed),
    }
}

fn render_response(response: Response, output: OutputFormat, exit_code: u8) -> PitchCliOutput {
    match output {
        OutputFormat::Json => PitchCliOutput {
            stdout: format!(
                "{}\n",
                serde_json::to_string_pretty(&response)
                    .expect("ответ CLI содержит сериализуемые значения")
            ),
            stderr: String::new(),
            exit_code,
        },
        OutputFormat::Human => {
            if let Some(error) = response.error {
                let mut stderr = format!(
                    "Ошибка операции «{}» [{}]: {}\n",
                    human_operation_name(&response.operation),
                    error.code,
                    error.message
                );
                stderr.push_str(&format!(
                    "Сохранённые данные изменились: {}\n",
                    if response.changed { "да" } else { "нет" }
                ));
                if let Some(batch_id) = response.batch_id {
                    stderr.push_str(&format!("Пакет: {batch_id}\n"));
                }
                for item in response.items {
                    append_human_item(&mut stderr, item);
                }
                return PitchCliOutput {
                    stdout: String::new(),
                    stderr,
                    exit_code,
                };
            }
            let mut stdout = format!(
                "Операция: {}; результат: {}; изменено: {}\n",
                human_operation_name(&response.operation),
                human_outcome_name(&response.outcome),
                if response.changed { "да" } else { "нет" }
            );
            if let Some(store) = response.store {
                stdout.push_str(&format!("Хранилище: {}\n", store.path));
            }
            if let Some(batch_id) = response.batch_id {
                stdout.push_str(&format!("Пакет: {batch_id}\n"));
            }
            for record in response.records {
                stdout.push_str(&format!(
                    "Написание {}  {}  SHA-256={}  имя для потребителя={}\n",
                    record.identity.key,
                    human_lifecycle_name(record.lifecycle),
                    record.sha256,
                    record.consumer_filename
                ));
            }
            for item in response.items {
                append_human_item(&mut stdout, item);
            }
            if let Some(path) = response.artifact {
                stdout.push_str(&format!("Артефакт проверки: {path}\n"));
            }
            PitchCliOutput {
                stdout,
                stderr: String::new(),
                exit_code,
            }
        }
    }
}

fn append_human_item(output: &mut String, item: ItemSummary) {
    output.push_str(&format!(
        "Элемент {}{}: {}\n",
        item.surface,
        item.reading
            .as_deref()
            .map_or_else(String::new, |reading| format!(" / {reading}")),
        human_status_name(item.status)
    ));
    if let Some(selection) = item.selection {
        output.push_str(&format!(
            "  Выбранная запись JPDB: ID {}, маршрут {}\n",
            selection.vocabulary_id, selection.detail_url
        ));
    }
    if let Some(sha256) = item.current_candidate_sha256 {
        output.push_str(&format!("  SHA-256 кандидата: {sha256}\n"));
    }
    if let Some(sha256) = item.canonical_sha256 {
        output.push_str(&format!("  SHA-256 канонической записи: {sha256}\n"));
    }
    if let Some(conflict) = item.owner_conflict {
        output.push_str(&format!(
            "  Конфликт записи владельца: {}\n",
            conflict.message
        ));
    }
    if let Some(failure) = item.failure {
        let (stage, message) = human_failure_detail(&failure);
        let retryability = item
            .failure_retryable
            .map_or_else(String::new, |retryable| {
                format!(
                    "; повтор {}",
                    if retryable {
                        "допустим"
                    } else {
                        "не поможет"
                    }
                )
            });
        output.push_str(&format!(
            "  Причина технической ошибки ({stage}{retryability}): {message}\n"
        ));
    }
    if !item.ambiguity.is_empty() {
        output.push_str("  Требуется выбрать точную запись JPDB:\n");
        for candidate in item.ambiguity {
            output.push_str(&format!(
                "    ID {} — {}; формы: {}; чтения: {}; части речи: {}; значения: {}\n",
                candidate.vocabulary_id,
                candidate.detail_url,
                joined_or_dash(&candidate.surface_forms),
                joined_or_dash(&candidate.readings),
                joined_or_dash(&candidate.part_of_speech),
                joined_or_dash(&candidate.meanings)
            ));
        }
    }
}

fn joined_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "—".into()
    } else {
        values.join("; ")
    }
}

fn human_operation_name(operation: &str) -> &str {
    match operation {
        "corpus_list" => "список корпуса",
        "corpus_check" => "проверка корпуса",
        "ensure" => "получение и проверка",
        "batch_start" => "создание пакета",
        "batch_run" => "выполнение пакета",
        "batch_resume" => "возобновление пакета",
        "batch_status" => "состояние пакета",
        "batch_review" => "подготовка проверки",
        "batch_select" => "выбор записи JPDB",
        "batch_retry" => "повтор элемента",
        "batch_reacquire" => "повторное получение",
        "batch_reject" => "отклонение кандидата",
        _ => operation,
    }
}

fn human_outcome_name(outcome: &str) -> &str {
    match outcome {
        "verified" => "проверен",
        "listed" => "выведен список",
        "resolved" => "разрешён",
        "needs_review" => "требуется решение пользователя",
        "started" => "создан",
        "pending" => "ожидает обработки",
        "review_ready" => "материалы проверки готовы",
        "updated" => "обновлён",
        _ => outcome,
    }
}

fn human_status_name(status: PitchBatchItemStatus) -> &'static str {
    match status {
        PitchBatchItemStatus::Pending => "ожидает получения",
        PitchBatchItemStatus::AcquiredVerified => "проверен, готов к публикации",
        PitchBatchItemStatus::CandidateRejected => "кандидат отклонён",
        PitchBatchItemStatus::NoPitchAccentOnSource => "в источнике JPDB нет ударения",
        PitchBatchItemStatus::AmbiguousVocabulary => "неоднозначность, требуется выбор",
        PitchBatchItemStatus::VocabularyNotFound => "запись JPDB не найдена",
        PitchBatchItemStatus::TechnicalFailure => "техническая ошибка",
        PitchBatchItemStatus::PublicationPending => "ожидает публикации",
        PitchBatchItemStatus::Published => "опубликован",
        PitchBatchItemStatus::ExistingVerified => "уже проверен в каноническом корпусе",
        PitchBatchItemStatus::Conflict => "конфликт с текущей записью",
    }
}

fn human_lifecycle_name(status: LifecycleState) -> &'static str {
    match status {
        LifecycleState::Pending => "ожидает проверки",
        LifecycleState::Verified => "проверен",
        LifecycleState::Quarantined => "отправлен в карантин",
    }
}

fn human_failure_detail(failure: &JpdbPitchFailure) -> (String, String) {
    use JpdbPitchFailure as Failure;
    let stage = match failure {
        Failure::InvalidQuery { stage, .. }
        | Failure::BrowserSetup { stage, .. }
        | Failure::BrowserConfiguration { stage, .. }
        | Failure::Navigation { stage, .. }
        | Failure::BrowserEvaluation { stage, .. }
        | Failure::Timeout { stage, .. }
        | Failure::Telemetry { stage, .. }
        | Failure::PageContract { stage, .. }
        | Failure::DetailIdentityMismatch { stage, .. }
        | Failure::InvalidSelection { stage, .. }
        | Failure::ExplicitSelectionMismatch { stage, .. }
        | Failure::SessionFailure { stage, .. }
        | Failure::DarkThemeUnverified { stage, .. }
        | Failure::CaptureContract { stage, .. }
        | Failure::Screenshot { stage, .. }
        | Failure::InvalidPng { stage, .. } => *stage,
    };
    let stage = match stage {
        JpdbPitchStage::ConfigureBrowser => "настройка браузера",
        JpdbPitchStage::SearchNavigation => "переход к поиску",
        JpdbPitchStage::SearchReadiness => "ожидание результатов поиска",
        JpdbPitchStage::SearchResolution => "разрешение результата поиска",
        JpdbPitchStage::DetailNavigation => "переход к записи",
        JpdbPitchStage::DetailReadiness => "ожидание записи",
        JpdbPitchStage::DetailVerification => "проверка записи",
        JpdbPitchStage::PitchInspection => "проверка ударения",
        JpdbPitchStage::Capture => "снимок графика",
        JpdbPitchStage::PostCaptureVerification => "проверка снимка",
    }
    .to_owned();
    let message = match failure {
        Failure::InvalidQuery { message, .. }
        | Failure::BrowserSetup { message, .. }
        | Failure::BrowserConfiguration { message, .. }
        | Failure::Navigation { message, .. }
        | Failure::BrowserEvaluation { message, .. }
        | Failure::Telemetry { message, .. }
        | Failure::PageContract { message, .. }
        | Failure::InvalidSelection { message, .. }
        | Failure::ExplicitSelectionMismatch { message, .. }
        | Failure::SessionFailure { message, .. }
        | Failure::DarkThemeUnverified { message, .. }
        | Failure::CaptureContract { message, .. }
        | Failure::Screenshot { message, .. }
        | Failure::InvalidPng { message, .. } => message.clone(),
        Failure::Timeout { diagnostic, .. } => diagnostic
            .clone()
            .unwrap_or_else(|| "истекло время ожидания".into()),
        Failure::DetailIdentityMismatch {
            expected_surface,
            expected_reading,
            vocabulary_id,
            observed_surface_forms,
            observed_readings,
            ..
        } => format!(
            "ожидалась запись {}{} (ID {:?}), получены написания [{}] и чтения [{}]",
            expected_surface,
            expected_reading
                .as_deref()
                .map_or_else(String::new, |reading| format!(" / {reading}")),
            vocabulary_id,
            joined_or_dash(observed_surface_forms),
            joined_or_dash(observed_readings)
        ),
    };
    (stage, message)
}

fn render_error(
    operation: String,
    path: Option<&Path>,
    error: AssetError,
    output: OutputFormat,
    changed: bool,
) -> PitchCliOutput {
    let exit_code = error.exit_code();
    render_response(
        Response {
            schema_version: CLI_SCHEMA_VERSION,
            operation,
            outcome: "failed".into(),
            changed,
            store: path.map(|path| StoreSummary {
                path: path.display().to_string(),
                store_id: String::new(),
            }),
            batch_id: None,
            batch: None,
            items: Vec::new(),
            records: Vec::new(),
            blockers: Vec::new(),
            artifact: None,
            error: Some(ErrorSummary {
                code: error.code.as_str().to_owned(),
                message: error.message,
                details: error.details,
            }),
        },
        output,
        exit_code,
    )
}

fn render_batch_error(
    store: &AssetStore,
    summary: StoreSummary,
    batch_id: &str,
    operation: &str,
    error: AssetError,
    output: OutputFormat,
    changed: bool,
) -> PitchCliOutput {
    let exit_code = error.exit_code();
    let error = ErrorSummary {
        code: error.code.as_str().to_owned(),
        message: error.message,
        details: error.details,
    };
    let response = match load_batch(store, batch_id) {
        Ok(batch) => {
            let mut response = batch_response(
                operation,
                "failed",
                changed,
                Some(summary.clone()),
                &batch,
                batch_blockers(&batch),
                None,
            );
            response.error = Some(error);
            response
        }
        Err(_) => Response {
            schema_version: CLI_SCHEMA_VERSION,
            operation: operation.into(),
            outcome: "failed".into(),
            changed,
            store: Some(summary),
            batch_id: Some(batch_id.into()),
            batch: None,
            items: Vec::new(),
            records: Vec::new(),
            blockers: Vec::new(),
            artifact: None,
            error: Some(error),
        },
    };
    render_response(response, output, exit_code)
}

fn batch_state_revision(store: &AssetStore, batch_id: &str) -> Result<Option<u64>, AssetError> {
    let state_path = store
        .root()
        .join(".runtime/batches")
        .join(batch_id)
        .join("state.json");
    match fs::symlink_metadata(&state_path) {
        Ok(_) => load_batch(store, batch_id).map(|batch| Some(batch.revision)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AssetError::io(
            "не удалось проверить состояние пакета",
            error,
        )),
    }
}

fn batch_state_changed_since(store: &AssetStore, batch_id: &str, before: Option<u64>) -> bool {
    batch_state_revision(store, batch_id).map_or(true, |after| after != before)
}

fn invalid_plan(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidIdentity, message)
}

static GENERATED_BATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

async fn ensure(
    store: &AssetStore,
    summary: StoreSummary,
    items: Vec<PitchPlanItem>,
    refresh: bool,
    output: OutputFormat,
    store_changed: bool,
) -> PitchCliOutput {
    let batch_id = generated_batch_id();
    let before_revision = batch_state_revision(store, &batch_id).unwrap_or(None);
    let result = (|| {
        let mut batch = create_batch(store, &batch_id, &items, None)?.0;
        if refresh {
            let mut runtime = PitchAccentBatchRuntime::open(store.root(), &batch_id)?;
            batch = runtime
                .load()?
                .ok_or_else(|| invalid_plan("созданное состояние batch не найдено"))?;
            let surfaces = batch
                .items
                .iter()
                .map(|item| item.identity.key.clone())
                .collect::<Vec<_>>();
            for surface in surfaces {
                batch.reacquire(&surface, "ensure --refresh".into())?;
            }
            runtime.save(&batch)?;
        }
        Ok(batch)
    })();
    match result {
        Ok(batch) => {
            let LoggedBatchRun {
                result,
                diagnostic_log,
            } = run_batch_logged(store, &batch.batch_id, output, "ensure").await;
            let rendered = match result {
                Ok((batch, run_changed)) => {
                    let blockers = batch_blockers(&batch);
                    let resolved = batch.is_resolved();
                    render_response(
                        batch_response(
                            "ensure",
                            if resolved { "resolved" } else { "needs_review" },
                            store_changed
                                || run_changed
                                || batch_state_changed_since(store, &batch_id, before_revision),
                            Some(summary),
                            &batch,
                            blockers,
                            None,
                        ),
                        output,
                        if resolved { 0 } else { 3 },
                    )
                }
                Err(error) => render_batch_error(
                    store,
                    summary,
                    &batch_id,
                    "ensure",
                    error,
                    output,
                    store_changed || batch_state_changed_since(store, &batch_id, before_revision),
                ),
            };
            attach_diagnostic_log(rendered, output, diagnostic_log.as_deref())
        }
        Err(error) => render_batch_error(
            store,
            summary,
            &batch_id,
            "ensure",
            error,
            output,
            store_changed || batch_state_changed_since(store, &batch_id, before_revision),
        ),
    }
}

async fn execute_batch(
    store: &AssetStore,
    summary: StoreSummary,
    command: PitchBatchCommand,
    plan: Option<PitchPlan>,
    output: OutputFormat,
    store_changed: bool,
) -> PitchCliOutput {
    match command {
        PitchBatchCommand::Start { batch_id, .. } => {
            let batch_id = batch_id.unwrap_or_else(generated_batch_id);
            let before_revision = batch_state_revision(store, &batch_id).unwrap_or(None);
            let plan = plan.unwrap_or(PitchPlan {
                schema_version: PITCH_PLAN_SCHEMA_VERSION,
                items: Vec::new(),
            });
            let plan_identity = match plan_identity(&plan) {
                Ok(identity) => identity,
                Err(error) => {
                    return render_batch_error(
                        store,
                        summary,
                        &batch_id,
                        "batch_start",
                        error,
                        output,
                        store_changed,
                    );
                }
            };
            match create_batch(store, &batch_id, &plan.items, Some(plan_identity)) {
                Ok((batch, state_changed)) => render_response(
                    batch_response(
                        "batch_start",
                        "started",
                        store_changed || state_changed,
                        Some(summary),
                        &batch,
                        batch_blockers(&batch),
                        None,
                    ),
                    output,
                    0,
                ),
                Err(error) => render_batch_error(
                    store,
                    summary,
                    &batch_id,
                    "batch_start",
                    error,
                    output,
                    store_changed || batch_state_changed_since(store, &batch_id, before_revision),
                ),
            }
        }
        PitchBatchCommand::Run { batch_id } => {
            run_batch_output(
                store,
                &batch_id,
                "batch_run",
                summary,
                output,
                store_changed,
            )
            .await
        }
        PitchBatchCommand::Resume { batch_id } => {
            run_batch_output(
                store,
                &batch_id,
                "batch_resume",
                summary,
                output,
                store_changed,
            )
            .await
        }
        PitchBatchCommand::Status { batch_id } => {
            let before_revision = batch_state_revision(store, &batch_id).unwrap_or(None);
            match reconcile_loaded_batch(store, &batch_id) {
                Ok((batch, reconciled)) => render_response(
                    batch_response(
                        "batch_status",
                        if batch.is_resolved() {
                            "resolved"
                        } else {
                            "pending"
                        },
                        store_changed || reconciled,
                        Some(summary),
                        &batch,
                        batch_blockers(&batch),
                        None,
                    ),
                    output,
                    0,
                ),
                Err(error) => render_batch_error(
                    store,
                    summary,
                    &batch_id,
                    "batch_status",
                    error,
                    output,
                    store_changed || batch_state_changed_since(store, &batch_id, before_revision),
                ),
            }
        }
        PitchBatchCommand::Review { batch_id } => {
            let before_revision = batch_state_revision(store, &batch_id).unwrap_or(None);
            match write_review(store, &batch_id) {
                Ok((batch, artifact)) => render_response(
                    batch_response(
                        "batch_review",
                        "review_ready",
                        true,
                        Some(summary),
                        &batch,
                        batch_blockers(&batch),
                        Some(artifact.display().to_string()),
                    ),
                    output,
                    0,
                ),
                Err(error) => render_batch_error(
                    store,
                    summary,
                    &batch_id,
                    "batch_review",
                    error,
                    output,
                    store_changed || batch_state_changed_since(store, &batch_id, before_revision),
                ),
            }
        }
        PitchBatchCommand::Select {
            batch_id,
            surface,
            vocabulary_id,
            detail_url,
        } => update_batch(
            store,
            &batch_id,
            "batch_select",
            output,
            summary,
            store_changed,
            |batch| batch.select_candidate(&surface, vocabulary_id, detail_url),
        ),
        PitchBatchCommand::Retry {
            batch_id,
            surface,
            reason,
        } => update_batch(
            store,
            &batch_id,
            "batch_retry",
            output,
            summary,
            store_changed,
            |batch| batch.retry(&surface, reason),
        ),
        PitchBatchCommand::Reacquire {
            batch_id,
            surface,
            reason,
        } => update_batch(
            store,
            &batch_id,
            "batch_reacquire",
            output,
            summary,
            store_changed,
            |batch| batch.reacquire(&surface, reason),
        ),
        PitchBatchCommand::Reject {
            batch_id,
            surface,
            sha256,
            reason,
        } => reject_batch(store, &batch_id, &surface, &sha256, reason, summary, output),
    }
}

async fn run_batch_output(
    store: &AssetStore,
    batch_id: &str,
    operation: &'static str,
    summary: StoreSummary,
    output: OutputFormat,
    store_changed: bool,
) -> PitchCliOutput {
    let before_revision = batch_state_revision(store, batch_id).unwrap_or(None);
    let LoggedBatchRun {
        result,
        diagnostic_log,
    } = run_batch_logged(store, batch_id, output, operation).await;
    let rendered = match result {
        Ok((batch, changed)) => {
            let resolved = batch.is_resolved();
            render_response(
                batch_response(
                    operation,
                    if resolved { "resolved" } else { "needs_review" },
                    store_changed || changed,
                    Some(summary),
                    &batch,
                    batch_blockers(&batch),
                    None,
                ),
                output,
                if resolved { 0 } else { 3 },
            )
        }
        Err(error) => render_batch_error(
            store,
            summary,
            batch_id,
            operation,
            error,
            output,
            store_changed || batch_state_changed_since(store, batch_id, before_revision),
        ),
    };
    attach_diagnostic_log(rendered, output, diagnostic_log.as_deref())
}

fn update_batch(
    store: &AssetStore,
    batch_id: &str,
    operation: &'static str,
    output: OutputFormat,
    summary: StoreSummary,
    store_changed: bool,
    update: impl FnOnce(&mut PitchAccentBatch) -> Result<(), AssetError>,
) -> PitchCliOutput {
    let before_revision = batch_state_revision(store, batch_id).unwrap_or(None);
    let result = (|| {
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        let before = batch.clone();
        let owner = owner_snapshot(store)?;
        batch.reconcile_owner(&owner)?;
        let reconciliation_changed = batch != before;
        if let Err(error) = update(&mut batch) {
            if reconciliation_changed {
                runtime.save(&batch)?;
            }
            return Err(error);
        }
        let changed = batch != before;
        if changed {
            runtime.save(&batch)?;
        }
        Ok((batch, changed))
    })();
    match result {
        Ok((batch, changed)) => render_response(
            batch_response(
                operation,
                "updated",
                store_changed || changed,
                Some(summary),
                &batch,
                batch_blockers(&batch),
                None,
            ),
            output,
            0,
        ),
        Err(error) => render_batch_error(
            store,
            summary,
            batch_id,
            operation,
            error,
            output,
            store_changed || batch_state_changed_since(store, batch_id, before_revision),
        ),
    }
}

fn reject_batch(
    store: &AssetStore,
    batch_id: &str,
    surface: &str,
    sha256: &str,
    reason: String,
    summary: StoreSummary,
    output: OutputFormat,
) -> PitchCliOutput {
    let store_changed = store.did_mutate_on_open();
    let before_revision = batch_state_revision(store, batch_id).unwrap_or(None);
    let mut owner_changed = false;
    let result = (|| {
        let owner_records = store.verify_integrity()?;
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        batch.reconcile_owner(&PitchBatchOwnerSnapshot::from_records(
            owner_records.clone(),
        )?)?;
        if batch
            .item(surface)
            .and_then(|item| item.candidate(sha256))
            .is_none()
        {
            return Err(invalid_plan(
                "точный SHA кандидата отсутствует в истории пакета",
            ));
        }

        // Точный кандидат пакета проверяется до изменения записи владельца. Если
        // запись содержит ту же идентичность и SHA, решение пользователя также
        // помещает эти канонические байты в карантин через API хранилища.
        batch.reject_candidate(surface, sha256, reason.clone())?;
        let identity = batch
            .item(surface)
            .map(|item| item.identity.clone())
            .ok_or_else(|| invalid_plan("surface отсутствует в pitch batch"))?;
        if owner_records
            .iter()
            .any(|record| record.identity == identity && record.sha256 == sha256)
        {
            let rejected = store.attest(HumanAttestationRequest {
                identity,
                expected_sha256: sha256.to_owned(),
                decision: HumanDecision::Reject,
                reason,
            })?;
            owner_changed = true;
            // Сохраняем отклонённый SHA записи владельца как текущее CAS-наблюдение.
            // Из этого состояния разрешено явное повторное получение; новый
            // принятый кандидат заменит именно эти байты.
            batch.observe_owner(&rejected.asset)?;
        }
        runtime.save(&batch)?;
        Ok(batch)
    })();
    match result {
        Ok(batch) => render_response(
            batch_response(
                "batch_reject",
                "updated",
                store_changed
                    || owner_changed
                    || batch_state_changed_since(store, batch_id, before_revision),
                Some(summary),
                &batch,
                batch_blockers(&batch),
                None,
            ),
            output,
            0,
        ),
        Err(error) => render_batch_error(
            store,
            summary,
            batch_id,
            "batch_reject",
            error,
            output,
            store_changed
                || owner_changed
                || batch_state_changed_since(store, batch_id, before_revision),
        ),
    }
}

fn create_batch(
    store: &AssetStore,
    batch_id: &str,
    items: &[PitchPlanItem],
    supplied_identity: Option<PitchBatchPlanIdentity>,
) -> Result<(PitchAccentBatch, bool), AssetError> {
    crate::batch_runtime::validate_batch_id(batch_id)?;
    let requested = items
        .iter()
        .map(PitchPlanItem::request)
        .collect::<Result<Vec<_>, _>>()?;
    let validator = PitchAccentImageValidator::validator_identity();
    let plan_identity = match supplied_identity {
        Some(identity) => identity,
        None => PitchBatchPlanIdentity::new(PITCH_PLAN_SCHEMA_VERSION, requested.clone())?,
    };
    let proposed =
        PitchAccentBatch::new_with_plan_identity(batch_id, requested, validator, plan_identity)?;
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let (mut batch, created) = match runtime.load()? {
        Some(existing) => {
            if existing.original_plan != proposed.original_plan {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    "batch_id уже занят пакетом с другим исходным планом",
                    json!({"batch_id": batch_id}),
                ));
            }
            (existing, false)
        }
        None => {
            runtime.save(&proposed)?;
            (proposed, true)
        }
    };
    let before_reconcile = batch.clone();
    let records = store.verify_integrity()?;
    let snapshot = PitchBatchOwnerSnapshot::from_records(records)?;
    batch.reconcile_owner(&snapshot)?;
    let reconciled = batch != before_reconcile;
    if reconciled {
        runtime.save(&batch)?;
    }
    Ok((batch, created || reconciled))
}

fn plan_identity(plan: &PitchPlan) -> Result<PitchBatchPlanIdentity, AssetError> {
    let requests = plan
        .items
        .iter()
        .map(PitchPlanItem::request)
        .collect::<Result<Vec<_>, _>>()?;
    PitchBatchPlanIdentity::new(plan.schema_version, requests)
}

fn load_batch(store: &AssetStore, batch_id: &str) -> Result<PitchAccentBatch, AssetError> {
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    runtime
        .load()?
        .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))
}

fn reconcile_loaded_batch(
    store: &AssetStore,
    batch_id: &str,
) -> Result<(PitchAccentBatch, bool), AssetError> {
    let owner = owner_snapshot(store)?;
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let mut batch = runtime
        .load()?
        .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
    let before = batch.clone();
    batch.reconcile_owner(&owner)?;
    let changed = batch != before;
    if changed {
        runtime.save(&batch)?;
    }
    Ok((batch, changed))
}

// 64 перехода ограничивают накопленные ресурсы Chromium/CDP, 20 минут —
// возраст сессии. Смена сессии происходит после надёжного сохранения результата.
// Уже запущенный запрос сохраняет собственный 90-секундный тайм-аут провайдера.
const PITCH_SESSION_MAX_ITEMS: usize = 64;
const PITCH_SESSION_MAX_AGE: Duration = Duration::from_secs(20 * 60);
const PITCH_PROGRESS_HEARTBEAT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct PitchRunPolicy {
    max_items: usize,
    max_age: Duration,
    heartbeat: Duration,
}

impl Default for PitchRunPolicy {
    fn default() -> Self {
        Self {
            max_items: PITCH_SESSION_MAX_ITEMS,
            max_age: PITCH_SESSION_MAX_AGE,
            heartbeat: PITCH_PROGRESS_HEARTBEAT,
        }
    }
}

// Локальная граница для детерминированных автономных тестов. Сохранённое
// состояние, token/CAS и байты кандидатов принадлежат владельцу пакета;
// провайдер их не записывает.
trait PitchRunDriver {
    type Session;

    async fn launch(&mut self) -> Result<Self::Session, String>;
    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> JpdbPitchAcquisitionReport;
    async fn close(&mut self, session: Self::Session);
}

struct JpdbRunDriver;

impl PitchRunDriver for JpdbRunDriver {
    type Session = BrowserSession;

    async fn launch(&mut self) -> Result<Self::Session, String> {
        BrowserSession::launch(pitch_browser_runtime_config()).await
    }

    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> JpdbPitchAcquisitionReport {
        JpdbPitchProvider::acquire_requests_in_session(session, std::slice::from_ref(request)).await
    }

    async fn close(&mut self, session: Self::Session) {
        session.close().await;
    }
}

// Общая JSONL-оболочка kanji progress; у pitch нет раундов и лимитов раундов.
#[derive(Debug, Clone, Serialize)]
struct PitchProgressEvent {
    schema_version: u32,
    operation: &'static str,
    event: &'static str,
    batch_id: String,
    elapsed_ms: u128,
    run_completed: usize,
    run_total: usize,
    batch_total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<AssetIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_session: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

trait PitchProgressSink {
    fn emit(&mut self, event: PitchProgressEvent) -> Result<(), AssetError>;
}

struct PitchStderrProgressSink(OutputFormat);

impl PitchProgressSink for PitchStderrProgressSink {
    fn emit(&mut self, event: PitchProgressEvent) -> Result<(), AssetError> {
        write_pitch_progress(&event, self.0, &mut io::stderr().lock())
    }
}

fn write_pitch_progress(
    event: &PitchProgressEvent,
    output: OutputFormat,
    writer: &mut impl Write,
) -> Result<(), AssetError> {
    let line = match output {
        OutputFormat::Json => serde_json::to_string(event)
            .map_err(|error| invalid_plan(format!("сериализация progress: {error}")))?,
        OutputFormat::Human => format!(
            "Прогресс: {} · сохранено {}/{} · всего {} · {:.1} с · запись={} · сессия={} · результат={} {}",
            human_pitch_progress_event(event.event),
            event.run_completed,
            event.run_total,
            event.batch_total,
            event.elapsed_ms as f64 / 1000.0,
            event
                .identity
                .as_ref()
                .map(|identity| identity.key.as_str())
                .unwrap_or("-"),
            event
                .session
                .map_or_else(|| "-".into(), |session| session.to_string()),
            event
                .outcome
                .as_deref()
                .map(human_pitch_progress_outcome)
                .unwrap_or("-"),
            event
                .reason
                .as_deref()
                .map(human_pitch_progress_reason)
                .unwrap_or("")
        ),
    };
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|error| AssetError::io("запись progress pitch в stderr", error))
}

fn human_pitch_progress_event(event: &str) -> &'static str {
    match event {
        "run_started" => "получение начато",
        "run_finished" => "получение завершено",
        "run_stopped" => "получение остановлено",
        "browser_session_started" => "сессия браузера запущена",
        "browser_session_ended" => "сессия браузера закрыта",
        "browser_session_rotated" => "смена сессии браузера",
        "item_started" => "получение записи начато",
        "retry_started" => "повторная попытка начата",
        "heartbeat" => "ожидание браузера",
        "item_checkpointed" => "результат записи сохранён",
        "item_discarded_stale" => "устаревший результат отброшен",
        _ => "событие получения",
    }
}

fn human_pitch_progress_outcome(outcome: &str) -> &'static str {
    match outcome {
        "pending" => "ожидает получения",
        "acquired_verified" => "проверен, готов к публикации",
        "candidate_rejected" => "кандидат отклонён",
        "no_pitch_accent_on_source" => "в источнике JPDB нет ударения",
        "ambiguous_vocabulary" => "неоднозначность, требуется выбор",
        "vocabulary_not_found" => "запись JPDB не найдена",
        "technical_failure" => "техническая ошибка",
        "publication_pending" => "ожидает публикации",
        "published" => "опубликован",
        "existing_verified" => "уже проверен в каноническом корпусе",
        "conflict" => "конфликт с текущей записью",
        _ => "результат обработки",
    }
}

fn human_pitch_progress_reason(reason: &str) -> &'static str {
    match reason {
        "item_limit" => "достигнут лимит записей сессии",
        "age_limit" => "достигнут лимит времени сессии",
        "interrupted" => "получен Ctrl+C",
        "session_failure" => "ошибка сессии браузера",
        "ctrl_c_listener_failed" => "ошибка обработчика Ctrl+C",
        "invalid_provider_report" => "некорректный ответ провайдера",
        "item_token_changed" => "запись изменена другим действием",
        _ => "ошибка выполнения",
    }
}

// Контекст принадлежит событию: heartbeat запуска новой сессии не может
// перенимать identity или attempt ранее сохранённой записи.
#[derive(Clone, Copy, Default)]
struct PitchProgressContext<'a> {
    identity: Option<&'a AssetIdentity>,
    session: Option<u32>,
    next_session: Option<u32>,
    attempt: Option<usize>,
}

struct PitchProgressReporter<'a> {
    sink: &'a mut dyn PitchProgressSink,
    batch_id: String,
    operation: &'static str,
    started: Instant,
    completed: usize,
    total: usize,
    batch_total: usize,
}

impl PitchProgressReporter<'_> {
    fn emit(
        &mut self,
        event: &'static str,
        context: PitchProgressContext<'_>,
        outcome: Option<String>,
        reason: Option<String>,
    ) -> Result<(), AssetError> {
        let reason = reason.map(|reason| safe_message(&reason));
        tracing::info!(
            operation = self.operation,
            batch_id = self.batch_id,
            stage = "pitch_runtime",
            code = event,
            session = context.session,
            next_session = context.next_session,
            identity = context
                .identity
                .map(|identity| safe_message(identity.key.as_str())),
            attempt = context.attempt,
            outcome = outcome.as_deref(),
            reason = ?reason,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            run_completed = self.completed,
            run_total = self.total,
            batch_total = self.batch_total,
            "событие выполнения pitch-accent"
        );
        self.sink.emit(PitchProgressEvent {
            schema_version: 1,
            operation: self.operation,
            event,
            batch_id: self.batch_id.clone(),
            elapsed_ms: self.started.elapsed().as_millis(),
            run_completed: self.completed,
            run_total: self.total,
            batch_total: self.batch_total,
            identity: context.identity.cloned(),
            session: context.session,
            next_session: context.next_session,
            attempt: context.attempt,
            outcome,
            reason,
        })
    }
}

struct PitchCtrlCListener(tokio::task::JoinHandle<Result<(), io::Error>>);

impl Drop for PitchCtrlCListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn pitch_run_stopped(reason: &str, message: impl Into<String>) -> AssetError {
    AssetError::with_details(
        if reason == "interrupted" {
            ErrorCode::InvalidTransition
        } else {
            ErrorCode::ValidatorFailure
        },
        message,
        json!({"run_stop_reason": reason}),
    )
}

fn trace_pitch_failure(
    failure: &JpdbPitchFailure,
    context: PitchProgressContext<'_>,
    session_failure: bool,
) {
    let serialized = serde_json::to_value(failure).expect("типизированная ошибка сериализуется");
    let (_, message) = human_failure_detail(failure);
    tracing::error!(
        event = "pitch_failure",
        stage = serialized["stage"].as_str(),
        code = serialized["code"].as_str(),
        message = %crate::diagnostics::safe_message(&message),
        identity = context
            .identity
            .map(|identity| safe_message(identity.key.as_str())),
        session = context.session,
        attempt = context.attempt,
        session_failure,
        retryable = is_retryable_failure(failure),
        tail_started = false,
    );
}

fn pitch_session_failure(failure: JpdbPitchFailure) -> AssetError {
    AssetError::with_details(
        ErrorCode::ValidatorFailure,
        "получение остановлено из-за ошибки сессии браузера; хвост доступен для batch resume",
        json!({"run_stop_reason": "session_failure", "session_failure": failure}),
    )
}

fn interruption_result(result: Result<(), String>) -> AssetError {
    tracing::warn!(
        stage = "pitch_runtime",
        code = "interruption_acknowledged",
        listener_ok = result.is_ok(),
        "сигнал остановки принят на границе операции"
    );
    match result {
        Ok(()) => pitch_run_stopped(
            "interrupted",
            "получение остановлено по Ctrl+C; пакет доступен для resume",
        ),
        Err(message) => pitch_run_stopped("ctrl_c_listener_failed", message),
    }
}

async fn wait_pitch_operation<T>(
    operation: impl Future<Output = T>,
    mut interruption: Pin<&mut impl Future<Output = Result<(), String>>>,
    progress: &mut PitchProgressReporter<'_>,
    context: PitchProgressContext<'_>,
    heartbeat_period: Duration,
) -> Result<T, AssetError> {
    tokio::pin!(operation);
    let mut heartbeat = tokio::time::interval(heartbeat_period);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    loop {
        tokio::select! {
            biased;
            // Готовый outcome выигрывает у interruption; caller сохраняет его
            // синхронно без дополнительного await или проверки сигнала.
            result = &mut operation => return Ok(result),
            result = interruption.as_mut() => return Err(interruption_result(result)),
            _ = heartbeat.tick() => progress.emit("heartbeat", context, None, None)?,
        }
    }
}

async fn check_pitch_interruption(
    mut interruption: Pin<&mut impl Future<Output = Result<(), String>>>,
) -> Result<(), AssetError> {
    tokio::task::yield_now().await;
    tokio::select! {
        biased;
        signal = interruption.as_mut() => Err(interruption_result(signal)),
        _ = std::future::ready(()) => Ok(()),
    }
}

// Внутри BrowserSession::launch есть spawned CDP handler: запуск нельзя отменить
// до получения owning Session. Запоминаем сигнал, дожидаемся ограниченного запуска
// и закрываем полученную сессию до начала обработки identity.
async fn wait_pitch_launch<T>(
    launch: impl Future<Output = Result<T, String>>,
    mut interruption: Pin<&mut impl Future<Output = Result<(), String>>>,
    progress: &mut PitchProgressReporter<'_>,
    context: PitchProgressContext<'_>,
    heartbeat_period: Duration,
) -> (Result<T, String>, Option<AssetError>) {
    tokio::pin!(launch);
    let mut heartbeat = tokio::time::interval(heartbeat_period);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut stop = None;
    loop {
        tokio::select! {
            biased;
            result = &mut launch => return (result, stop),
            signal = interruption.as_mut(), if stop.is_none() => {
                stop = Some(interruption_result(signal));
            }
            _ = heartbeat.tick(), if stop.is_none() => {
                if let Err(error) = progress.emit("heartbeat", context, None, None) {
                    stop = Some(error);
                }
            }
        }
    }
}

async fn run_batch(
    store: &AssetStore,
    batch_id: &str,
    output: OutputFormat,
    operation: &'static str,
) -> Result<(PitchAccentBatch, bool), AssetError> {
    let mut listener = PitchCtrlCListener(tokio::spawn(tokio::signal::ctrl_c()));
    let interruption = async {
        match (&mut listener.0).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("обработчик Ctrl+C: {error}")),
            Err(error) => Err(format!("задача обработчика Ctrl+C: {error}")),
        }
    };
    run_batch_with_driver(
        store,
        batch_id,
        operation,
        &mut JpdbRunDriver,
        &mut PitchStderrProgressSink(output),
        PitchRunPolicy::default(),
        interruption,
    )
    .await
}

struct LoggedBatchRun {
    result: Result<(PitchAccentBatch, bool), AssetError>,
    diagnostic_log: Option<String>,
}

async fn run_batch_logged(
    store: &AssetStore,
    batch_id: &str,
    output: OutputFormat,
    operation: &'static str,
) -> LoggedBatchRun {
    let run_log = match SafeBatchRuntime::open(store.root(), batch_id)
        .and_then(|runtime| runtime.create_run_log())
    {
        Ok(run_log) => run_log,
        Err(error) => {
            return LoggedBatchRun {
                result: Err(error),
                diagnostic_log: None,
            };
        }
    };
    let run_id = run_log.run_id;
    let path = run_log.path.display().to_string();
    let mode = match output {
        OutputFormat::Human => DiagnosticOutputMode::Human,
        OutputFormat::Json => DiagnosticOutputMode::Json,
    };
    let guard = RunLogGuard::new(run_log.file, mode);
    if matches!(output, OutputFormat::Human)
        && let Err(error) = io::stderr()
            .lock()
            .write_all(format!("Диагностический журнал: {path}\n").as_bytes())
    {
        guard.with_default(|| {
            tracing::error!(
                operation,
                batch_id,
                run_id,
                diagnostic_log = path,
                stage = "diagnostic_log_path",
                code = "stderr_write_failed",
                message = %safe_message(&error.to_string()),
                "Не удалось вывести путь диагностического файла"
            );
        });
        let original = AssetError::io("вывод пути диагностического файла", error);
        let result = match guard.finish() {
            Ok(()) => Err(original),
            Err(logging_error) => Err(diagnostic_write_error(Some(original), logging_error, &path)),
        };
        return LoggedBatchRun {
            result,
            diagnostic_log: Some(path),
        };
    }

    let future = async {
        let span = tracing::info_span!(
            "pitch_batch_run",
            operation,
            batch_id,
            run_id,
            diagnostic_log = path,
        );
        async {
            tracing::info!(
                event = "run_started",
                stage = "run",
                code = "batch_run_started",
                operation,
                batch_id,
                run_id,
                "Начата обработка pitch-accent"
            );
            let result = run_batch(store, batch_id, output, operation).await;
            match &result {
                Ok((batch, changed)) => tracing::info!(
                    event = "run_finished",
                    stage = "run",
                    code = "batch_run_finished",
                    operation,
                    batch_id,
                    run_id,
                    changed,
                    total_items = batch.items.len(),
                    total_attempts = batch
                        .items
                        .iter()
                        .map(|item| item.attempts.len())
                        .sum::<usize>(),
                    "Обработка pitch-accent завершена"
                ),
                Err(error) => tracing::error!(
                    event = "run_stopped",
                    stage = "run",
                    code = error.code.as_str(),
                    message = %safe_message(&error.message),
                    operation,
                    batch_id,
                    run_id,
                    "Обработка pitch-accent остановлена"
                ),
            }
            result
        }
        .instrument(span)
        .await
    }
    .with_subscriber(guard.dispatch());
    let result = future.await;
    let result = match guard.finish() {
        Ok(()) => result,
        Err(logging_error) => Err(diagnostic_write_error(result.err(), logging_error, &path)),
    };
    LoggedBatchRun {
        result,
        diagnostic_log: Some(path),
    }
}

fn diagnostic_write_error(
    original: Option<AssetError>,
    logging_error: String,
    path: &str,
) -> AssetError {
    let original = original.map_or(serde_json::Value::Null, |error| {
        serde_json::json!({
            "code": error.code.as_str(),
            "message": safe_message(&error.message),
            "details": error.details,
        })
    });
    AssetError::with_details(
        ErrorCode::IoFailure,
        match &original {
            serde_json::Value::Null => {
                format!("не удалось записать диагностический журнал {path}: {logging_error}")
            }
            _ => format!(
                "обработка завершилась ошибкой, и не удалось записать диагностический журнал {path}: {logging_error}"
            ),
        },
        json!({
            "diagnostic_log": path,
            "diagnostic_log_error": logging_error,
            "original_error": original,
        }),
    )
}

fn attach_diagnostic_log(
    mut output: PitchCliOutput,
    format: OutputFormat,
    diagnostic_log: Option<&str>,
) -> PitchCliOutput {
    let Some(path) = diagnostic_log else {
        return output;
    };
    match format {
        OutputFormat::Json => {
            let mut response: serde_json::Value = serde_json::from_str(&output.stdout)
                .expect("JSON-ответ batch содержит самостоятельный документ");
            response["diagnostic_log"] = serde_json::Value::String(path.to_owned());
            output.stdout = format!(
                "{}\n",
                serde_json::to_string_pretty(&response)
                    .expect("ответ с путём диагностического файла сериализуется")
            );
        }
        OutputFormat::Human => {
            let line = format!("Диагностический журнал: {path}\n");
            if output.exit_code == 0 || output.stderr.is_empty() {
                output.stdout.push_str(&line);
            } else {
                output.stderr.push_str(&line);
            }
        }
    }
    output
}

async fn run_batch_with_driver(
    store: &AssetStore,
    batch_id: &str,
    operation: &'static str,
    driver: &mut impl PitchRunDriver,
    sink: &mut dyn PitchProgressSink,
    policy: PitchRunPolicy,
    interruption: impl Future<Output = Result<(), String>>,
) -> Result<(PitchAccentBatch, bool), AssetError> {
    if policy.max_items == 0 || policy.max_age.is_zero() || policy.heartbeat.is_zero() {
        return Err(invalid_plan(
            "лимиты pitch runtime должны быть положительными",
        ));
    }
    let initial_revision = {
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        let revision = batch.revision;
        let before = batch.clone();
        reconcile_batch(store, &mut batch)?;
        if batch != before {
            runtime.save(&batch)?;
        }
        revision
    };
    publish_ready(store, batch_id)?;
    let (batch, _) = reconcile_loaded_batch(store, batch_id)?;
    let acquisitions = batch
        .items
        .iter()
        .filter(|item| item.status() == PitchBatchItemStatus::Pending)
        .map(|item| {
            Ok((
                batch.item_token(&item.identity.key)?,
                item.request.clone(),
                item.attempts.len() + 1,
            ))
        })
        .collect::<Result<Vec<_>, AssetError>>()?;
    tracing::debug!(
        stage = "pitch_runtime",
        code = "runtime_lock_released",
        batch_id,
        pending_items = acquisitions.len(),
        "Блокировка runtime освобождена до обращения к браузеру"
    );
    let mut progress = PitchProgressReporter {
        sink,
        batch_id: batch_id.into(),
        operation: if operation == "batch_resume" {
            "batch_run"
        } else {
            operation
        },
        started: Instant::now(),
        completed: 0,
        total: acquisitions.len(),
        batch_total: batch.items.len(),
    };
    progress.emit("run_started", PitchProgressContext::default(), None, None)?;
    tokio::pin!(interruption);
    let mut session = None;
    let mut session_started = Instant::now();
    let mut session_items = 0;
    let mut session_index = 0;
    let result = async {
        for (token, request, attempt) in acquisitions {
            // Обработчик может увидеть сигнал во время синхронной записи результата
            // или публикации. Проверяем его до начала следующей записи, даже если
            // следующий результат провайдера готов немедленно.
            check_pitch_interruption(interruption.as_mut()).await?;
            if session.is_some()
                && (session_items >= policy.max_items
                    || session_started.elapsed() >= policy.max_age)
            {
                let reason = if session_items >= policy.max_items {
                    "item_limit"
                } else {
                    "age_limit"
                };
                if let Some(active) = session.take() {
                    tracing::info!(
                        stage = "browser_close",
                        code = "browser_close_started",
                        session = session_index,
                        reason,
                        "закрытие сессии перед ротацией"
                    );
                    driver.close(active).await;
                }
                let ended_context = PitchProgressContext {
                    session: Some(session_index),
                    ..PitchProgressContext::default()
                };
                progress.emit(
                    "browser_session_ended",
                    ended_context,
                    None,
                    Some(reason.into()),
                )?;
                progress.emit(
                    "browser_session_rotated",
                    PitchProgressContext {
                        next_session: Some(session_index + 1),
                        ..ended_context
                    },
                    None,
                    Some(reason.into()),
                )?;
            }
            if session.is_none() {
                session_index += 1;
                let launch_context = PitchProgressContext {
                    session: Some(session_index),
                    ..PitchProgressContext::default()
                };
                check_pitch_interruption(interruption.as_mut()).await?;
                tracing::info!(
                    stage = "browser_launch",
                    code = "browser_launch_started",
                    session = session_index,
                    "запуск сессии браузера"
                );
                let (launched, stop) = wait_pitch_launch(
                    driver.launch(),
                    interruption.as_mut(),
                    &mut progress,
                    launch_context,
                    policy.heartbeat,
                )
                .await;
                match launched {
                    Ok(launched) => {
                        session = Some(launched);
                        session_started = Instant::now();
                        session_items = 0;
                        progress.emit("browser_session_started", launch_context, None, None)?;
                    }
                    Err(message) => {
                        return Err(stop.unwrap_or_else(|| {
                            let failure = JpdbPitchFailure::BrowserSetup {
                                stage: JpdbPitchStage::ConfigureBrowser,
                                message,
                            };
                            trace_pitch_failure(&failure, launch_context, true);
                            pitch_session_failure(failure)
                        }));
                    }
                }
                if let Some(error) = stop {
                    return Err(error);
                }
                check_pitch_interruption(interruption.as_mut()).await?;
            }
            let item_context = PitchProgressContext {
                identity: Some(&token.identity),
                session: Some(session_index),
                attempt: Some(attempt),
                next_session: None,
            };
            progress.emit("item_started", item_context, None, None)?;
            if attempt > 1 {
                progress.emit("retry_started", item_context, None, None)?;
            }
            let mut report = wait_pitch_operation(
                driver.acquire(session.as_ref().expect("launched session"), &request),
                interruption.as_mut(),
                &mut progress,
                item_context,
                policy.heartbeat,
            )
            .await?;
            if report.outcomes.len() > 1
                || (report.outcomes.is_empty() && report.session_failure.is_none())
            {
                return Err(pitch_run_stopped(
                    "invalid_provider_report",
                    "Провайдер JPDB вернул некорректное число результатов",
                ));
            }
            if let Some(outcome) = report.outcomes.pop() {
                if let JpdbPitchOutcome::Failed { error } = &outcome {
                    trace_pitch_failure(error, item_context, false);
                }
                // После получения результата CAS-checkpoint выполняется до следующего ожидания.
                let recorded = record_one_outcome(store, batch_id, &token, outcome)?;
                session_items += 1;
                if recorded {
                    progress.completed += 1;
                    let saved = load_batch(store, batch_id)?;
                    let status = saved
                        .items
                        .iter()
                        .find(|item| item.identity == token.identity)
                        .map(|item| serde_json::to_value(item.status()))
                        .transpose()
                        .map_err(|error| invalid_plan(format!("сериализация статуса: {error}")))?
                        .and_then(|value| value.as_str().map(str::to_owned));
                    progress.emit("item_checkpointed", item_context, status, None)?;
                    publish_ready(store, batch_id)?;
                } else {
                    progress.emit(
                        "item_discarded_stale",
                        item_context,
                        None,
                        Some("item_token_changed".into()),
                    )?;
                }
            }
            if let Some(failure) = report.session_failure {
                trace_pitch_failure(&failure, item_context, true);
                return Err(pitch_session_failure(failure));
            }
            check_pitch_interruption(interruption.as_mut()).await?;
        }
        let (batch, _) = reconcile_loaded_batch(store, batch_id)?;
        Ok((batch.clone(), batch.revision != initial_revision))
    }
    .await;
    // Эта задача закрывает активную сессию при любом выходе: ошибка вывода
    // прогресса, сохранения, Ctrl+C или сбой провайдера либо сессии.
    if let Some(active) = session.take() {
        tracing::info!(
            stage = "browser_close",
            code = "browser_close_started",
            session = session_index,
            "закрытие активной сессии после получения"
        );
        driver.close(active).await;
        let reason = result
            .as_ref()
            .err()
            .and_then(|error| error.details["run_stop_reason"].as_str())
            .map(str::to_owned);
        progress.emit(
            "browser_session_ended",
            PitchProgressContext {
                session: Some(session_index),
                ..PitchProgressContext::default()
            },
            None,
            reason,
        )?;
    }
    match &result {
        Ok(_) => progress.emit("run_finished", PitchProgressContext::default(), None, None)?,
        Err(error) => progress.emit(
            "run_stopped",
            PitchProgressContext::default(),
            None,
            Some(
                error.details["run_stop_reason"]
                    .as_str()
                    .unwrap_or(error.code.as_str())
                    .into(),
            ),
        )?,
    }
    result
}

fn record_one_outcome(
    store: &AssetStore,
    batch_id: &str,
    token: &PitchBatchItemToken,
    outcome: JpdbPitchOutcome,
) -> Result<bool, AssetError> {
    let checkpoint = tracing::info_span!(
        "pitch_checkpoint",
        batch_id,
        identity = %safe_message(token.identity.key.as_str()),
        expected_generation = token.generation,
        expected_item_revision = token.item_revision,
        request_fingerprint = token.request_fingerprint,
    );
    let _entered = checkpoint.enter();
    tracing::info!(
        stage = "checkpoint",
        code = "checkpoint_begin",
        "начало сохранения результата"
    );
    let result = (|| {
        let owner = owner_snapshot(store)?;
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        tracing::debug!(
            stage = "checkpoint",
            code = "runtime_reloaded",
            revision = batch.revision,
            "runtime повторно загружен под lock"
        );
        let before_reconcile = batch.clone();
        batch.reconcile_owner(&owner)?;
        if batch != before_reconcile {
            runtime.save(&batch)?;
            tracing::info!(
                stage = "owner_reconciliation",
                code = "owner_reconciled",
                "изменения владельца сохранены"
            );
        }
        let observed = batch.item(&token.identity.key);
        tracing::debug!(
            stage = "checkpoint",
            code = "cas_observed",
            observed_generation = observed.map(|item| item.generation),
            observed_item_revision = observed.map(|item| item.item_revision),
            "проверка актуальности token"
        );
        // record_outcome сохраняет полученные байты до смены состояния. Проверка token/CAS
        // отбрасывает результат, если параллельное действие пользователя изменило элемент.
        runtime.record_outcome(&mut batch, token, outcome)
    })();
    match &result {
        Ok(true) => tracing::info!(
            stage = "checkpoint",
            code = "checkpoint_succeeded",
            "результат сохранён"
        ),
        Ok(false) => tracing::warn!(
            stage = "checkpoint",
            code = "cas_stale_discard",
            "устаревший результат отброшен"
        ),
        Err(error) => {
            tracing::error!(stage = "checkpoint", code = error.code.as_str(), message = %crate::diagnostics::safe_message(&error.message), event = "checkpoint_failed")
        }
    }
    result
}

fn publish_ready(store: &AssetStore, batch_id: &str) -> Result<(), AssetError> {
    loop {
        let owner_before = owner_snapshot(store)?;
        let publish = {
            let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
            let mut batch = runtime
                .load()?
                .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
            let before_reconcile = batch.clone();
            batch.reconcile_owner(&owner_before)?;
            if batch != before_reconcile {
                runtime.save(&batch)?;
            }

            let next_publication = batch.items.iter().find_map(|item| match item.status() {
                PitchBatchItemStatus::AcquiredVerified => item
                    .current_candidate_sha256
                    .clone()
                    .zip(item.current_candidate_attempt_index)
                    .map(|(sha, attempt_index)| {
                        (
                            item.identity.key.clone(),
                            sha,
                            attempt_index,
                            item.refresh_expected_sha256.clone(),
                            true,
                        )
                    }),
                PitchBatchItemStatus::PublicationPending => item
                    .publication
                    .as_ref()
                    .filter(|publication| {
                        publication.status == PitchBatchPublicationStatus::Pending
                    })
                    .map(|publication| {
                        (
                            item.identity.key.clone(),
                            publication.candidate_sha256.clone(),
                            publication.candidate_attempt_index,
                            publication.expected_previous_sha256.clone(),
                            false,
                        )
                    }),
                _ => None,
            });
            let Some((
                surface,
                candidate_sha256,
                candidate_attempt_index,
                expected_previous_sha256,
                begin_intent,
            )) = next_publication
            else {
                return Ok(());
            };
            if batch
                .item(&surface)
                .is_none_or(|item| item.owner_current_sha256 != expected_previous_sha256)
            {
                return Err(AssetError::new(
                    ErrorCode::IdentityConflict,
                    "снимок владельца изменился после подготовки кандидата; выполните status и явно примите новый SHA через reacquire",
                ));
            }
            if begin_intent {
                batch.begin_publication(
                    &surface,
                    &candidate_sha256,
                    expected_previous_sha256.clone(),
                )?;
                runtime.save(&batch)?;
            }
            let candidate = batch
                .item(&surface)
                .and_then(|item| item.candidate_at(candidate_attempt_index, &candidate_sha256))
                .cloned()
                .ok_or_else(|| invalid_plan("кандидат публикации отсутствует"))?;
            let bytes = runtime.read_candidate(&batch, &candidate)?;
            let request = VerifiedIngestRequest {
                identity: candidate_identity(&batch, &surface)?,
                bytes,
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb.io".into(),
                },
                domain_metadata: Some(serde_json::to_value(&candidate.metadata).map_err(
                    |error| {
                        AssetError::new(
                            ErrorCode::InvalidValidationEvidence,
                            format!("не удалось сериализовать метаданные pitch accent: {error}"),
                        )
                    },
                )?),
                replace_expected_sha256: expected_previous_sha256,
            };
            Some(request)
        };

        let Some(request) = publish else {
            return Ok(());
        };
        tracing::info!(
            stage = "owner_publication",
            code = "publication_begin",
            batch_id,
            identity = %safe_message(request.identity.key.as_str()),
            "публикация проверенного кандидата"
        );
        let outcome = store.ingest_verified(request, &PitchAccentImageValidator).inspect_err(|error| {
            tracing::error!(stage = "owner_publication", code = error.code.as_str(), batch_id, message = %crate::diagnostics::safe_message(&error.message), event = "publication_failed");
        })?;
        if outcome.status != SemanticStatus::Verified || outcome.asset.is_none() {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "хранилище отказало в публикации кандидата pitch-accent со статусом VERIFIED",
            ));
        }

        let owner_after = owner_snapshot(store)?;
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        batch.reconcile_owner(&owner_after)?;
        runtime.save(&batch)?;
        tracing::info!(
            stage = "owner_publication",
            code = "publication_reconciled",
            batch_id,
            "публикация подтверждена снимком владельца"
        );
    }
}

fn candidate_identity(
    batch: &PitchAccentBatch,
    surface: &str,
) -> Result<AssetIdentity, AssetError> {
    batch
        .item(surface)
        .map(|item| item.identity.clone())
        .ok_or_else(|| invalid_plan("identity отсутствует в batch"))
}

fn reconcile_batch(store: &AssetStore, batch: &mut PitchAccentBatch) -> Result<(), AssetError> {
    let snapshot = owner_snapshot(store)?;
    batch.reconcile_owner(&snapshot)
}

fn owner_snapshot(store: &AssetStore) -> Result<PitchBatchOwnerSnapshot, AssetError> {
    PitchBatchOwnerSnapshot::from_records(store.verify_integrity()?)
}

fn write_review(
    store: &AssetStore,
    batch_id: &str,
) -> Result<(PitchAccentBatch, PathBuf), AssetError> {
    let owner_records = store.verify_integrity()?;
    let snapshot = PitchBatchOwnerSnapshot::from_records(owner_records.clone())?;
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let mut batch = runtime
        .load()?
        .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
    let before_reconcile = batch.clone();
    batch.reconcile_owner(&snapshot)?;
    if batch != before_reconcile {
        runtime.save(&batch)?;
    }

    let expected = batch
        .items
        .iter()
        .filter_map(|item| {
            let expected_sha = item
                .existing_verified_sha256
                .as_ref()
                .or(item.published_sha256.as_ref())?;
            let record = snapshot.current(&item.identity)?;
            (record.sha256 == *expected_sha
                && record.lifecycle == LifecycleState::Verified
                && record.storage_path.starts_with("assets/"))
            .then(|| (item.identity.clone(), expected_sha.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let identities = expected.keys().cloned().collect::<Vec<_>>();
    let verified = if identities.is_empty() {
        Vec::new()
    } else {
        AssetStore::read_verified_with_policy(
            store.root(),
            &identities,
            &batch.validator,
            &PitchAccentDomainPolicy,
        )?
    };
    let verified_records = verified
        .into_iter()
        .filter_map(|asset| {
            expected
                .get(&asset.record.identity)
                .is_some_and(|sha| *sha == asset.record.sha256)
                .then_some(asset.record)
        })
        .collect::<Vec<_>>();
    let artifact = runtime.write_review(&batch, &verified_records)?;
    Ok((batch, artifact))
}

fn batch_response(
    operation: &str,
    outcome: &str,
    changed: bool,
    store: Option<StoreSummary>,
    batch: &PitchAccentBatch,
    blockers: Vec<String>,
    artifact: Option<String>,
) -> Response {
    Response {
        schema_version: CLI_SCHEMA_VERSION,
        operation: operation.into(),
        outcome: outcome.into(),
        changed,
        store,
        batch_id: Some(batch.batch_id.clone()),
        batch: Some(serde_json::to_value(batch).expect("состояние пакета сериализуется")),
        items: batch
            .items
            .iter()
            .map(|item| ItemSummary {
                surface: item.identity.key.clone(),
                reading: item.request.query.reading.clone(),
                status: item.status(),
                current_candidate_sha256: item.current_candidate_sha256.clone(),
                canonical_sha256: item.canonical_sha256.clone(),
                last_outcome: item.current_outcome().cloned(),
                selection: item.request.selection.clone(),
                failure: match item.current_outcome() {
                    Some(PitchBatchOutcome::Failed { error }) => Some(error.clone()),
                    _ => None,
                },
                failure_retryable: match item.current_outcome() {
                    Some(PitchBatchOutcome::Failed { error }) => Some(is_retryable_failure(error)),
                    _ => None,
                },
                ambiguity: match item.current_outcome() {
                    Some(PitchBatchOutcome::AmbiguousVocabulary { candidates, .. }) => {
                        candidates.clone()
                    }
                    _ => Vec::new(),
                },
                owner_conflict: item.owner_conflict.clone(),
            })
            .collect(),
        records: Vec::new(),
        blockers,
        artifact,
        error: None,
    }
}

fn batch_blockers(batch: &PitchAccentBatch) -> Vec<String> {
    batch
        .items
        .iter()
        .filter(|item| !item.status().is_resolved())
        .map(|item| format!("{}: {}", item.identity.key, status_name(item.status())))
        .collect()
}

fn status_name(status: PitchBatchItemStatus) -> &'static str {
    match status {
        PitchBatchItemStatus::Pending => "pending",
        PitchBatchItemStatus::AcquiredVerified => "acquired_verified",
        PitchBatchItemStatus::CandidateRejected => "candidate_rejected",
        PitchBatchItemStatus::NoPitchAccentOnSource => "no_pitch_accent_on_source",
        PitchBatchItemStatus::AmbiguousVocabulary => "ambiguous_vocabulary",
        PitchBatchItemStatus::VocabularyNotFound => "vocabulary_not_found",
        PitchBatchItemStatus::TechnicalFailure => "technical_failure",
        PitchBatchItemStatus::PublicationPending => "publication_pending",
        PitchBatchItemStatus::Published => "published",
        PitchBatchItemStatus::ExistingVerified => "existing_verified",
        PitchBatchItemStatus::Conflict => "conflict",
    }
}

fn generated_batch_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = GENERATED_BATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("pitch-{timestamp:x}-{:x}-{counter:x}", std::process::id())
}

#[cfg(test)]
#[path = "pitch_cli_tests.rs"]
mod tests;
