//! Program-owned filesystem store с атомарным versioned manifest.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, FlockOperation, Mode, OFlags, flock, linkat, mkdirat, open, openat, renameat, unlinkat,
};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::error::{AssetError, ErrorCode};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, MANIFEST_SCHEMA_VERSION, Manifest,
    Provenance, SemanticDecision, SemanticStatus, ValidationRecord, ValidatorIdentity,
};
use crate::selection::{SelectionMode, select_assets};
use crate::validation::{SemanticValidator, ValidationAttempt, ValidationReport, ValidatorFailure};

const MANIFEST_FILE: &str = "manifest.json";
const OWNER_FILE: &str = ".owner.json";
const LOCK_FILE: &str = ".lock";
const OBJECTS_DIR: &str = "objects";
const TEMP_DIR: &str = ".tmp";
const OWNER_SCHEMA_VERSION: u32 = 1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Параметры открытия store и защищённых от пересечения каталогов.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub root: PathBuf,
    protected_roots: Vec<PathBuf>,
}

impl StoreOptions {
    /// Открывает отдельный store без дополнительных protected roots.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            protected_roots: Vec::new(),
        }
    }

    /// Запрещает store пересекаться с указанным пользовательским деревом.
    pub fn protect_from(mut self, path: impl Into<PathBuf>) -> Self {
        self.protected_roots.push(path.into());
        self
    }
}

/// Открытый, проверяемый asset store.
#[derive(Debug)]
pub struct AssetStore {
    /// Канонический путь для вывода пользователю.
    root: PathBuf,
    /// Открытый directory handle — все store I/O остаётся привязанным к этому inode.
    root_handle: File,
    store_id: String,
    initialized_on_open: bool,
    #[cfg(test)]
    fail_next_manifest_write: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    lock_test_hooks: std::sync::Mutex<Option<std::sync::Arc<LockTestHooks>>>,
}

#[cfg(test)]
struct LockTestHooks {
    before_lock: std::sync::Arc<dyn Fn(&File) + Send + Sync>,
    after_lock: std::sync::Arc<dyn Fn(&File) + Send + Sync>,
}

#[cfg(test)]
impl std::fmt::Debug for LockTestHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LockTestHooks")
    }
}

/// Запрос explicit ingest одного названного локального файла.
#[derive(Debug, Clone)]
pub struct IngestRequest {
    pub identity: AssetIdentity,
    pub source_path: PathBuf,
    /// Доменное расширение identity, которое generic core сохраняет без
    /// интерпретации (например character и Unicode code points для kanji).
    pub domain_metadata: Option<serde_json::Value>,
    /// Для явной замены требуется hash версии, которую вызывающий ожидает.
    pub replace_expected_sha256: Option<String>,
}

/// Итог explicit ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub asset: AssetRecord,
    pub previous: Option<AssetRecord>,
    pub changed: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerMarker {
    schema_version: u32,
    store_id: String,
}

#[derive(Debug)]
struct TempArtifact {
    directory: File,
    name: OsString,
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        let _ = unlinkat(&self.directory, &self.name, AtFlags::empty());
    }
}

#[derive(Debug)]
struct StagedObject {
    artifact: TempArtifact,
    sha256: String,
    byte_length: u64,
    format: DetectedFormat,
}

#[derive(Debug)]
struct StoreLock {
    directory: File,
    lock_file: File,
}

impl StoreLock {
    fn unlock(self) -> Result<(), AssetError> {
        flock(&self.lock_file, FlockOperation::Unlock).map_err(|error| {
            AssetError::io("не удалось снять lock store", std::io::Error::from(error))
        })?;
        flock(&self.directory, FlockOperation::Unlock).map_err(|error| {
            AssetError::io(
                "не удалось снять directory lock store",
                std::io::Error::from(error),
            )
        })
    }
}

