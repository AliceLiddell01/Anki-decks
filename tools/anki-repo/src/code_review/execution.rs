//! Явное исполнение проверок в отдельной рабочей копии точного Git-снимка.
//!
//! Изоляция ресурсов сотрудничающих исполнителей не ограничивает права кода:
//! произвольный код имеет права пользователя, доступ к сети и может покинуть
//! собственную процессную группу. Такие ограничения входят в результат.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::model::ReviewPack;
use super::scope::GitTarget;
use crate::error::{DomainError, ErrorCode};

pub const EXECUTION_SCHEMA_VERSION: u32 = 1;
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_ARGUMENT_BYTES: usize = 64 * 1024;
const SURFACES: &[&str] = &[
    "target",
    "tmp",
    "scratch",
    "outputs",
    "config",
    "cargo-home",
    "home",
    "logs",
    "hooks",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum ExecutionMode {
    IsolatedChecks,
    DisposableSourceExperiment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSource {
    pub snapshot: GitTarget,
    pub review_pack_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_variant: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PrepareOptions {
    pub mode: ExecutionMode,
    pub scope: String,
    /// Канонический исходный review.json; относительный путь считается от текущего каталога.
    pub source_pack: PathBuf,
    /// Необязательное утверждение номера PR; пространство имён наследуется из source_pack.
    pub pr_number: Option<String>,
}

/// Значения переменных не наследуются без явного выбора политики.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentPolicy {
    /// PATH и местоположение rustup; секреты и настройки Git/Cargo не наследуются.
    #[default]
    Minimal,
    /// Явно переданные значения; переменные собственных путей задания остаются обязательными.
    Explicit {
        #[serde(serialize_with = "serialize_redacted_environment")]
        values: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOptions {
    pub timeout_ms: Option<u64>,
    pub output_limit_bytes: usize,
    pub environment: EnvironmentPolicy,
    /// Общий предел активных заданий в данном репозитории, 1..=64.
    pub max_parallel_jobs: usize,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout_ms: Some(300_000),
            output_limit_bytes: 64 * 1024,
            environment: EnvironmentPolicy::Minimal,
            max_parallel_jobs: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequest {
    /// Уже разобранные argv; оболочка возможна только как явный argv[0].
    #[serde(serialize_with = "serialize_safe_argv")]
    pub argv: Vec<String>,
    /// Относительный путь от корня worktree задания; "." означает сам worktree.
    pub cwd: String,
    pub options: RunOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    /// Подготовка идёт и ещё не подтвердила рабочее дерево.
    ///
    /// Состояние записывается под удержанной блокировкой задания до создания
    /// runtime-каталогов и до `git worktree add`. Пока оно опубликовано,
    /// задание не считается готовым к запуску: `Prepared` появляется только
    /// последним шагом подготовки. Вариант добавлен аддитивно, поэтому
    /// `EXECUTION_SCHEMA_VERSION` не меняется и прежние артефакты читаются.
    Preparing,
    Prepared,
    Running,
    Completed,
    PreparationFailed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Passed,
    Failed,
    TimedOut,
    Cancelled,
    Unavailable,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputEvidence {
    /// Потери при замене некорректного UTF-8 фиксируются отдельно.
    pub text: String,
    pub total_bytes: u64,
    pub truncated: bool,
    pub utf8_lossy: bool,
    /// Относительно каталога задания; полное содержимое хранится только здесь.
    pub log: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enforcement {
    pub resource_isolation: String,
    pub security_sandbox: String,
    pub process_cleanup: String,
    pub limitations: Vec<String>,
}
impl Default for Enforcement {
    fn default() -> Self {
        Self {
            resource_isolation: "private_worktree_and_runtime_paths".into(),
            security_sandbox: "absent".into(),
            process_cleanup: "not_started".into(),
            limitations: vec![
                "Код исполняется с правами пользователя; файловая система и сеть не ограничены security sandbox.".into(),
                "Изоляция исходников обеспечена отдельным worktree с detached HEAD; записи вне задания не блокируются ОС.".into(),
                "Объекты и метаданные Git общие; проверяемому коду не запрещён прямой доступ к ним.".into(),
                "Потомки могут уйти из процессной группы; их завершение не гарантируется.".into(),
                "Полные логи хранятся на диске без лимита и без маскирования секретов; распознанные значения маскируются только в ограниченном выводе внутри JSON.".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResult {
    pub schema_version: u32,
    pub job_id: String,
    pub source: ExecutionSource,
    pub mode: ExecutionMode,
    pub scope: String,
    pub namespace: String,
    pub request: CommandRequest,
    /// Хеш безопасного представления argv; распознанные секреты в нём заменены.
    pub argv_sha256: String,
    pub lifecycle: LifecycleStatus,
    pub status: ExecutionStatus,
    pub exit: Option<ProcessExit>,
    pub stdout: OutputEvidence,
    pub stderr: OutputEvidence,
    pub duration_ms: u64,
    pub enforcement: Enforcement,
    pub failure: Option<String>,
    pub cleanup: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobMetadata {
    pub schema_version: u32,
    pub job_id: String,
    pub source: ExecutionSource,
    pub mode: ExecutionMode,
    pub scope: String,
    pub namespace: String,
    /// Случайный nonce подтверждает владение каталогом; он не является секретом.
    pub owner_nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobInspection {
    pub job: JobMetadata,
    pub lifecycle: LifecycleStatus,
    pub result: Option<ExecutionResult>,
    pub workspace_removed: bool,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupResult {
    pub job_id: String,
    pub workspace_removed: bool,
    pub evidence_retained: bool,
    pub limitation: Option<String>,
}

/// Объект нельзя создать с произвольным путём; `open_job` проверяет его идентичность.
#[derive(Debug, Clone)]
pub struct PreparedJob {
    root: PathBuf,
    directory: PathBuf,
    metadata: JobMetadata,
}
impl PreparedJob {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn worktree(&self) -> PathBuf {
        self.directory.join("worktree")
    }
    pub fn metadata(&self) -> &JobMetadata {
        &self.metadata
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    owner_nonce: String,
    lifecycle: LifecycleStatus,
    workspace_removed: bool,
}

/// Проверяет идентичность исходных свидетельств до любых операций с временными каталогами.
fn source_pack_location(
    root: &Path,
    pack: &ReviewPack,
    bytes: &[u8],
    options: &PrepareOptions,
) -> Result<(String, Option<String>), DomainError> {
    // Path::components нормализует внутренние "."; проверяем исходное написание тоже.
    if options
        .source_pack
        .as_os_str()
        .as_encoded_bytes()
        .split(|byte| *byte == b'/')
        .any(|component| component == b"." || component == b"..")
    {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Псевдонимы пути, . и .. в исходном review.json запрещены",
        ));
    }
    let source_path = absolute_path(&options.source_pack)?;
    let relative = source_path.strip_prefix(root).map_err(|_| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "Исходный review.json находится вне канонической рабочей области репозитория",
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    let (owner, review, namespace, head, variant, filename) = match components.as_slice() {
        [
            Component::Normal(owner),
            Component::Normal(review),
            Component::Normal(namespace),
            Component::Normal(head),
            Component::Normal(filename),
        ] => (owner, review, namespace, head, None, filename),
        [
            Component::Normal(owner),
            Component::Normal(review),
            Component::Normal(namespace),
            Component::Normal(head),
            Component::Normal(variant),
            Component::Normal(filename),
        ] => (owner, review, namespace, head, Some(variant), filename),
        _ => {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "Исходный пакет должен находиться в канонической рабочей области PR/local и полного HEAD SHA",
            ));
        }
    };
    if *owner != ".anki-repo" || *review != "review" || *filename != "review.json" {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "исходный пакет должен быть каноническим review.json",
        ));
    }
    if variant.is_some_and(|name| {
        name.to_str()
            .is_none_or(|name| !super::workflow::is_review_snapshot_variant(name))
    }) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Вариант workspace должен иметь вид `snapshot-<32 hex>`",
        ));
    }
    let namespace = namespace.to_str().ok_or_else(|| {
        DomainError::new(ErrorCode::InvalidRequest, "Пространство имён не в UTF-8")
    })?;
    if namespace != "local"
        && namespace.parse::<u64>().map_or(true, |number| {
            number == 0 || number.to_string() != namespace
        })
    {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Пространство имён должно быть `local` или каноническим положительным номером PR",
        ));
    }
    if let Some(number) = &options.pr_number {
        if number
            .parse::<u64>()
            .map_or(true, |value| value == 0 || value.to_string() != *number)
        {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "Параметр pr_number должен быть каноническим положительным номером PR",
            ));
        }
        if number != namespace {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "Номер PR не совпадает с пространством имён исходного review.json",
            ));
        }
    }
    if head.to_str() != Some(pack.target.head_sha.as_str()) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "HEAD каталога исходного review.json не совпадает с пакетом ревью",
        ));
    }
    let mut canonical = root
        .join(".anki-repo/review")
        .join(namespace)
        .join(&pack.target.head_sha);
    if let Some(variant) = variant {
        canonical.push(variant);
    }
    canonical.push("review.json");
    if source_path.as_os_str() != canonical.as_os_str() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "alias исходного review.json вместо канонического пути запрещён",
        ));
    }
    let directory = safe_dir(canonical.parent().expect("review.json имеет каталог"))?;
    let stored = read_optional_file_at(
        &directory,
        "review.json",
        super::workflow::MAX_REVIEW_ARTIFACT_BYTES,
    )?
    .ok_or_else(|| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "канонический исходный review.json отсутствует",
        )
    })?;
    if stored != bytes {
        return Err(invalid(
            "байты исходного review.json не совпадают с каноническим evidence",
        ));
    }
    let workspace_variant = variant.map(|name| {
        name.to_str()
            .expect("validated snapshot variant is UTF-8")
            .to_owned()
    });
    Ok((namespace.to_owned(), workspace_variant))
}

fn job_runs_directory(root: &Path, namespace: &str, source: &ExecutionSource) -> PathBuf {
    let mut workspace = root
        .join(".anki-repo/review")
        .join(namespace)
        .join(&source.snapshot.head_sha);
    if let Some(variant) = &source.workspace_variant {
        workspace.push(variant);
    }
    workspace.join("runs")
}

/// Создаёт уникальное задание и точный worktree с detached HEAD; код проекта не запускается.
pub fn prepare_job(
    root: &Path,
    pack: &ReviewPack,
    bytes: &[u8],
    options: PrepareOptions,
) -> Result<PreparedJob, DomainError> {
    platform_supported()?;
    super::delta::validate_review_pack(pack)?;
    let parsed: ReviewPack = serde_json::from_slice(bytes)
        .map_err(|e| invalid(format!("исходный review.json не разобран: {e}")))?;
    if parsed != *pack {
        return Err(invalid(
            "байты review.json не соответствуют переданному пакету",
        ));
    }
    if options.scope.trim().is_empty() || options.scope.len() > 128 || options.scope.contains('\0')
    {
        return Err(invalid("Область задания не может быть пустой"));
    }
    for sha in [
        &pack.target.head_sha,
        &pack.target.base_sha,
        &pack.target.merge_base_sha,
    ] {
        validate_sha(sha)?;
    }
    let root = root.canonicalize().map_err(read_error)?;
    safe_dir(&root)?;
    let (namespace, workspace_variant) = source_pack_location(&root, pack, bytes, &options)?;
    verify_snapshot(&root, &pack.target)?;
    let source = ExecutionSource {
        snapshot: pack.target.clone(),
        review_pack_sha256: format!("{:x}", Sha256::digest(bytes)),
        workspace_variant,
    };
    let parent = job_runs_directory(&root, &namespace, &source);
    super::workflow::reject_tracked_review_workspace(
        &root,
        Path::new(".anki-repo")
            .join("review")
            .join(&namespace)
            .join(&pack.target.head_sha)
            .as_path(),
    )?;
    ensure_dir(&parent)?;
    let owner_nonce = random_id()?;
    let (directory, job_id) = unique_directory(&parent)?;
    let metadata = JobMetadata {
        schema_version: EXECUTION_SCHEMA_VERSION,
        job_id,
        source,
        mode: options.mode,
        scope: options.scope,
        namespace,
        owner_nonce,
    };
    let job = PreparedJob {
        root,
        directory,
        metadata,
    };
    let initialized: Result<File, DomainError> = (|| {
        // Блокировка берётся сразу после создания каталога и до записи любых его
        // файлов: иначе конкурентная операция успела бы «увести» блокировку у
        // только что созданного задания, а `inspect` увидел бы каталог без
        // `job.json`. Дальше блокировка удерживается на всём критическом участке
        // подготовки: пока не опубликовано `Prepared`, никто не может ни
        // запустить задание, ни удалить его ресурсы (конкурентный `cleanup`
        // получает `ExecutionBusy`).
        let lock = job_lock(&job.directory)?;
        write_document(&job.directory, "job.json", &job.metadata, false)?;
        write_new(&job.directory, "source-review.json", bytes)?;
        // `Preparing` публикуется до создания runtime-каталогов и до
        // `git worktree add`: даже падение на этом шаге не оставит ложное `Prepared`.
        save_state(&job, LifecycleStatus::Preparing, false)?;
        Ok(lock)
    })();
    let active_lock = match initialized {
        Ok(lock) => lock,
        Err(error) => {
            if let Err(cleanup_error) = remove_partial_job_directory(
                &job_runs_directory(&job.root, &job.metadata.namespace, &job.metadata.source),
                &job.metadata.job_id,
            ) {
                return Err(DomainError::with_details(
                    error.code,
                    format!(
                        "{}; не удалось удалить частично созданное задание: {}",
                        error.message, cleanup_error.message
                    ),
                    crate::details! { "job_dir" => job.directory.display().to_string(), "job_id" => job.metadata.job_id },
                ));
            }
            return Err(error);
        }
    };
    let preparation: Result<(), DomainError> = (|| {
        for surface in SURFACES {
            ensure_dir(&job.directory.join(surface))?;
        }
        let mut command = trusted_git(&job.root, &job.directory.join("hooks"))?;
        command.args(["worktree", "add", "--detach", "--no-checkout"]);
        command
            .arg(job.worktree())
            .arg(&job.metadata.source.snapshot.head_sha);
        git_success_redacting(
            command,
            "создание рабочего дерева с detached HEAD",
            &[&job.directory, &job.worktree()],
        )?;
        let mut command = trusted_git(&job.worktree(), &job.directory.join("hooks"))?;
        command.args([
            "checkout",
            "--detach",
            &job.metadata.source.snapshot.head_sha,
        ]);
        git_success_redacting(
            command,
            "извлечение точного source",
            &[&job.directory, &job.worktree()],
        )?;
        verify_prepared_worktree(&job)
    })();
    if let Err(error) = preparation {
        let state_error = save_state(&job, LifecycleStatus::PreparationFailed, false).err();
        let unlock_error = fs2::FileExt::unlock(&active_lock).err().map(process_error);
        drop(active_lock);
        let mut message = error.message;
        if let Some(state_error) = &state_error {
            message.push_str(&format!(
                "; не удалось сохранить состояние PreparationFailed: {}",
                state_error.message
            ));
        }
        if let Some(unlock_error) = &unlock_error {
            message.push_str(&format!(
                "; не удалось освободить блокировку: {}",
                unlock_error.message
            ));
        }
        return Err(DomainError::with_details(
            error.code,
            message,
            crate::details! {
                "job_dir" => job.directory.display().to_string(),
                "job_id" => job.metadata.job_id,
                "state_save_error" => state_error.map_or_else(String::new, |error| error.message),
                "unlock_error" => unlock_error.map_or_else(String::new, |error| error.message),
            },
        ));
    }
    // Последний шаг подготовки: `Prepared` публикуется только после полного
    // создания и проверки рабочего дерева и всё ещё под той же блокировкой.
    let published = save_state(&job, LifecycleStatus::Prepared, false);
    // Освобождение явное, а не закрытием дескриптора: копия дескриптора,
    // унаследованная параллельным fork, иначе удерживала бы flock до своего exec.
    let unlocked = fs2::FileExt::unlock(&active_lock).map_err(process_error);
    drop(active_lock);
    published?;
    unlocked?;
    Ok(job)
}

