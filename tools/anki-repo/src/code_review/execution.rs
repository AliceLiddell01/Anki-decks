//! Явное исполнение проверок в отдельной рабочей копии точного Git-снимка.
//!
//! Это изоляция ресурсов сотрудничающих исполнителей, а не security sandbox:
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
}

#[derive(Debug, Clone)]
pub struct PrepareOptions {
    pub mode: ExecutionMode,
    pub scope: String,
    /// Канонический исходный review.json; относительный путь считается от root.
    pub source_pack: PathBuf,
    /// Необязательное утверждение номера PR; namespace наследуется из source_pack.
    pub pr_number: Option<String>,
}

/// Значения переменных не наследуются без явного выбора политики.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentPolicy {
    /// PATH и местоположение rustup; секреты, Git overrides и Cargo flags не наследуются.
    #[default]
    Minimal,
    /// Явно переданные значения; job-private переменные остаются обязательными.
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
    /// Верхняя граница конкурентных job в данном репозитории, 1..=64.
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
    /// Уже разобранные argv; shell не добавляется. Shell возможен только как явный argv[0].
    #[serde(serialize_with = "serialize_safe_argv")]
    pub argv: Vec<String>,
    /// Переносимый путь от корня job worktree; "." означает сам worktree.
    pub cwd: String,
    pub options: RunOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
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
    /// Относительно job directory, полное содержимое хранится только здесь.
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
                "Изоляция source обеспечена отдельным detached worktree; запрещённые записи вне job не блокируются ОС.".into(),
                "Git objects и metadata общие; проверяемому коду не запрещён прямой доступ к ним.".into(),
                "Потомки могут уйти из процессной группы; их завершение не гарантируется.".into(),
                "Полные логи хранятся на диске без лимита; ограничен только вывод внутри JSON.".into(),
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
    /// Digest фактического argv до безопасного представления в JSON result.
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
    /// Случайный nonce подтверждает владение directory; он не является секретом.
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

/// Handle нельзя конструировать с произвольным путем; open_job проверяет identity.
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

/// Проверяет identity исходного evidence до любых операций с runtime-каталогами.
fn source_pack_namespace(
    root: &Path,
    pack: &ReviewPack,
    bytes: &[u8],
    options: &PrepareOptions,
) -> Result<String, DomainError> {
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
            "alias path, . и .. в исходном review.json запрещены",
        ));
    }
    let source_path = if options.source_pack.is_absolute() {
        options.source_pack.clone()
    } else {
        root.join(&options.source_pack)
    };
    let relative = source_path.strip_prefix(root).map_err(|_| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "исходный review.json вне канонического repository workspace",
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    let [
        Component::Normal(owner),
        Component::Normal(review),
        Component::Normal(namespace),
        Component::Normal(head),
        Component::Normal(filename),
    ] = components.as_slice()
    else {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "исходный пакет должен быть .anki-repo/review/<PR|local>/<full-head-sha>/review.json",
        ));
    };
    if *owner != ".anki-repo" || *review != "review" || *filename != "review.json" {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "исходный пакет должен быть каноническим review.json",
        ));
    }
    let namespace = namespace.to_str().ok_or_else(|| {
        DomainError::new(ErrorCode::InvalidRequest, "namespace не является UTF-8")
    })?;
    if namespace != "local"
        && namespace.parse::<u64>().map_or(true, |number| {
            number == 0 || number.to_string() != namespace
        })
    {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "namespace должен быть local или каноническим положительным номером PR",
        ));
    }
    if let Some(number) = &options.pr_number {
        if number
            .parse::<u64>()
            .map_or(true, |value| value == 0 || value.to_string() != *number)
        {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "pr_number должен быть каноническим положительным номером PR",
            ));
        }
        if number != namespace {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                "номер PR не совпадает с namespace исходного review.json",
            ));
        }
    }
    if head.to_str() != Some(pack.target.head_sha.as_str()) {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "HEAD каталога исходного review.json не совпадает с source pack",
        ));
    }
    let canonical = root
        .join(".anki-repo/review")
        .join(namespace)
        .join(&pack.target.head_sha)
        .join("review.json");
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
    Ok(namespace.to_owned())
}

