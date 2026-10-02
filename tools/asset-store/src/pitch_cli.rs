//! Самостоятельная командная строка для получения и публикации pitch accent.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::browser_runtime::{BrowserRuntimeConfig, BrowserSession, DeviceMetrics};
use crate::domain::AssetDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::jpdb::{
    JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchProvider, JpdbPitchQuery, JpdbPitchRequest,
    JpdbPitchSelection, JpdbPitchStage,
};
use crate::model::{
    AssetIdentity, AssetRecord, HumanDecision, LifecycleState, Provenance, SemanticStatus,
};
use crate::pitch_accent::{PitchAccentDomainPolicy, PitchAccentImageValidator};
use crate::pitch_batch::{
    PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchItemStatus, PitchBatchItemToken,
    PitchBatchOutcome, PitchBatchOwnerSnapshot, PitchBatchPublicationStatus,
};
use crate::store::{AssetStore, HumanAttestationRequest, StoreOptions, VerifiedIngestRequest};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;

const PLAN_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_BYTES: u64 = 8 * 1024 * 1024;

/// Командная строка pitch-accent domain.
#[derive(Debug, Parser)]
#[command(
    name = "pitch-assets",
    version,
    about = "Получение и verified-only публикация изображений pitch accent"
)]
pub struct PitchCli {
    /// Корень локального pitch store; по умолчанию `.asset-store/pitch-accent`.
    #[arg(long, global = true)]
    pub store: Option<PathBuf>,
    /// Корень checkout, относительно которого защищается дерево `decks/`.
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
    /// Просмотр и read-only проверка canonical pitch corpus.
    Corpus {
        #[command(subcommand)]
        command: CorpusCommand,
    },
    /// Одноразовое получение простого запроса или versioned набора запросов.
    Ensure {
        /// Strict versioned JSON-план. Несовместим с аргументами одного запроса.
        #[arg(long, conflicts_with_all = ["surface", "reading", "vocabulary_id", "detail_url"])]
        plan: Option<PathBuf>,
        /// Точная surface form одного запроса.
        #[arg(long)]
        surface: Option<String>,
        /// Необязательное точное чтение запроса.
        #[arg(long, requires = "surface")]
        reading: Option<String>,
        /// Явный JPDB vocabulary ID для разрешения неоднозначности.
        #[arg(long, requires_all = ["surface", "detail_url"])]
        vocabulary_id: Option<u64>,
        /// Точный JPDB detail route для явного выбора.
        #[arg(long, requires_all = ["surface", "vocabulary_id"])]
        detail_url: Option<String>,
        /// Разрешает targeted refresh существующей identity с CAS по её текущему SHA.
        #[arg(long)]
        refresh: bool,
    },
    /// Возобновляемый lifecycle сохранённого пакета.
    Batch {
        #[command(subcommand)]
        command: PitchBatchCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CorpusCommand {
    /// Выводит проверенные canonical records без candidate runtime.
    List,
    /// Проверяет весь присутствующий corpus без создания runtime или lock-файлов.
    Check,
}

#[derive(Debug, Subcommand)]
pub enum PitchBatchCommand {
    /// Создаёт пакет из versioned JSON-плана.
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
    /// Возобновляет пакет тем же lifecycle-путём, что и `run`.
    Resume {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Возвращает сохранённое состояние без сетевого acquisition.
    Status {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Создаёт локальный HTML review artifact в runtime boundary пакета.
    Review {
        #[arg(long, required = true)]
        batch_id: String,
    },
    /// Сохраняет exact human choice; следующий `run` подтвердит его свежим JPDB search.
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
    /// Повторяет только указанный surface; соседние resolved items не сбрасываются.
    Retry {
        #[arg(long, required = true)]
        batch_id: String,
        #[arg(long, required = true)]
        surface: String,
        #[arg(long, required = true)]
        reason: String,
    },
    /// Повторно получает exact identity после явного refresh/reacquire решения.
    Reacquire {
        #[arg(long, required = true)]
        batch_id: String,
        #[arg(long, required = true)]
        surface: String,
        #[arg(long, required = true)]
        reason: String,
    },
    /// Отмечает точные визуально отклонённые bytes без влияния на следующие SHA.
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

/// Строгая внешняя схема плана. Версия сохраняется вместе с входными данными batch.
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

/// Вывод CLI, совместимый по оболочке с `kanji-assets`.
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
}

#[derive(Debug, Serialize)]
struct Response {
    schema_version: u32,
    operation: String,
    outcome: String,
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

/// Выполняет команду, отдавая всё machine-readable содержимое в stdout при JSON-режиме.
pub async fn execute(cli: PitchCli) -> PitchCliOutput {
    let operation = operation_name(&cli.command).to_owned();
    let store_path = store_path(cli.store.as_deref(), &cli.repository_root);
    let prevalidated = prevalidate_command(&cli.command);
    let (plan, direct_items) = match prevalidated {
        Ok(value) => value,
        Err(error) => {
            return render_error(operation, None, error, cli.output);
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
        Err(error) => return render_error(operation, Some(&store_path), error, cli.output),
    };
    let summary = StoreSummary {
        path: store.root().display().to_string(),
        store_id: store.store_id().to_owned(),
    };

    match cli.command {
        PitchCommand::Corpus {
            command: CorpusCommand::List,
        } => list_corpus(&store, operation, summary, cli.output),
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
            )
            .await
        }
        PitchCommand::Batch { command } => {
            execute_batch(
                &store,
                summary,
                command,
                plan.map(|plan| plan.items),
                cli.output,
            )
            .await
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
                    "ensure требует `--plan` либо `--surface` с optional reading",
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
                        schema_version: PLAN_SCHEMA_VERSION,
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
    if reason.trim().is_empty() || reason.len() > 4096 {
        return Err(invalid_plan(
            "reason должна содержать непустое пояснение длиной не более 4096 байт",
        ));
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
    file.by_ref()
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
    if plan.schema_version != PLAN_SCHEMA_VERSION {
        return Err(invalid_plan(format!(
            "неподдерживаемая версия JSON-плана {}; ожидается {PLAN_SCHEMA_VERSION}",
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
                        "surface `{}` повторяется с несовместимыми reading или JPDB selection",
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
        (ancestor.join("Cargo.toml").is_file() && ancestor.join("decks").is_dir())
            .then(|| ancestor.to_path_buf())
    })
}

fn open_store(path: &Path, repository_root: &Path, create: bool) -> Result<AssetStore, AssetError> {
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
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить текущий каталог", error))?
            .join(path)
    };
    let lexical = normalize_path(&absolute);
    let mut decks = discover_decks_roots(&absolute);
    decks.extend(discover_decks_roots(repository_root));
    for root in decks {
        let lexical_root = normalize_path(&root);
        if lexical.starts_with(&lexical_root) || lexical_root.starts_with(&lexical) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "pitch store пересекается с защищённым деревом `decks/`",
            ));
        }
        if let (Ok(canonical_path), Ok(canonical_root)) =
            (std::fs::canonicalize(path), std::fs::canonicalize(root))
            && (canonical_path.starts_with(&canonical_root)
                || canonical_root.starts_with(&canonical_path))
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "pitch store пересекается с защищённым деревом `decks/` через alias",
            ));
        }
    }
    Ok(())
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
        Err(error) => render_error(operation, Some(path), error, output),
    }
}

fn list_corpus(
    store: &AssetStore,
    operation: String,
    summary: StoreSummary,
    output: OutputFormat,
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
                changed: false,
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
        Err(error) => render_error(operation, None, error, output),
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
            let mut stdout = format!(
                "Операция: {}; результат: {}; изменено: {}\n",
                response.operation,
                response.outcome,
                if response.changed { "да" } else { "нет" }
            );
            if let Some(store) = response.store {
                stdout.push_str(&format!("Store: {}\n", store.path));
            }
            if let Some(batch_id) = response.batch_id {
                stdout.push_str(&format!("Пакет: {batch_id}\n"));
            }
            for record in response.records {
                stdout.push_str(&format!(
                    "{}  {}  SHA-256={}  consumer={}\n",
                    record.identity, record.lifecycle, record.sha256, record.consumer_filename
                ));
            }
            for item in response.items {
                stdout.push_str(&format!(
                    "{}  {}  candidate_SHA-256={}  canonical_SHA-256={}\n",
                    item.surface,
                    status_name(item.status),
                    item.current_candidate_sha256.as_deref().unwrap_or("-"),
                    item.canonical_sha256.as_deref().unwrap_or("-")
                ));
            }
            for blocker in response.blockers {
                stdout.push_str(&format!("Блокер: {blocker}\n"));
            }
            if let Some(path) = response.artifact {
                stdout.push_str(&format!("Артефакт проверки: {path}\n"));
            }
            if let Some(error) = response.error {
                stdout.push_str(&format!("Ошибка {}: {}\n", error.code, error.message));
            }
            PitchCliOutput {
                stdout,
                stderr: String::new(),
                exit_code,
            }
        }
    }
}