impl AssetStore {
    /// Открывает существующий или создаёт новый store после проверки boundary.
    pub fn open(options: StoreOptions) -> Result<Self, AssetError> {
        let requested_root = resolve_store_root(&options.root)?;
        for protected in &options.protected_roots {
            for protected in resolve_protected_paths(protected)? {
                if paths_overlap(&requested_root, &protected) {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        format!(
                            "store root {} пересекается с защищённым каталогом {}",
                            requested_root.display(),
                            protected.display()
                        ),
                    ));
                }
            }
        }

        let (root_handle, root_created) = open_or_create_store_root(&requested_root, |_| {})?;
        let canonical_root = fd_canonical_path(&root_handle)?;
        if canonical_root != requested_root {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "store root изменился во время открытия",
            ));
        }

        for protected in &options.protected_roots {
            for protected in resolve_protected_paths(protected)? {
                if paths_overlap(&canonical_root, &protected) {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        format!(
                            "store root {} пересекается с защищённым каталогом {}",
                            canonical_root.display(),
                            protected.display()
                        ),
                    ));
                }
            }
        }

        // Directory flock сериализует первоначальную проверку и bootstrap,
        // не оставляя lock-файла в существующем unowned root.
        flock(&root_handle, FlockOperation::LockExclusive).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store root",
                std::io::Error::from(error),
            )
        })?;
        let state = preflight_root_ownership(&root_handle, root_created)?;
        let lock = match state {
            RootState::NewlyCreated | RootState::ExistingEmpty => create_lock_file(&root_handle)?,
            RootState::Owned => open_lock_file(&root_handle, true)?,
        };
        flock(&lock, FlockOperation::LockExclusive).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store",
                std::io::Error::from(error),
            )
        })?;
        let initialized_on_open = initialize_or_load(&root_handle, state)?;

        let manifest = load_manifest(&root_handle)?;
        validate_manifest(&root_handle, &manifest)?;

        let store = Self {
            root: canonical_root,
            root_handle,
            store_id: manifest.store_id,
            initialized_on_open,
            #[cfg(test)]
            fail_next_manifest_write: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            lock_test_hooks: std::sync::Mutex::new(None),
        };
        flock(&lock, FlockOperation::Unlock).map_err(|error| {
            AssetError::io("не удалось снять lock store", std::io::Error::from(error))
        })?;
        flock(&store.root_handle, FlockOperation::Unlock).map_err(|error| {
            AssetError::io(
                "не удалось снять lock store root",
                std::io::Error::from(error),
            )
        })?;
        Ok(store)
    }

    /// Канонический абсолютный program-owned root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Постоянный идентификатор store.
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// Истина, если этот вызов `open` создал store или завершил его первичную
    /// инициализацию.
    pub const fn initialized_on_open(&self) -> bool {
        self.initialized_on_open
    }

    /// Проверяет manifest и каждый файл, на который он ссылается.
    pub fn verify_integrity(&self) -> Result<Vec<AssetRecord>, AssetError> {
        let lock = self.lock_shared()?;
        let manifest = load_manifest(&self.root_handle)?;
        validate_manifest(&self.root_handle, &manifest)?;
        let assets = manifest.assets;
        lock.unlock()?;
        Ok(assets)
    }

    /// Явно импортирует один файл, вычисляя SHA-256 по скопированным bytes.
    /// Повтор той же identity/hash — no-op; другой hash требует ожидаемый hash.
    pub fn ingest(&self, request: IngestRequest) -> Result<IngestOutcome, AssetError> {
        request
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        let source = open_source_file(&request.source_path)?;
        self.ingest_from_file(request, source)
    }

    /// Импортирует уже открытый и проверенный CLI source handle.
    pub(crate) fn ingest_from_file(
        &self,
        request: IngestRequest,
        source: File,
    ) -> Result<IngestOutcome, AssetError> {
        request
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        let lock = self.lock_exclusive()?;
        let mut manifest = load_manifest(&self.root_handle)?;
        validate_manifest(&self.root_handle, &manifest)?;
        let staged = stage_source(&self.root_handle, source)?;
        let source_name = request
            .source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unnamed")
            .to_owned();

        let existing = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == request.identity)
            .cloned();

        if let Some(existing) = &existing {
            if existing.sha256 == staged.sha256 {
                drop(staged);
                lock.unlock()?;
                return Ok(IngestOutcome {
                    asset: existing.clone(),
                    previous: Some(existing.clone()),
                    changed: false,
                });
            }
            if request.replace_expected_sha256.as_deref() != Some(existing.sha256.as_str()) {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    format!(
                        "identity {} уже привязана к {}; для явной замены укажите этот hash",
                        existing.identity, existing.sha256
                    ),
                    serde_json::json!({
                        "identity": existing.identity,
                        "existing_sha256": existing.sha256,
                        "candidate_sha256": staged.sha256,
                    }),
                ));
            }
        } else if request.replace_expected_sha256.is_some() {
            return Err(AssetError::with_details(
                ErrorCode::IdentityConflict,
                "ожидаемый hash замены указан, но identity ещё отсутствует",
                serde_json::json!({
                    "identity": request.identity,
                    "existing_sha256": serde_json::Value::Null,
                    "candidate_sha256": staged.sha256,
                }),
            ));
        }

        let storage_path = object_relative_path(&staged.sha256);
        publish_object(&staged, &self.root_handle, &staged.sha256)?;
        let record = AssetRecord {
            identity: request.identity,
            storage_path,
            sha256: staged.sha256.clone(),
            byte_length: staged.byte_length,
            format: staged.format,
            provenance: Provenance {
                source_kind: "local_import".to_owned(),
                source_name,
            },
            lifecycle: LifecycleState::Pending,
            validation: None,
            domain_metadata: request.domain_metadata,
        };
        validate_object(&self.root_handle, &record)?;

        if existing.is_some() {
            let slot = manifest
                .assets
                .iter_mut()
                .find(|asset| asset.identity == record.identity)
                .expect("предварительно найденная identity остаётся в manifest");
            *slot = record.clone();
        } else {
            manifest.assets.push(record.clone());
        }
        sort_assets(&mut manifest.assets);
        manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| {
            AssetError::new(ErrorCode::ManifestCorrupt, "revision manifest переполнен")
        })?;
        save_manifest(&self.root_handle, &manifest, false)?;
        drop(staged);
        lock.unlock()?;
        Ok(IngestOutcome {
            asset: record,
            previous: existing,
            changed: true,
        })
    }

    /// Выбирает `new` или `full` из общего manifest без запуска validator'а.
    pub fn select(
        &self,
        mode: SelectionMode,
        validator: &ValidatorIdentity,
    ) -> Result<Vec<AssetRecord>, AssetError> {
        validate_validator_identity(validator)?;
        let lock = self.lock_shared()?;
        let manifest = load_manifest(&self.root_handle)?;
        validate_manifest(&self.root_handle, &manifest)?;
        let assets = select_assets(&manifest.assets, mode, validator)
            .into_iter()
            .cloned()
            .collect();
        lock.unlock()?;
        Ok(assets)
    }

    /// Запускает injected semantic validator и атомарно фиксирует все полученные
    /// decisions. При техническом отказе конкретный asset остаётся без нового
    /// decision и не становится `verified`.
    pub fn validate<V: SemanticValidator>(
        &self,
        mode: SelectionMode,
        validator: &V,
    ) -> Result<ValidationReport, AssetError> {
        let validator_id = validator.identity();
        validate_validator_identity(&validator_id)?;
        let lock = self.lock_exclusive()?;
        let mut manifest = load_manifest(&self.root_handle)?;
        validate_manifest(&self.root_handle, &manifest)?;
        let selected: Vec<_> = select_assets(&manifest.assets, mode, &validator_id)
            .into_iter()
            .cloned()
            .collect();
        let mut report = ValidationReport::new(mode, validator_id.clone());
        report.considered = selected.len();

        // Сначала выполняются все domain calls и проверяется evidence. До этого
        // места manifest и trusted state не меняются.
        let mut decisions: Vec<(AssetRecord, SemanticDecision)> = Vec::new();
        for record in selected {
            let mut file = checked_object_file(&self.root_handle, &record)?;
            let original_state = record.lifecycle;
            match validator.validate(&record, &mut file) {
                Ok(decision) => {
                    validate_decision(&decision)?;
                    decisions.push((record, decision));
                }
                Err(failure) => {
                    let code = stable_failure_code(&failure);
                    report.blockers.push(code.clone());
                    report.attempts.push(ValidationAttempt {
                        identity: record.identity,
                        from_state: original_state,
                        to_state: original_state,
                        content_sha256: record.sha256,
                        status: None,
                        evidence: Vec::new(),
                        changed: false,
                        blocker: Some(code),
                        blocker_message: Some(failure.message),
                    });
                }
            }
        }

        let mut changed_records = Vec::new();
        for (old_record, decision) in decisions {
            let lifecycle = match decision.status {
                SemanticStatus::Verified => LifecycleState::Verified,
                SemanticStatus::Rejected | SemanticStatus::Uncertain | SemanticStatus::Corrupt => {
                    LifecycleState::Quarantined
                }
            };
            let validation = ValidationRecord {
                status: decision.status,
                validator: validator_id.clone(),
                content_sha256: old_record.sha256.clone(),
                evidence: decision.evidence.clone(),
            };
            let mut new_record = old_record.clone();
            new_record.lifecycle = lifecycle;
            new_record.validation = Some(validation);
            let changed = new_record.lifecycle != old_record.lifecycle
                || new_record.validation != old_record.validation;
            if changed {
                let target = manifest
                    .assets
                    .iter_mut()
                    .find(|asset| asset.identity == new_record.identity)
                    .expect("selected identity remains in locked manifest");
                *target = new_record.clone();
                changed_records.push(new_record.clone());
            }
            report.attempts.push(ValidationAttempt {
                identity: old_record.identity,
                from_state: old_record.lifecycle,
                to_state: lifecycle,
                content_sha256: old_record.sha256,
                status: Some(decision.status),
                evidence: decision.evidence,
                changed,
                blocker: None,
                blocker_message: None,
            });
        }

        // Validator мог работать параллельно с процессом, не использующим наш
        // lock. Повторно проверяем bytes перед тем, как зафиксировать decision.
        for record in &changed_records {
            validate_object(&self.root_handle, record)?;
        }

        report
            .attempts
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        report.blockers.sort();
        report.blockers.dedup();
        report.changed = changed_records.len();
        if report.changed > 0 {
            sort_assets(&mut manifest.assets);
            manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| {
                AssetError::new(ErrorCode::ManifestCorrupt, "revision manifest переполнен")
            })?;
            #[cfg(test)]
            save_manifest_with_test_hook(
                &self.root_handle,
                &manifest,
                false,
                &self.fail_next_manifest_write,
            )?;
            #[cfg(not(test))]
            save_manifest(&self.root_handle, &manifest, false)?;
        }
        lock.unlock()?;
        Ok(report)
    }

    fn lock_shared(&self) -> Result<StoreLock, AssetError> {
        self.lock(false)
    }

    fn lock_exclusive(&self) -> Result<StoreLock, AssetError> {
        self.lock(true)
    }

    fn lock(&self, exclusive: bool) -> Result<StoreLock, AssetError> {
        let directory = open_directory_at(&self.root_handle, ".")
            .map_err(|error| AssetError::io("не удалось открыть store directory handle", error))?;
        #[cfg(test)]
        let hooks = if exclusive {
            self.lock_test_hooks
                .lock()
                .expect("lock test hook mutex is not poisoned")
                .clone()
        } else {
            None
        };
        #[cfg(test)]
        if let Some(hooks) = &hooks {
            (hooks.before_lock)(&directory);
        }
        let directory_operation = if exclusive {
            FlockOperation::LockExclusive
        } else {
            FlockOperation::LockShared
        };
        flock(&directory, directory_operation).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store directory",
                std::io::Error::from(error),
            )
        })?;
        let lock_file = open_lock_file(&self.root_handle, false)?;
        let lock_operation = if exclusive {
            FlockOperation::LockExclusive
        } else {
            FlockOperation::LockShared
        };
        flock(&lock_file, lock_operation).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store",
                std::io::Error::from(error),
            )
        })?;
        #[cfg(test)]
        if let Some(hooks) = &hooks {
            (hooks.after_lock)(&directory);
        }
        Ok(StoreLock {
            directory,
            lock_file,
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_manifest_write(&self) {
        self.fail_next_manifest_write.store(true, Ordering::SeqCst);
    }
}

fn validate_validator_identity(identity: &ValidatorIdentity) -> Result<(), AssetError> {
    ValidatorIdentity::new(identity.id.clone(), identity.version.clone())
        .map(|_| ())
        .map_err(|message| AssetError::new(ErrorCode::InvalidValidatorIdentity, message))
}

fn validate_decision(decision: &SemanticDecision) -> Result<(), AssetError> {
    if decision.evidence.is_empty()
        || decision
            .evidence
            .iter()
            .any(|evidence| evidence.kind.trim().is_empty() || evidence.summary.trim().is_empty())
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "semantic decision обязан содержать непустые kind и summary evidence",
        ));
    }
    Ok(())
}