/// Создаёт unique job и точный detached worktree; код проекта не запускается.
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
        return Err(invalid("scope job не может быть пустым"));
    }
    for sha in [
        &pack.target.head_sha,
        &pack.target.base_sha,
        &pack.target.merge_base_sha,
    ] {
        validate_sha(sha)?;
    }
    let root = root.canonicalize().map_err(io_error)?;
    safe_dir(&root)?;
    let namespace = source_pack_namespace(&root, pack, bytes, &options)?;
    verify_snapshot(&root, &pack.target)?;
    let parent = root
        .join(".anki-repo/review")
        .join(&namespace)
        .join(&pack.target.head_sha)
        .join("runs");
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
        source: ExecutionSource {
            snapshot: pack.target.clone(),
            review_pack_sha256: format!("{:x}", Sha256::digest(bytes)),
        },
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
    let initialized: Result<(), DomainError> = (|| {
        write_document(&job.directory, "job.json", &job.metadata, false)?;
        write_new(&job.directory, "source-review.json", bytes)?;
        let _lock = job_lock(&job.directory)?;
        save_state(&job, LifecycleStatus::Prepared, false)
    })();
    if let Err(error) = initialized {
        if let Err(cleanup_error) = remove_partial_job_directory(
            &job.root
                .join(".anki-repo/review")
                .join(&job.metadata.namespace)
                .join(&job.metadata.source.snapshot.head_sha)
                .join("runs"),
            &job.metadata.job_id,
        ) {
            return Err(DomainError::with_details(
                error.code,
                format!(
                    "{}; не удалось удалить частично созданный job: {}",
                    error.message, cleanup_error.message
                ),
                crate::details! { "job_dir" => job.directory.display().to_string(), "job_id" => job.metadata.job_id },
            ));
        }
        return Err(error);
    }
    let preparation: Result<(), DomainError> = (|| {
        for surface in SURFACES {
            ensure_dir(&job.directory.join(surface))?;
        }
        let mut command = trusted_git(&job.root, &job.directory.join("hooks"))?;
        command.args(["worktree", "add", "--detach", "--no-checkout"]);
        command
            .arg(job.worktree())
            .arg(&job.metadata.source.snapshot.head_sha);
        git_success(command, "создание detached worktree")?;
        let mut command = trusted_git(&job.worktree(), &job.directory.join("hooks"))?;
        command.args([
            "checkout",
            "--detach",
            &job.metadata.source.snapshot.head_sha,
        ]);
        git_success(command, "извлечение точного source")?;
        Ok(())
    })();
    if let Err(error) = preparation {
        save_state(&job, LifecycleStatus::PreparationFailed, false)?;
        return Err(DomainError::with_details(
            error.code,
            error.message,
            crate::details! { "job_dir" => job.directory.display().to_string(), "job_id" => job.metadata.job_id },
        ));
    }
    Ok(job)
}