fn render_error(
    operation: String,
    path: Option<&Path>,
    error: AssetError,
    output: OutputFormat,
) -> PitchCliOutput {
    let exit_code = error.exit_code();
    render_response(
        Response {
            schema_version: CLI_SCHEMA_VERSION,
            operation,
            outcome: "failed".into(),
            changed: false,
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
                false,
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
            changed: false,
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
) -> PitchCliOutput {
    let batch_id = generated_batch_id();
    let result = (|| {
        let mut batch = create_batch(store, &batch_id, &items)?;
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
        Ok(batch) => match run_batch(store, &batch.batch_id).await {
            Ok((batch, _run_changed)) => {
                let blockers = batch_blockers(&batch);
                let resolved = batch.is_resolved();
                render_response(
                    batch_response(
                        "ensure",
                        if resolved { "resolved" } else { "needs_review" },
                        true,
                        Some(summary),
                        &batch,
                        blockers,
                        None,
                    ),
                    output,
                    if resolved { 0 } else { 3 },
                )
            }
            Err(error) => render_batch_error(store, summary, &batch_id, "ensure", error, output),
        },
        Err(error) => render_batch_error(store, summary, &batch_id, "ensure", error, output),
    }
}

async fn execute_batch(
    store: &AssetStore,
    summary: StoreSummary,
    command: PitchBatchCommand,
    plan: Option<Vec<PitchPlanItem>>,
    output: OutputFormat,
) -> PitchCliOutput {
    match command {
        PitchBatchCommand::Start { batch_id, .. } => {
            let batch_id = batch_id.unwrap_or_else(generated_batch_id);
            match create_batch(store, &batch_id, &plan.unwrap_or_default()) {
                Ok(batch) => render_response(
                    batch_response(
                        "batch_start",
                        "started",
                        true,
                        Some(summary),
                        &batch,
                        batch_blockers(&batch),
                        None,
                    ),
                    output,
                    0,
                ),
                Err(error) => {
                    render_batch_error(store, summary, &batch_id, "batch_start", error, output)
                }
            }
        }
        PitchBatchCommand::Run { batch_id } => {
            run_batch_output(store, &batch_id, "batch_run", summary, output).await
        }
        PitchBatchCommand::Resume { batch_id } => {
            run_batch_output(store, &batch_id, "batch_resume", summary, output).await
        }
        PitchBatchCommand::Status { batch_id } => match load_batch(store, &batch_id) {
            Ok(batch) => render_response(
                batch_response(
                    "batch_status",
                    if batch.is_resolved() {
                        "resolved"
                    } else {
                        "pending"
                    },
                    false,
                    Some(summary),
                    &batch,
                    batch_blockers(&batch),
                    None,
                ),
                output,
                0,
            ),
            Err(error) => {
                render_batch_error(store, summary, &batch_id, "batch_status", error, output)
            }
        },
        PitchBatchCommand::Review { batch_id } => match write_review(store, &batch_id) {
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
            Err(error) => {
                render_batch_error(store, summary, &batch_id, "batch_review", error, output)
            }
        },
        PitchBatchCommand::Select {
            batch_id,
            surface,
            vocabulary_id,
            detail_url,
        } => update_batch(store, &batch_id, "batch_select", output, summary, |batch| {
            batch.select_candidate(&surface, vocabulary_id, detail_url)
        }),
        PitchBatchCommand::Retry {
            batch_id,
            surface,
            reason,
        } => update_batch(store, &batch_id, "batch_retry", output, summary, |batch| {
            batch.retry(&surface, reason)
        }),
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
) -> PitchCliOutput {
    match run_batch(store, batch_id).await {
        Ok((batch, changed)) => {
            let resolved = batch.is_resolved();
            render_response(
                batch_response(
                    operation,
                    if resolved { "resolved" } else { "needs_review" },
                    changed,
                    Some(summary),
                    &batch,
                    batch_blockers(&batch),
                    None,
                ),
                output,
                if resolved { 0 } else { 3 },
            )
        }
        Err(error) => render_batch_error(store, summary, batch_id, operation, error, output),
    }
}

fn update_batch(
    store: &AssetStore,
    batch_id: &str,
    operation: &'static str,
    output: OutputFormat,
    summary: StoreSummary,
    update: impl FnOnce(&mut PitchAccentBatch) -> Result<(), AssetError>,
) -> PitchCliOutput {
    let result = (|| {
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        update(&mut batch)?;
        runtime.save(&batch)?;
        Ok(batch)
    })();
    match result {
        Ok(batch) => render_response(
            batch_response(
                operation,
                "updated",
                true,
                Some(summary),
                &batch,
                batch_blockers(&batch),
                None,
            ),
            output,
            0,
        ),
        Err(error) => render_batch_error(store, summary, batch_id, operation, error, output),
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
                "точный candidate SHA отсутствует в batch history",
            ));
        }

        // Точный candidate batch проверяется до любого изменения owner. Если
        // owner record содержит ту же identity и SHA, решение пользователя также
        // помещает именно эти canonical bytes в quarantine через API store.
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
            // Сохраняем отклонённый SHA owner как текущее CAS-наблюдение. Из этого
            // состояния разрешён явный `reacquire`; принятый новый candidate заменит
            // именно эти байты.
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
                true,
                Some(summary),
                &batch,
                batch_blockers(&batch),
                None,
            ),
            output,
            0,
        ),
        Err(error) => render_batch_error(store, summary, batch_id, "batch_reject", error, output),
    }
}