fn stable_failure_code(failure: &ValidatorFailure) -> String {
    let code = failure.code.trim();
    if code.is_empty()
        || code.len() > 128
        || !code.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        "validator_failure".to_owned()
    } else {
        code.to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    NewlyCreated,
    ExistingEmpty,
    Owned,
}

fn initialize_or_load(root: &File, state: RootState) -> Result<bool, AssetError> {
    match state {
        RootState::NewlyCreated | RootState::ExistingEmpty => {
            // Empty root is the only unowned state safe to initialize. Both
            // the directory bootstrap lock and the store lock are held.
            let _objects = ensure_dir_entry(root, OBJECTS_DIR)?;
            let _temporary = ensure_dir_entry(root, TEMP_DIR)?;
            let store_id = new_store_id();
            let manifest = Manifest::empty(store_id.clone());
            write_owner_marker(root, &store_id)?;
            save_manifest(root, &manifest, true)?;
            Ok(true)
        }
        RootState::Owned => {
            // Missing internal directories may be repaired only after both
            // independent ownership files have been validated.
            ensure_dir_entry(root, OBJECTS_DIR)?;
            ensure_dir_entry(root, TEMP_DIR)?;
            Ok(false)
        }
    }
}

/// Classifies root without creating files or directories.
fn preflight_root_ownership(root: &File, root_created: bool) -> Result<RootState, AssetError> {
    let names = inspect_top_level(root, ErrorCode::StoreNotOwned)?;
    if names.is_empty() {
        return Ok(if root_created {
            RootState::NewlyCreated
        } else {
            RootState::ExistingEmpty
        });
    }
    if !names.contains(OWNER_FILE) {
        return Err(AssetError::new(
            ErrorCode::StoreNotOwned,
            "непустой store root без owner marker не принадлежит asset store",
        ));
    }
    if !names.contains(MANIFEST_FILE) {
        return Err(AssetError::new(
            ErrorCode::ManifestMissing,
            "owner marker существует, но canonical manifest отсутствует",
        ));
    }

    let owner = read_owner_marker(root)?;
    let manifest = read_manifest_file(root)?;
    check_schema(&manifest)?;
    if manifest.store_id != owner.store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id manifest не совпадает с owner marker",
        ));
    }
    Ok(RootState::Owned)
}