/// Открытие существующего job не исполняет код и не возобновляет незавершённый запуск.
pub fn open_job(root: &Path, directory: &Path) -> Result<PreparedJob, DomainError> {
    platform_supported()?;
    let root = root.canonicalize().map_err(io_error)?;
    let directory = absolute_path(directory)?;
    safe_dir(&directory)?;
    let metadata: JobMetadata = read_document(&directory, "job.json")?;
    let expected = root
        .join(".anki-repo/review")
        .join(&metadata.namespace)
        .join(&metadata.source.snapshot.head_sha)
        .join("runs")
        .join(&metadata.job_id);
    valid_component(&metadata.namespace)?;
    valid_component(&metadata.job_id)?;
    if metadata.namespace != "local"
        && (!metadata.namespace.bytes().all(|byte| byte.is_ascii_digit())
            || metadata.namespace.parse::<u64>().map_or(true, |number| {
                number == 0 || number.to_string() != metadata.namespace
            }))
    {
        return Err(invalid(
            "namespace job должен быть local или положительным номером PR",
        ));
    }
    validate_hex(&metadata.job_id, 32, "job ID")?;
    validate_hex(&metadata.owner_nonce, 32, "owner nonce")?;
    validate_sha(&metadata.source.snapshot.head_sha)?;
    validate_sha(&metadata.source.snapshot.base_sha)?;
    validate_sha(&metadata.source.snapshot.merge_base_sha)?;
    validate_hex(
        &metadata.source.review_pack_sha256,
        64,
        "digest review pack",
    )?;
    if metadata.schema_version != EXECUTION_SCHEMA_VERSION
        || directory != expected
        || metadata.owner_nonce.len() != 32
    {
        return Err(invalid(
            "job path или ownership identity не соответствует manifest",
        ));
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

/// Исполняет один explicit argv; повторный запуск того же job запрещён.
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
        return Err(conflict(
            "job уже исполнялся, не готов или его workspace удалён",
        ));
    }
    let _slot = execution_slot(&job.root, request.options.max_parallel_jobs)?;
    let cwd = source_cwd(job, &request.cwd)?;
    verify_worktree_head(job)?;
    if job.metadata.mode == ExecutionMode::IsolatedChecks && source_changed(job)? {
        return Err(conflict(
            "isolated checks требует неизменённого pinned source; создайте новый job или disposable experiment",
        ));
    }
    save_state(job, LifecycleStatus::Running, false)?;
    let stdout = open_file(&safe_dir(&job.directory.join("logs"))?, "stdout.log", true)?;
    let stderr = open_file(&safe_dir(&job.directory.join("logs"))?, "stderr.log", true)?;
    let mut command = Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    configure_environment(&mut command, job, &request.options.environment)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let start = Instant::now();
    let mut enforcement = Enforcement::default();
    let mut failure = None;
    let mut exit = None;
    let cancelled = cancel.load(Ordering::SeqCst) || cancel_requested(job)?;
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
                // PID остаётся живым до kill группы: это защищает от повторного
                // назначения идентификатора, пока выполняется остановка owned группы.
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
                                // waitid(NOWAIT) оставляет leader waitable: его PID
                                // нельзя повторно назначить до kill owned группы.
                                stop_child(&mut child, &mut enforcement, &mut failure);
                                match child.wait() {
                                    Ok(waited) => exit = Some(exit_summary(waited)),
                                    Err(error) => {
                                        failure = Some(format!("reap после завершения: {error}"));
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
                            failure = Some(format!("не удалось reap процесс: {error}"));
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
        failure = Some("Проверка изменила source собственного worktree; isolated check не может считаться воспроизводимо завершённой.".into());
        if matches!(status, ExecutionStatus::Passed | ExecutionStatus::Failed) {
            ExecutionStatus::Incomplete
        } else {
            status
        }
    } else {
        status
    };
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
            &sensitive_environment_values(&request.options.environment),
        )?,
        stderr: output_evidence(
            job,
            "stderr.log",
            request.options.output_limit_bytes,
            &sensitive_environment_values(&request.options.environment),
        )?,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        enforcement,
        failure,
        cleanup: "evidence_and_workspace_retained; descendants_may_still_exist".into(),
    };
    write_document(&job.directory, "result.json", &result, false)?;
    save_state(job, LifecycleStatus::Completed, false)?;
    fs2::FileExt::unlock(&active_lock).map_err(io_error)?;
    Ok(result)
}

/// Cancel marker создаётся атомарно, независимо от удерживаемого active lock.
pub fn request_cancel(directory: &Path) -> Result<JobInspection, DomainError> {
    let inspection = inspect_job(directory)?;
    if inspection.lifecycle == LifecycleStatus::Prepared
        || inspection.lifecycle == LifecycleStatus::Running
    {
        let dir = safe_dir(&absolute_path(directory)?)?;
        match write_new_fd(&dir, "cancel.json", inspection.job.owner_nonce.as_bytes()) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(io_error(error)),
        }
    }
    inspect_job(directory)
}

/// Read-only observation: Running без живого lock означает Interrupted, а не success.
pub fn inspect_job(directory: &Path) -> Result<JobInspection, DomainError> {
    let directory = absolute_path(directory)?;
    let root = directory
        .ancestors()
        .nth(6)
        .ok_or_else(|| invalid("job вне repository review namespace"))?;
    let verified = open_job(root, &directory)?;
    let metadata = verified.metadata;
    let state: State = read_document(&directory, "state.json")?;
    if metadata.owner_nonce != state.owner_nonce {
        return Err(invalid("ownership manifest/state mismatch"));
    }
    let mut lifecycle = state.lifecycle;
    let mut limitations = Vec::new();
    if lifecycle == LifecycleStatus::Running && !lock_is_active(&directory)? {
        lifecycle = LifecycleStatus::Interrupted;
        limitations.push("Исполнитель не удерживает lock; потомки и полнота evidence не проверены, автоматическая очистка запрещена.".into());
    }
    let result = match read_document::<ExecutionResult>(&directory, "result.json") {
        Ok(result) => {
            validate_result(&metadata, &result)?;
            Some(result)
        }
        Err(error)
            if directory
                .join("result.json")
                .symlink_metadata()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            let _ = error;
            None
        }
        Err(error) => return Err(error),
    };
    if lifecycle == LifecycleStatus::Completed && result.is_none() {
        return Err(invalid("completed job не имеет typed result"));
    }
    Ok(JobInspection {
        job: metadata,
        lifecycle,
        result,
        workspace_removed: state.workspace_removed,
        limitations,
    })
}