/// Подтверждает, что созданное рабочее дерево стоит на закреплённом HEAD.
fn verify_prepared_worktree(job: &PreparedJob) -> Result<(), DomainError> {
    let mut command = trusted_git(&job.worktree(), &job.directory.join("hooks"))?;
    command.args(["rev-parse", "HEAD"]);
    let output = command.output().map_err(process_error)?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            format!(
                "Git не смог подтвердить HEAD подготовленного рабочего дерева: {}",
                stable_message(
                    &String::from_utf8_lossy(&output.stderr),
                    &[&job.directory, &job.worktree()],
                )
            ),
        ));
    }
    if String::from_utf8_lossy(&output.stdout).trim() != job.metadata.source.snapshot.head_sha {
        return Err(invalid(
            "Подготовленное рабочее дерево не соответствует закреплённому HEAD",
        ));
    }
    Ok(())
}

/// Открытие существующего задания не исполняет код и не возобновляет незавершённый запуск.
pub fn open_job(root: &Path, directory: &Path) -> Result<PreparedJob, DomainError> {
    platform_supported()?;
    let root = root.canonicalize().map_err(read_error)?;
    let directory = absolute_path(directory)?;
    if fs::symlink_metadata(&directory)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(DomainError::new(
            ErrorCode::NotFound,
            "Каталог задания исполнения не найден",
        ));
    }
    safe_dir(&directory)?;
    let metadata: JobMetadata = read_document(&directory, "job.json")?;
    valid_component(&metadata.namespace)?;
    valid_component(&metadata.job_id)?;
    if metadata.namespace != "local"
        && (!metadata.namespace.bytes().all(|byte| byte.is_ascii_digit())
            || metadata.namespace.parse::<u64>().map_or(true, |number| {
                number == 0 || number.to_string() != metadata.namespace
            }))
    {
        return Err(invalid(
            "Пространство имён задания должно быть `local` или положительным номером PR",
        ));
    }
    validate_hex(&metadata.job_id, 32, "идентификатор задания")?;
    validate_hex(&metadata.owner_nonce, 32, "идентификатор владельца")?;
    validate_sha(&metadata.source.snapshot.head_sha)?;
    validate_sha(&metadata.source.snapshot.base_sha)?;
    validate_sha(&metadata.source.snapshot.merge_base_sha)?;
    if metadata
        .source
        .workspace_variant
        .as_deref()
        .is_some_and(|variant| !super::workflow::is_review_snapshot_variant(variant))
    {
        return Err(invalid("Вариант workspace задания имеет неверный формат"));
    }
    let expected =
        job_runs_directory(&root, &metadata.namespace, &metadata.source).join(&metadata.job_id);
    validate_hex(
        &metadata.source.review_pack_sha256,
        64,
        "digest review pack",
    )?;
    if metadata.schema_version != EXECUTION_SCHEMA_VERSION {
        return Err(invalid("Версия манифеста задания не поддерживается"));
    }
    if directory != expected {
        return Err(invalid("Путь задания не соответствует манифесту"));
    }
    if metadata.owner_nonce.len() != 32 {
        return Err(invalid("Идентификатор владельца имеет неверную длину"));
    }
    let job = PreparedJob {
        root,
        directory,
        metadata,
    };
    verify_snapshot(&job.root, &job.metadata.source.snapshot)?;
    verify_owner(&job)?;
    Ok(job)
}

/// Исполняет один явно переданный argv; повторный запуск задания запрещён.
pub fn run_job(
    job: &PreparedJob,
    request: &CommandRequest,
    cancel: &AtomicBool,
) -> Result<ExecutionResult, DomainError> {
    platform_supported()?;
    validate_request(request)?;
    let active_lock = job_lock(&job.directory)?;
    let state = verify_owner(job)?;
    if state.lifecycle != LifecycleStatus::Prepared || state.workspace_removed {
        return Err(conflict(if state.lifecycle == LifecycleStatus::Preparing {
            "Подготовка задания не завершена: рабочее дерево не подтверждено, запуск запрещён"
        } else {
            "Задание уже исполнялось, не готово или его рабочая область удалена"
        }));
    }
    let _slot = execution_slot(&job.root, request.options.max_parallel_jobs)?;
    let cwd = source_cwd(job, &request.cwd)?;
    verify_worktree_head(job)?;
    if job.metadata.mode == ExecutionMode::IsolatedChecks && source_changed(job)? {
        return Err(conflict(
            "Режим `isolated_checks` требует неизменённых закреплённых исходников; создайте новое задание или выберите `disposable_source_experiment`.",
        ));
    }
    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .current_dir(&cwd)
        .stdin(Stdio::null());
    configure_environment(&mut command, job, &request.options.environment)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let cancelled = cancel.load(Ordering::SeqCst) || cancel_requested(job)?;
    let logs = safe_dir(&job.directory.join("logs"))?;
    let stdout = open_file(&logs, "stdout.log", true)?;
    let stderr = match open_file(&logs, "stderr.log", true) {
        Ok(stderr) => stderr,
        Err(error) => {
            drop(stdout);
            if let Err(cleanup_error) =
                rustix::fs::unlinkat(&logs, "stdout.log", rustix::fs::AtFlags::empty())
            {
                return Err(DomainError::new(
                    error.code,
                    format!(
                        "{}; не удалось удалить частичный `stdout.log`: {cleanup_error}",
                        error.message
                    ),
                ));
            }
            return Err(error);
        }
    };
    command.stdout(stdout).stderr(stderr);
    // До этой записи ошибки доказуемо предшествуют запуску процесса.
    save_state(job, LifecycleStatus::Running, false)?;
    let start = Instant::now();
    let mut enforcement = Enforcement::default();
    let mut failure = None;
    let mut exit = None;
    let status = if cancelled {
        ExecutionStatus::Cancelled
    } else {
        match command.spawn() {
            Err(error) => {
                failure = Some(format!("команда недоступна: {error}"));
                ExecutionStatus::Unavailable
            }
            Ok(mut child) => {
                let mut status = ExecutionStatus::Incomplete;
                // PID остаётся живым до остановки группы: это защищает от повторного
                // назначения идентификатора, пока выполняется остановка собственной группы.
                loop {
                    if cancel.load(Ordering::SeqCst) || cancel_requested(job).unwrap_or(true) {
                        status = ExecutionStatus::Cancelled;
                        stop_child(&mut child, &mut enforcement, &mut failure);
                        break;
                    }
                    if request
                        .options
                        .timeout_ms
                        .is_some_and(|ms| start.elapsed() >= Duration::from_millis(ms))
                    {
                        status = ExecutionStatus::TimedOut;
                        stop_child(&mut child, &mut enforcement, &mut failure);
                        break;
                    }
                    match observe_child(&mut child) {
                        Ok(Some(observed)) => {
                            status = if observed.code == Some(0) {
                                ExecutionStatus::Passed
                            } else {
                                ExecutionStatus::Failed
                            };
                            #[cfg(target_os = "linux")]
                            {
                                // waitid(NOWAIT) сохраняет ведущий процесс до получения его статуса: PID
                                // нельзя повторно назначить до остановки собственной группы.
                                stop_child(&mut child, &mut enforcement, &mut failure);
                                match child.wait() {
                                    Ok(waited) => exit = Some(exit_summary(waited)),
                                    Err(error) => {
                                        failure = Some(format!(
                                            "не удалось получить статус завершения процесса: {error}"
                                        ));
                                        status = ExecutionStatus::Incomplete;
                                    }
                                }
                                if enforcement.process_cleanup == "process_group_kill_failed" {
                                    status = ExecutionStatus::Incomplete;
                                }
                            }
                            #[cfg(not(target_os = "linux"))]
                            {
                                exit = Some(observed);
                                enforcement.process_cleanup =
                                    "direct_child_reaped_descendants_unverified".into();
                            }
                            break;
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(10)),
                        Err(error) => {
                            failure = Some(format!("не удалось наблюдать процесс: {error}"));
                            stop_child(&mut child, &mut enforcement, &mut failure);
                            break;
                        }
                    }
                }
                if exit.is_none() {
                    match child.wait() {
                        Ok(observed) => exit = Some(exit_summary(observed)),
                        Err(error) => {
                            failure =
                                Some(format!("не удалось дождаться завершения процесса: {error}"));
                            status = ExecutionStatus::Incomplete;
                        }
                    }
                }
                status
            }
        }
    };
    let status = if job.metadata.mode == ExecutionMode::IsolatedChecks
        && source_changed(job).unwrap_or(true)
    {
        failure = Some("Проверка изменила исходники собственного рабочего дерева; изолированную проверку нельзя считать воспроизводимо завершённой.".into());
        if matches!(status, ExecutionStatus::Passed | ExecutionStatus::Failed) {
            ExecutionStatus::Incomplete
        } else {
            status
        }
    } else {
        status
    };
    let sensitive_values = sensitive_request_values(request);
    let result = ExecutionResult {
        schema_version: EXECUTION_SCHEMA_VERSION,
        job_id: job.metadata.job_id.clone(),
        source: job.metadata.source.clone(),
        mode: job.metadata.mode,
        scope: job.metadata.scope.clone(),
        namespace: job.metadata.namespace.clone(),
        request: request.clone(),
        argv_sha256: argv_digest(&request.argv),
        lifecycle: LifecycleStatus::Completed,
        status,
        exit,
        stdout: output_evidence(
            job,
            "stdout.log",
            request.options.output_limit_bytes,
            &sensitive_values,
        )?,
        stderr: output_evidence(
            job,
            "stderr.log",
            request.options.output_limit_bytes,
            &sensitive_values,
        )?,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        failure,
        cleanup: if enforcement.process_cleanup == "not_started" {
            "evidence_retained; workspace_cleanup_allowed"
        } else {
            "evidence_and_workspace_retained; descendant_confirmation_required"
        }
        .into(),
        enforcement,
    };
    write_document(&job.directory, "result.json", &result, false)?;
    save_state(job, LifecycleStatus::Completed, false)?;
    fs2::FileExt::unlock(&active_lock).map_err(process_error)?;
    Ok(result)
}

/// Маркер отмены создаётся атомарно, независимо от блокировки активного задания.
pub fn request_cancel(directory: &Path) -> Result<JobInspection, DomainError> {
    let inspection = inspect_job(directory)?;
    // `Preparing` тоже принимает маркер: иначе отмена идущей подготовки молча
    // ничего не делала бы, а завершённое задание сразу увидело бы её при запуске.
    if inspection.lifecycle == LifecycleStatus::Preparing
        || inspection.lifecycle == LifecycleStatus::Prepared
        || inspection.lifecycle == LifecycleStatus::Running
    {
        let dir = safe_dir(&absolute_path(directory)?)?;
        match write_new_fd(&dir, "cancel.json", inspection.job.owner_nonce.as_bytes()) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(write_error(error)),
        }
    }
    inspect_job(directory)
}

/// Наблюдение без записи: Running без активной блокировки означает Interrupted.
pub fn inspect_job(directory: &Path) -> Result<JobInspection, DomainError> {
    let directory = absolute_path(directory)?;
    if fs::symlink_metadata(&directory)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(DomainError::new(
            ErrorCode::NotFound,
            "Каталог задания исполнения не найден",
        ));
    }
    let root = repository_root_for_job(&directory)?;
    let verified = open_job(&root, &directory)?;
    let metadata = verified.metadata;
    let state: State = read_document(&directory, "state.json")?;
    if metadata.owner_nonce != state.owner_nonce {
        return Err(invalid("Манифест задания и его состояние не совпадают"));
    }
    let mut lifecycle = state.lifecycle;
    let mut limitations = Vec::new();
    if lifecycle == LifecycleStatus::Running && !lock_is_active(&directory)? {
        lifecycle = LifecycleStatus::Interrupted;
        limitations.push(if state.workspace_removed {
            "Исполнитель не удерживает lock; исполнение прервано. Ресурсы удалены по подтверждению оператора, отсутствие потомков инструментом не доказано."
        } else {
            "Исполнитель не удерживает lock; потомки и полнота evidence не проверены, автоматическая очистка запрещена."
        }.into());
    }
    if lifecycle == LifecycleStatus::Preparing {
        // `Preparing` никогда не выдаётся за готовое задание: различаем идущую,
        // брошенную и уже очищенную подготовку по той же блокировке, которой
        // пользуется cleanup.
        limitations.push(
            match (lock_is_active(&directory)?, state.workspace_removed) {
                (true, _) => "Подготовка задания выполняется другим процессом: рабочее дерево и runtime-каталоги ещё не подтверждены, запуск и очистка недоступны до её завершения.",
                (false, true) => "Подготовка задания не завершена, ресурсы частичного задания удалены очисткой: код не запускался, рабочее дерево не подтверждено, запуск запрещён.",
                (false, false) => "Подготовка задания не завершена и её никто не удерживает: исполнитель прерван до публикации готового состояния. Рабочее дерево не подтверждено, запуск запрещён; очистка допустима, потому что код этого задания не запускался.",
            }
            .into(),
        );
    }
    // Отсутствие результата определяет один `openat`: повторная проверка пути после
    // ошибки чтения гонится с исполнителем, публикующим `result.json` в этот момент.
    let result =
        match read_optional_file_at(&safe_dir(&directory)?, "result.json", MAX_DOCUMENT_BYTES)? {
            Some(bytes) => {
                let result: ExecutionResult = serde_json::from_slice(&bytes)
                    .map_err(|e| invalid(format!("невалидный result.json: {e}")))?;
                validate_result(&metadata, &result)?;
                Some(result)
            }
            None => None,
        };
    if lifecycle == LifecycleStatus::Completed && result.is_none() {
        return Err(invalid(
            "Завершённое задание не имеет типизированного результата",
        ));
    }
    Ok(JobInspection {
        job: metadata,
        lifecycle,
        result,
        workspace_removed: state.workspace_removed,
        limitations,
    })
}