fn inspect_top_level(root: &File, unknown_code: ErrorCode) -> Result<BTreeSet<String>, AssetError> {
    let root_path = fd_path(root);
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&root_path)
        .map_err(|error| AssetError::io("не удалось прочитать store root", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать store entry", error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            LOCK_FILE | OWNER_FILE | MANIFEST_FILE | OBJECTS_DIR | TEMP_DIR
        ) {
            return Err(AssetError::new(
                unknown_code,
                format!("неожиданный файл в store root: {name}"),
            ));
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AssetError::io("не удалось проверить store entry", error))?;
        if metadata.file_type().is_symlink() {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!("symlink запрещён в store root: {name}"),
            ));
        }
        if matches!(name.as_str(), OBJECTS_DIR | TEMP_DIR) && !metadata.is_dir() {
            return Err(AssetError::new(
                unknown_code,
                format!("{name} существует, но не является каталогом"),
            ));
        }
        if matches!(name.as_str(), LOCK_FILE | OWNER_FILE | MANIFEST_FILE) && !metadata.is_file() {
            return Err(AssetError::new(
                unknown_code,
                format!("{name} существует, но не является обычным файлом"),
            ));
        }
        names.insert(name);
    }
    Ok(names)
}

fn ensure_top_level(root: &File) -> Result<(), AssetError> {
    inspect_top_level(root, ErrorCode::UnexpectedPath).map(|_| ())
}

fn open_directory_at(parent: &File, name: impl rustix::path::Arg) -> std::io::Result<File> {
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(File::from(fd))
}

fn ensure_dir_entry(root: &File, name: &str) -> Result<File, AssetError> {
    match open_directory_at(root, name) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match mkdirat(root, name, Mode::from_raw_mode(0o755)) {
                Ok(()) => open_directory_at(root, name)
                    .map_err(|error| directory_entry_error(name, error)),
                Err(create_error)
                    if std::io::Error::from(create_error).kind()
                        == std::io::ErrorKind::AlreadyExists =>
                {
                    open_directory_at(root, name)
                        .map_err(|error| directory_entry_error(name, error))
                }
                Err(create_error) => Err(AssetError::io(
                    format!("не удалось создать {name}"),
                    std::io::Error::from(create_error),
                )),
            }
        }
        Err(error) => Err(directory_entry_error(name, error)),
    }
}

fn directory_entry_error(name: &str, error: std::io::Error) -> AssetError {
    if is_symlink_error(&error) || error.kind() == std::io::ErrorKind::NotADirectory {
        AssetError::new(
            ErrorCode::BoundaryViolation,
            format!("{name} должен быть обычным каталогом без symlink"),
        )
    } else {
        AssetError::io(format!("не удалось проверить {name}"), error)
    }
}

fn create_lock_file(root: &File) -> Result<File, AssetError> {
    let lock = openat(
        root,
        LOCK_FILE,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AssetError::new(
                ErrorCode::StoreNotOwned,
                "lock появился в root до подтверждения program ownership",
            )
        } else {
            AssetError::io("не удалось создать lock store", error)
        }
    })?;
    let lock = File::from(lock);
    if !lock
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить lock store", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "lock path должен быть обычным файлом",
        ));
    }
    Ok(lock)
}

fn open_lock_file(root: &File, create_if_missing: bool) -> Result<File, AssetError> {
    match openat(
        root,
        LOCK_FILE,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let lock = File::from(fd);
            if !lock
                .metadata()
                .map_err(|error| AssetError::io("не удалось проверить lock store", error))?
                .is_file()
            {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    "lock path должен быть обычным файлом без symlink",
                ));
            }
            Ok(lock)
        }
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
            if create_if_missing {
                create_lock_file(root)
            } else {
                Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "lock отсутствует в program-owned store",
                ))
            }
        }
        Err(error) => {
            let error = std::io::Error::from(error);
            if is_symlink_error(&error) {
                Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    "lock path должен быть обычным файлом без symlink",
                ))
            } else {
                Err(AssetError::io("не удалось открыть lock store", error))
            }
        }
    }
}

fn write_owner_marker(root: &File, store_id: &str) -> Result<(), AssetError> {
    let marker = OwnerMarker {
        schema_version: OWNER_SCHEMA_VERSION,
        store_id: store_id.to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(root, OWNER_FILE, &bytes, true, None)
}

fn load_manifest(root: &File) -> Result<Manifest, AssetError> {
    ensure_top_level(root)?;
    let manifest = read_manifest_file(root)?;
    check_schema(&manifest)?;
    let owner = read_owner_marker(root)?;
    if owner.store_id != manifest.store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id manifest не совпадает с owner marker",
        ));
    }
    Ok(manifest)
}

fn read_manifest_file(root: &File) -> Result<Manifest, AssetError> {
    let mut file = open_regular_at(root, MANIFEST_FILE, ErrorCode::ManifestMissing)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("не удалось прочитать manifest", error))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                "manifest schema_version отсутствует или не является целым числом",
            )
        })?;
    if schema_version != u64::from(MANIFEST_SCHEMA_VERSION) {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "manifest schema_version {schema_version} не поддерживается (ожидается {})",
                MANIFEST_SCHEMA_VERSION
            ),
        ));
    }
    serde_json::from_value(value)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))
}

