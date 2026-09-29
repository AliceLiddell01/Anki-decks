//! CLI adapter предметной kanji boundary поверх общего `asset_store` core.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{AssetError, ErrorCode};
use crate::kanji_validator::KanjiImageValidator;
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, SemanticStatus, ValidationEvidence,
    ValidatorIdentity,
};
use crate::selection::SelectionMode;
use crate::store::{
    AssetStore, IngestRequest, StoreOptions, VerifiedIngestRequest, fd_canonical_path,
    open_source_file,
};
use crate::validation::SemanticValidator;
use crate::yarxi::{AcquiredMedia, SelectionResult, acquire_many};

/// Командная строка `kanji-assets`.
#[derive(Debug, Parser)]
#[command(
    name = "kanji-assets",
    version,
    about = "Безопасный локальный lifecycle store для kanji assets"
)]
pub struct Cli {
    /// Program-owned runtime root; содержимое по умолчанию исключено из Git.
    #[arg(long, global = true)]
    pub store: Option<PathBuf>,
    /// Корень checkout, относительно которого запрещено пересечение с `decks/`.
    #[arg(long, global = true, default_value = ".")]
    pub repository_root: PathBuf,
    /// Формат вывода.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub output: OutputFormat,
    /// Разрешает пройти ожидаемый TLS interstitial только для www.yarxi.su.
    #[arg(long, global = true)]
    pub allow_insecure_tls: bool,
    #[command(subcommand)]
    pub command: Command,
}

/// CLI команды.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Создаёт пустой store либо проверяет уже существующий.
    Init,
    /// Явно импортирует ровно один локальный файл как pending candidate.
    Ingest {
        /// Unicode character или последовательность, например `漢`.
        #[arg(long)]
        character: String,
        /// Один явно названный source file.
        #[arg(long)]
        file: PathBuf,
        /// Разрешает замену только если текущий hash совпадает с этим значением.
        #[arg(long)]
        replace_expected_sha256: Option<String>,
    },
    /// Проверяет integrity и выводит текущие записи manifest.
    List,
    /// Показывает детерминированный набор целей без записи.
    Plan {
        #[arg(long, value_enum)]
        mode: ModeArg,
        #[arg(long)]
        validator_id: String,
        #[arg(long)]
        validator_version: String,
    },
    /// Получает media через Yarxi и публикует только после semantic VERIFIED.
    Ensure {
        /// Один или несколько символов; каждый аргумент должен содержать один Unicode scalar.
        #[arg(required = true, num_args = 1..)]
        characters: Vec<String>,
    },
    /// Запускает production semantic validator для существующего корпуса.
    Validate {
        #[arg(long, value_enum)]
        mode: ModeArg,
    },
}

/// Human или machine-readable вывод.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

/// Режим отбора для `plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    New,
    Full,
}

impl From<ModeArg> for SelectionMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::New => Self::New,
            ModeArg::Full => Self::Full,
        }
    }
}

/// Kanji character identity без нормализации или догадки по имени файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanjiCharacter(String);

impl FromStr for KanjiCharacter {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.chars().count() != 1 || value.len() > 4 || value.chars().any(char::is_control) {
            return Err(
                "character должен содержать ровно один Unicode scalar без управляющих символов"
                    .into(),
            );
        }
        Ok(Self(value.to_owned()))
    }
}