fn repository_root_for_job(directory: &Path) -> Result<PathBuf, DomainError> {
    let runs = directory
        .parent()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("runs"))
        .ok_or_else(|| invalid("Задание находится вне каталога runs"))?;
    let workspace = runs
        .parent()
        .ok_or_else(|| invalid("У каталога runs нет workspace"))?;
    let head_directory = if workspace
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(super::workflow::is_review_snapshot_variant)
    {
        workspace
            .parent()
            .ok_or_else(|| invalid("У варианта workspace нет каталога HEAD"))?
    } else {
        workspace
    };
    let head = head_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("В каталоге задания отсутствует HEAD SHA"))?;
    validate_sha(head)?;
    let namespace = head_directory
        .parent()
        .ok_or_else(|| invalid("У каталога HEAD нет пространства имён"))?;
    let review = namespace
        .parent()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("review"))
        .ok_or_else(|| invalid("Задание находится вне каталога review"))?;
    let anki_repo = review
        .parent()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some(".anki-repo"))
        .ok_or_else(|| invalid("Задание находится вне каталога .anki-repo"))?;
    anki_repo
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| invalid("Задание находится вне корня репозитория"))
}

/// Читает только завершённый результат; не исполняет повторно и не меняет артефакты.
pub fn read_result(directory: &Path) -> Result<ExecutionResult, DomainError> {
    let inspection = inspect_job(directory)?;
    if inspection.lifecycle != LifecycleStatus::Completed {
        return Err(conflict(
            "Задание не завершено; доступна только команда `inspect`",
        ));
    }
    inspection
        .result
        .ok_or_else(|| invalid("отсутствует result.json"))
}

/// Подтверждение оператора не доказывает технически отсутствие потомков.
#[derive(Debug, Clone, Copy, Default)]
pub struct CleanupOptions {
    pub confirm_no_live_descendants: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupAttestation {
    schema_version: u32,
    owner_nonce: String,
    limitation: String,
}

const CLEANUP_ATTESTATION: &str = "cleanup-attestation.json";
const OPERATOR_CLEANUP_LIMITATION: &str = "Ресурсы удалены по явному подтверждению оператора об отсутствии живых потомков. Инструмент не доказывает отсутствие процессов, покинувших группу; оператор отвечает за их предварительную остановку.";

/// По умолчанию ресурсы после возможного запуска процесса сохраняются.
pub fn cleanup_workspace(job: &PreparedJob) -> Result<CleanupResult, DomainError> {
    cleanup_workspace_with_options(job, &CleanupOptions::default())
}

/// Сохраняет результаты и свидетельства при удалении ресурсов задания.
pub fn cleanup_workspace_with_options(
    job: &PreparedJob,
    options: &CleanupOptions,
) -> Result<CleanupResult, DomainError> {
    let directory = safe_dir(&job.directory)?;
    let _lock = job_lock_at(&directory)?;
    let state = verify_owner_at(job, &directory)?;
    let not_started = match state.lifecycle {
        // Блокировка задания удержана этим вызовом, поэтому `Preparing` здесь
        // доказуемо означает брошенную подготовку: код этого задания не запускался.
        // Идущую подготовку сюда не пропускает `job_lock_at` (ExecutionBusy).
        LifecycleStatus::Preparing
        | LifecycleStatus::Prepared
        | LifecycleStatus::PreparationFailed => true,
        LifecycleStatus::Completed => {
            let result: ExecutionResult = read_document_at(&directory, "result.json")?;
            validate_result(&job.metadata, &result)?;
            result.exit.is_none() && result.enforcement.process_cleanup == "not_started"
        }
        // Блокировка прежнего исполнителя отсутствует, но потомки могли сохраниться.
        LifecycleStatus::Running | LifecycleStatus::Interrupted => false,
    };
    if state.workspace_removed {
        let limitation = read_cleanup_attestation(job, &directory)?;
        if !not_started && limitation.is_none() {
            return Err(conflict(
                "После операторской очистки отсутствует принадлежащее заданию подтверждение",
            ));
        }
        // Повторная очистка обязана убрать собственную застарелую запись Git:
        // иначе она рапортует успех, оставляя запись навсегда.
        if let Err(error) = remove_stale_own_worktree_records(job, &cleanup_hooks(&directory)?) {
            return Ok(CleanupResult {
                job_id: job.metadata.job_id.clone(),
                workspace_removed: false,
                evidence_retained: true,
                limitation: Some(match limitation {
                    Some(limit) => format!(
                        "{limit} Застарелая запись рабочего дерева не удалена: {}",
                        error.message
                    ),
                    None => format!(
                        "Застарелая запись рабочего дерева не удалена: {}",
                        error.message
                    ),
                }),
            });
        }
        return Ok(CleanupResult {
            job_id: job.metadata.job_id.clone(),
            workspace_removed: true,
            evidence_retained: true,
            limitation,
        });
    }
    if !not_started && !options.confirm_no_live_descendants {
        if matches!(
            state.lifecycle,
            LifecycleStatus::Running | LifecycleStatus::Interrupted
        ) {
            return Err(conflict(
                "Очистка прерванного задания запрещена без проверки потомков. Остановите всех его потомков, затем явно передайте --confirm-no-live-descendants; это подтверждение оператора, а не гарантия инструмента.",
            ));
        }
        return Ok(CleanupResult {
            job_id: job.metadata.job_id.clone(),
            workspace_removed: false,
            evidence_retained: true,
            limitation: Some("Рабочая область сохранена: отсутствие потомков, покинувших группу, не доказано. Остановите всех потомков и явно передайте cleanup --confirm-no-live-descendants; подтверждение оператора не является технической гарантией.".into()),
        });
    }
    // Проверяем все поверхности до первой записи или удаления.
    let surfaces = cleanup_preflight(job, &directory)?;
    let worktree_exists = surfaces.iter().any(|(name, _)| name == "worktree");
    let owned_path = cleanup_descriptor_path(&directory)?;
    // Проверка HEAD относится только к завершённой подготовке: у `Preparing`
    // рабочее дерево могло быть создано частично, а запуск кода доказанно
    // невозможен (состояние и удержанная блокировка). Удаление идёт по
    // закреплённому дескриптору поверхности, а не по внешнему пути.
    if worktree_exists && state.lifecycle == LifecycleStatus::Prepared {
        let mut command = Command::new("git");
        command
            .current_dir(owned_path.join("worktree"))
            .args(["rev-parse", "HEAD"]);
        let output = command.output().map_err(process_error)?;
        if !output.status.success() {
            return Err(DomainError::new(
                ErrorCode::ProcessOperationFailed,
                "Git не смог проверить HEAD перед очисткой",
            ));
        }
        if String::from_utf8_lossy(&output.stdout).trim() != job.metadata.source.snapshot.head_sha {
            return Err(conflict(
                "HEAD собственного рабочего дерева изменён до запуска",
            ));
        }
    }
    let limitation = (!not_started).then(|| OPERATOR_CLEANUP_LIMITATION.to_owned());
    if !not_started && read_cleanup_attestation(job, &directory)?.is_none() {
        write_document_at(
            &directory,
            CLEANUP_ATTESTATION,
            &CleanupAttestation {
                schema_version: EXECUTION_SCHEMA_VERSION,
                owner_nonce: job.metadata.owner_nonce.clone(),
                limitation: OPERATOR_CLEANUP_LIMITATION.into(),
            },
            false,
        )?;
    }
    if worktree_exists && let Err(error) = remove_cleanup_worktree(job, &directory, &surfaces) {
        return Ok(CleanupResult {
            job_id: job.metadata.job_id.clone(),
            workspace_removed: false,
            evidence_retained: true,
            limitation: Some(match limitation {
                Some(limit) => {
                    format!(
                        "{limit} Удаление рабочего дерева не завершено: {}",
                        error.message
                    )
                }
                None => error.message,
            }),
        });
    }
    // Каталога рабочего дерева может уже не быть: прежняя очистка успела удалить
    // каталог и была прервана до удаления административной записи, либо запись
    // не удалилась с первого раза. Свою запись убираем и в этом состоянии, чтобы
    // очистка не отчитывалась об успехе, оставляя prunable-запись навсегда.
    if !worktree_exists
        && let Err(error) = remove_stale_own_worktree_records(job, &cleanup_hooks(&directory)?)
    {
        return Ok(CleanupResult {
            job_id: job.metadata.job_id.clone(),
            workspace_removed: false,
            evidence_retained: true,
            limitation: Some(match limitation {
                Some(limit) => format!(
                    "{limit} Застарелая запись рабочего дерева не удалена: {}",
                    error.message
                ),
                None => format!(
                    "Застарелая запись рабочего дерева не удалена: {}",
                    error.message
                ),
            }),
        });
    }
    for (name, _) in &surfaces {
        if matches!(name.as_str(), "worktree" | "logs" | "outputs") {
            continue;
        }
        remove_cleanup_runtime(&directory, name, &surfaces)?;
    }
    write_document_at(
        &directory,
        "state.json",
        &State {
            owner_nonce: job.metadata.owner_nonce.clone(),
            lifecycle: state.lifecycle,
            workspace_removed: true,
        },
        true,
    )?;
    Ok(CleanupResult {
        job_id: job.metadata.job_id.clone(),
        workspace_removed: true,
        evidence_retained: true,
        limitation,
    })
}

fn read_cleanup_attestation(
    job: &PreparedJob,
    directory: &File,
) -> Result<Option<String>, DomainError> {
    let Some(bytes) = read_optional_file_at(directory, CLEANUP_ATTESTATION, MAX_DOCUMENT_BYTES)?
    else {
        return Ok(None);
    };
    let attestation: CleanupAttestation = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("Повреждено подтверждение очистки: {error}")))?;
    if attestation.schema_version != EXECUTION_SCHEMA_VERSION
        || attestation.owner_nonce != job.metadata.owner_nonce
        || attestation.limitation != OPERATOR_CLEANUP_LIMITATION
    {
        return Err(conflict(
            "Подтверждение очистки не принадлежит этому заданию или изменено",
        ));
    }
    Ok(Some(attestation.limitation))
}

fn cleanup_preflight(
    job: &PreparedJob,
    directory: &File,
) -> Result<Vec<(String, File)>, DomainError> {
    let owned_path = cleanup_descriptor_path(directory)?;
    for entry in fs::read_dir(&owned_path).map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(conflict(
                "Каталог задания содержит неизвестное имя; ресурсы сохранены",
            ));
        };
        if SURFACES.contains(&name) || name == "worktree" {
            continue;
        }
        if name.starts_with(".publish-") || name.starts_with(".review-publish-") {
            let file_type = entry.file_type().map_err(read_error)?;
            if file_type.is_file() && !file_type.is_symlink() {
                drop(open_file(directory, name, false)?);
                continue;
            }
        }
        if !matches!(
            name,
            "job.json"
                | "state.json"
                | "source-review.json"
                | "result.json"
                | "active.lock"
                | "cancel.json"
                | CLEANUP_ATTESTATION
        ) || !entry.file_type().map_err(read_error)?.is_file()
        {
            return Err(conflict(
                "Каталог задания содержит неизвестный файл или символическую ссылку; ресурсы сохранены",
            ));
        }
        drop(open_file(directory, name, false)?);
    }
    read_cleanup_attestation(job, directory)?;
    let mut surfaces = Vec::new();
    for name in std::iter::once("worktree").chain(SURFACES.iter().copied()) {
        if let Some(surface) = open_cleanup_surface(directory, name)? {
            surfaces.push((name.to_owned(), surface));
        }
    }
    Ok(surfaces)
}

fn verify_cleanup_surface_identity(
    directory: &File,
    name: &str,
    surfaces: &[(String, File)],
) -> Result<(), DomainError> {
    let original = surfaces
        .iter()
        .find(|(surface, _)| surface == name)
        .ok_or_else(|| conflict("Поверхность отсутствует в проверенном плане очистки"))?;
    let current = open_cleanup_surface(directory, name)?
        .ok_or_else(|| conflict("Поверхность исчезла после проверки очистки"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let before = original.1.metadata().map_err(read_error)?;
        let after = current.metadata().map_err(read_error)?;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(conflict(
                "Поверхность задания подменена после проверки; очистка остановлена",
            ));
        }
    }
    Ok(())
}

fn remove_cleanup_worktree(
    job: &PreparedJob,
    directory: &File,
    surfaces: &[(String, File)],
) -> Result<(), DomainError> {
    verify_cleanup_surface_identity(directory, "worktree", surfaces)?;
    let source = surfaces
        .iter()
        .find(|(name, _)| name == "worktree")
        .ok_or_else(|| conflict("Нет закреплённого рабочего дерева для очистки"))?;
    let path = cleanup_descriptor_path(&source.1)?;
    let hooks = cleanup_hooks(directory)?;
    if worktree_marker_present(&source.1)? {
        let records = own_worktree_records(job, &source.1, &hooks)?;
        if records.iter().any(|record| record.locked) {
            return Err(DomainError::new(
                ErrorCode::ProcessOperationFailed,
                "Удаление рабочего дерева не выполнено: оно защищено явной блокировкой Git. Снимите её вручную командой `git worktree unlock` для рабочего дерева этого задания, затем повторите очистку.",
            ));
        }
    }
    let mut command = trusted_git(&job.root, &hooks)?;
    command.args(["worktree", "remove", "--force"]).arg(&path);
    let output = command.output().map_err(process_error)?;
    if output.status.success() {
        return Ok(());
    }
    let reason = stable_message(
        &String::from_utf8_lossy(&output.stderr),
        &[&path, &job.directory, &job.worktree(), &job.root],
    );
    // Административные записи Git, указывающие ровно на рабочее дерево этого
    // задания: чужие записи не перечисляются и не удаляются.
    let records = own_worktree_records(job, &source.1, &hooks)?;
    if worktree_marker_present(&source.1)? {
        // Каталог остаётся рабочим деревом Git: расхождение регистрации
        // (например, перемещённый каталог задания) — честный отказ, чужие
        // ресурсы и записи Git не трогаются. Явную блокировку Git не снимаем:
        // её мог поставить оператор, чтобы сохранить рабочее дерево.
        if records.iter().any(|record| record.locked) {
            return Err(DomainError::new(
                ErrorCode::ProcessOperationFailed,
                format!(
                    "Удаление рабочего дерева не выполнено: {reason}; оно защищено явной блокировкой Git. Снимите её вручную командой `git worktree unlock` для рабочего дерева этого задания, затем повторите очистку."
                ),
            ));
        }
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            format!("Удаление рабочего дерева не завершено: {reason}"),
        ));
    }
    // Git о каталоге не знает (авария внутри `git worktree add`): удаляем
    // собственный каталог тем же путём, что и остальные поверхности, и ровно
    // свою застарелую запись. Репозиторий целиком не чистится: чужие записи,
    // включая prunable, остаются на месте.
    remove_cleanup_runtime(directory, "worktree", surfaces)?;
    for record in &records {
        remove_own_worktree_record(record)?;
    }
    Ok(())
}

/// Административная запись Git о рабочем дереве, принадлежащая заданию.
struct OwnWorktreeRecord {
    directory: PathBuf,
    locked: bool,
}