fn read_owner_marker(root: &File) -> Result<OwnerMarker, AssetError> {
    let mut file = open_regular_at(root, OWNER_FILE, ErrorCode::StoreNotOwned)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("не удалось прочитать owner marker", error))?;
    let marker: OwnerMarker = serde_json::from_slice(&bytes).map_err(|error| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("owner marker невалиден: {error}"),
        )
    })?;
    if marker.schema_version != OWNER_SCHEMA_VERSION {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "неподдерживаемая версия owner marker {}",
                marker.schema_version
            ),
        ));
    }
    if marker.store_id.is_empty() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "owner marker store_id пуст",
        ));
    }
    Ok(marker)
}

fn check_schema(manifest: &Manifest) -> Result<(), AssetError> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "manifest schema_version {} не поддерживается (ожидается {})",
                manifest.schema_version, MANIFEST_SCHEMA_VERSION
            ),
        ));
    }
    Ok(())
}

fn validate_manifest(root: &File, manifest: &Manifest) -> Result<(), AssetError> {
    check_schema(manifest)?;
    if manifest.store_id.is_empty() || manifest.revision > i64::MAX as u64 {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id или revision manifest некорректны",
        ));
    }
    let mut previous_identity: Option<&AssetIdentity> = None;
    for record in &manifest.assets {
        record
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::ManifestCorrupt, message))?;
        if previous_identity.is_some_and(|previous| previous >= &record.identity) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "assets manifest должны быть уникальны и отсортированы по identity",
            ));
        }
        previous_identity = Some(&record.identity);
        validate_hash(&record.sha256)?;
        if record.byte_length == 0 && record.format != DetectedFormat::Unknown {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "пустой файл должен иметь format unknown",
            ));
        }
        let expected_path = object_relative_path(&record.sha256);
        validate_relative_path(&record.storage_path, &expected_path)?;
        match (&record.lifecycle, &record.validation) {
            (LifecycleState::Pending, None) => {}
            (LifecycleState::Verified, Some(validation))
                if validation.status == SemanticStatus::Verified =>
            {
                validate_validation_record(record, validation)?;
            }
            (LifecycleState::Quarantined, Some(validation))
                if validation.status != SemanticStatus::Verified =>
            {
                validate_validation_record(record, validation)?;
            }
            _ => {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    format!(
                        "lifecycle и semantic decision не согласованы для {}",
                        record.identity
                    ),
                ));
            }
        }
        if record.provenance.source_kind.is_empty() || record.provenance.source_name.is_empty() {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("provenance отсутствует для {}", record.identity),
            ));
        }
        if record.provenance.source_name.contains(['/', '\\']) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "provenance source_name не должен содержать path",
            ));
        }
        validate_object(root, record)?;
    }
    validate_object_directory(root)?;
    Ok(())
}

fn validate_validation_record(
    record: &AssetRecord,
    validation: &ValidationRecord,
) -> Result<(), AssetError> {
    validate_hash(&validation.content_sha256)?;
    if validation.content_sha256 != record.sha256 || validation.evidence.is_empty() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!(
                "semantic decision не привязан к текущему hash для {}",
                record.identity
            ),
        ));
    }
    validate_validator_identity(&validation.validator)?;
    if validation
        .evidence
        .iter()
        .any(|evidence| evidence.kind.trim().is_empty() || evidence.summary.trim().is_empty())
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("evidence некорректен для {}", record.identity),
        ));
    }
    Ok(())
}

fn validate_object(root: &File, record: &AssetRecord) -> Result<(), AssetError> {
    let file = checked_object_file(root, record)?;
    let (sha256, byte_length, format) = hash_file(file)?;
    if sha256 != record.sha256 || byte_length != record.byte_length || format != record.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            format!("файл asset {} не совпадает с manifest", record.identity),
        ));
    }
    Ok(())
}

fn validate_object_directory(root: &File) -> Result<(), AssetError> {
    let objects = open_directory_at(root, OBJECTS_DIR).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                "каталог objects отсутствует в program-owned store",
            )
        } else {
            AssetError::io("не удалось открыть object store", error)
        }
    })?;
    for entry in fs::read_dir(fd_path(&objects))
        .map_err(|error| AssetError::io("не удалось прочитать object store", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать object entry", error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(hash) = name.strip_suffix(".blob") else {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("неожиданный файл в object store: {name}"),
            ));
        };
        validate_hash(hash)?;
        let file = open_regular_at(&objects, entry.file_name(), ErrorCode::MissingAssetFile)?;
        let (actual_hash, _, _) = hash_file(file)?;
        if actual_hash != hash {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                format!("object {name} не соответствует своему SHA-256"),
            ));
        }
        // Незарегистрированный content-addressed object возможен, если процесс
        // завершился после публикации bytes и до manifest. Он не является
        // asset без identity в manifest и не попадает в `full` или `new`.
    }
    Ok(())
}

fn validate_relative_path(actual: &str, expected: &str) -> Result<(), AssetError> {
    let path = Path::new(actual);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "storage_path содержит выход за store root",
        ));
    }
    if actual != expected {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "storage_path не совпадает с content-addressed layout",
        ));
    }
    Ok(())
}

fn checked_object_file(root: &File, record: &AssetRecord) -> Result<File, AssetError> {
    let expected = object_relative_path(&record.sha256);
    validate_relative_path(&record.storage_path, &expected)?;
    let objects = open_directory_at(root, OBJECTS_DIR).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                "каталог objects отсутствует в program-owned store",
            )
        } else {
            AssetError::io("не удалось открыть object store", error)
        }
    })?;
    let name = format!("{}.blob", record.sha256);
    open_regular_at(&objects, &name, ErrorCode::MissingAssetFile)
}

fn object_relative_path(hash: &str) -> String {
    format!("{OBJECTS_DIR}/{hash}.blob")
}

fn validate_hash(hash: &str) -> Result<(), AssetError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "SHA-256 должен состоять из 64 строчных hex-символов",
        ));
    }
    Ok(())
}