impl KanjiCharacter {
    fn identity(&self) -> AssetIdentity {
        AssetIdentity {
            namespace: "kanji".to_owned(),
            key: self.0.clone(),
        }
    }

    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "domain": "kanji",
            "character": self.0,
            "unicode_codepoints": self
                .0
                .chars()
                .map(|character| format!("U+{:04X}", u32::from(character)))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct StoreSummary {
    path: String,
    store_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct AssetSummary {
    identity: AssetIdentity,
    from_state: Option<LifecycleState>,
    to_state: Option<LifecycleState>,
    sha256: Option<String>,
    previous_sha256: Option<String>,
    validation_status: Option<SemanticStatus>,
    validator: Option<ValidatorIdentity>,
    evidence: Vec<crate::model::ValidationEvidence>,
    domain_metadata: Option<serde_json::Value>,
    changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_outcome: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConflictSummary {
    code: String,
    identity: Option<AssetIdentity>,
    existing_sha256: Option<String>,
    candidate_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorSummary {
    code: String,
    category: &'static str,
    message: String,
    details: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Response {
    schema_version: u32,
    operation: String,
    store: Option<StoreSummary>,
    mode: Option<String>,
    assets: Vec<AssetSummary>,
    changed: bool,
    conflicts: Vec<ConflictSummary>,
    blockers: Vec<String>,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validator_available: Option<bool>,
}

/// Обработанный CLI-ответ и его process exit code.
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: u8,
}

/// Выполняет команду и рендерит стабильный stdout.
pub fn execute(cli: Cli) -> CliOutput {
    let operation = cli.command.operation_name().to_owned();
    let store_path = cli
        .store
        .clone()
        .unwrap_or_else(|| default_store_path(&cli.repository_root));
    let mut protected_roots = BTreeSet::new();
    protected_roots.insert(cli.repository_root.join("decks"));
    for protected_root in discover_decks_roots(&store_path) {
        protected_roots.insert(protected_root);
    }
    for protected_root in discover_decks_roots(&cli.repository_root) {
        protected_roots.insert(protected_root);
    }
    if let Command::Ingest { file, .. } = &cli.command {
        for protected_root in discover_decks_roots(file) {
            protected_roots.insert(protected_root);
        }
    }
    let prevalidated = match &cli.command {
        Command::Ingest {
            character, file, ..
        } => character
            .parse::<KanjiCharacter>()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))
            .and_then(|character| {
                validate_explicit_source(file, &protected_roots)
                    .map(|source| (Some(character.identity()), Some(source)))
            }),
        Command::Plan {
            validator_id,
            validator_version,
            ..
        } => ValidatorIdentity::new(validator_id.clone(), validator_version.clone())
            .map(|_| (None, None))
            .map_err(|message| AssetError::new(ErrorCode::InvalidValidatorIdentity, message)),
        Command::Ensure { characters } => characters
            .iter()
            .map(|value| value.parse::<KanjiCharacter>())
            .collect::<Result<Vec<_>, _>>()
            .map(|_| (None, None))
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message)),
        Command::Validate { .. } => Ok((None, None)),
        _ => Ok((None, None)),
    };
    let (selected_identity, source_file) = match prevalidated {
        Ok((identity, source)) => (identity, source),
        Err(error) => {
            return render_error(
                operation,
                None,
                error,
                StoreSummary {
                    path: store_path.display().to_string(),
                    store_id: None,
                },
                cli.output,
                false,
            );
        }
    };
    let mut options = StoreOptions::new(&store_path);
    for protected_root in protected_roots {
        options = options.protect_from(protected_root);
    }
    let open_existing = matches!(
        &cli.command,
        Command::List | Command::Plan { .. } | Command::Validate { .. }
    );
    let open_result = if open_existing {
        AssetStore::open_existing(options)
    } else {
        AssetStore::open(options)
    };
    match open_result {
        Ok(store) => {
            let store_summary = StoreSummary {
                path: store.root().display().to_string(),
                store_id: Some(store.store_id().to_owned()),
            };
            match execute_with_store(
                &store,
                store_summary.clone(),
                cli.command,
                source_file,
                cli.allow_insecure_tls,
            ) {
                Ok((response, exit_code)) => render_response(response, cli.output, exit_code),
                Err(error) => render_error(
                    operation,
                    selected_identity,
                    error,
                    store_summary,
                    cli.output,
                    store.initialized_on_open(),
                ),
            }
        }
        Err(error) => render_error(
            operation,
            selected_identity,
            error,
            StoreSummary {
                path: store_path.display().to_string(),
                store_id: None,
            },
            cli.output,
            false,
        ),
    }
}

fn validate_explicit_source(
    path: &std::path::Path,
    protected_roots: &BTreeSet<PathBuf>,
) -> Result<std::fs::File, AssetError> {
    let lexical_source = normalize_absolute_path(path)?;
    for protected_root in protected_roots {
        let lexical_root = normalize_absolute_path(protected_root)?;
        if lexical_source.starts_with(&lexical_root) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "kanji CLI не читает source из защищённого дерева decks",
            ));
        }
    }

    let source = open_source_file(path)?;
    let opened_source = fd_canonical_path(&source)?;
    for protected_root in protected_roots {
        let canonical_root = match std::fs::canonicalize(protected_root) {
            Ok(root) => Some(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(AssetError::io(
                    "не удалось проверить защищённый каталог decks",
                    error,
                ));
            }
        };
        if canonical_root.is_some_and(|root| opened_source.starts_with(root)) {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "kanji CLI не читает source из защищённого дерева decks",
            ));
        }
    }
    Ok(source)
}