/// Перечисляет административные записи Git, которые указывают на `.git` именно
/// закреплённого рабочего дерева задания.
///
/// Принадлежность подтверждается дважды: содержимым файла `gitdir` записи и
/// совпадением каталога по внешнему пути с закреплённым дескриптором. Записи
/// чужих рабочих деревьев не возвращаются.
fn own_worktree_records(
    job: &PreparedJob,
    worktree: &File,
    hooks: &Path,
) -> Result<Vec<OwnWorktreeRecord>, DomainError> {
    let pinned = worktree.metadata().map_err(read_error)?;
    match fs::metadata(job.worktree()) {
        Ok(actual) if same_directory(&pinned, &actual) => (),
        // Каталог задания подменён, перемещён или недоступен: записи не трогаем.
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    }
    own_worktree_records_by_gitdir(job, hooks)
}

/// Убирает собственную административную запись Git, когда каталога рабочего
/// дерева уже нет.
///
/// Прерванная очистка (или разовый отказ удаления записи после удаления
/// каталога) оставляет нашу запись без каталога: Git считает её prunable и
/// удаляет без ожидания grace period, а инструмент обязан убрать свой мусор сам,
/// иначе повторная очистка рапортует успех, оставляя запись навсегда.
///
/// Владение подтверждается двумя независимыми фактами: запись
/// зарегистрирована в административном каталоге ЭТОГО репозитория, а её файл
/// `gitdir` точно указывает на `<job>/worktree/.git` — путь, которым владеет
/// только это задание и который берётся из манифеста задания, а не из
/// файловой системы. Признак закреплённого дескриптора здесь недоступен:
/// каталога уже нет. Его отсутствие не превращается в отказ от удаления
/// собственной записи — но и живой каталог на пути задания не трогается.
fn remove_stale_own_worktree_records(job: &PreparedJob, hooks: &Path) -> Result<(), DomainError> {
    // Пока каталог на месте, за запись отвечает `remove_cleanup_worktree`:
    // там владение подтверждается закреплённым дескриптором.
    match fs::symlink_metadata(job.worktree()) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(read_error(error)),
    }
    for record in own_worktree_records_by_gitdir(job, hooks)? {
        remove_own_worktree_record(&record)?;
    }
    Ok(())
}

/// Перечисляет записи административного каталога этого репозитория, чей `gitdir`
/// указывает на `<job>/worktree/.git`. Совпадение пути обязательно: записи чужих
/// рабочих деревьев не возвращаются.
fn own_worktree_records_by_gitdir(
    job: &PreparedJob,
    hooks: &Path,
) -> Result<Vec<OwnWorktreeRecord>, DomainError> {
    let expected = job.worktree().join(".git");
    let admin = worktree_admin_directory(job, hooks)?;
    let entries = match fs::read_dir(&admin) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.map_err(read_error)?;
        let directory = entry.path();
        if !entry.file_type().map_err(read_error)?.is_dir() {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(directory.join("gitdir")) else {
            continue;
        };
        if !same_record_target(&gitdir, &expected) {
            continue;
        }
        records.push(OwnWorktreeRecord {
            locked: directory.join("locked").is_file(),
            directory,
        });
    }
    Ok(records)
}

/// Каталог hooks, который передаётся Git во время очистки задания.
fn cleanup_hooks(directory: &File) -> Result<PathBuf, DomainError> {
    Ok(cleanup_descriptor_path(directory)?.join("hooks"))
}

/// Каталог административных записей рабочих деревьев репозитория задания.
fn worktree_admin_directory(job: &PreparedJob, hooks: &Path) -> Result<PathBuf, DomainError> {
    let mut command = trusted_git(&job.root, hooks)?;
    command.args(["rev-parse", "--git-path", "worktrees"]);
    let output = command.output().map_err(process_error)?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            "Git не смог определить каталог административных записей рабочих деревьев",
        ));
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(if path.is_absolute() {
        path
    } else {
        job.root.join(path)
    })
}

/// Сверяет точный путь из административной записи с ожидаемым `.git` задания.
fn same_record_target(recorded: &str, expected: &Path) -> bool {
    Path::new(recorded.trim()) == expected
}

/// Совпадают ли два каталога: подтверждение принадлежности записи заданию.
#[cfg(unix)]
fn same_directory(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_directory(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

/// Удаляет административную запись, принадлежащую заданию. Чужие записи сюда не
/// попадают: путь получен перечислением каталога записей, а принадлежность
/// подтверждена содержимым `gitdir` и закреплённым каталогом.
fn remove_own_worktree_record(record: &OwnWorktreeRecord) -> Result<(), DomainError> {
    let metadata = fs::symlink_metadata(&record.directory).map_err(read_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(conflict(
            "Административная запись рабочего дерева заменена не каталогом; ресурсы сохранены",
        ));
    }
    fs::remove_dir_all(&record.directory).map_err(write_error)
}

/// Есть ли у закреплённого каталога признак рабочего дерева Git: без него Git о
/// каталоге не знает и административной записи быть не может.
fn worktree_marker_present(worktree: &File) -> Result<bool, DomainError> {
    let marker = cleanup_descriptor_path(worktree)?.join(".git");
    match fs::symlink_metadata(&marker) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(read_error(error)),
    }
}
fn remove_cleanup_runtime(
    directory: &File,
    name: &str,
    surfaces: &[(String, File)],
) -> Result<(), DomainError> {
    verify_cleanup_surface_identity(directory, name, surfaces)?;
    // На Linux std удаляет относительно открытых каталогов и не проходит
    // по символической ссылке. Закреплён и родитель: его исходный путь может быть заменён.
    fs::remove_dir_all(cleanup_descriptor_path(directory)?.join(name)).map_err(write_error)
}

fn cleanup_descriptor_path(directory: &File) -> Result<PathBuf, DomainError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        // PID относится к исполнителю: дочерний Git может читать дескриптор
        // родителя, хотя его собственные дескрипторы закрываются при exec.
        let path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            directory.as_raw_fd()
        ));
        let direct = directory.metadata().map_err(read_error)?;
        let anchored = fs::metadata(&path).map_err(read_error)?;
        if direct.dev() != anchored.dev() || direct.ino() != anchored.ino() {
            return Err(conflict(
                "Закреплённый каталог /proc не соответствует дескриптору",
            ));
        }
        Ok(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        Err(invalid(
            "Безопасная очистка ресурсов требует Linux и доступного /proc; исходные пути не используются как fallback",
        ))
    }
}
#[cfg(unix)]
fn open_cleanup_surface(directory: &File, name: &str) -> Result<Option<File>, DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => Err(conflict(
            "Принадлежащая заданию поверхность заменена символической ссылкой или не каталогом; ресурсы сохранены",
        )),
        Err(error) => Err(read_error(error.into())),
    }
}
#[cfg(not(unix))]
fn open_cleanup_surface(_directory: &File, _name: &str) -> Result<Option<File>, DomainError> {
    platform_supported()?;
    unreachable!()
}
#[cfg(not(unix))]
fn job_lock_at(_directory: &File) -> Result<File, DomainError> {
    platform_supported()?;
    unreachable!()
}

fn validate_result(metadata: &JobMetadata, result: &ExecutionResult) -> Result<(), DomainError> {
    if result.schema_version != EXECUTION_SCHEMA_VERSION
        || result.job_id != metadata.job_id
        || result.source != metadata.source
        || result.mode != metadata.mode
        || result.scope != metadata.scope
        || result.namespace != metadata.namespace
        || result.lifecycle != LifecycleStatus::Completed
        || result.stdout.log != "logs/stdout.log"
        || result.stderr.log != "logs/stderr.log"
    {
        return Err(invalid(
            "Идентичность результата или относительные пути логов не соответствуют заданию",
        ));
    }
    validate_request(&result.request)?;
    validate_hex(&result.argv_sha256, 64, "хеш argv")
}

pub fn safe_argv(argv: &[String]) -> Vec<String> {
    redact_argv(argv).0
}

/// Один разбор задаёт безопасное представление argv и значения для очистки вывода.
fn redact_argv(argv: &[String]) -> (Vec<String>, Vec<String>) {
    let mut safe = argv.to_vec();
    let mut values = Vec::new();
    let mut index = 1; // argv[0] — имя исполняемого файла, а не параметр.
    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--" {
            break;
        }
        if matches!(argument.as_str(), "-H" | "--header" | "--proxy-header") {
            if let Some(value) = argv.get(index + 1) {
                if let Some(redacted) = redact_header(value, &mut values) {
                    safe[index + 1] = redacted;
                }
                index += 1;
            }
        } else if let Some(redacted) = redact_header(argument, &mut values) {
            safe[index] = redacted;
        } else if let Some(value) = argument.strip_prefix("-H") {
            if let Some(redacted) = redact_header(value, &mut values) {
                safe[index] = format!("-H{redacted}");
            }
        } else if let Some((key, value)) = argument.split_once('=') {
            if matches!(key, "--header" | "--proxy-header") {
                if let Some(redacted) = redact_header(value, &mut values) {
                    safe[index] = format!("{key}={redacted}");
                }
            } else if is_sensitive_option_key(key)
                || (!key.starts_with('-') && is_sensitive_environment_key(key))
            {
                remember_sensitive_value(value, &mut values);
                safe[index] = format!("{key}=<redacted>");
            }
        } else if argument.starts_with('-')
            && is_sensitive_option_key(argument)
            && let Some(value) = argv.get(index + 1)
        {
            remember_sensitive_value(value, &mut values);
            safe[index + 1] = "<redacted>".into();
            index += 1;
        }
        index += 1;
    }
    (safe, values)
}

fn is_sensitive_option_key(value: &str) -> bool {
    matches!(
        value
            .trim_start_matches('-')
            .to_ascii_lowercase()
            .replace('_', "-")
            .as_str(),
        "token"
            | "api-token"
            | "access-token"
            | "auth-token"
            | "refresh-token"
            | "secret"
            | "client-secret"
            | "password"
            | "passwd"
            | "api-key"
            | "apikey"
            | "authorization"
            | "proxy-authorization"
            | "credential"
            | "credentials"
            | "cookie"
    )
}

fn is_sensitive_environment_key(value: &str) -> bool {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return false;
    }
    value.split('_').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "token"
                | "secret"
                | "password"
                | "passwd"
                | "apikey"
                | "authorization"
                | "credential"
                | "credentials"
                | "cookie"
        )
    }) || value.eq_ignore_ascii_case("API_KEY")
        || value.to_ascii_uppercase().ends_with("_API_KEY")
}

fn remember_sensitive_value(value: &str, values: &mut Vec<String>) {
    if !value.is_empty() {
        values.push(value.to_owned());
        // Заголовок Authorization может отражаться вместе со схемой или без неё.
        if let Some((scheme, credential)) = value.split_once(char::is_whitespace)
            && (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic"))
            && !credential.trim().is_empty()
        {
            values.push(credential.trim().to_owned());
        }
    }
}

fn redact_header(value: &str, values: &mut Vec<String>) -> Option<String> {
    let (key, content) = value.split_once(':')?;
    if !matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "x-api-key" | "api-key"
    ) {
        return None;
    }
    remember_sensitive_value(content.trim(), values);
    Some(format!("{key}: <redacted>"))
}

fn sensitive_request_values(request: &CommandRequest) -> Vec<String> {
    let (_, mut values) = redact_argv(&request.argv);
    if let EnvironmentPolicy::Explicit {
        values: environment,
    } = &request.options.environment
    {
        for (key, value) in environment {
            if is_sensitive_environment_key(key) {
                remember_sensitive_value(value, &mut values);
            }
        }
    }
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn argv_digest(argv: &[String]) -> String {
    let mut digest = Sha256::new();
    for argument in safe_argv(argv) {
        digest.update((argument.len() as u64).to_le_bytes());
        digest.update(argument.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn serialize_safe_argv<S>(argv: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    safe_argv(argv).serialize(serializer)
}

fn serialize_redacted_environment<S>(
    values: &BTreeMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    values
        .keys()
        .map(|key| (key, "<redacted>"))
        .collect::<BTreeMap<_, _>>()
        .serialize(serializer)
}

fn validate_request(request: &CommandRequest) -> Result<(), DomainError> {
    let argument_bytes = request
        .argv
        .iter()
        .map(String::len)
        .fold(0_usize, usize::saturating_add);
    if request.argv.is_empty()
        || request.argv[0].is_empty()
        || request.argv.iter().any(|arg| arg.contains('\0'))
        || argument_bytes > MAX_REQUEST_ARGUMENT_BYTES
    {
        return Err(invalid(
            "Нужен явно переданный argv ограниченного размера без NUL",
        ));
    }
    if request.options.output_limit_bytes > MAX_OUTPUT_BYTES
        || !(1..=64).contains(&request.options.max_parallel_jobs)
        || request
            .options
            .timeout_ms
            .is_none_or(|timeout| timeout == 0)
    {
        return Err(invalid(
            "Неверный предел вывода, время выполнения или предел параллелизма",
        ));
    }
    relative_path(&request.cwd)?;
    if let EnvironmentPolicy::Explicit { values } = &request.options.environment {
        for (key, value) in values {
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return Err(invalid("Неверная переменная окружения"));
            }
            if PRIVATE_ENV.contains(&key.as_str()) {
                return Err(invalid(format!(
                    "Переменная собственного пути задания {key} не переопределяется"
                )));
            }
            if key.len().saturating_add(value.len()) > MAX_REQUEST_ARGUMENT_BYTES {
                return Err(invalid("значения --env превышают общий лимит размера"));
            }
        }
    }
    Ok(())
}
const PRIVATE_ENV: &[&str] = &[
    "CARGO_TARGET_DIR",
    "CARGO_HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "HOME",
    "USERPROFILE",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
];
fn configure_environment(
    command: &mut Command,
    job: &PreparedJob,
    policy: &EnvironmentPolicy,
) -> Result<(), DomainError> {
    command.env_clear();
    for name in ["PATH", "RUSTUP_HOME", "SystemRoot", "WINDIR", "PATHEXT"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    // Прокси rustup должны найти установленный набор инструментов даже с отдельным HOME.
    if std::env::var_os("RUSTUP_HOME").is_none()
        && let Some(home) = std::env::var_os("HOME")
    {
        let rustup = PathBuf::from(home).join(".rustup");
        if rustup.is_dir() {
            command.env("RUSTUP_HOME", rustup);
        }
    }
    if let EnvironmentPolicy::Explicit { values } = policy {
        command.envs(values);
    }
    for (name, surface) in [
        ("CARGO_TARGET_DIR", "target"),
        ("CARGO_HOME", "cargo-home"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "scratch"),
        ("XDG_DATA_HOME", "scratch"),
    ] {
        let path = job.directory.join(surface);
        safe_dir(&path)?;
        command.env(name, path);
    }
    Ok(())
}

fn relative_path(value: &str) -> Result<PathBuf, DomainError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid("cwd должен быть относительным путём без .."));
    }
    Ok(path.to_path_buf())
}
fn source_cwd(job: &PreparedJob, value: &str) -> Result<PathBuf, DomainError> {
    let path = relative_path(value)?;
    let cwd = if value == "." {
        job.worktree()
    } else {
        job.worktree().join(path)
    };
    safe_dir(&cwd)?;
    Ok(cwd)
}
fn valid_component(value: &str) -> Result<(), DomainError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid("Неверный компонент пути рабочей области"));
    }
    Ok(())
}
pub(super) fn valid_artifact_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
fn validate_hex(value: &str, length: usize, description: &str) -> Result<(), DomainError> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!("некорректный {description}")));
    }
    Ok(())
}
fn validate_sha(value: &str) -> Result<(), DomainError> {
    if ![40, 64].contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "Нужен полный Git SHA в шестнадцатеричной форме со строчными буквами",
        ));
    }
    Ok(())
}
fn random_id() -> Result<String, DomainError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| {
        invalid(format!(
            "Не удалось получить случайный идентификатор задания: {e}"
        ))
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn unique_directory(parent: &Path) -> Result<(PathBuf, String), DomainError> {
    let dir = safe_dir(parent)?;
    for _ in 0..16 {
        let id = random_id()?;
        match mkdir_fd(&dir, &id) {
            Ok(()) => return Ok((parent.join(&id), id)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(write_error(error)),
        }
    }
    Err(conflict(
        "Не удалось зарезервировать уникальный каталог задания",
    ))
}
fn absolute_path(path: &Path) -> Result<PathBuf, DomainError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(read_error)?.join(path)
    };
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("Псевдонимы пути, . и .. в пути задания запрещены"));
    }
    Ok(path)
}