fn create_batch(
    store: &AssetStore,
    batch_id: &str,
    items: &[PitchPlanItem],
) -> Result<PitchAccentBatch, AssetError> {
    crate::batch_runtime::validate_batch_id(batch_id)?;
    let requested = items
        .iter()
        .map(PitchPlanItem::request)
        .collect::<Result<Vec<_>, _>>()?;
    let validator = PitchAccentImageValidator::validator_identity();
    let proposed = PitchAccentBatch::new(batch_id, requested.clone(), validator)?;
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let mut batch = match runtime.load()? {
        Some(existing) => {
            if !plan_matches_batch(&requested, &existing) {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    "batch_id уже занят пакетом с другим versioned plan",
                    json!({"batch_id": batch_id}),
                ));
            }
            existing
        }
        None => {
            runtime.save(&proposed)?;
            proposed
        }
    };
    let records = store.verify_integrity()?;
    let snapshot = PitchBatchOwnerSnapshot::from_records(records)?;
    batch.reconcile_owner(&snapshot)?;
    runtime.save(&batch)?;
    Ok(batch)
}

fn plan_matches_batch(requests: &[JpdbPitchRequest], batch: &PitchAccentBatch) -> bool {
    if requests.len() != batch.items.len() {
        return false;
    }
    requests.iter().zip(&batch.items).all(|(request, item)| {
        item.request == *request
            || item
                .attempts
                .iter()
                .any(|attempt| attempt.request == *request)
    })
}