fn stage_source(root: &File, mut input: File) -> Result<StagedObject, AssetError> {
    if !input
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить source handle", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "explicit source должен быть обычным файлом без symlink",
        ));
    }
    let (artifact, mut output) = create_temp_file(root, "ingest")?;
    let mut digest = Sha256::new();
    let mut byte_length = 0_u64;
    let mut signature = Vec::with_capacity(12);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| AssetError::io("не удалось прочитать source file", error))?;
        if count == 0 {
            break;
        }
        byte_length = byte_length.checked_add(count as u64).ok_or_else(|| {
            AssetError::new(ErrorCode::IoFailure, "размер source file переполнен")
        })?;
        if signature.len() < 12 {
            let take = (12 - signature.len()).min(count);
            signature.extend_from_slice(&buffer[..take]);
        }
        digest.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| AssetError::io("не удалось записать staging file", error))?;
    }
    output
        .flush()
        .and_then(|()| output.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать staging file", error))?;
    let sha256 = format!("{:x}", digest.finalize());
    Ok(StagedObject {
        artifact,
        sha256,
        byte_length,
        format: DetectedFormat::from_signature(&signature),
    })
}

fn publish_object(staged: &StagedObject, root: &File, hash: &str) -> Result<(), AssetError> {
    let objects = open_directory_at(root, OBJECTS_DIR)
        .map_err(|error| AssetError::io("не удалось открыть object store", error))?;
    let name = format!("{hash}.blob");
    match open_regular_at(&objects, &name, ErrorCode::MissingAssetFile) {
        Ok(existing) => {
            verify_staged_file(staged, existing)?;
            sync_directory(&objects)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            match linkat(
                &staged.artifact.directory,
                &staged.artifact.name,
                &objects,
                &name,
                AtFlags::empty(),
            ) {
                Ok(()) => {
                    let object = open_regular_at(&objects, &name, ErrorCode::MissingAssetFile)?;
                    verify_staged_file(staged, object)?;
                    sync_directory(&objects)
                }
                Err(link_error)
                    if std::io::Error::from(link_error).kind()
                        == std::io::ErrorKind::AlreadyExists =>
                {
                    let existing = open_regular_at(&objects, &name, ErrorCode::MissingAssetFile)?;
                    verify_staged_file(staged, existing)
                }
                Err(link_error) => Err(AssetError::io(
                    "не удалось опубликовать object",
                    std::io::Error::from(link_error),
                )),
            }
        }
        Err(error) => Err(error),
    }
}

fn verify_staged_file(staged: &StagedObject, file: File) -> Result<(), AssetError> {
    let (hash, length, format) = hash_file(
        file.try_clone()
            .map_err(|error| AssetError::io("не удалось открыть опубликованный object", error))?,
    )?;
    if hash != staged.sha256 || length != staged.byte_length || format != staged.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "content-addressed object не совпадает с вычисленным hash",
        ));
    }
    make_readonly(&file)
}

fn make_readonly(file: &File) -> Result<(), AssetError> {
    let metadata = file
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить object permissions", error))?;
    if !metadata.is_file() {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "object должен быть обычным файлом",
        ));
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)
        .map_err(|error| AssetError::io("не удалось защитить object от случайной записи", error))?;
    file.sync_all()
        .map_err(|error| AssetError::io("не удалось синхронизировать object metadata", error))
}

fn save_manifest(root: &File, manifest: &Manifest, initial: bool) -> Result<(), AssetError> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(root, MANIFEST_FILE, &bytes, initial, None)
}

#[cfg(test)]
fn save_manifest_with_test_hook(
    root: &File,
    manifest: &Manifest,
    initial: bool,
    fail_before_commit: &std::sync::atomic::AtomicBool,
) -> Result<(), AssetError> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(
        root,
        MANIFEST_FILE,
        &bytes,
        initial,
        Some(fail_before_commit),
    )
}

fn atomic_write(
    root: &File,
    destination: &str,
    bytes: &[u8],
    initial: bool,
    #[allow(unused_variables)] fail_before_commit: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), AssetError> {
    let (temporary, mut file) = create_temp_file(root, "state")?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать state file", error))?;
    drop(file);

    #[cfg(test)]
    if fail_before_commit.is_some_and(|hook| hook.swap(false, Ordering::SeqCst)) {
        return Err(AssetError::new(
            ErrorCode::IoFailure,
            "тестовая ошибка после подготовки temporary manifest и до canonical commit",
        ));
    }

    let published = if initial {
        linkat(
            &temporary.directory,
            &temporary.name,
            root,
            destination,
            AtFlags::empty(),
        )
    } else {
        renameat(&temporary.directory, &temporary.name, root, destination)
    };
    match published {
        Ok(()) => sync_directory(root),
        Err(error)
            if initial
                && std::io::Error::from(error).kind() == std::io::ErrorKind::AlreadyExists =>
        {
            Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "state file уже существует при инициализации нового store",
            ))
        }
        Err(error) => Err(AssetError::io(
            "не удалось атомарно опубликовать state file",
            std::io::Error::from(error),
        )),
    }
}

fn create_temp_file(root: &File, prefix: &str) -> Result<(TempArtifact, File), AssetError> {
    let directory = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!("{prefix}-{}-{counter}.tmp", std::process::id()));
        match openat(
            &directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                return Ok((TempArtifact { directory, name }, File::from(file)));
            }
            Err(error)
                if std::io::Error::from(error).kind() == std::io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(error) => {
                return Err(AssetError::io(
                    "не удалось создать временный файл",
                    std::io::Error::from(error),
                ));
            }
        }
    }
    Err(AssetError::new(
        ErrorCode::IoFailure,
        "не удалось выбрать свободное имя временного файла",
    ))
}

fn hash_file(mut file: File) -> Result<(String, u64, DetectedFormat), AssetError> {
    let mut digest = Sha256::new();
    let mut byte_length = 0_u64;
    let mut signature = Vec::with_capacity(12);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| AssetError::io("не удалось прочитать object", error))?;
        if count == 0 {
            break;
        }
        byte_length = byte_length.checked_add(count as u64).ok_or_else(|| {
            AssetError::new(ErrorCode::IntegrityMismatch, "размер object переполнен")
        })?;
        if signature.len() < 12 {
            let take = (12 - signature.len()).min(count);
            signature.extend_from_slice(&buffer[..take]);
        }
        digest.update(&buffer[..count]);
    }
    Ok((
        format!("{:x}", digest.finalize()),
        byte_length,
        DetectedFormat::from_signature(&signature),
    ))
}