/// Читает только завершённый result; не исполняет повторно и не меняет artifacts.
pub fn read_result(directory: &Path) -> Result<ExecutionResult, DomainError> {
    let inspection = inspect_job(directory)?;
    if inspection.lifecycle != LifecycleStatus::Completed {
        return Err(conflict("job не завершён; доступен только inspect"));
    }
    inspection
        .result
        .ok_or_else(|| invalid("отсутствует result.json"))
}

/// По умолчанию evidence и непроверенные процессы сохраняются.
/// Удаление runtime разрешено только для job, у которого процесс вообще не запускался.
/// Для executed job отсутствие escaped descendants невозможно доказать этими safe APIs.
pub fn cleanup_workspace(job: &PreparedJob) -> Result<CleanupResult, DomainError> {
    let _lock = job_lock(&job.directory)?;
    let state = verify_owner(job)?;
    if state.workspace_removed {
        return Ok(CleanupResult {
            job_id: job.metadata.job_id.clone(),
            workspace_removed: true,
            evidence_retained: true,
            limitation: None,
        });
    }
    let not_started = matches!(
        state.lifecycle,
        LifecycleStatus::Prepared | LifecycleStatus::PreparationFailed
    );
    if !not_started && state.lifecycle != LifecycleStatus::Completed {
        return Err(conflict("очистка active или interrupted job запрещена"));
    }
    if !not_started {
        let result = read_result(&job.directory)?;
        if result.exit.is_some() || result.enforcement.process_cleanup != "not_started" {
            return Ok(CleanupResult { job_id: job.metadata.job_id.clone(), workspace_removed: false, evidence_retained: true,
                limitation: Some("Workspace сохранён: невозможно доказать отсутствие потомков, покинувших группу. Завершение direct child не является разрешением удалить их ресурсы.".into()) });
        }
    }
    match fs::symlink_metadata(job.worktree()) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(conflict("owned worktree заменён symlink или не-каталогом"));
        }
        Ok(_) => {
            if state.lifecycle == LifecycleStatus::Prepared {
                verify_worktree_head(job)?;
            }
            safe_dir(&job.worktree())?;
            let mut command = trusted_git(&job.root, &job.directory.join("hooks"))?;
            command
                .args(["worktree", "remove", "--force"])
                .arg(job.worktree());
            if let Err(error) = git_success(command, "очистка owned worktree") {
                return Ok(CleanupResult {
                    job_id: job.metadata.job_id.clone(),
                    workspace_removed: false,
                    evidence_retained: true,
                    limitation: Some(error.message),
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(io_error(error)),
    }
    // Не удаляем job directory, логи, result и marker. Runtime без процессов
    // может быть удалён только после проверки каждого exact surface.
    for name in SURFACES.iter().filter(|name| **name != "logs") {
        let path = job.directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(conflict(
                    "owned runtime surface заменена symlink или не-каталогом",
                ));
            }
            Ok(_) => {
                safe_dir(&path)?;
                fs::remove_dir_all(&path).map_err(io_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_error(error)),
        }
    }
    save_state(job, state.lifecycle, true)?;
    Ok(CleanupResult {
        job_id: job.metadata.job_id.clone(),
        workspace_removed: true,
        evidence_retained: true,
        limitation: None,
    })
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
            "result identity или portable log paths не соответствуют job",
        ));
    }
    validate_request(&result.request)?;
    validate_hex(&result.argv_sha256, 64, "argv digest")
}