fn normalize_absolute_path(path: &std::path::Path) -> Result<PathBuf, AssetError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить cwd", error))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
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
    Ok(normalized)
}

fn default_store_path(repository_root: &std::path::Path) -> PathBuf {
    find_workspace_root(repository_root)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| find_workspace_root(&cwd))
        })
        .map(|root| root.join(".asset-store/kanji"))
        .unwrap_or_else(|| PathBuf::from(".asset-store/kanji"))
}

fn find_workspace_root(start: &std::path::Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(start).ok()?;
    canonical.ancestors().find_map(|ancestor| {
        (ancestor.join("Cargo.toml").is_file() && ancestor.join("decks").is_dir())
            .then(|| ancestor.to_path_buf())
    })
}

/// Находит только прямые `decks/` у предков cwd и указанного store path.
/// Это проверка владения путём, не обход дерева и не чтение media.
fn discover_decks_roots(path: &std::path::Path) -> Vec<PathBuf> {
    let absolute = match normalize_absolute_path(path) {
        Ok(absolute) => absolute,
        Err(_) => return Vec::new(),
    };
    let mut roots = BTreeSet::new();
    for ancestor in absolute.ancestors() {
        let candidate = ancestor.join("decks");
        if is_directory_or_directory_symlink(&candidate) {
            roots.insert(candidate);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            let candidate = ancestor.join("decks");
            if is_directory_or_directory_symlink(&candidate) {
                roots.insert(candidate);
            }
        }
    }
    roots.into_iter().collect()
}

fn is_directory_or_directory_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_dir()
            || (metadata.file_type().is_symlink()
                && std::fs::metadata(path).is_ok_and(|target| target.is_dir()))
    })
}

impl Command {
    fn operation_name(&self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Ingest { .. } => "ingest",
            Self::List => "list",
            Self::Plan { .. } => "plan",
            Self::Ensure { .. } => "ensure",
            Self::Validate { .. } => "validate",
        }
    }
}