pub(crate) fn open_source_file(path: &Path) -> Result<File, AssetError> {
    let file = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::SourceMissing,
                format!("explicit source file отсутствует: {}", path.display()),
            )
        } else if is_symlink_error(&error) {
            AssetError::new(
                ErrorCode::SourceNotRegular,
                "explicit source должен быть обычным файлом без symlink",
            )
        } else {
            AssetError::io("не удалось открыть explicit source", error)
        }
    })?;
    let file = File::from(file);
    if !file
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить explicit source", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "explicit source должен быть обычным файлом без symlink",
        ));
    }
    Ok(file)
}

fn open_regular_at(
    directory: &File,
    name: impl rustix::path::Arg,
    missing_code: ErrorCode,
) -> Result<File, AssetError> {
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(missing_code, "файл отсутствует в program-owned store")
        } else if is_symlink_error(&error) {
            AssetError::new(
                ErrorCode::BoundaryViolation,
                "symlink запрещён внутри asset store",
            )
        } else {
            AssetError::io("не удалось открыть файл store", error)
        }
    })?;
    let file = File::from(file);
    if !file
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить файл store", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            "вместо обычного файла найден другой filesystem object",
        ));
    }
    Ok(file)
}

fn resolve_store_root(root: &Path) -> Result<PathBuf, AssetError> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить cwd", error))?
            .join(root)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(AssetError::new(
                    ErrorCode::InvalidStoreRoot,
                    "store root не должен содержать компонент '..'",
                ));
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// Открывает каждый компонент относительно уже открытого родителя без follow
/// symlink; отсутствующие компоненты создаёт через тот же directory handle.
fn open_or_create_store_root(
    path: &Path,
    mut after_component: impl FnMut(&Path),
) -> Result<(File, bool), AssetError> {
    let mut current = File::open("/")
        .map_err(|error| AssetError::io("не удалось открыть filesystem root", error))?;
    let parts: Vec<OsString> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();
    let mut root_created = false;
    let mut expected_parent = PathBuf::from("/");

    for (index, part) in parts.iter().enumerate() {
        if fd_canonical_path(&current)? != expected_parent {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!(
                    "компонент store root изменился во время открытия: {}",
                    path.display()
                ),
            ));
        }
        let checked_path = expected_parent.join(part);
        match open_directory_at(&current, part) {
            Ok(directory) => current = directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match mkdirat(&current, part, Mode::from_raw_mode(0o755)) {
                    Ok(()) => {
                        if index + 1 == parts.len() {
                            root_created = true;
                        }
                    }
                    Err(create_error)
                        if std::io::Error::from(create_error).kind()
                            == std::io::ErrorKind::AlreadyExists => {}
                    Err(create_error) => {
                        return Err(AssetError::io(
                            "не удалось создать компонент store root",
                            std::io::Error::from(create_error),
                        ));
                    }
                }
                current = open_directory_at(&current, part).map_err(|open_error| {
                    if is_symlink_error(&open_error)
                        || (open_error.kind() == std::io::ErrorKind::NotADirectory
                            && fs::symlink_metadata(&checked_path)
                                .is_ok_and(|metadata| metadata.file_type().is_symlink()))
                    {
                        AssetError::new(
                            ErrorCode::BoundaryViolation,
                            format!("store root проходит через symlink: {}", path.display()),
                        )
                    } else if open_error.kind() == std::io::ErrorKind::NotADirectory {
                        AssetError::new(
                            ErrorCode::InvalidStoreRoot,
                            format!(
                                "компонент store root не является каталогом: {}",
                                path.display()
                            ),
                        )
                    } else {
                        AssetError::io("не удалось открыть компонент store root", open_error)
                    }
                })?;
            }
            Err(error) if is_symlink_error(&error) => {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    format!("store root проходит через symlink: {}", path.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                if fs::symlink_metadata(&checked_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        format!("store root проходит через symlink: {}", path.display()),
                    ));
                }
                return Err(AssetError::new(
                    ErrorCode::InvalidStoreRoot,
                    format!(
                        "компонент store root не является каталогом: {}",
                        path.display()
                    ),
                ));
            }
            Err(error) => {
                return Err(AssetError::io("не удалось открыть store root", error));
            }
        }
        expected_parent = checked_path;
        after_component(&expected_parent);
    }

    Ok((current, root_created))
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

pub(crate) fn fd_canonical_path(file: &File) -> Result<PathBuf, AssetError> {
    fs::canonicalize(fd_path(file))
        .map_err(|error| AssetError::io("не удалось разрешить открытый filesystem object", error))
}

fn is_symlink_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(Errno::LOOP.raw_os_error())
}

fn resolve_protected_paths(path: &Path) -> Result<Vec<PathBuf>, AssetError> {
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
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    let mut paths = vec![normalized.clone()];
    match fs::canonicalize(&normalized) {
        Ok(canonical) if canonical != normalized => paths.push(canonical),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AssetError::io("не удалось разрешить protected root", error));
        }
    }
    Ok(paths)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn sort_assets(assets: &mut [AssetRecord]) {
    assets.sort_by(|left, right| left.identity.cmp(&right.identity));
}

fn new_store_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let material = format!("{}:{now}:{counter}", std::process::id());
    format!("{:x}", Sha256::digest(material.as_bytes()))
}