fn load_batch(store: &AssetStore, batch_id: &str) -> Result<PitchAccentBatch, AssetError> {
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    runtime
        .load()?
        .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))
}

async fn run_batch(
    store: &AssetStore,
    batch_id: &str,
) -> Result<(PitchAccentBatch, bool), AssetError> {
    let initial_revision = {
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        let initial_revision = batch.revision;
        reconcile_batch(store, &mut batch)?;
        runtime.save(&batch)?;
        initial_revision
    };

    publish_ready(store, batch_id)?;

    let acquisitions = {
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        reconcile_batch(store, &mut batch)?;
        runtime.save(&batch)?;
        let mut pending = Vec::new();
        for item in &batch.items {
            if item.status() == PitchBatchItemStatus::Pending {
                pending.push((batch.item_token(&item.identity.key)?, item.request.clone()));
            }
        }
        pending
    };

    if !acquisitions.is_empty() {
        let metrics = DeviceMetrics::new(1280, 1200, 3.0)
            .map_err(|message| AssetError::new(ErrorCode::InvalidValidationEvidence, message))?;
        let config = BrowserRuntimeConfig {
            device_metrics: Some(metrics),
            prefers_color_scheme: Some("dark".into()),
            ..BrowserRuntimeConfig::default()
        };
        match BrowserSession::launch(config).await {
            Ok(session) => {
                let acquisition_result = async {
                    for (token, request) in acquisitions {
                        // На время browser search/capture блокировка batch снята.
                        // Каждый точный результат сохраняется до начала следующего элемента.
                        let mut outcomes = JpdbPitchProvider::acquire_requests_in_session(
                            &session,
                            std::slice::from_ref(&request),
                        )
                        .await;
                        if outcomes.len() != 1 {
                            return Err(AssetError::new(
                                ErrorCode::ValidatorFailure,
                                "JPDB provider вернул число результатов, отличное от одного запроса",
                            ));
                        }
                        let outcome = outcomes.pop().ok_or_else(|| {
                            AssetError::new(
                                ErrorCode::ValidatorFailure,
                                "JPDB provider не вернул результат для элемента",
                            )
                        })?;
                        record_one_outcome(store, batch_id, &token, outcome)?;
                        publish_ready(store, batch_id)?;
                    }
                    Ok::<(), AssetError>(())
                }
                .await;
                session.close().await;
                acquisition_result?;
            }
            Err(message) => {
                for (token, _) in acquisitions {
                    record_one_outcome(
                        store,
                        batch_id,
                        &token,
                        JpdbPitchOutcome::Failed {
                            error: JpdbPitchFailure::BrowserSetup {
                                stage: JpdbPitchStage::ConfigureBrowser,
                                message: message.clone(),
                            },
                        },
                    )?;
                }
            }
        }
    }

    let batch = {
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        reconcile_batch(store, &mut batch)?;
        runtime.save(&batch)?;
        batch
    };
    Ok((batch.clone(), batch.revision != initial_revision))
}

