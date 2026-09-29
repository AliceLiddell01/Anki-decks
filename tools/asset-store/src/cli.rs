//! CLI adapter предметной kanji boundary поверх общего `asset_store` core.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::error::{AssetError, ErrorCode};
use crate::model::{AssetIdentity, AssetRecord, LifecycleState, SemanticStatus, ValidatorIdentity};
use crate::selection::SelectionMode;
use crate::store::{AssetStore, IngestRequest, StoreOptions};

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
    /// Показывает детерминированный набор целей; CV validator на этом этапе нет.
    Plan {
        #[arg(long, value_enum)]
        mode: ModeArg,
        #[arg(long)]
        validator_id: String,
        #[arg(long)]
        validator_version: String,
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
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(
                "character должен быть непустой Unicode-строкой без управляющих символов".into(),
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
    to_state: LifecycleState,
    sha256: String,
    previous_sha256: Option<String>,
    validation_status: Option<SemanticStatus>,
    validator: Option<ValidatorIdentity>,
    evidence: Vec<crate::model::ValidationEvidence>,
    domain_metadata: Option<serde_json::Value>,
    changed: bool,
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
                validate_explicit_source(file, &protected_roots)?;
                Ok(Some(character.identity()))
            }),
        Command::Plan {
            validator_id,
            validator_version,
            ..
        } => ValidatorIdentity::new(validator_id.clone(), validator_version.clone())
            .map(|_| None)
            .map_err(|message| AssetError::new(ErrorCode::InvalidValidatorIdentity, message)),
        _ => Ok(None),
    };
    let selected_identity = match prevalidated {
        Ok(identity) => identity,
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
    match AssetStore::open(options) {
        Ok(store) => {
            let store_summary = StoreSummary {
                path: store.root().display().to_string(),
                store_id: Some(store.store_id().to_owned()),
            };
            match execute_with_store(&store, store_summary.clone(), cli.command) {
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
) -> Result<(), AssetError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::SourceMissing,
                format!("explicit source file отсутствует: {}", path.display()),
            )
        } else {
            AssetError::io("не удалось проверить explicit source", error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "explicit source должен быть обычным файлом без symlink",
        ));
    }
    let lexical_source = normalize_absolute_path(path)?;
    let canonical_source = std::fs::canonicalize(path)
        .map_err(|error| AssetError::io("не удалось разрешить explicit source", error))?;
    for protected_root in protected_roots {
        let lexical_root = normalize_absolute_path(protected_root)?;
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
        if lexical_source.starts_with(&lexical_root)
            || canonical_root.is_some_and(|root| canonical_source.starts_with(root))
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "kanji CLI не читает source из защищённого дерева decks",
            ));
        }
    }
    std::fs::File::open(path).map(|_| ()).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::SourceMissing,
                format!("explicit source file отсутствует: {}", path.display()),
            )
        } else {
            AssetError::io("explicit source недоступен для чтения", error)
        }
    })
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
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    absolute.ancestors().find_map(|ancestor| {
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
        }
    }
}

fn execute_with_store(
    store: &AssetStore,
    store_summary: StoreSummary,
    command: Command,
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
            let outcome = store.ingest(IngestRequest {
                identity: character.identity(),
                source_path: file,
                domain_metadata: Some(character.metadata()),
                replace_expected_sha256,
            })?;
            let summary = AssetSummary {
                identity: outcome.asset.identity.clone(),
                from_state: outcome.previous.as_ref().map(|asset| asset.lifecycle),
                to_state: outcome.asset.lifecycle,
                sha256: outcome.asset.sha256.clone(),
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
            response
                .blockers
                .push("semantic_validator_unavailable".to_owned());
            response.validator_available = Some(false);
            Ok((response, 0))
        }
    }
}

fn summary_for_current(record: AssetRecord) -> AssetSummary {
    AssetSummary {
        identity: record.identity,
        from_state: Some(record.lifecycle),
        to_state: record.lifecycle,
        sha256: record.sha256,
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
                    asset.to_state,
                    asset.sha256,
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