fn verify_snapshot(root: &Path, target: &GitTarget) -> Result<(), DomainError> {
    let collected = super::scope::collect_scope(root, &target.base_sha, &target.head_sha)
        .map_err(|e| invalid(format!("Git snapshot недоступен: {e}")))?;
    if collected.target != *target {
        return Err(invalid(
            "Идентичность репозитория и base/head/merge-base не соответствует объектам Git",
        ));
    }
    Ok(())
}
fn verify_worktree_head(job: &PreparedJob) -> Result<(), DomainError> {
    safe_dir(&job.worktree())?;
    let output = Command::new("git")
        .current_dir(job.worktree())
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(process_error)?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            format!(
                "Git не смог прочитать HEAD рабочего дерева: {}",
                stable_message(
                    &String::from_utf8_lossy(&output.stderr),
                    &[&job.directory, &job.worktree()],
                )
            ),
        ));
    }
    if String::from_utf8_lossy(&output.stdout).trim() != job.metadata.source.snapshot.head_sha {
        return Err(invalid(
            "Рабочее дерево задания больше не соответствует закреплённому HEAD",
        ));
    }
    Ok(())
}
fn source_changed(job: &PreparedJob) -> Result<bool, DomainError> {
    let output = Command::new("git")
        .current_dir(job.worktree())
        .args([
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain",
            "--untracked-files=all",
        ])
        .output()
        .map_err(process_error)?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            "Git не смог проверить изменения исходников",
        ));
    }
    Ok(!output.stdout.is_empty() || verify_worktree_head(job).is_err())
}
fn trusted_git(root: &Path, hooks: &Path) -> Result<Command, DomainError> {
    let mut command = Command::new("git");
    command.current_dir(root).args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "submodule.recurse=false",
    ]);
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()));
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "config",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(smudge|process|required)$",
        ])
        .output()
        .map_err(process_error)?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            "Git не смог проверить фильтры checkout",
        ));
    }
    let names = String::from_utf8(output.stdout)
        .map_err(|e| invalid(format!("Имена фильтров Git не в UTF-8: {e}")))?;
    for name in names.lines() {
        if name.contains(['\n', '\r', '\0', '=']) {
            return Err(invalid("небезопасное имя Git filter"));
        }
        command.arg("-c").arg(format!(
            "{name}={}",
            if name.ends_with(".required") {
                "false"
            } else {
                ""
            }
        ));
    }
    Ok(command)
}
/// Сообщения Git не должны раскрывать сырые локальные пути: `paths` — известные
/// вызывающему пути задания, которые заменяются стабильным описанием.
fn git_success_redacting(
    mut command: Command,
    operation: &str,
    paths: &[&Path],
) -> Result<(), DomainError> {
    let output = command.output().map_err(process_error)?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::ProcessOperationFailed,
            format!(
                "{operation}: {}",
                stable_message(&String::from_utf8_lossy(&output.stderr), paths)
            ),
        ));
    }
    Ok(())
}

/// Заменяет сырые локальные пути стабильным описанием: и перечисленные пути
/// задания, и закреплённые дескрипторы вида `/proc/<pid>/fd/<n>`.
fn stable_message(message: &str, paths: &[&Path]) -> String {
    let mut text = message.to_owned();
    for path in paths {
        let raw = path.display().to_string();
        if raw.len() > 1 {
            text = text.replace(&raw, "<путь задания>");
        }
    }
    redact_descriptor_paths(&text)
}

/// Дескриптор вида `/proc/<pid>/fd/<n>` заменяется описанием без номера процесса.
fn redact_descriptor_paths(message: &str) -> String {
    const MARKER: &str = "/proc/";
    let mut text = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(index) = rest.find(MARKER) {
        text.push_str(&rest[..index]);
        let tail = &rest[index..];
        let end = tail
            .find(|symbol: char| {
                symbol.is_whitespace() || matches!(symbol, '\'' | '"' | ',' | ')' | ']' | ':' | ';')
            })
            .unwrap_or(tail.len());
        text.push_str("<закреплённый дескриптор задания>");
        rest = &tail[end..];
    }
    text.push_str(rest);
    text
}
fn verify_owner(job: &PreparedJob) -> Result<State, DomainError> {
    verify_owner_at(job, &safe_dir(&job.directory)?)
}
fn verify_owner_at(job: &PreparedJob, directory: &File) -> Result<State, DomainError> {
    let metadata: JobMetadata = read_document_at(directory, "job.json")?;
    let state: State = read_document_at(directory, "state.json")?;
    if metadata != job.metadata || metadata.owner_nonce != state.owner_nonce {
        return Err(invalid("Владелец задания изменился"));
    }
    let bytes = read_optional_file_at(
        directory,
        "source-review.json",
        super::workflow::MAX_REVIEW_ARTIFACT_BYTES,
    )?
    .ok_or_else(|| read_error(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
    if format!("{:x}", Sha256::digest(&bytes)) != metadata.source.review_pack_sha256 {
        return Err(invalid("Идентичность байтов исходного пакета изменилась"));
    }
    let pack: ReviewPack = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("source-review.json повреждён: {error}")))?;
    if pack.target != metadata.source.snapshot {
        return Err(invalid(
            "source-review.json не соответствует снимку из манифеста задания",
        ));
    }
    Ok(state)
}
fn save_state(
    job: &PreparedJob,
    lifecycle: LifecycleStatus,
    workspace_removed: bool,
) -> Result<(), DomainError> {
    write_document(
        &job.directory,
        "state.json",
        &State {
            owner_nonce: job.metadata.owner_nonce.clone(),
            lifecycle,
            workspace_removed,
        },
        true,
    )
}
fn cancel_requested(job: &PreparedJob) -> Result<bool, DomainError> {
    match read_optional_file_at(&safe_dir(&job.directory)?, "cancel.json", 128)? {
        None => Ok(false),
        Some(bytes) if bytes == job.metadata.owner_nonce.as_bytes() => Ok(true),
        Some(_) => Err(invalid("Владелец маркера отмены не совпадает с заданием")),
    }
}
fn output_evidence(
    job: &PreparedJob,
    name: &str,
    limit: usize,
    sensitive_values: &[String],
) -> Result<OutputEvidence, DomainError> {
    let directory = safe_dir(&job.directory.join("logs"))?;
    let mut file = open_file(&directory, name, false)?;
    let total_bytes = file.metadata().map_err(read_error)?.len();
    let redaction_context_bytes = sensitive_values
        .iter()
        .filter(|value| !value.is_empty())
        .map(String::len)
        .max()
        .unwrap_or_default();
    let read_limit = limit.saturating_add(redaction_context_bytes);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(read_limit as u64)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    let intervals = sensitive_output_intervals(&bytes, sensitive_values);
    let mut cutoff = bytes.len().min(limit);
    for &(begin, end) in &intervals {
        if begin < cutoff && cutoff < end {
            cutoff = begin;
            break;
        }
    }
    let utf8_lossy = std::str::from_utf8(&bytes[..cutoff]).is_err();
    let mut text = String::new();
    let mut copied = 0;
    for &(begin, end) in &intervals {
        if begin >= cutoff {
            break;
        }
        text.push_str(&String::from_utf8_lossy(&bytes[copied..begin]));
        text.push_str("<redacted>");
        copied = end;
    }
    text.push_str(&String::from_utf8_lossy(&bytes[copied..cutoff]));
    let truncated = total_bytes > cutoff as u64 || text.len() > limit;
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Ok(OutputEvidence {
        text,
        total_bytes,
        truncated,
        utf8_lossy,
        log: format!("logs/{name}"),
    })
}

/// Объединяем пересечения по исходным байтам до замены: последовательный replace
/// теряет перекрывающиеся секреты и способен раскрыть их оставшийся суффикс.
fn sensitive_output_intervals(bytes: &[u8], sensitive_values: &[String]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    for value in sensitive_values.iter().filter(|value| !value.is_empty()) {
        let pattern = value.as_bytes();
        if pattern.len() > bytes.len() {
            continue;
        }
        let mut previous: Option<(usize, usize)> = None;
        for (begin, window) in bytes.windows(pattern.len()).enumerate() {
            if window == pattern {
                let end = begin + pattern.len();
                if let Some((_, previous_end)) = previous.as_mut()
                    && begin <= *previous_end
                {
                    *previous_end = end;
                } else {
                    if let Some(interval) = previous {
                        matches.push(interval);
                    }
                    previous = Some((begin, end));
                }
            }
        }
        if let Some(interval) = previous {
            matches.push(interval);
        }
    }
    matches.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (begin, end) in matches {
        if let Some((_, previous_end)) = merged.last_mut()
            && begin <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((begin, end));
        }
    }
    merged
}

fn exit_summary(status: ExitStatus) -> ProcessExit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ProcessExit {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        ProcessExit {
            code: status.code(),
            signal: None,
        }
    }
}
#[cfg(target_os = "linux")]
fn observe_child(child: &mut std::process::Child) -> std::io::Result<Option<ProcessExit>> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let pid = Pid::from_raw(child.id() as i32)
        .ok_or_else(|| std::io::Error::other("Некорректный PID дочернего процесса"))?;
    waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )
    .map(|status| {
        status.map(|s| ProcessExit {
            code: s.exit_status(),
            signal: s.terminating_signal(),
        })
    })
    .map_err(Into::into)
}
#[cfg(not(target_os = "linux"))]
fn observe_child(child: &mut std::process::Child) -> std::io::Result<Option<ProcessExit>> {
    child.try_wait().map(|status| status.map(exit_summary))
}
fn stop_child(
    child: &mut std::process::Child,
    enforcement: &mut Enforcement,
    failure: &mut Option<String>,
) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        match Pid::from_raw(child.id() as i32)
            .ok_or(rustix::io::Errno::INVAL)
            .and_then(|pid| kill_process_group(pid, Signal::KILL))
        {
            Ok(()) => enforcement.process_cleanup = "process_group_killed_partial".into(),
            Err(error) => {
                enforcement.process_cleanup = "process_group_kill_failed".into();
                *failure = Some(format!(
                    "Не удалось завершить принадлежащую заданию группу процессов: {error}"
                ));
            }
        }
    }
    #[cfg(not(unix))]
    {
        enforcement.process_cleanup = "direct_child_only".into();
    }
    if let Err(error) = child.kill() {
        *failure = Some(format!(
            "Не удалось завершить непосредственный дочерний процесс: {error}"
        ));
    }
}

#[cfg(target_os = "linux")]
fn platform_supported() -> Result<(), DomainError> {
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn platform_supported() -> Result<(), DomainError> {
    Err(invalid(
        "Выполнение задания доступно только на Linux с доступным /proc для безопасной очистки ресурсов",
    ))
}

// Все собственные файлы открываются через дескрипторы каталогов и O_NOFOLLOW.
// Это исключает чтение и публикацию по символической ссылке, включая последний компонент.
#[cfg(unix)]
pub(super) fn safe_dir(path: &Path) -> Result<File, DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    let path = absolute_path(path)?;
    let mut directory = File::open("/").map_err(read_error)?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                directory = File::from(
                    openat(
                        &directory,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|error| match error {
                        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => DomainError::new(
                            ErrorCode::InvalidRequest,
                            "Путь пространства ревью содержит символическую ссылку или не каталог",
                        ),
                        _ => read_error(error.into()),
                    })?,
                )
            }
            _ => return Err(invalid("Неподдерживаемый компонент пути каталога")),
        }
    }
    Ok(directory)
}
#[cfg(not(unix))]
pub(super) fn safe_dir(_path: &Path) -> Result<File, DomainError> {
    platform_supported()?;
    unreachable!()
}

#[cfg(unix)]
pub(super) fn ensure_dir(path: &Path) -> Result<(), DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    let path = absolute_path(path)?;
    let mut directory = File::open("/").map_err(write_error)?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            match rustix::fs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                Err(error) => return Err(write_error(error.into())),
            }
            directory = File::from(
                openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| match error {
                    rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => DomainError::new(
                        ErrorCode::InvalidRequest,
                        "Путь пространства ревью содержит символическую ссылку или не каталог",
                    ),
                    _ => write_error(error.into()),
                })?,
            );
        }
    }
    Ok(())
}
#[cfg(not(unix))]
pub(super) fn ensure_dir(_path: &Path) -> Result<(), DomainError> {
    platform_supported()
}
#[cfg(unix)]
fn mkdir_fd(directory: &File, name: &str) -> std::io::Result<()> {
    rustix::fs::mkdirat(directory, name, rustix::fs::Mode::from_raw_mode(0o700)).map_err(Into::into)
}
#[cfg(not(unix))]
fn mkdir_fd(_directory: &File, _name: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("Платформа не поддерживается"))
}

#[cfg(unix)]
fn open_file(directory: &File, name: &str, create: bool) -> Result<File, DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    if !valid_artifact_name(name) {
        return Err(invalid(
            "Имя артефакта задания должно быть одним безопасным компонентом",
        ));
    }
    let flags = if create {
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL
    } else {
        OFlags::RDONLY
    };
    let file = File::from(
        openat(
            directory,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| {
            if create {
                write_error(error.into())
            } else {
                read_error(error.into())
            }
        })?,
    );
    if !file.metadata().map_err(read_error)?.is_file() {
        return Err(invalid("Артефакт задания должен быть обычным файлом"));
    }
    Ok(file)
}
#[cfg(not(unix))]
fn open_file(_directory: &File, _name: &str, _create: bool) -> Result<File, DomainError> {
    platform_supported()?;
    unreachable!()
}