fn execute_with_store(
    store: &AssetStore,
    store_summary: StoreSummary,
    command: Command,
    mut source_file: Option<std::fs::File>,
    allow_insecure_tls: bool,
) -> Result<(Response, u8), AssetError> {
    match command {
        Command::Init => {
            let changed = store.initialized_on_open();
            Ok((
                response(
                    "init",
                    store_summary,
                    None,
                    Vec::new(),
                    changed,
                    if changed {
                        "initialized"
                    } else {
                        "already_initialized"
                    },
                ),
                0,
            ))
        }
        Command::Ingest {
            character,
            file,
            replace_expected_sha256,
        } => {
            let character = character
                .parse::<KanjiCharacter>()
                .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
            let outcome = store.ingest_from_file(
                IngestRequest {
                    identity: character.identity(),
                    source_path: file,
                    domain_metadata: Some(character.metadata()),
                    replace_expected_sha256,
                },
                source_file.take().ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::IoFailure,
                        "CLI потерял проверенный explicit source handle",
                    )
                })?,
            )?;
            let summary = AssetSummary {
                identity: outcome.asset.identity.clone(),
                from_state: outcome.previous.as_ref().map(|asset| asset.lifecycle),
                to_state: Some(outcome.asset.lifecycle),
                sha256: Some(outcome.asset.sha256.clone()),
                previous_sha256: outcome.previous.as_ref().map(|asset| asset.sha256.clone()),
                validation_status: outcome
                    .asset
                    .validation
                    .as_ref()
                    .map(|validation| validation.status),
                validator: outcome
                    .asset
                    .validation
                    .as_ref()
                    .map(|validation| validation.validator.clone()),
                evidence: outcome
                    .asset
                    .validation
                    .as_ref()
                    .map(|validation| validation.evidence.clone())
                    .unwrap_or_default(),
                domain_metadata: outcome.asset.domain_metadata.clone(),
                changed: outcome.changed,
                item_outcome: Some(if outcome.changed {
                    "candidate_created".into()
                } else {
                    "already_present".into()
                }),
            };
            let changed = outcome.changed || store.initialized_on_open();
            Ok((
                response(
                    "ingest",
                    store_summary,
                    None,
                    vec![summary],
                    changed,
                    if outcome.changed {
                        "candidate_created"
                    } else {
                        "already_present"
                    },
                ),
                0,
            ))
        }
        Command::List => {
            let assets = store.verify_integrity()?;
            let summaries = assets.into_iter().map(summary_for_current).collect();
            Ok((
                response(
                    "list",
                    store_summary,
                    None,
                    summaries,
                    store.initialized_on_open(),
                    "ok",
                ),
                0,
            ))
        }
        Command::Plan {
            mode,
            validator_id,
            validator_version,
        } => {
            let validator = ValidatorIdentity::new(validator_id, validator_version)
                .map_err(|message| AssetError::new(ErrorCode::InvalidValidatorIdentity, message))?;
            let selection_mode: SelectionMode = mode.into();
            let assets = store.select(selection_mode, &validator)?;
            let summaries = assets.into_iter().map(summary_for_current).collect();
            let mut response = response(
                "plan",
                store_summary,
                Some(selection_mode.as_str().to_owned()),
                summaries,
                store.initialized_on_open(),
                "planned",
            );
            response.validator_available = Some(true);
            Ok((response, 0))
        }
        Command::Ensure { characters } => {
            ensure_characters(store, store_summary, characters, allow_insecure_tls)
        }
        Command::Validate { mode } => {
            let selection_mode: SelectionMode = mode.into();
            let validator = KanjiImageValidator::new();
            let report = store.validate(selection_mode, &validator)?;
            let mut summaries = Vec::with_capacity(report.attempts.len());
            let mut blockers = report.blockers.clone();
            for attempt in &report.attempts {
                let successful =
                    attempt.status == Some(SemanticStatus::Verified) && attempt.blocker.is_none();
                if !successful {
                    blockers.push(format!(
                        "{}:{}",
                        attempt.identity,
                        attempt
                            .blocker
                            .as_deref()
                            .or_else(|| attempt.status.map(SemanticStatus::as_str))
                            .unwrap_or("validation_failed")
                    ));
                }
                summaries.push(AssetSummary {
                    identity: attempt.identity.clone(),
                    from_state: Some(attempt.from_state),
                    to_state: Some(attempt.to_state),
                    sha256: Some(attempt.content_sha256.clone()),
                    previous_sha256: None,
                    validation_status: attempt.status,
                    validator: Some(report.validator.clone()),
                    evidence: attempt.evidence.clone(),
                    domain_metadata: None,
                    changed: attempt.changed,
                    item_outcome: Some(if successful {
                        "verified".into()
                    } else {
                        attempt.blocker.clone().unwrap_or_else(|| {
                            attempt
                                .status
                                .map(|status| status.as_str())
                                .unwrap_or("failed")
                                .to_owned()
                        })
                    }),
                });
            }
            let mut response = response(
                "validate",
                store_summary,
                Some(selection_mode.as_str().to_owned()),
                summaries,
                report.changed > 0 || store.initialized_on_open(),
                if blockers.is_empty() {
                    "validated"
                } else {
                    "validation_blocked"
                },
            );
            response.blockers = blockers.clone();
            response.validator_available = Some(true);
            Ok((response, if blockers.is_empty() { 0 } else { 3 }))
        }
    }
}