fn record_one_outcome(
    store: &AssetStore,
    batch_id: &str,
    token: &PitchBatchItemToken,
    outcome: JpdbPitchOutcome,
) -> Result<(), AssetError> {
    let owner = owner_snapshot(store)?;
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let mut batch = runtime
        .load()?
        .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
    batch.reconcile_owner(&owner)?;
    runtime.save(&batch)?;
    // record_outcome сохраняет полученные байты до смены состояния. Его token CAS
    // отбрасывает результат, если параллельное действие пользователя изменило этот элемент.
    let _recorded = runtime.record_outcome(&mut batch, token, outcome)?;
    Ok(())
}

fn publish_ready(store: &AssetStore, batch_id: &str) -> Result<(), AssetError> {
    loop {
        let owner_before = owner_snapshot(store)?;
        let publish = {
            let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
            let mut batch = runtime
                .load()?
                .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
            batch.reconcile_owner(&owner_before)?;
            runtime.save(&batch)?;

            let next_publication = batch.items.iter().find_map(|item| match item.status() {
                PitchBatchItemStatus::AcquiredVerified => {
                    item.current_candidate_sha256.clone().map(|sha| {
                        (
                            item.identity.key.clone(),
                            sha,
                            item.refresh_expected_sha256.clone(),
                            true,
                        )
                    })
                }
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
                            publication.expected_previous_sha256.clone(),
                            false,
                        )
                    }),
                _ => None,
            });
            let Some((surface, candidate_sha256, expected_previous_sha256, begin_intent)) =
                next_publication
            else {
                return Ok(());
            };
            if batch
                .item(&surface)
                .is_none_or(|item| item.owner_current_sha256 != expected_previous_sha256)
            {
                return Err(AssetError::new(
                    ErrorCode::IdentityConflict,
                    "owner snapshot изменился после подготовки candidate; выполните status и примите решение",
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
                .and_then(|item| item.candidate(&candidate_sha256))
                .cloned()
                .ok_or_else(|| invalid_plan("candidate для publication отсутствует"))?;
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
                            format!("не удалось сериализовать pitch metadata: {error}"),
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
        let outcome = store.ingest_verified(request, &PitchAccentImageValidator)?;
        if outcome.status != SemanticStatus::Verified || outcome.asset.is_none() {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "store отказал в VERIFIED публикации pitch candidate",
            ));
        }

        let owner_after = owner_snapshot(store)?;
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
        let mut batch = runtime
            .load()?
            .ok_or_else(|| invalid_plan("сохранённое состояние batch не найдено"))?;
        batch.reconcile_owner(&owner_after)?;
        runtime.save(&batch)?;
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
    batch.reconcile_owner(&snapshot)?;
    runtime.save(&batch)?;

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
        batch: Some(serde_json::to_value(batch).expect("batch state сериализуется")),
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