fn sync_directory(directory: &File) -> Result<(), AssetError> {
    directory
        .sync_all()
        .map_err(|error| AssetError::io("не удалось синхронизировать каталог store", error))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use super::*;
    use crate::model::{SemanticDecision, ValidationEvidence};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let count = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("asset-store-unit-{}-{count}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temporary test root");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct VerifiedValidator;

    impl SemanticValidator for VerifiedValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity {
                id: "unit-test".to_owned(),
                version: "1".to_owned(),
            }
        }

        fn validate(
            &self,
            _asset: &AssetRecord,
            bytes: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            let mut contents = Vec::new();
            bytes
                .read_to_end(&mut contents)
                .map_err(|error| ValidatorFailure::new("read_failed", error.to_string()))?;
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "synthetic".to_owned(),
                    summary: format!("{} synthetic bytes", contents.len()),
                    details: None,
                }],
            ))
        }
    }

    #[test]
    fn failed_manifest_publication_cannot_leave_false_verified_state() {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&root)).expect("empty store opens");
        let source = temp.0.join("source.bin");
        fs::write(&source, b"publication failure fixture").expect("source writes");
        store
            .ingest(IngestRequest {
                identity: AssetIdentity::new("generic", "one").unwrap(),
                source_path: source,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .expect("candidate ingests");

        let previous_manifest = fs::read(root.join(MANIFEST_FILE)).expect("manifest exists");
        store.fail_next_manifest_write();
        let error = store
            .validate(SelectionMode::Full, &VerifiedValidator)
            .expect_err("injected publication failure is surfaced");
        assert_eq!(error.code, ErrorCode::IoFailure);
        drop(store);
        assert_eq!(
            fs::read(root.join(MANIFEST_FILE)).expect("canonical manifest remains readable"),
            previous_manifest,
            "failure before rename keeps the previous canonical bytes"
        );
        assert_eq!(
            fs::read_dir(root.join(TEMP_DIR))
                .expect("temporary directory exists")
                .count(),
            0,
            "failed temporary publication is cleaned up"
        );

        let reopened = AssetStore::open(StoreOptions::new(&root)).expect("store remains readable");
        let records = reopened
            .verify_integrity()
            .expect("canonical state remains valid");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lifecycle, LifecycleState::Pending);
        assert!(records[0].validation.is_none());
    }

    fn run_serialization_probe(contested_identity: bool) {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = Arc::new(AssetStore::open(StoreOptions::new(&root)).expect("store opens"));
        let before_count = Arc::new(AtomicUsize::new(0));
        let after_count = Arc::new(AtomicUsize::new(0));
        let (first_locked_tx, first_locked_rx) = mpsc::sync_channel(1);
        let (release_first_tx, release_first_rx) = mpsc::sync_channel(1);
        let (second_probe_tx, second_probe_rx) = mpsc::sync_channel(1);
        let (second_locked_tx, second_locked_rx) = mpsc::sync_channel(1);
        let release_first_rx = std::sync::Mutex::new(release_first_rx);

        let before_count_hook = Arc::clone(&before_count);
        let before_lock = Arc::new(move |lock: &File| {
            if before_count_hook.fetch_add(1, Ordering::SeqCst) == 1 {
                let probe = flock(lock, FlockOperation::NonBlockingLockExclusive);
                let blocked = probe
                    .as_ref()
                    .is_err_and(|error| *error == Errno::WOULDBLOCK);
                if probe.is_ok() {
                    flock(lock, FlockOperation::Unlock).expect("successful lock probe is released");
                }
                second_probe_tx
                    .send(blocked)
                    .expect("main test receives the second writer probe");
            }
        });

        let after_count_hook = Arc::clone(&after_count);
        let after_lock = Arc::new(move |_lock: &File| {
            if after_count_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                first_locked_tx
                    .send(())
                    .expect("main test observes first writer holding the lock");
                release_first_rx
                    .lock()
                    .expect("release receiver mutex is healthy")
                    .recv()
                    .expect("main test releases the first writer");
            } else {
                second_locked_tx
                    .send(())
                    .expect("main test observes second writer acquire the lock");
            }
        });
        *store
            .lock_test_hooks
            .lock()
            .expect("test hook mutex is healthy") = Some(Arc::new(LockTestHooks {
            before_lock,
            after_lock,
        }));

        let first_source = temp.0.join("first.bin");
        let second_source = temp.0.join("second.bin");
        fs::write(&first_source, b"first writer bytes").expect("first source writes");
        fs::write(&second_source, b"second writer bytes").expect("second source writes");
        let first_store = Arc::clone(&store);
        let first = thread::spawn(move || {
            first_store.ingest(IngestRequest {
                identity: AssetIdentity::new(
                    "generic",
                    if contested_identity { "same" } else { "first" },
                )
                .unwrap(),
                source_path: first_source,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
        });
        first_locked_rx
            .recv()
            .expect("first writer entered the exclusive lock section");

        let second_store = Arc::clone(&store);
        let second = thread::spawn(move || {
            second_store.ingest(IngestRequest {
                identity: AssetIdentity::new(
                    "generic",
                    if contested_identity { "same" } else { "second" },
                )
                .unwrap(),
                source_path: second_source,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
        });
        let second_was_blocked = second_probe_rx
            .recv()
            .expect("second writer probes the lock while the first holds it");
        release_first_tx.send(()).expect("release first writer");
        let first_result = first.join().expect("first writer thread completes");
        let second_result = second.join().expect("second writer thread completes");
        second_locked_rx
            .recv()
            .expect("second writer acquires the lock after release");

        assert!(
            second_was_blocked,
            "the second writer must observe the first writer's held exclusive lock"
        );
        assert!(first_result.is_ok());
        if contested_identity {
            assert_eq!(second_result.unwrap_err().code, ErrorCode::IdentityConflict);
            assert_eq!(store.verify_integrity().unwrap().len(), 1);
        } else {
            second_result.expect("different-identity writer succeeds");
            assert_eq!(store.verify_integrity().unwrap().len(), 2);
        }
    }

    #[test]
    fn concurrent_writers_are_serialized_inside_the_mutation_lock() {
        run_serialization_probe(false);
    }

    #[test]
    fn same_identity_writers_have_one_winner_inside_the_mutation_lock() {
        run_serialization_probe(true);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn store_root_component_replacement_is_detected_before_nested_creation() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let parent = temp.0.join("requested-parent");
        let moved_parent = temp.0.join("moved-parent");
        let outside = temp.0.join("outside");
        fs::create_dir(&parent).expect("requested parent exists");
        fs::create_dir(&outside).expect("symlink target exists");
        let requested_root = parent.join("store");
        let mut replaced = false;

        let error = open_or_create_store_root(&requested_root, |opened_component| {
            if !replaced && opened_component == parent {
                fs::rename(&parent, &moved_parent).expect("opened parent moves");
                symlink(&outside, &parent).expect("original pathname becomes a symlink");
                replaced = true;
            }
        })
        .expect_err("the changed parent handle is rejected before creating its child");

        assert!(
            replaced,
            "the test replaced the component after it was opened"
        );
        assert_eq!(error.code, ErrorCode::BoundaryViolation);
        assert!(
            !requested_root.exists(),
            "store state is not created via the new symlink"
        );
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "external target remains untouched"
        );
        assert!(
            fs::read_dir(&moved_parent).unwrap().next().is_none(),
            "pinned original directory remains untouched"
        );
    }
}