fn read_bytes(directory: &Path, name: &str, max: u64) -> Result<Vec<u8>, DomainError> {
    let mut file = open_file(&safe_dir(directory)?, name, false)?;
    if file.metadata().map_err(read_error)?.len() > max {
        return Err(invalid("Артефакт задания превышает лимит чтения"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > max {
        return Err(invalid("Артефакт задания вырос при чтении"));
    }
    Ok(bytes)
}
fn read_document_at<T: serde::de::DeserializeOwned>(
    directory: &File,
    name: &str,
) -> Result<T, DomainError> {
    let bytes = read_optional_file_at(directory, name, MAX_DOCUMENT_BYTES)?
        .ok_or_else(|| read_error(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
    serde_json::from_slice(&bytes).map_err(|error| invalid(format!("Невалидный {name}: {error}")))
}
fn write_document_at<T: Serialize>(
    directory: &File,
    name: &str,
    value: &T,
    replace: bool,
) -> Result<(), DomainError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| invalid(format!("Не удалось сериализовать {name}: {error}")))?;
    bytes.push(b'\n');
    if replace {
        replace_file_at(directory, name, &bytes).map_err(write_error)
    } else {
        write_new_fd(directory, name, &bytes).map_err(write_error)
    }
}

fn read_document<T: serde::de::DeserializeOwned>(
    directory: &Path,
    name: &str,
) -> Result<T, DomainError> {
    serde_json::from_slice(&read_bytes(directory, name, MAX_DOCUMENT_BYTES)?)
        .map_err(|e| invalid(format!("невалидный {name}: {e}")))
}
fn write_new(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), DomainError> {
    write_new_fd(&safe_dir(directory)?, name, bytes).map_err(write_error)
}
pub(super) fn write_new_fd(directory: &File, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    if !valid_artifact_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Имя артефакта должно быть одним безопасным компонентом",
        ));
    }
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, unlinkat};
        let nonce = random_id().map_err(|error| std::io::Error::other(error.message))?;
        let temporary = format!(".publish-{nonce}");
        let mut file = File::from(
            openat(
                directory,
                temporary.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(std::io::Error::from)?,
        );
        let publish = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                linkat(
                    directory,
                    temporary.as_str(),
                    directory,
                    name,
                    AtFlags::empty(),
                )
                .map_err(std::io::Error::from)
            });
        drop(file);
        let cleanup =
            unlinkat(directory, temporary.as_str(), AtFlags::empty()).map_err(std::io::Error::from);
        if let Err(error) = publish {
            let _ = cleanup;
            return Err(error);
        }
        cleanup?;
        directory.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, name, bytes);
        Err(std::io::Error::other("Платформа не поддерживается"))
    }
}

/// Читает обычный файл через закреплённый каталог, не переходя по символическим ссылкам.
pub(super) fn read_optional_file_at(
    directory: &File,
    name: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, DomainError> {
    if !valid_artifact_name(name) {
        return Err(invalid(
            "Имя артефакта ревью должно быть одним безопасным компонентом",
        ));
    }
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, openat};
        let mut file = match openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(rustix::io::Errno::LOOP) => {
                return Err(conflict(
                    "Артефакт ревью не может быть символической ссылкой",
                ));
            }
            Err(error) => return Err(read_error(error.into())),
        };
        let metadata = file.metadata().map_err(read_error)?;
        if !metadata.is_file() {
            return Err(conflict("Артефакт ревью должен быть обычным файлом"));
        }
        if metadata.len() > max_bytes {
            return Err(invalid("Артефакт ревью превышает лимит чтения"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        if bytes.len() as u64 > max_bytes {
            return Err(invalid("Артефакт ревью вырос при чтении"));
        }
        Ok(Some(bytes))
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, max_bytes);
        platform_supported()?;
        unreachable!()
    }
}

/// Атомарно заменяет только имя внутри уже открытого каталога.
pub(super) fn replace_file_at(directory: &File, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    if !valid_artifact_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Имя артефакта ревью должно быть одним безопасным компонентом",
        ));
    }
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, renameat, unlinkat};
        let temporary = format!(
            ".review-publish-{}",
            random_id().map_err(|error| { std::io::Error::other(error.message) })?
        );
        write_new_fd(directory, &temporary, bytes)?;
        if let Err(error) = renameat(directory, temporary.as_str(), directory, name) {
            let _ = unlinkat(directory, temporary.as_str(), AtFlags::empty());
            return Err(error.into());
        }
        directory.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, bytes);
        Err(std::io::Error::other("Платформа не поддерживается"))
    }
}

#[cfg(unix)]
fn remove_partial_job_directory(parent: &Path, job_id: &str) -> Result<(), DomainError> {
    use rustix::fs::{AtFlags, unlinkat};
    valid_component(job_id)?;
    let parent_fd = safe_dir(parent)?;
    let job_path = parent.join(job_id);
    let job_fd = safe_dir(&job_path)?;
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(&job_path).map_err(read_error)? {
        let entry = entry.map_err(read_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("В частичном задании есть имя файла не в UTF-8"))?;
        if !matches!(
            name.as_str(),
            "job.json" | "source-review.json" | "state.json" | "active.lock"
        ) || !entry.file_type().map_err(read_error)?.is_file()
        {
            return Err(conflict(
                "В частичном задании есть неизвестный файл или символическая ссылка; каталог сохранён",
            ));
        }
        files.insert(name);
    }
    for name in files {
        // open_file использует O_NOFOLLOW и подтверждает, что это обычный файл, перед unlinkat.
        drop(open_file(&job_fd, &name, false)?);
        unlinkat(&job_fd, name.as_str(), AtFlags::empty())
            .map_err(|error| write_error(error.into()))?;
    }
    unlinkat(&parent_fd, job_id, AtFlags::REMOVEDIR).map_err(|error| write_error(error.into()))?;
    Ok(())
}
#[cfg(not(unix))]
fn remove_partial_job_directory(_parent: &Path, _job_id: &str) -> Result<(), DomainError> {
    platform_supported()
}
fn write_document<T: Serialize>(
    directory: &Path,
    name: &str,
    value: &T,
    replace: bool,
) -> Result<(), DomainError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| invalid(format!("Не удалось сериализовать артефакт задания: {e}")))?;
    bytes.push(b'\n');
    let dir = safe_dir(directory)?;
    if !replace {
        return write_new_fd(&dir, name, &bytes).map_err(write_error);
    }
    let temporary = format!(".publish-{}", random_id()?);
    write_new_fd(&dir, &temporary, &bytes).map_err(write_error)?;
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, renameat, unlinkat};
        if let Err(error) = renameat(&dir, temporary.as_str(), &dir, name) {
            let _ = unlinkat(&dir, temporary.as_str(), AtFlags::empty());
            return Err(write_error(error.into()));
        }
        dir.sync_all().map_err(write_error)?;
    }
    #[cfg(not(unix))]
    {
        return platform_supported();
    }
    Ok(())
}

/// Рекомендательные блокировки упорядочивают согласованные записи, не ограничивая права кода.
#[cfg(unix)]
fn job_lock(directory: &Path) -> Result<File, DomainError> {
    job_lock_at(&safe_dir(directory)?)
}
#[cfg(unix)]
fn job_lock_at(directory: &File) -> Result<File, DomainError> {
    use fs2::FileExt;
    let file = lock_file(directory, "active.lock")?;
    match file.try_lock_exclusive() {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(busy(
                "job",
                "job_active",
                "Задание уже выполняется или очищается; повторите попытку позже",
            ));
        }
        Err(error) => return Err(process_error(error)),
    }
    Ok(file)
}
#[cfg(not(unix))]
fn job_lock(_directory: &Path) -> Result<File, DomainError> {
    platform_supported()?;
    unreachable!()
}
#[cfg(unix)]
pub(super) fn lock_file(directory: &File, name: &str) -> Result<File, DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    if !valid_artifact_name(name) {
        return Err(invalid("имя lock должно быть одним безопасным компонентом"));
    }
    let file = File::from(
        openat(
            directory,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| write_error(e.into()))?,
    );
    if !file.metadata().map_err(write_error)?.is_file() {
        return Err(invalid("Файл блокировки должен быть обычным файлом"));
    }
    Ok(file)
}
#[cfg(unix)]
fn lock_is_active(directory: &Path) -> Result<bool, DomainError> {
    use fs2::FileExt;
    // O_RDONLY без CREATE: inspect не создаёт ни одного файла.
    let file = open_file(&safe_dir(directory)?, "active.lock", false)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(process_error(error)),
    }
}
#[cfg(not(unix))]
fn lock_is_active(_directory: &Path) -> Result<bool, DomainError> {
    platform_supported()?;
    unreachable!()
}
#[cfg(unix)]
fn execution_slot(root: &Path, limit: usize) -> Result<File, DomainError> {
    use fs2::FileExt;
    let path = root.join(".anki-repo/review/execution-locks");
    ensure_dir(&path)?;
    let dir = safe_dir(&path)?;
    // Блокировка упорядочивает проверку общей политики и занятие слота.
    let allocation = lock_file(&dir, "allocation.lock")?;
    allocation.lock_exclusive().map_err(process_error)?;
    let mut available = Vec::new();
    let mut active = 0;
    for index in 0..64 {
        let file = lock_file(&dir, &format!("slot-{index}.lock"))?;
        match file.try_lock_exclusive() {
            Ok(()) => available.push((index, file)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => active += 1,
            Err(error) => return Err(process_error(error)),
        }
    }
    let policy = read_optional_file_at(&dir, "parallel-policy.json", MAX_DOCUMENT_BYTES)?;
    let old_limit = policy
        .map(|bytes| {
            serde_json::from_slice::<usize>(&bytes).map_err(|error| {
                invalid(format!(
                    "Повреждена репозиторная политика параллелизма: {error}"
                ))
            })
        })
        .transpose()?;
    if active > 0 && old_limit != Some(limit) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Запрошенный max-parallel-jobs несовместим с пределом активных заданий этого репозитория; дождитесь их завершения или используйте текущий предел",
        ));
    }
    if active == 0 {
        replace_file_at(
            &dir,
            "parallel-policy.json",
            &serde_json::to_vec(&limit).map_err(|error| {
                invalid(format!(
                    "Не удалось сохранить политику параллелизма: {error}"
                ))
            })?,
        )
        .map_err(write_error)?;
    }
    if active >= limit {
        return Err(busy(
            "repository",
            "execution_capacity",
            "Достигнут предел одновременных заданий исполнения; повторите после завершения другого задания",
        ));
    }
    let (_, slot) = available
        .into_iter()
        .find(|(index, _)| *index < limit)
        .ok_or_else(|| {
            busy(
                "repository",
                "execution_capacity",
                "Свободный слот исполнения временно недоступен",
            )
        })?;
    Ok(slot)
}

#[cfg(not(unix))]
fn execution_slot(_root: &Path, _limit: usize) -> Result<File, DomainError> {
    platform_supported()?;
    unreachable!()
}