fn ensure_characters(
    store: &AssetStore,
    store_summary: StoreSummary,
    raw_characters: Vec<String>,
    allow_insecure_tls: bool,
) -> Result<(Response, u8), AssetError> {
    let characters = raw_characters
        .iter()
        .map(|value| {
            value
                .parse::<KanjiCharacter>()
                .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let validator = KanjiImageValidator::new();
    let validator_id = validator.identity();
    let current = store.verify_integrity()?;
    let mut summaries = Vec::with_capacity(characters.len());
    let mut missing = Vec::new();
    let mut missing_indices = Vec::new();
    let mut blockers = Vec::new();

    for character in &characters {
        let existing = current
            .iter()
            .find(|asset| asset.identity == character.identity());
        let is_current = existing.is_some_and(|asset| {
            asset.lifecycle == LifecycleState::Verified
                && asset.validation.as_ref().is_some_and(|decision| {
                    decision.status == SemanticStatus::Verified
                        && decision.content_sha256 == asset.sha256
                        && decision.validator == validator_id
                })
        });
        if is_current {
            let mut summary =
                summary_for_current(existing.expect("verified record exists").clone());
            summary.item_outcome = Some("already_verified".into());
            summaries.push(Some(summary));
        } else {
            missing_indices.push(summaries.len());
            missing.push(character.0.clone());
            summaries.push(None);
        }
    }

    if !missing.is_empty() {
        match acquire_many(&missing, allow_insecure_tls) {
            Err(error) => {
                for (offset, character) in missing.iter().enumerate() {
                    blockers.push(format!("acquisition_failed:{character}"));
                    summaries[missing_indices[offset]] = Some(failed_summary(
                        character,
                        "acquisition_failed",
                        &error,
                        None,
                        None,
                        Vec::new(),
                        None,
                    ));
                }
            }
            Ok(acquired) => {
                for (offset, character) in missing.iter().enumerate() {
                    let target_index = missing_indices[offset];
                    let old = current.iter().find(|asset| {
                        asset.identity == KanjiCharacter(character.clone()).identity()
                    });
                    let result = acquired.get(offset);
                    let Some(Ok(media)) = result else {
                        let message = match result {
                            Some(Err(message)) => message.as_str(),
                            None => "provider outcome count mismatch",
                            Some(Ok(_)) => unreachable!(),
                        };
                        blockers.push(format!("acquisition_failed:{character}"));
                        summaries[target_index] = Some(failed_summary(
                            character,
                            "acquisition_failed",
                            message,
                            old.map(|asset| asset.lifecycle),
                            None,
                            Vec::new(),
                            None,
                        ));
                        continue;
                    };
                    let metadata = acquired_metadata(character, media);
                    if let Err(message) = validate_selected_format(media.selection, &media.bytes) {
                        blockers.push(format!("media_format_mismatch:{character}"));
                        summaries[target_index] = Some(failed_summary(
                            character,
                            "media_format_mismatch",
                            &message,
                            old.map(|asset| asset.lifecycle),
                            Some(format!("{:x}", Sha256::digest(&media.bytes))),
                            Vec::new(),
                            Some(metadata),
                        ));
                        continue;
                    }
                    let source_name = match media.selection {
                        SelectionResult::PrimaryGif => "yarxi-primary.gif",
                        SelectionResult::LeftmostPngFallback => "yarxi-leftmost.png",
                        SelectionResult::RenderedFontSamplePng => "yarxi-font-sample.png",
                    };
                    let request = VerifiedIngestRequest {
                        identity: KanjiCharacter(character.clone()).identity(),
                        bytes: media.bytes.clone(),
                        provenance: crate::model::Provenance {
                            source_kind: media.evidence.provider.clone(),
                            source_name: source_name.into(),
                        },
                        domain_metadata: Some(metadata.clone()),
                        replace_expected_sha256: old.map(|asset| asset.sha256.clone()),
                    };
                    match store.ingest_verified(request, &validator) {
                        Ok(outcome) => {
                            if outcome.status != SemanticStatus::Verified {
                                blockers.push(format!("{}:{character}", outcome.status.as_str()));
                                summaries[target_index] = Some(AssetSummary {
                                    identity: KanjiCharacter(character.clone()).identity(),
                                    from_state: old.map(|asset| asset.lifecycle),
                                    to_state: None,
                                    sha256: Some(outcome.sha256),
                                    previous_sha256: old.map(|asset| asset.sha256.clone()),
                                    validation_status: Some(outcome.status),
                                    validator: Some(validator_id.clone()),
                                    evidence: outcome.evidence,
                                    domain_metadata: Some(metadata),
                                    changed: false,
                                    item_outcome: Some(outcome.status.as_str().to_owned()),
                                });
                            } else if let Some(record) = outcome.asset {
                                let mut summary = summary_for_current(record);
                                summary.changed = outcome.changed;
                                summary.previous_sha256 = old.map(|asset| asset.sha256.clone());
                                summary.item_outcome = Some(if outcome.changed {
                                    "verified_published".into()
                                } else {
                                    "already_verified".into()
                                });
                                summaries[target_index] = Some(summary);
                            }
                        }
                        Err(error) => {
                            blockers.push(format!("{}:{character}", error.code.as_str()));
                            let candidate_hash = format!("{:x}", Sha256::digest(&media.bytes));
                            summaries[target_index] = Some(failed_summary(
                                character,
                                error.code.as_str(),
                                &error.message,
                                old.map(|asset| asset.lifecycle),
                                Some(candidate_hash),
                                Vec::new(),
                                Some(metadata),
                            ));
                        }
                    }
                }
            }
        }
    }

    let assets: Vec<_> = summaries.into_iter().flatten().collect();
    let successful = blockers.is_empty()
        && assets.iter().all(|asset| {
            asset.validation_status == Some(SemanticStatus::Verified)
                && asset.to_state == Some(LifecycleState::Verified)
        });
    let changed = assets.iter().any(|asset| asset.changed) || store.initialized_on_open();
    let mut response = response(
        "ensure",
        store_summary,
        None,
        assets,
        changed,
        if successful {
            "verified"
        } else {
            "partial_failure"
        },
    );
    response.blockers = blockers;
    response.validator_available = Some(true);
    Ok((response, if successful { 0 } else { 3 }))
}

fn validate_selected_format(selection: SelectionResult, bytes: &[u8]) -> Result<(), String> {
    let actual = DetectedFormat::from_signature(bytes);
    let expected = match selection {
        SelectionResult::PrimaryGif => DetectedFormat::Gif,
        SelectionResult::LeftmostPngFallback | SelectionResult::RenderedFontSamplePng => {
            DetectedFormat::Png
        }
    };
    if actual != expected {
        return Err(format!(
            "selected {selection:?} requires {expected:?} magic bytes, received {actual:?}"
        ));
    }
    Ok(())
}

fn acquired_metadata(character: &str, media: &AcquiredMedia) -> serde_json::Value {
    let mut metadata = KanjiCharacter(character.to_owned()).metadata();
    metadata["yarxi"] = serde_json::to_value(&media.evidence).unwrap_or(serde_json::Value::Null);
    metadata
}

fn failed_summary(
    character: &str,
    outcome: &str,
    message: &str,
    from_state: Option<LifecycleState>,
    hash: Option<String>,
    evidence: Vec<ValidationEvidence>,
    domain_metadata: Option<serde_json::Value>,
) -> AssetSummary {
    AssetSummary {
        identity: KanjiCharacter(character.to_owned()).identity(),
        from_state,
        to_state: None,
        sha256: hash,
        previous_sha256: None,
        validation_status: None,
        validator: None,
        evidence: if message.is_empty() {
            evidence
        } else {
            let mut evidence = evidence;
            evidence.push(ValidationEvidence {
                kind: outcome.to_owned(),
                summary: message.chars().take(500).collect(),
                details: None,
            });
            evidence
        },
        domain_metadata,
        changed: false,
        item_outcome: Some(outcome.to_owned()),
    }
}

fn summary_for_current(record: AssetRecord) -> AssetSummary {
    AssetSummary {
        identity: record.identity,
        from_state: Some(record.lifecycle),
        to_state: Some(record.lifecycle),
        sha256: Some(record.sha256),
        previous_sha256: None,
        validation_status: record
            .validation
            .as_ref()
            .map(|validation| validation.status),
        validator: record
            .validation
            .as_ref()
            .map(|validation| validation.validator.clone()),
        evidence: record
            .validation
            .map(|validation| validation.evidence)
            .unwrap_or_default(),
        domain_metadata: record.domain_metadata,
        changed: false,
        item_outcome: None,
    }
}

fn response(
    operation: &str,
    store: StoreSummary,
    mode: Option<String>,
    assets: Vec<AssetSummary>,
    changed: bool,
    outcome: &str,
) -> Response {
    Response {
        schema_version: 1,
        operation: operation.to_owned(),
        store: Some(store),
        mode,
        assets,
        changed,
        conflicts: Vec::new(),
        blockers: Vec::new(),
        outcome: outcome.to_owned(),
        error: None,
        validator_available: None,
    }
}

fn render_error(
    operation: String,
    identity: Option<AssetIdentity>,
    error: AssetError,
    store: StoreSummary,
    output: OutputFormat,
    changed: bool,
) -> CliOutput {
    let code = error.code.as_str().to_owned();
    let is_conflict = error.code == ErrorCode::IdentityConflict;
    let is_blocker = error.code.exit_code() == 3 || error.code.exit_code() == 4;
    let mut response = Response {
        schema_version: 1,
        operation,
        store: Some(store),
        mode: None,
        assets: Vec::new(),
        changed,
        conflicts: Vec::new(),
        blockers: Vec::new(),
        outcome: if is_conflict {
            "conflict".to_owned()
        } else if is_blocker {
            "blocked".to_owned()
        } else {
            "error".to_owned()
        },
        error: Some(ErrorSummary {
            code: code.clone(),
            category: if is_conflict {
                "conflict"
            } else if is_blocker {
                "domain_blocker"
            } else {
                "io_failure"
            },
            message: error.message.clone(),
            details: error.details.clone(),
        }),
        validator_available: None,
    };
    if is_conflict {
        response.conflicts.push(ConflictSummary {
            code,
            identity: error
                .details
                .get("identity")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .or(identity),
            existing_sha256: error
                .details
                .get("existing_sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            candidate_sha256: error
                .details
                .get("candidate_sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    } else {
        response.blockers.push(code);
    }
    match output {
        OutputFormat::Json => CliOutput {
            stdout: format!(
                "{}\n",
                serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".into())
            ),
            stderr: String::new(),
            exit_code: error.exit_code(),
        },
        OutputFormat::Human => CliOutput {
            stdout: String::new(),
            stderr: format!("{}: {}\n", response.outcome, error.message),
            exit_code: error.exit_code(),
        },
    }
}

fn render_response(response: Response, output: OutputFormat, exit_code: u8) -> CliOutput {
    match output {
        OutputFormat::Json => CliOutput {
            stdout: format!(
                "{}\n",
                serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".into())
            ),
            stderr: String::new(),
            exit_code,
        },
        OutputFormat::Human => {
            let mut text = format!("{}: {}", response.operation, response.outcome);
            if let Some(mode) = &response.mode {
                text.push_str(&format!(" (mode={mode})"));
            }
            text.push('\n');
            for asset in &response.assets {
                text.push_str(&format!(
                    "{}  {}  {}  {}\n",
                    asset.identity,
                    asset.to_state.map_or("-", LifecycleState::as_str),
                    asset.sha256.as_deref().unwrap_or("-"),
                    asset
                        .validation_status
                        .map_or("no decision", |status| status.as_str())
                ));
            }
            for blocker in &response.blockers {
                text.push_str(&format!("blocker: {blocker}\n"));
            }
            CliOutput {
                stdout: text,
                stderr: String::new(),
                exit_code,
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sha2::{Digest, Sha256};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let count = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kanji-assets-source-boundary-{}-{count}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("synthetic test root is created");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn source_replacement_after_boundary_check_keeps_the_opened_object() {
        let temp = TempDir::new();
        let repository = temp.0.join("repository");
        let decks = repository.join("decks");
        fs::create_dir_all(&decks).expect("protected tree is created");
        let protected_source = decks.join("protected.bin");
        let protected_bytes = b"must not be read from decks";
        fs::write(&protected_source, protected_bytes).expect("protected fixture writes");

        let candidate = temp.0.join("candidate.bin");
        let candidate_bytes = b"opened candidate remains the source";
        fs::write(&candidate, candidate_bytes).expect("safe fixture writes");
        let protected_roots = BTreeSet::from([decks.clone()]);
        let opened = validate_explicit_source(&candidate, &protected_roots)
            .expect("source handle passes boundary check");

        fs::remove_file(&candidate).expect("original pathname removed");
        symlink(&protected_source, &candidate).expect("path now points into protected tree");

        let store_root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&store_root).protect_from(&decks))
            .expect("separate store opens");
        let outcome = store
            .ingest_from_file(
                IngestRequest {
                    identity: AssetIdentity::new("generic", "source-boundary").unwrap(),
                    source_path: candidate,
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                opened,
            )
            .expect("ingest consumes the checked open handle");

        assert_eq!(
            outcome.asset.sha256,
            format!("{:x}", Sha256::digest(candidate_bytes)),
            "the bytes belong to the descriptor checked before pathname replacement"
        );
        assert_ne!(
            outcome.asset.sha256,
            format!("{:x}", Sha256::digest(protected_bytes))
        );
    }

    #[test]
    fn misleading_source_url_or_selection_cannot_override_magic_bytes() {
        let png_magic = b"\x89PNG\r\n\x1a\nsynthetic";
        assert!(validate_selected_format(SelectionResult::PrimaryGif, png_magic).is_err());
        assert!(
            validate_selected_format(SelectionResult::LeftmostPngFallback, b"GIF89a synthetic")
                .is_err()
        );
        assert!(
            validate_selected_format(SelectionResult::RenderedFontSamplePng, b"GIF89a synthetic")
                .is_err()
        );
        assert!(validate_selected_format(SelectionResult::PrimaryGif, b"GIF89a synthetic").is_ok());
        assert!(
            validate_selected_format(SelectionResult::RenderedFontSamplePng, png_magic).is_ok()
        );
    }
}