pub fn safe_argv(argv: &[String]) -> Vec<String> {
    let mut redact_next = false;
    argv.iter()
        .map(|argument| {
            if std::mem::take(&mut redact_next) {
                return "<redacted>".to_owned();
            }
            if let Some((key, _)) = argument.split_once('=')
                && is_sensitive_key(key)
            {
                return format!("{key}=<redacted>");
            }
            if is_sensitive_key(argument) {
                if argument.starts_with('-') {
                    redact_next = true;
                    argument.clone()
                } else {
                    "<redacted>".to_owned()
                }
            } else {
                argument.clone()
            }
        })
        .collect()
}

fn is_sensitive_key(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "api-key",
        "apikey",
        "authorization",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

fn argv_digest(argv: &[String]) -> String {
    let mut digest = Sha256::new();
    for argument in argv {
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
        return Err(invalid("нужен ограниченный explicit argv без NUL"));
    }
    if request.options.output_limit_bytes > MAX_OUTPUT_BYTES
        || !(1..=64).contains(&request.options.max_parallel_jobs)
        || request
            .options
            .timeout_ms
            .is_none_or(|timeout| timeout == 0)
    {
        return Err(invalid("неверный output limit, timeout или parallel limit"));
    }
    relative_path(&request.cwd)?;
    if let EnvironmentPolicy::Explicit { values } = &request.options.environment {
        for (key, value) in values {
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return Err(invalid("неверная environment variable"));
            }
            if PRIVATE_ENV.contains(&key.as_str()) {
                return Err(invalid(format!(
                    "job-private variable {key} не переопределяется"
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
    // rustup proxies должны найти установленный toolchain, даже при private HOME.
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
        return Err(invalid("неверный компонент workspace path"));
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
        return Err(invalid("нужен полный Git SHA в lowercase hex"));
    }
    Ok(())
}
fn random_id() -> Result<String, DomainError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| invalid(format!("не удалось получить job nonce: {e}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn unique_directory(parent: &Path) -> Result<(PathBuf, String), DomainError> {
    let dir = safe_dir(parent)?;
    for _ in 0..16 {
        let id = random_id()?;
        match mkdir_fd(&dir, &id) {
            Ok(()) => return Ok((parent.join(&id), id)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(conflict("не удалось зарезервировать unique job directory"))
}
fn absolute_path(path: &Path) -> Result<PathBuf, DomainError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io_error)?.join(path)
    };
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("alias path, . и .. в job path запрещены"));
    }
    Ok(path)
}

fn verify_snapshot(root: &Path, target: &GitTarget) -> Result<(), DomainError> {
    let collected = super::scope::collect_scope(root, &target.base_sha, &target.head_sha)
        .map_err(|e| invalid(format!("Git snapshot недоступен: {e}")))?;
    if collected.target != *target {
        return Err(invalid(
            "repository/base/head/merge-base identity не соответствует Git objects",
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
        .map_err(io_error)?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout).trim() != job.metadata.source.snapshot.head_sha
    {
        return Err(invalid("job worktree больше не соответствует pinned HEAD"));
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
        .map_err(io_error)?;
    if !output.status.success() {
        return Err(invalid("не удалось проверить source после исполнения"));
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
        .map_err(io_error)?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(invalid("не удалось проверить Git checkout filters"));
    }
    let names = String::from_utf8(output.stdout)
        .map_err(|e| invalid(format!("Git filter names не UTF-8: {e}")))?;
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
fn git_success(mut command: Command, operation: &str) -> Result<(), DomainError> {
    let output = command.output().map_err(io_error)?;
    if !output.status.success() {
        return Err(DomainError::new(
            ErrorCode::GitEvidenceFailed,
            format!("{operation}: {}", String::from_utf8_lossy(&output.stderr)),
        ));
    }
    Ok(())
}
fn verify_owner(job: &PreparedJob) -> Result<State, DomainError> {
    let metadata: JobMetadata = read_document(&job.directory, "job.json")?;
    let state: State = read_document(&job.directory, "state.json")?;
    if metadata != job.metadata || metadata.owner_nonce != state.owner_nonce {
        return Err(invalid("job ownership изменился"));
    }
    let bytes = read_bytes(
        &job.directory,
        "source-review.json",
        super::workflow::MAX_REVIEW_ARTIFACT_BYTES,
    )?;
    if format!("{:x}", Sha256::digest(&bytes)) != metadata.source.review_pack_sha256 {
        return Err(invalid("source pack bytes identity изменился"));
    }
    let pack: ReviewPack = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("source-review.json повреждён: {error}")))?;
    if pack.target != metadata.source.snapshot {
        return Err(invalid(
            "source-review.json не соответствует snapshot из job manifest",
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
    match read_bytes(&job.directory, "cancel.json", 128) {
        Ok(bytes) if bytes == job.metadata.owner_nonce.as_bytes() => Ok(true),
        Ok(_) => Err(invalid("cancel ownership mismatch")),
        Err(error)
            if job
                .directory
                .join("cancel.json")
                .symlink_metadata()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            let _ = error;
            Ok(false)
        }
        Err(error) => Err(error),
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
    let total_bytes = file.metadata().map_err(io_error)?.len();
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    let utf8_lossy = std::str::from_utf8(&bytes).is_err();
    let mut truncated = total_bytes > bytes.len() as u64;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    for value in sensitive_values.iter().filter(|value| !value.is_empty()) {
        text = text.replace(value, "<redacted>");
    }
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    Ok(OutputEvidence {
        text,
        total_bytes,
        truncated,
        utf8_lossy,
        log: format!("logs/{name}"),
    })
}

fn sensitive_environment_values(environment: &EnvironmentPolicy) -> Vec<String> {
    match environment {
        EnvironmentPolicy::Minimal => Vec::new(),
        EnvironmentPolicy::Explicit { values } => values
            .iter()
            .filter(|(key, _)| is_sensitive_key(key))
            .map(|(_, value)| value.clone())
            .collect(),
    }
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
        .ok_or_else(|| std::io::Error::other("invalid child PID"))?;
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
                *failure = Some(format!("не удалось завершить owned process group: {error}"));
            }
        }
    }
    #[cfg(not(unix))]
    {
        enforcement.process_cleanup = "direct_child_only".into();
    }
    if let Err(error) = child.kill() {
        *failure = Some(format!("direct-child kill: {error}"));
    }
}

#[cfg(unix)]
fn platform_supported() -> Result<(), DomainError> {
    Ok(())
}
#[cfg(not(unix))]
fn platform_supported() -> Result<(), DomainError> {
    Err(invalid(
        "job execution недоступен: на платформе нет реализованного ownership-safe locking и filesystem boundary",
    ))
}

// Все собственные файлы открываются через directory descriptors и O_NOFOLLOW.
// Это исключает чтение/публикацию по symlink, включая последний компонент.
#[cfg(unix)]
pub(super) fn safe_dir(path: &Path) -> Result<File, DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    let path = absolute_path(path)?;
    let mut directory = File::open("/").map_err(io_error)?;
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
                            "review workspace path содержит symlink или не-каталог",
                        ),
                        _ => io_error(error.into()),
                    })?,
                )
            }
            _ => return Err(invalid("неподдерживаемый directory component")),
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
    let mut directory = File::open("/").map_err(io_error)?;
    for component in path.components() {
        if let Component::Normal(name) = component {
            match rustix::fs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                Err(error) => return Err(io_error(error.into())),
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
                        "review workspace path содержит symlink или не-каталог",
                    ),
                    _ => io_error(error.into()),
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
    Err(std::io::Error::other("unsupported platform"))
}

#[cfg(unix)]
fn open_file(directory: &File, name: &str, create: bool) -> Result<File, DomainError> {
    use rustix::fs::{Mode, OFlags, openat};
    if !valid_artifact_name(name) {
        return Err(invalid(
            "имя job artifact должно быть одним безопасным компонентом",
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
        .map_err(|e| io_error(e.into()))?,
    );
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(invalid("job artifact должен быть regular file"));
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
    if file.metadata().map_err(io_error)?.len() > max {
        return Err(invalid("job artifact превышает лимит чтения"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > max {
        return Err(invalid("job artifact вырос при чтении"));
    }
    Ok(bytes)
}
fn read_document<T: serde::de::DeserializeOwned>(
    directory: &Path,
    name: &str,
) -> Result<T, DomainError> {
    serde_json::from_slice(&read_bytes(directory, name, MAX_DOCUMENT_BYTES)?)
        .map_err(|e| invalid(format!("невалидный {name}: {e}")))
}
fn write_new(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), DomainError> {
    write_new_fd(&safe_dir(directory)?, name, bytes).map_err(io_error)
}
pub(super) fn write_new_fd(directory: &File, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    if !valid_artifact_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "имя artifact должно быть одним безопасным компонентом",
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
        Err(std::io::Error::other("unsupported platform"))
    }
}

/// Читает regular file через закреплённый каталог, не следуя symlink.
pub(super) fn read_optional_file_at(
    directory: &File,
    name: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, DomainError> {
    if !valid_artifact_name(name) {
        return Err(invalid(
            "имя review artifact должно быть одним безопасным компонентом",
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
                return Err(conflict("review artifact не может быть symlink"));
            }
            Err(error) => return Err(io_error(error.into())),
        };
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(conflict("review artifact должен быть regular file"));
        }
        if metadata.len() > max_bytes {
            return Err(invalid("review artifact превышает лимит чтения"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > max_bytes {
            return Err(invalid("review artifact вырос при чтении"));
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
            "имя review artifact должно быть одним безопасным компонентом",
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
        Err(std::io::Error::other("unsupported platform"))
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
    for entry in fs::read_dir(&job_path).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("частичный job содержит имя не UTF-8"))?;
        if !matches!(
            name.as_str(),
            "job.json" | "source-review.json" | "state.json" | "active.lock"
        ) || !entry.file_type().map_err(io_error)?.is_file()
        {
            return Err(conflict(
                "частичный job содержит неизвестный файл или symlink; каталог сохранён",
            ));
        }
        files.insert(name);
    }
    for name in files {
        // open_file использует O_NOFOLLOW и подтверждает regular file перед unlinkat.
        drop(open_file(&job_fd, &name, false)?);
        unlinkat(&job_fd, name.as_str(), AtFlags::empty())
            .map_err(|error| io_error(error.into()))?;
    }
    unlinkat(&parent_fd, job_id, AtFlags::REMOVEDIR).map_err(|error| io_error(error.into()))?;
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
        .map_err(|e| invalid(format!("не удалось сериализовать job artifact: {e}")))?;
    bytes.push(b'\n');
    let dir = safe_dir(directory)?;
    if !replace {
        return write_new_fd(&dir, name, &bytes).map_err(io_error);
    }
    let temporary = format!(".publish-{}", random_id()?);
    write_new_fd(&dir, &temporary, &bytes).map_err(io_error)?;
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, renameat, unlinkat};
        if let Err(error) = renameat(&dir, temporary.as_str(), &dir, name) {
            let _ = unlinkat(&dir, temporary.as_str(), AtFlags::empty());
            return Err(io_error(error.into()));
        }
        dir.sync_all().map_err(io_error)?;
    }
    #[cfg(not(unix))]
    {
        return platform_supported();
    }
    Ok(())
}

/// Advisory locks сериализуют cooperating writers; не являются security boundary.
#[cfg(unix)]
fn job_lock(directory: &Path) -> Result<File, DomainError> {
    use fs2::FileExt;
    let file = lock_file(&safe_dir(directory)?, "active.lock")?;
    match file.try_lock_exclusive() {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(conflict("job уже активен или очищается"));
        }
        Err(error) => return Err(io_error(error)),
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
        .map_err(|e| io_error(e.into()))?,
    );
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(invalid("lock должен быть regular file"));
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
        Err(error) => Err(io_error(error)),
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
    for index in 0..limit {
        let file = lock_file(&dir, &format!("slot-{index}.lock"))?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(conflict(
        "достигнут предел конкурентных execution jobs; повторите после завершения другого job",
    ))
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
fn io_error(error: std::io::Error) -> DomainError {
    DomainError::new(
        ErrorCode::WriteFailed,
        format!("job filesystem/process operation: {error}"),
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
            LifecycleStatus::Interrupted
        );
        assert!(cleanup_workspace(&job).is_err());
        assert!(unrelated.is_dir());
        let job = repo.prepare(ExecutionMode::IsolatedChecks);
        save_state(&job, LifecycleStatus::Running, false).unwrap();
        assert_eq!(
            inspect_job(job.directory()).unwrap().lifecycle,
            LifecycleStatus::Interrupted
        );
        assert!(read_result(job.directory()).is_err());
        assert!(cleanup_workspace(&job).is_err());
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
}