fn invalid(message: impl Into<String>) -> DomainError {
    DomainError::new(ErrorCode::ReviewArtifactInvalid, message)
}
fn conflict(message: impl Into<String>) -> DomainError {
    DomainError::new(ErrorCode::ReviewArtifactConflict, message)
}
fn read_error(error: std::io::Error) -> DomainError {
    DomainError::new(
        ErrorCode::InputUnreadable,
        format!("Не удалось прочитать ресурс задания: {error}"),
    )
}
fn write_error(error: std::io::Error) -> DomainError {
    DomainError::new(
        ErrorCode::WriteFailed,
        format!("Не удалось записать ресурс задания: {error}"),
    )
}
fn process_error(error: std::io::Error) -> DomainError {
    DomainError::new(
        ErrorCode::ProcessOperationFailed,
        format!("Не удалось выполнить операцию с процессом задания: {error}"),
    )
}
fn busy(resource: &str, reason: &str, message: &str) -> DomainError {
    DomainError::with_details(
        ErrorCode::ExecutionBusy,
        message,
        crate::details! {
            "retryable" => true, "resource" => resource, "reason" => reason,
        },
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::code_review::{language::LanguageScan, model::ReviewScope};
    use std::sync::{Arc, Barrier};

    struct Repo {
        root: PathBuf,
        pack: ReviewPack,
        bytes: Vec<u8>,
    }
    impl Repo {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("anki-execution-{}", random_id().unwrap()));
            fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            git(&root, &["init", "--quiet"]);
            fs::write(root.join("source.txt"), "pinned source\n").unwrap();
            git(&root, &["add", "source.txt"]);
            git(
                &root,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "Исходный снимок",
                ],
            );
            let head = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_owned();
            let target = super::super::scope::collect_scope(&root, &head, &head)
                .unwrap()
                .target;
            let pack = ReviewPack {
                schema_version: 1,
                target: target.clone(),
                scope: ReviewScope {
                    merge_base_sha: target.merge_base_sha,
                    text_image_limit_bytes: 1024,
                    files: vec![],
                },
                diagnostics: vec![],
                candidates: vec![],
                language: LanguageScan {
                    schema_version: 1,
                    files: vec![],
                    candidates: vec![],
                    skipped: vec![],
                },
                dependencies: vec![],
                tests: vec![],
                suppressions: vec![],
                risk_surfaces: vec![],
                tool_runs: vec![],
            };
            let bytes = serde_json::to_vec(&pack).unwrap();
            let repo = Self { root, pack, bytes };
            repo.store_pack("local", &repo.pack, &repo.bytes);
            repo
        }
        fn store_pack(&self, namespace: &str, pack: &ReviewPack, bytes: &[u8]) -> PathBuf {
            let directory = self
                .root
                .join(".anki-repo/review")
                .join(namespace)
                .join(&pack.target.head_sha);
            fs::create_dir_all(&directory).unwrap();
            let path = directory.join("review.json");
            fs::write(&path, bytes).unwrap();
            path
        }
        fn source_path(&self, namespace: &str) -> PathBuf {
            self.root
                .join(".anki-repo/review")
                .join(namespace)
                .join(&self.pack.target.head_sha)
                .join("review.json")
        }
        fn prepare(&self, mode: ExecutionMode) -> PreparedJob {
            prepare_job(
                &self.root,
                &self.pack,
                &self.bytes,
                PrepareOptions {
                    mode,
                    scope: "regression".into(),
                    source_pack: self.source_path("local"),
                    pr_number: None,
                },
            )
            .unwrap()
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    fn request(mode: &str) -> CommandRequest {
        CommandRequest {
            argv: vec![
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                "--exact".into(),
                "code_review::execution::tests::fake_local_command".into(),
                "--nocapture".into(),
            ],
            cwd: ".".into(),
            options: RunOptions {
                timeout_ms: Some(2_000),
                environment: EnvironmentPolicy::Explicit {
                    values: BTreeMap::from([("ANKI_EXECUTION_FAKE_MODE".into(), mode.into())]),
                },
                ..RunOptions::default()
            },
        }
    }
    /// Отдельный вызов текущего test executable служит fake local command без shell.
    #[test]
    fn fake_local_command() {
        let Ok(mode) = std::env::var("ANKI_EXECUTION_FAKE_MODE") else {
            return;
        };
        match mode.as_str() {
            "pass" => {
                println!("проверка выполнена");
                eprintln!("диагностика");
            }
            "fail" => {
                eprintln!("проверка упала");
                std::process::exit(7);
            }
            "sleep" => {
                println!("готов к отмене");
                thread::sleep(Duration::from_secs(20));
            }
            "surfaces" => {
                for variable in PRIVATE_ENV {
                    println!("{variable}={}", std::env::var(variable).unwrap());
                }
                for variable in ["CARGO_TARGET_DIR", "TMPDIR", "HOME", "XDG_CONFIG_HOME"] {
                    fs::write(
                        PathBuf::from(std::env::var(variable).unwrap()).join("same-binary-name"),
                        std::env::var("CARGO_TARGET_DIR").unwrap(),
                    )
                    .unwrap();
                }
                thread::sleep(Duration::from_millis(100));
            }
            "mutate" => fs::write("source.txt", "disposable mutation\n").unwrap(),
            "large" => println!("{}", "Ж".repeat(1000)),
            "echo_secret" => println!(
                "{}",
                std::env::var("SERVICE_API_TOKEN").expect("тестовый secret передан явно")
            ),
            "descendant" => {
                let program = std::env::current_exe().unwrap();
                let child = Command::new(program)
                    .args([
                        "--exact",
                        "code_review::execution::tests::fake_local_command",
                        "--nocapture",
                    ])
                    .env("ANKI_EXECUTION_FAKE_MODE", "sleep")
                    .spawn()
                    .unwrap();
                println!("child_pid={}", child.id());
                std::io::stdout().flush().unwrap();
                // Ownership потомка здесь проверяет внешний execution supervisor.
                std::mem::forget(child);
                thread::sleep(Duration::from_secs(20));
            }
            _ => panic!("неизвестный fake mode"),
        }
    }

    #[test]
    fn unique_jobs_share_source_identity_but_not_writable_paths() {
        let repo = Repo::new();
        let first = repo.prepare(ExecutionMode::IsolatedChecks);
        let second = repo.prepare(ExecutionMode::IsolatedChecks);
        assert_ne!(first.directory(), second.directory());
        assert_eq!(first.metadata.source, second.metadata.source);
        for surface in SURFACES {
            assert_ne!(
                first.directory.join(surface),
                second.directory.join(surface)
            );
        }
        assert_eq!(
            fs::read(first.worktree().join("source.txt")).unwrap(),
            b"pinned source\n"
        );
        assert!(
            first
                .directory()
                .to_string_lossy()
                .contains(&repo.pack.target.head_sha)
        );
        assert_eq!(
            inspect_job(first.directory()).unwrap().lifecycle,
            LifecycleStatus::Prepared
        );
    }

    #[test]
    fn prepared_cleanup_removes_only_owned_runtime_and_keeps_manifest() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let worktree = job.worktree();
        let job_dir = job.directory().to_path_buf();
        let result = cleanup_workspace(&job).unwrap();
        assert!(result.workspace_removed);
        assert!(result.evidence_retained);
        assert!(!worktree.exists());
        assert!(job_dir.join("job.json").is_file());
        assert!(job_dir.join("source-review.json").is_file());
        let inspection = inspect_job(&job_dir).unwrap();
        assert_eq!(inspection.lifecycle, LifecycleStatus::Prepared);
        assert!(inspection.workspace_removed);
        assert!(run_job(&job, &request("pass"), &AtomicBool::new(false)).is_err());
    }

    fn prepare_source(
        repo: &Repo,
        source_pack: PathBuf,
        pr_number: Option<&str>,
    ) -> Result<PreparedJob, DomainError> {
        prepare_job(
            &repo.root,
            &repo.pack,
            &repo.bytes,
            PrepareOptions {
                mode: ExecutionMode::IsolatedChecks,
                scope: "namespace-contract".into(),
                source_pack,
                pr_number: pr_number.map(str::to_owned),
            },
        )
    }
    fn assert_no_runtime(repo: &Repo) {
        assert!(!repo.root.join(".git/worktrees").exists());
        for namespace in ["local", "17", "18"] {
            assert!(
                !repo
                    .root
                    .join(".anki-repo/review")
                    .join(namespace)
                    .join(&repo.pack.target.head_sha)
                    .join("runs")
                    .exists()
            );
        }
    }

    #[test]
    fn preparation_inherits_local_and_pr_from_source_workspace() {
        for (namespace, assertion) in [("local", None), ("17", None), ("17", Some("17"))] {
            let repo = Repo::new();
            let source = repo.store_pack(namespace, &repo.pack, &repo.bytes);
            let job = prepare_source(&repo, source, assertion).unwrap();
            assert_eq!(job.metadata.namespace, namespace);
            assert_eq!(
                job.directory.parent().unwrap(),
                repo.root
                    .join(".anki-repo/review")
                    .join(namespace)
                    .join(&repo.pack.target.head_sha)
                    .join("runs")
            );
            assert_eq!(
                fs::read(job.directory.join("source-review.json")).unwrap(),
                repo.bytes
            );
        }
    }

    #[test]
    fn namespace_assertions_fail_before_runtime_side_effects() {
        for (namespace, assertion) in [
            ("local", "17"),
            ("17", "18"),
            ("17", "local"),
            ("17", "017"),
        ] {
            let repo = Repo::new();
            let source = repo.store_pack(namespace, &repo.pack, &repo.bytes);
            let error = prepare_source(&repo, source, Some(assertion)).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert_no_runtime(&repo);
        }
    }

    #[test]
    fn noncanonical_source_paths_fail_before_runtime_side_effects() {
        let repo = Repo::new();
        let head = &repo.pack.target.head_sha;
        let paths = [
            repo.root.join("review.json"),
            repo.root
                .join(format!(".anki-repo/review/017/{head}/review.json")),
            repo.root.join(format!(
                ".anki-repo/review/local/{}/review.json",
                &head[..12]
            )),
            repo.root.join(format!(
                ".anki-repo/review/local/{}/review.json",
                "0".repeat(head.len())
            )),
            repo.root
                .join(format!(".anki-repo/review/local/{head}/queue.json")),
            repo.root.join(format!(
                ".anki-repo/review/local/../local/{head}/review.json"
            )),
            repo.root
                .join(format!(".anki-repo/review/local/./{head}/review.json")),
            repo.root
                .join(format!(".anki-repo/review//local/{head}/review.json")),
        ];
        for source in paths {
            let error = prepare_source(&repo, source, None).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert_no_runtime(&repo);
        }
    }

    #[test]
    fn symlink_source_pack_or_workspace_fails_before_runtime_side_effects() {
        use std::os::unix::fs::symlink;
        for directory_link in [false, true] {
            let repo = Repo::new();
            let source = repo.source_path("local");
            if directory_link {
                let workspace = source.parent().unwrap();
                let moved = repo.root.join("evidence-directory");
                fs::rename(workspace, &moved).unwrap();
                symlink(&moved, workspace).unwrap();
            } else {
                let moved = repo.root.join("evidence.json");
                fs::rename(&source, &moved).unwrap();
                symlink(&moved, &source).unwrap();
            }
            let error = prepare_source(&repo, source, None).unwrap_err();
            assert!(matches!(
                error.code,
                ErrorCode::InvalidRequest | ErrorCode::ReviewArtifactConflict
            ));
            assert_no_runtime(&repo);
        }
    }

    #[test]
    fn source_bytes_are_verified_against_existing_canonical_evidence() {
        let repo = Repo::new();
        let source = repo.source_path("local");
        fs::write(&source, b"unrelated evidence").unwrap();
        let error = prepare_source(&repo, source, None).unwrap_err();
        assert_eq!(error.code, ErrorCode::ReviewArtifactInvalid);
        assert_no_runtime(&repo);
    }

    #[test]
    fn preparation_ignores_source_checkout_filters_and_hooks() {
        let repo = Repo::new();
        fs::write(
            repo.root.join(".gitattributes"),
            "source.txt filter=probe\n",
        )
        .unwrap();
        git(&repo.root, &["add", ".gitattributes"]);
        git(
            &repo.root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "Фильтр для проверки",
            ],
        );
        git(
            &repo.root,
            &["config", "filter.probe.smudge", "invalid-source-program"],
        );
        git(&repo.root, &["config", "filter.probe.required", "true"]);
        let head = String::from_utf8(git(&repo.root, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        let mut pack = repo.pack.clone();
        pack.target = super::super::scope::collect_scope(&repo.root, &head, &head)
            .unwrap()
            .target;
        pack.scope.merge_base_sha = pack.target.merge_base_sha.clone();
        let bytes = serde_json::to_vec(&pack).unwrap();
        let job = prepare_job(
            &repo.root,
            &pack,
            &bytes,
            PrepareOptions {
                mode: ExecutionMode::IsolatedChecks,
                scope: "filters".into(),
                source_pack: repo.store_pack("local", &pack, &bytes),
                pr_number: None,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read(job.worktree().join("source.txt")).unwrap(),
            b"pinned source\n"
        );
    }

    #[test]
    fn concurrent_jobs_cannot_replace_same_named_neighbor_outputs() {
        let repo = Repo::new();
        let first = repo.prepare(ExecutionMode::IsolatedChecks);
        let second = repo.prepare(ExecutionMode::IsolatedChecks);
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = [first.clone(), second.clone()]
            .into_iter()
            .map(|job| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    run_job(&job, &request("surfaces"), &AtomicBool::new(false)).unwrap()
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            assert_eq!(handle.join().unwrap().status, ExecutionStatus::Passed);
        }
        for job in [&first, &second] {
            for surface in ["target", "tmp", "home", "config"] {
                assert_eq!(
                    fs::read_to_string(job.directory.join(surface).join("same-binary-name"))
                        .unwrap(),
                    job.directory.join("target").to_string_lossy()
                );
            }
        }
    }

    #[test]
    fn normal_failure_unavailable_and_bounded_results_are_distinct() {
        let repo = Repo::new();
        for (mode, expected) in [
            ("pass", ExecutionStatus::Passed),
            ("fail", ExecutionStatus::Failed),
        ] {
            let job = repo.prepare(ExecutionMode::IsolatedChecks);
            let result = run_job(&job, &request(mode), &AtomicBool::new(false)).unwrap();
            assert_eq!(result.status, expected);
            let stored = read_result(job.directory()).unwrap();
            assert_eq!(stored.status, result.status);
            assert_eq!(stored.argv_sha256, result.argv_sha256);
            assert!(run_job(&job, &request(mode), &AtomicBool::new(false)).is_err());
            assert_eq!(result.enforcement.security_sandbox, "absent");
        }
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let mut command = request("pass");
        command.argv = vec!["anki-execution-nonexistent-command".into()];
        assert_eq!(
            run_job(&job, &command, &AtomicBool::new(false))
                .unwrap()
                .status,
            ExecutionStatus::Unavailable
        );
        let cleanup = cleanup_workspace(&job).unwrap();
        assert!(cleanup.workspace_removed);
        assert!(job.directory.join("result.json").is_file());
        assert!(read_result(job.directory()).is_ok());
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let mut command = request("large");
        command.options.output_limit_bytes = 53;
        let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
        assert!(result.stdout.truncated);
        assert!(result.stdout.total_bytes > 1000);
        assert!(
            fs::metadata(job.directory.join(&result.stdout.log))
                .unwrap()
                .len()
                > 1000
        );
    }

    #[test]
    fn regression_reflected_argv_secrets_are_redacted_in_both_streams() {
        let repo = Repo::new();
        for arguments in [
            vec!["--api-key=argv-secret"],
            vec!["--api-key", "argv-secret"],
        ] {
            let job = repo.prepare(ExecutionMode::IsolatedChecks);
            let command = CommandRequest {
                argv: [
                    vec![
                        "sh",
                        "-c",
                        "printf '%s\\n' \"$@\"; printf '%s\\n' \"$@\" >&2",
                        "fixture",
                    ],
                    arguments,
                ]
                .concat()
                .into_iter()
                .map(str::to_owned)
                .collect(),
                cwd: ".".into(),
                options: RunOptions::default(),
            };
            let original = command.argv.clone();
            let digest = argv_digest(&original);
            let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
            assert!(!result.stdout.text.contains("argv-secret"));
            assert!(!result.stderr.text.contains("argv-secret"));
            assert_eq!(command.argv, original);
            assert_eq!(result.request.argv, original);
            assert_eq!(result.argv_sha256, digest);
            assert_eq!(read_result(job.directory()).unwrap().argv_sha256, digest);
        }
    }

    #[test]
    fn argv_digest_does_not_depend_on_recognized_secret_values() {
        let first = ["tool", "--password", "low-entropy-first"]
            .map(str::to_owned)
            .to_vec();
        let second = ["tool", "--password", "low-entropy-second"]
            .map(str::to_owned)
            .to_vec();

        assert_eq!(argv_digest(&first), argv_digest(&second));
        assert_eq!(argv_digest(&first), argv_digest(&safe_argv(&first)));
    }

    #[test]
    fn regression_short_secrets_mark_final_text_truncated() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        fs::write(job.directory.join("logs/stdout.log"), "x x x").unwrap();
        let result = output_evidence(&job, "stdout.log", 5, &["x".into()]).unwrap();
        assert!(result.truncated);
        assert_eq!(result.total_bytes, 5);
        assert_eq!(result.text, "<reda");
        assert!(!result.utf8_lossy);
    }

    #[test]
    fn regression_regular_tokenizer_arguments_stay_readable() {
        let argv = [
            "tool",
            "src/tokenizer.rs",
            "--detector",
            "tokenizer",
            "--secretary=public",
            "token",
            "password",
        ]
        .map(str::to_owned);
        assert_eq!(safe_argv(&argv), argv);
    }

    #[test]
    fn reflected_secret_crossing_source_limit_never_exposes_prefix() {
        let repo = Repo::new();
        for (arguments, expected) in [
            (vec!["--api-key=boundary-secret"], "--api-key="),
            (vec!["--api-key", "boundary-secret"], "--api-key\n"),
        ] {
            let job = repo.prepare(ExecutionMode::IsolatedChecks);
            let command = CommandRequest {
                argv: [
                    vec![
                        "sh",
                        "-c",
                        "printf '%s\\n' \"$@\"; printf '%s\\n' \"$@\" >&2",
                        "fixture",
                    ],
                    arguments,
                ]
                .concat()
                .into_iter()
                .map(str::to_owned)
                .collect(),
                cwd: ".".into(),
                options: RunOptions {
                    output_limit_bytes: 14,
                    ..RunOptions::default()
                },
            };
            let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
            for evidence in [&result.stdout, &result.stderr] {
                assert_eq!(evidence.text, expected);
                assert!(evidence.truncated);
                assert!(!evidence.text.contains("boun"));
                assert!(
                    fs::read_to_string(job.directory.join(&evidence.log))
                        .unwrap()
                        .contains("boundary-secret")
                );
            }
            assert_eq!(result.argv_sha256, argv_digest(&command.argv));
        }
    }

    #[test]
    fn sensitive_argument_forms_share_one_precise_parser() {
        let mut command = request("pass");
        command.argv = [
            "secret-tool",
            "--token",
            "first",
            "--api-key=ключ",
            "--password",
            "",
            "-H",
            "Authorization: Bearer auth-value",
            "--header=Cookie: session=value",
            "-HProxy-Authorization: Basic encoded=",
            "X-Api-Key: header-key",
            "SERVICE_API_TOKEN=env-assignment",
            "--detector",
            "tokenizer",
            "--header",
            "X-Tokenizer: public",
            "--",
            "--password",
            "ordinary-positional",
        ]
        .map(str::to_owned)
        .into();
        command.options.environment = EnvironmentPolicy::Explicit {
            values: BTreeMap::from([
                ("SERVICE_API_TOKEN".into(), "env-value".into()),
                ("SERVICE_API_KEY".into(), "key-value".into()),
                ("TOKENIZER_PATH".into(), "src/tokenizer.rs".into()),
                ("SECRETARY_NAME".into(), "public-name".into()),
                ("PASSWORD".into(), "".into()),
            ]),
        };
        let original = command.argv.clone();
        let safe = safe_argv(&original);
        assert_eq!(safe[0], "secret-tool");
        assert_eq!(safe[1], "--token");
        assert_eq!(safe[2], "<redacted>");
        assert_eq!(safe[3], "--api-key=<redacted>");
        assert_eq!(safe[5], "<redacted>");
        assert_eq!(safe[7], "Authorization: <redacted>");
        assert_eq!(safe[8], "--header=Cookie: <redacted>");
        assert_eq!(safe[9], "-HProxy-Authorization: <redacted>");
        assert_eq!(safe[10], "X-Api-Key: <redacted>");
        assert_eq!(safe[11], "SERVICE_API_TOKEN=<redacted>");
        assert_eq!(&safe[12..], &original[12..]);
        assert_eq!(
            sensitive_request_values(&command)
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [
                "first",
                "ключ",
                "Bearer auth-value",
                "auth-value",
                "session=value",
                "Basic encoded=",
                "encoded=",
                "header-key",
                "env-assignment",
                "env-value",
                "key-value"
            ]
            .map(str::to_owned)
            .into_iter()
            .collect()
        );
        assert_eq!(command.argv, original);
    }

    #[test]
    fn output_redaction_handles_unicode_overlaps_and_source_boundaries() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        for (source, limit, secrets, expected, truncated, lossy) in [
            (
                "abcdef!".as_bytes(),
                64,
                vec!["abc", "bcdef"],
                "<redacted>!",
                false,
                false,
            ),
            (
                "visible:abcdef tail".as_bytes(),
                12,
                vec!["abc", "bcdef"],
                "visible:",
                true,
                false,
            ),
            (
                "口:秘密 tail".as_bytes(),
                8,
                vec!["秘密"],
                "口:",
                true,
                false,
            ),
            (
                "口:秘密!".as_bytes(),
                64,
                vec!["秘密"],
                "口:<redacted>!",
                false,
                false,
            ),
            ("éé".as_bytes(), 3, vec![], "é", true, true),
            ("é x".as_bytes(), 4, vec!["x"], "é <", true, false),
            ("".as_bytes(), 0, vec![""], "", false, false),
            ("plain".as_bytes(), 0, vec!["plain"], "", true, false),
            (
                b"\xffsecret!",
                64,
                vec!["secret"],
                "�<redacted>!",
                false,
                true,
            ),
        ] {
            fs::write(job.directory.join("logs/stdout.log"), source).unwrap();
            let values = secrets.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let evidence = output_evidence(&job, "stdout.log", limit, &values).unwrap();
            assert_eq!(evidence.text, expected, "исходные байты: {source:?}");
            assert_eq!(evidence.total_bytes, source.len() as u64);
            assert_eq!(evidence.truncated, truncated);
            assert_eq!(evidence.utf8_lossy, lossy);
            assert!(evidence.text.len() <= limit);
            assert_eq!(
                fs::read(job.directory.join("logs/stdout.log")).unwrap(),
                source
            );
        }
    }

    #[test]
    fn reflected_header_and_unicode_secrets_preserve_raw_logs_and_digest() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let command = CommandRequest {
            argv: [
                "sh",
                "-c",
                "printf '%s\\n' \"$@\"; printf '%s\\n' \"$@\" >&2",
                "fixture",
                "--password",
                "長い秘密",
                "--api-key=short-key",
                "-H",
                "Authorization: Bearer auth-secret",
                "--detector",
                "tokenizer",
                "src/tokenizer.rs",
            ]
            .map(str::to_owned)
            .into(),
            cwd: ".".into(),
            options: RunOptions::default(),
        };
        let digest = argv_digest(&command.argv);
        let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
        for evidence in [&result.stdout, &result.stderr] {
            for secret in ["長い秘密", "short-key", "auth-secret"] {
                assert!(!evidence.text.contains(secret));
                assert!(
                    fs::read_to_string(job.directory.join(&evidence.log))
                        .unwrap()
                        .contains(secret)
                );
            }
            assert!(evidence.text.contains("tokenizer"));
            assert!(evidence.text.contains("src/tokenizer.rs"));
            assert!(!evidence.truncated);
        }
        assert_eq!(result.argv_sha256, digest);
        assert_eq!(result.request.argv, command.argv);
        let persisted = fs::read_to_string(job.directory.join("result.json")).unwrap();
        for secret in ["長い秘密", "short-key", "auth-secret"] {
            assert!(!persisted.contains(secret));
        }
        assert_eq!(read_result(job.directory()).unwrap().argv_sha256, digest);
    }

    #[test]
    fn structured_result_redacts_sensitive_arguments_and_environment_values() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let mut command = request("echo_secret");
        if let EnvironmentPolicy::Explicit { values } = &mut command.options.environment {
            values.insert("SERVICE_API_TOKEN".into(), "local-secret-value".into());
        }
        let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("local-secret-value"));
        assert!(serialized.contains("<redacted>"));
        assert!(!result.stdout.text.contains("local-secret-value"));
        assert!(result.stdout.text.contains("<redacted>"));
        assert_eq!(
            read_result(job.directory()).unwrap().argv_sha256,
            result.argv_sha256
        );
        assert_eq!(
            safe_argv(&[
                "cargo".into(),
                "--api-token".into(),
                "plain-secret-value".into(),
                "--mode=test".into(),
            ]),
            vec!["cargo", "--api-token", "<redacted>", "--mode=test",]
        );
    }

    #[test]
    fn output_redaction_drops_sensitive_values_crossing_the_limit() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let logs = job.directory.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let source = b"visible:local-secret-value tail";
        fs::write(logs.join("stdout.log"), source).unwrap();

        let secret = "local-secret-value".to_owned();
        let evidence = output_evidence(&job, "stdout.log", 18, &[secret]).unwrap();

        assert_eq!(evidence.text, "visible:");
        assert!(!evidence.text.contains("loc"));
        assert_eq!(evidence.total_bytes, source.len() as u64);
        assert!(evidence.truncated);
        assert!(evidence.text.len() <= 18);
    }

    #[test]
    fn output_redaction_does_not_include_bytes_past_the_original_limit() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let logs = job.directory.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let source = b"token=local-secret-value; boundary=DO_NOT_INCLUDE";
        fs::write(logs.join("stdout.log"), source).unwrap();

        let secret = "local-secret-value".to_owned();
        let limit = 30;
        let evidence =
            output_evidence(&job, "stdout.log", limit, std::slice::from_ref(&secret)).unwrap();
        let expected = String::from_utf8_lossy(&source[..limit]).replace(&secret, "<redacted>");

        assert_eq!(evidence.text, expected);
        assert!(!evidence.text.contains("boundary="));
        assert!(evidence.truncated);
    }

    #[test]
    fn timeout_cancel_and_neighbors_preserve_evidence() {
        let repo = Repo::new();
        let timed = repo.prepare(ExecutionMode::IsolatedChecks);
        let mut command = request("sleep");
        command.options.timeout_ms = Some(50);
        let result = run_job(&timed, &command, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.status, ExecutionStatus::TimedOut);
        assert_eq!(
            result.enforcement.process_cleanup,
            "process_group_killed_partial"
        );
        assert!(!cleanup_workspace(&timed).unwrap().workspace_removed);
        let cancelled = repo.prepare(ExecutionMode::IsolatedChecks);
        let child_job = cancelled.clone();
        let handle = thread::spawn(move || {
            run_job(&child_job, &request("sleep"), &AtomicBool::new(false)).unwrap()
        });
        let start = Instant::now();
        while inspect_job(cancelled.directory()).unwrap().lifecycle != LifecycleStatus::Running {
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        }
        assert!(cleanup_workspace(&cancelled).is_err());
        request_cancel(cancelled.directory()).unwrap();
        assert_eq!(handle.join().unwrap().status, ExecutionStatus::Cancelled);
        let neighbor = repo.prepare(ExecutionMode::IsolatedChecks);
        assert_eq!(
            run_job(&neighbor, &request("pass"), &AtomicBool::new(false))
                .unwrap()
                .status,
            ExecutionStatus::Passed
        );
        assert!(timed.directory.join("result.json").is_file());
    }

    #[test]
    fn disposable_mutation_and_user_changes_do_not_change_pinned_neighbor() {
        let repo = Repo::new();
        let isolated = repo.prepare(ExecutionMode::IsolatedChecks);
        let disposable = repo.prepare(ExecutionMode::DisposableSourceExperiment);
        fs::write(repo.root.join("source.txt"), "user unstaged contents\n").unwrap();
        let before = git(
            &repo.root,
            &["status", "--porcelain", "--untracked-files=no"],
        );
        assert_eq!(
            run_job(&disposable, &request("mutate"), &AtomicBool::new(false))
                .unwrap()
                .status,
            ExecutionStatus::Passed
        );
        assert_eq!(
            fs::read(isolated.worktree().join("source.txt")).unwrap(),
            b"pinned source\n"
        );
        assert_eq!(
            fs::read(repo.root.join("source.txt")).unwrap(),
            b"user unstaged contents\n"
        );
        assert_eq!(
            git(
                &repo.root,
                &["status", "--porcelain", "--untracked-files=no"]
            ),
            before
        );
        assert_eq!(
            run_job(&isolated, &request("mutate"), &AtomicBool::new(false))
                .unwrap()
                .status,
            ExecutionStatus::Incomplete
        );
    }

    #[test]
    fn argv_does_not_implicitly_invoke_a_shell() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let literal = "$(touch shell-created) ; `touch shell-created` > shell-created";
        let command = CommandRequest {
            argv: vec!["printf".into(), "%s".into(), literal.into()],
            cwd: ".".into(),
            options: RunOptions::default(),
        };
        let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.status, ExecutionStatus::Passed);
        assert_eq!(result.stdout.text, literal);
        assert!(!job.worktree().join("shell-created").exists());
        assert_eq!(result.request.argv, command.argv);
    }

    #[test]
    fn ownership_symlink_alias_and_interrupted_cleanup_fail_closed() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        assert!(open_job(&repo.root, &job.directory.join("..")).is_err());
        let alias = repo.root.join("alias");
        symlink(&job.directory, &alias).unwrap();
        assert!(inspect_job(&alias).is_err());
        let unrelated = repo.root.join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        assert!(request_cancel(&unrelated).is_err());
        assert!(!unrelated.join("cancel.json").exists());
        fs::remove_dir(job.directory.join("tmp")).unwrap();
        symlink(&unrelated, job.directory.join("tmp")).unwrap();
        assert!(run_job(&job, &request("pass"), &AtomicBool::new(false)).is_err());
        assert_eq!(
            inspect_job(job.directory()).unwrap().lifecycle,
            LifecycleStatus::Prepared
        );
        assert!(cleanup_workspace(&job).is_err());
        assert!(unrelated.is_dir());
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        save_state(&job, LifecycleStatus::Running, false).unwrap();
        // Соседний spawn может кратко унаследовать дескриптор CLOEXEC до exec.
        // В этом окне flock действительно активен; ждём его освобождения,
        // сохраняя проверку того, что прерванное задание распознаётся.
        let deadline = Instant::now() + Duration::from_secs(2);
        while inspect_job(job.directory()).unwrap().lifecycle != LifecycleStatus::Interrupted {
            assert!(
                Instant::now() < deadline,
                "Блокировка прерванного задания осталась активной"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(read_result(job.directory()).is_err());
        assert!(cleanup_workspace(&job).is_err());
    }

    #[test]
    fn pinned_cleanup_never_follows_replaced_job_parent() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let owned = repo.prepare(ExecutionMode::DisposableSourceExperiment);
        let neighbor = repo.prepare(ExecutionMode::DisposableSourceExperiment);
        let directory = safe_dir(owned.directory()).unwrap();
        let surfaces = cleanup_preflight(&owned, &directory).unwrap();
        fs::write(owned.directory.join("target/owned-marker"), b"owned").unwrap();
        fs::write(neighbor.directory.join("target/foreign-marker"), b"foreign").unwrap();
        let external_source = fs::read(neighbor.worktree().join("source.txt")).unwrap();
        let moved = owned.directory.with_extension("moved");
        fs::rename(owned.directory(), &moved).unwrap();
        symlink(neighbor.directory(), owned.directory()).unwrap();

        // Даже перечисление после подмены читает исходный закреплённый каталог.
        cleanup_preflight(&owned, &directory).unwrap();
        remove_cleanup_runtime(&directory, "target", &surfaces).unwrap();
        assert!(!moved.join("target").exists());
        assert_eq!(
            fs::read(neighbor.directory.join("target/foreign-marker")).unwrap(),
            b"foreign"
        );
        // Git получает дескриптор исходного worktree. После перемещения
        // регистрация не совпадает: ожидается отказ, сосед не удаляется.
        assert!(remove_cleanup_worktree(&owned, &directory, &surfaces).is_err());
        assert_eq!(
            fs::read(neighbor.worktree().join("source.txt")).unwrap(),
            external_source
        );
        assert!(neighbor.worktree().is_dir());
        fs::remove_file(owned.directory()).unwrap();
        fs::rename(moved, owned.directory()).unwrap();
        cleanup_workspace(&owned).unwrap();
        cleanup_workspace(&neighbor).unwrap();
    }

    #[test]
    fn process_group_timeout_stops_regular_descendants() {
        let repo = Repo::new();
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        let mut command = request("descendant");
        command.options.timeout_ms = Some(200);
        let result = run_job(&job, &command, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.status, ExecutionStatus::TimedOut);
        let pid: i32 = result
            .stdout
            .text
            .lines()
            .find_map(|line| line.strip_prefix("child_pid="))
            .unwrap_or_else(|| panic!("нет PID потомка в stdout: {:?}", result.stdout.text))
            .parse()
            .unwrap();
        let process_stat = PathBuf::from(format!("/proc/{pid}/stat"));
        // Завершённый потомок может быть zombie до reap внешним init.
        let stopped = || {
            fs::read_to_string(&process_stat).map_or(true, |stat| {
                stat.split(") ")
                    .nth(1)
                    .is_some_and(|state| state.starts_with('Z'))
            })
        };
        let start = Instant::now();
        while !stopped() {
            assert!(start.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            result
                .enforcement
                .limitations
                .iter()
                .any(|text| text.contains("групп"))
        );
    }

    #[test]
    fn stable_message_hides_descriptor_and_workspace_paths() {
        let directory = Path::new("/корень/задания/текущее");
        let message = format!(
            "fatal: '{}' is not a working tree; /proc/4242/fd/17 недоступен",
            directory.display()
        );
        let stable = stable_message(&message, &[directory]);
        assert!(stable.contains("<путь задания>"), "{stable}");
        assert!(
            stable.contains("<закреплённый дескриптор задания>"),
            "{stable}"
        );
        assert!(!stable.contains("/proc/"), "{stable}");
        assert!(!stable.contains("4242"), "{stable}");
        assert!(!stable.contains("/корень"), "{stable}");
        // Сообщение без локальных путей остаётся дословным.
        assert_eq!(
            stable_message("fatal: not a git repository", &[]),
            "fatal: not a git repository"
        );
    }
}
