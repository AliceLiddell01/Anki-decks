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
use crate::hashing::{encode_lower_hex, sha256_hex};
use crate::kanji_validator::MAX_MEDIA_BYTES;
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanAttestation, HumanDecision, LifecycleState,
    MANIFEST_SCHEMA_VERSION, Manifest, Provenance, SemanticDecision, SemanticStatus,
    ValidationRecord, ValidatorIdentity,
};
use crate::selection::{SelectionMode, select_assets};
use crate::validation::{SemanticValidator, ValidationAttempt, ValidationReport, ValidatorFailure};

const MANIFEST_FILE: &str = "manifest.json";
const OWNER_FILE: &str = ".owner.json";
const LOCK_FILE: &str = ".lock";
const ASSETS_DIR: &str = "assets";
const TEMP_DIR: &str = ".tmp";
const RUNTIME_DIR: &str = ".runtime";
// Domain-neutral local batch state. Его содержимое принадлежит batch owner;
// generic store проверяет только отдельный directory boundary.
const BATCHES_DIR: &str = "batches";
const REMOVAL_MARKER: &str = "removal.json";
const REMOVAL_BACKUP: &str = "removal.backup";
const TRANSITION_MARKER: &str = "transition.json";
const OWNER_SCHEMA_VERSION: u32 = 1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_REMOVAL_MANIFEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_AFTER_CANONICAL_TRANSITION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Проверенная запись и те же байты, для которых сверена контрольная сумма.
#[derive(Debug, Clone)]
pub struct VerifiedAssetBytes {
    pub record: AssetRecord,
    pub bytes: Vec<u8>,
}

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
    /// Локальное хранилище записей `Pending` и `Quarantined`; оно целиком исключено из Git.
    runtime_handle: File,
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
    /// Exact source CAS перед записью: bytes review не подменяются pending import.
    pub expected_source_sha256: Option<String>,
    /// Доменное расширение identity, которое generic core сохраняет без
    /// интерпретации (например character и Unicode code points для kanji).
    pub domain_metadata: Option<serde_json::Value>,
    /// Для явной замены требуется hash версии, которую вызывающий ожидает.
    pub replace_expected_sha256: Option<String>,
}

/// Запрос acquire→validate→publish: candidate остаётся во временном staging,
/// пока semantic validator не вернул `verified`.
#[derive(Debug, Clone)]
pub struct VerifiedIngestRequest {
    pub identity: AssetIdentity,
    pub bytes: Vec<u8>,
    pub provenance: Provenance,
    pub domain_metadata: Option<serde_json::Value>,
    pub replace_expected_sha256: Option<String>,
}

/// Явное пользовательское решение, защищённое compare-and-swap текущего hash.
#[derive(Debug, Clone)]
pub struct HumanAttestationRequest {
    pub identity: AssetIdentity,
    pub expected_sha256: String,
    pub decision: HumanDecision,
    pub reason: String,
}

/// Итог explicit ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub asset: AssetRecord,
    pub previous: Option<AssetRecord>,
    pub changed: bool,
}

/// Итог атомарной публикации только semantic-verified bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIngestOutcome {
    pub asset: Option<AssetRecord>,
    pub status: SemanticStatus,
    pub evidence: Vec<crate::model::ValidationEvidence>,
    pub sha256: String,
    pub byte_length: u64,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationObject {
    storage_path: String,
    sha256: String,
    format: DetectedFormat,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationTransaction {
    schema_version: u32,
    identity: AssetIdentity,
    previous: Option<PublicationObject>,
    next: PublicationObject,
    staged_name: String,
    backup_name: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemovalTransaction {
    schema_version: u32,
    record: AssetRecord,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionTransaction {
    schema_version: u32,
    identity: AssetIdentity,
    target: TransitionTarget,
    sha256: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransitionTarget {
    Canonical,
    Runtime,
}

#[derive(Debug)]
struct PendingPublication {
    marker_name: OsString,
    transaction: PublicationTransaction,
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
        Self::open_with_creation(options, true)
    }

    /// Открывает существующее принадлежащее asset-store хранилище без создания нового корня.
    /// При открытии инициализирует отсутствующий `.runtime` и переносит туда
    /// прежние записи `Pending` и `Quarantined`, чтобы восстановить границу жизненного цикла.
    pub fn open_existing(options: StoreOptions) -> Result<Self, AssetError> {
        Self::open_with_creation(options, false)
    }

    fn open_with_creation(
        options: StoreOptions,
        create_if_missing: bool,
    ) -> Result<Self, AssetError> {
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

        let (root_handle, root_created) = if create_if_missing {
            open_or_create_store_root(&requested_root, |_| {})?
        } else {
            (open_existing_store_root(&requested_root)?, false)
        };
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
        if !create_if_missing && state != RootState::Owned {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "store root не содержит инициализированный asset store",
            ));
        }
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
        let initialized_on_open = if create_if_missing {
            initialize_or_load(&root_handle, state)?
        } else {
            ensure_dir_entry(&root_handle, TEMP_DIR)?;
            false
        };

        let canonical_manifest = load_manifest(&root_handle)?;
        let runtime_handle =
            open_or_initialize_runtime(&root_handle, &canonical_manifest.store_id)?;
        recover_publications(&root_handle)?;
        recover_publications(&runtime_handle)?;
        let manifest = load_manifest(&root_handle)?;
        let runtime_manifest = load_runtime_manifest(&runtime_handle, &manifest.store_id)?;
        reconcile_runtime_boundary(&root_handle, &runtime_handle, &manifest, &runtime_manifest)?;
        let manifest = load_manifest(&root_handle)?;
        let runtime_manifest = load_runtime_manifest(&runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&root_handle, &manifest)?;
        validate_runtime_manifest(&runtime_handle, &runtime_manifest, &manifest.store_id)?;

        let store = Self {
            root: canonical_root,
            root_handle,
            runtime_handle,
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
        validate_verified_manifest(&self.root_handle, &manifest)?;
        let runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        let mut assets = manifest.assets;
        assets.extend(runtime_manifest.assets);
        assets.sort_by(|left, right| left.identity.cmp(&right.identity));
        lock.unlock()?;
        Ok(assets)
    }

    /// Читает только проверенные записи канонического хранилища, не создавая
    /// хранилище, не восстанавливая его и не обращаясь к `.runtime`.
    /// Проверяются именно возвращаемые байты, а не путь для последующего чтения.
    /// Общая блокировка каталога согласована с ingest/validate; дескрипторы,
    /// открытые с `NOFOLLOW`, не дают выйти за границы хранилища даже при
    /// внешней подмене пути.
    pub fn read_verified(
        root: impl AsRef<Path>,
        identities: &[AssetIdentity],
        expected_validator: &ValidatorIdentity,
    ) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
        Self::read_verified_snapshot(root.as_ref(), identities, expected_validator, |_| {})
    }

    fn read_verified_snapshot(
        root: &Path,
        identities: &[AssetIdentity],
        expected_validator: &ValidatorIdentity,
        mut before_read: impl FnMut(&AssetRecord),
    ) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
        validate_validator_identity(expected_validator)?;
        let requested = resolve_store_root(root)?;
        let directory = open_existing_store_root(&requested)?;
        flock(&directory, FlockOperation::LockShared)
            .map_err(|e| AssetError::io("блокировка чтения набора изображений", e.into()))?;
        let owner = read_owner_marker(&directory)?;
        let manifest = load_manifest(&directory)?;
        if owner.store_id != manifest.store_id {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "store_id отличается от owner",
            ));
        }
        validate_verified_manifest(&directory, &manifest)?;
        let mut result = Vec::new();
        for identity in identities {
            identity
                .validate()
                .map_err(|e| AssetError::new(ErrorCode::InvalidIdentity, e))?;
            let record = manifest
                .assets
                .iter()
                .find(|a| &a.identity == identity)
                .ok_or_else(|| {
                    AssetError::with_details(
                        ErrorCode::MissingAssetFile,
                        "в каноническом хранилище нет проверенного изображения",
                        serde_json::json!({"identity": identity}),
                    )
                })?;
            if !record.is_trusted_for(expected_validator) {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "изображение проверено другой версией валидатора",
                ));
            }
            before_read(record);
            let human_approved = record.current_human_decision() == Some(HumanDecision::Approve);
            if human_approved && record.byte_length > MAX_MEDIA_BYTES as u64 {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "изображение, одобренное человеком, превышает установленный предел размера",
                ));
            }
            let file = checked_asset_file(&directory, record)?;
            let bytes = if human_approved {
                read_bounded_asset_bytes(file, MAX_MEDIA_BYTES, "чтение проверенных байтов")?
            } else {
                let mut bytes = Vec::new();
                file.take(record.byte_length.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|error| AssetError::io("чтение проверенных байтов", error))?;
                bytes
            };
            let format = DetectedFormat::from_signature(&bytes);
            if format != record.format
                || bytes.len() as u64 != record.byte_length
                || sha256_hex(&bytes) != record.sha256
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "прочитанные байты не совпадают с проверенной записью",
                ));
            }
            if human_approved {
                validate_image_decode(&bytes)?;
            }
            result.push(VerifiedAssetBytes {
                record: record.clone(),
                bytes,
            });
        }
        Ok(result)
    }

    /// Проверяет публикуемое в Git представление корпуса кандзи без создания,
    /// восстановления или изменения файлов. Отсутствующий корпус допустим.
    pub fn verify_publishable_corpus(
        root: impl AsRef<Path>,
        expected_validator: &ValidatorIdentity,
    ) -> Result<(), AssetError> {
        validate_validator_identity(expected_validator)?;
        let requested_root = resolve_store_root(root.as_ref())?;
        let root_handle = match open_existing_store_root(&requested_root) {
            Ok(root) => root,
            Err(error) if error.code == ErrorCode::StoreMissing => return Ok(()),
            Err(error) => return Err(error),
        };
        // Все изменяющие хранилище операции сначала берут исключительную
        // блокировку flock каталога. Эта проверка только для чтения берёт
        // совместную блокировку того же inode; `.lock` игнорируется Git и может
        // отсутствовать в чистой копии репозитория.
        let directory_lock = open_directory_at(&root_handle, ".")
            .map_err(|error| AssetError::io("не удалось открыть каталог корпуса кандзи", error))?;
        flock(&directory_lock, FlockOperation::LockShared).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать каталог корпуса кандзи для проверки",
                std::io::Error::from(error),
            )
        })?;
        let result = (|| {
            let names = inspect_top_level(&root_handle, ErrorCode::UnexpectedPath)?;
            if !names.contains(OWNER_FILE) || !names.contains(MANIFEST_FILE) {
                return Err(AssetError::new(
                    ErrorCode::StoreNotOwned,
                    "в публикуемом корпусе отсутствует файл `.owner.json` или `manifest.json`",
                ));
            }
            let owner = read_owner_marker(&root_handle)?;
            let manifest = load_manifest(&root_handle)?;
            if manifest.store_id != owner.store_id {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "значение `store_id` в публикуемом манифесте не совпадает с маркером владельца",
                ));
            }
            validate_publishable_manifest(&root_handle, &manifest, expected_validator)?;
            ensure_directory_empty(&root_handle, TEMP_DIR)?;
            if names.contains(RUNTIME_DIR) {
                let runtime = open_directory_at(&root_handle, RUNTIME_DIR)
                    .map_err(|error| directory_entry_error(RUNTIME_DIR, error))?;
                let runtime_manifest = load_runtime_manifest(&runtime, &manifest.store_id)?;
                validate_runtime_manifest(&runtime, &runtime_manifest, &manifest.store_id)?;
                let published: BTreeSet<_> = manifest
                    .assets
                    .iter()
                    .map(|asset| &asset.identity)
                    .collect();
                if runtime_manifest
                    .assets
                    .iter()
                    .any(|asset| published.contains(&asset.identity))
                {
                    return Err(AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        "локальный кандидат пересекается с опубликованной идентичностью",
                    ));
                }
            }
            Ok(())
        })();
        let unlock = flock(&directory_lock, FlockOperation::Unlock).map_err(|error| {
            AssetError::io(
                "не удалось снять блокировку каталога корпуса кандзи",
                std::io::Error::from(error),
            )
        });
        result?;
        unlock?;
        Ok(())
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

    /// Проверяет bytes до публикации и добавляет в manifest только `verified`.
    /// Ошибка validator'а и любой non-verified статус оставляют canonical state
    /// неизменным. Повтор текущих hash/version возвращает idempotent success.
    pub fn ingest_verified<V: SemanticValidator>(
        &self,
        request: VerifiedIngestRequest,
        validator: &V,
    ) -> Result<VerifiedIngestOutcome, AssetError> {
        request
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if request.provenance.source_kind.trim().is_empty()
            || request.provenance.source_name.trim().is_empty()
            || request.provenance.source_name.contains(['/', '\\'])
        {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "Yarxi provenance должен содержать тип и имя источника без path",
            ));
        }
        let validator_id = validator.identity();
        validate_validator_identity(&validator_id)?;
        let staged = stage_bytes(&self.root_handle, &request.bytes)?;
        let lock = self.lock_exclusive()?;
        recover_publications(&self.root_handle)?;
        recover_publications(&self.runtime_handle)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
        )?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        let existing = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == request.identity)
            .or_else(|| {
                runtime_manifest
                    .assets
                    .iter()
                    .find(|asset| asset.identity == request.identity)
            })
            .cloned();
        if let Some(current) = &existing {
            if current.sha256 == staged.sha256
                && current.current_human_decision() == Some(HumanDecision::Reject)
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    "эти bytes явно отклонены человеком; требуется новое решение или другой hash",
                ));
            }
            if current.sha256 == staged.sha256 && current.is_trusted_for(&validator_id) {
                let outcome = VerifiedIngestOutcome {
                    asset: Some(current.clone()),
                    status: SemanticStatus::Verified,
                    evidence: current
                        .validation
                        .as_ref()
                        .map(|record| record.evidence.clone())
                        .unwrap_or_default(),
                    sha256: current.sha256.clone(),
                    byte_length: current.byte_length,
                    changed: false,
                };
                drop(staged);
                lock.unlock()?;
                return Ok(outcome);
            }
            if current.sha256 != staged.sha256
                && request.replace_expected_sha256.as_deref() != Some(current.sha256.as_str())
            {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    format!("identity {} уже привязана к другому hash", current.identity),
                    serde_json::json!({
                        "identity": current.identity,
                        "existing_sha256": current.sha256,
                        "candidate_sha256": staged.sha256,
                    }),
                ));
            }
        } else if request.replace_expected_sha256.is_some() {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "ожидаемый hash замены указан для отсутствующей identity",
            ));
        }

        let storage_path = canonical_asset_path(&request.identity, &staged.sha256, staged.format);
        let mut record = AssetRecord {
            identity: request.identity,
            storage_path,
            sha256: staged.sha256.clone(),
            byte_length: staged.byte_length,
            format: staged.format,
            provenance: request.provenance,
            lifecycle: LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: request.domain_metadata,
        };
        record.human_attestation = existing
            .as_ref()
            .filter(|current| current.sha256 == record.sha256)
            .and_then(|current| current.human_attestation.clone());
        let mut candidate = std::io::Cursor::new(request.bytes.as_slice());
        let decision = validator
            .validate(&record, &mut candidate)
            .map_err(|failure| {
                let blocker = stable_failure_code(&failure);
                AssetError::with_details(
                    ErrorCode::ValidatorFailure,
                    failure.message,
                    serde_json::json!({ "blocker": blocker }),
                )
            })?;
        validate_decision(&decision)?;
        if decision.status != SemanticStatus::Verified {
            let outcome = VerifiedIngestOutcome {
                asset: None,
                status: decision.status,
                evidence: decision.evidence.clone(),
                sha256: staged.sha256.clone(),
                byte_length: staged.byte_length,
                changed: false,
            };
            drop(staged);
            lock.unlock()?;
            return Ok(outcome);
        }
        record.lifecycle = LifecycleState::Verified;
        record.validation = Some(ValidationRecord {
            status: SemanticStatus::Verified,
            validator: validator_id,
            content_sha256: staged.sha256.clone(),
            evidence: decision.evidence.clone(),
        });
        let previous_canonical = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == record.identity)
            .cloned();
        let previous_runtime = runtime_manifest
            .assets
            .iter()
            .find(|asset| asset.identity == record.identity)
            .cloned();
        if previous_runtime.is_some() {
            begin_transition(
                &self.root_handle,
                &record.identity,
                TransitionTarget::Canonical,
                &record.sha256,
            )?;
        }
        commit_asset_record(
            &self.root_handle,
            &staged,
            previous_canonical.as_ref(),
            &record,
            &mut manifest,
            |root, manifest| {
                #[cfg(test)]
                {
                    save_manifest_with_test_hook(
                        root,
                        manifest,
                        false,
                        &self.fail_next_manifest_write,
                    )
                }
                #[cfg(not(test))]
                {
                    save_manifest(root, manifest, false)
                }
            },
        )?;
        if let Some(candidate) = previous_runtime {
            #[cfg(test)]
            if FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.replace(false)) {
                return Err(AssetError::new(
                    ErrorCode::IoFailure,
                    "тестовый сбой после canonical commit",
                ));
            }
            remove_record_from_area(&self.runtime_handle, &mut runtime_manifest, &candidate)?;
            clear_transition(&self.root_handle)?;
        }
        let sha256 = staged.sha256.clone();
        let byte_length = staged.byte_length;
        drop(staged);
        lock.unlock()?;
        Ok(VerifiedIngestOutcome {
            asset: Some(record),
            status: SemanticStatus::Verified,
            evidence: decision.evidence,
            sha256,
            byte_length,
            changed: true,
        })
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
        if let Some(expected) = &request.expected_source_sha256 {
            validate_hash(expected)?;
        }
        let staged = stage_source(&self.runtime_handle, source)?;
        if request
            .expected_source_sha256
            .as_ref()
            .is_some_and(|expected| expected != &staged.sha256)
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "source bytes изменились после exact candidate review",
            ));
        }
        let lock = self.lock_exclusive()?;
        recover_publications(&self.root_handle)?;
        recover_publications(&self.runtime_handle)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
        )?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        let source_name = request
            .source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unnamed")
            .to_owned();

        let existing_runtime = runtime_manifest
            .assets
            .iter()
            .find(|asset| asset.identity == request.identity)
            .cloned();
        let existing = existing_runtime.clone().or_else(|| {
            manifest
                .assets
                .iter()
                .find(|asset| asset.identity == request.identity)
                .cloned()
        });

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

        let storage_path = canonical_asset_path(&request.identity, &staged.sha256, staged.format);
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
            human_attestation: None,
            domain_metadata: request.domain_metadata,
        };
        let previous_canonical = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == record.identity)
            .cloned();
        if previous_canonical.is_some() {
            begin_transition(
                &self.root_handle,
                &record.identity,
                TransitionTarget::Runtime,
                &record.sha256,
            )?;
        }
        commit_asset_record(
            &self.runtime_handle,
            &staged,
            existing_runtime.as_ref(),
            &record,
            &mut runtime_manifest,
            |root, manifest| {
                #[cfg(test)]
                {
                    save_manifest_with_test_hook(
                        root,
                        manifest,
                        false,
                        &self.fail_next_manifest_write,
                    )
                }
                #[cfg(not(test))]
                {
                    save_manifest(root, manifest, false)
                }
            },
        )?;
        if let Some(canonical) = previous_canonical {
            remove_record_from_area(&self.root_handle, &mut manifest, &canonical)?;
            clear_transition(&self.root_handle)?;
        }
        validate_verified_manifest(&self.root_handle, &manifest)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
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
        validate_verified_manifest(&self.root_handle, &manifest)?;
        let runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        let mut records = manifest.assets;
        records.extend(runtime_manifest.assets);
        let assets = select_assets(&records, mode, validator)
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
        self.validate_selection(mode, validator, None)
    }

    /// Проверяет одну identity/hash и сохраняет исходное automated evidence.
    /// Scope не распространяется на соседние unresolved candidates других batch.
    pub fn validate_exact<V: SemanticValidator>(
        &self,
        identity: &AssetIdentity,
        expected_sha256: &str,
        validator: &V,
    ) -> Result<ValidationReport, AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        validate_hash(expected_sha256)?;
        self.validate_selection(
            SelectionMode::Full,
            validator,
            Some((identity, expected_sha256)),
        )
    }

    fn validate_selection<V: SemanticValidator>(
        &self,
        mode: SelectionMode,
        validator: &V,
        exact: Option<(&AssetIdentity, &str)>,
    ) -> Result<ValidationReport, AssetError> {
        let validator_id = validator.identity();
        validate_validator_identity(&validator_id)?;
        let lock = self.lock_exclusive()?;
        recover_publications(&self.root_handle)?;
        recover_publications(&self.runtime_handle)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
        )?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        let mut records = manifest.assets.clone();
        records.extend(runtime_manifest.assets.clone());
        let selected: Vec<_> = select_assets(&records, mode, &validator_id)
            .into_iter()
            .cloned()
            .collect();
        let selected = if let Some((identity, expected_sha256)) = exact {
            let record = selected
                .into_iter()
                .find(|record| &record.identity == identity)
                .ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::MissingAssetFile,
                        "exact validation identity отсутствует",
                    )
                })?;
            if record.sha256 != expected_sha256 {
                return Err(AssetError::new(
                    ErrorCode::IdentityConflict,
                    "exact validation candidate изменился",
                ));
            }
            vec![record]
        } else {
            selected
        };
        let mut report = ValidationReport::new(mode, validator_id.clone());
        report.considered = selected.len();

        // Сначала выполняются все вызовы предметной проверки и проверяются
        // доказательства (`evidence`). До этого места манифесты и опубликованные данные не меняются.
        let mut decisions: Vec<(AssetRecord, SemanticDecision)> = Vec::new();
        for record in selected {
            let area = if record.lifecycle == LifecycleState::Verified {
                &self.root_handle
            } else {
                &self.runtime_handle
            };
            let mut file = checked_asset_file(area, &record)?;
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

        for (old_record, decision) in decisions {
            let validation = ValidationRecord {
                status: decision.status,
                validator: validator_id.clone(),
                content_sha256: old_record.sha256.clone(),
                evidence: decision.evidence.clone(),
            };
            let mut new_record = old_record.clone();
            new_record.validation = Some(validation);
            let lifecycle = if new_record.effective_status() == Some(SemanticStatus::Verified) {
                LifecycleState::Verified
            } else {
                LifecycleState::Quarantined
            };
            new_record.lifecycle = lifecycle;
            let changed = new_record.lifecycle != old_record.lifecycle
                || new_record.validation != old_record.validation;
            if changed {
                self.commit_lifecycle_change(
                    &old_record,
                    &new_record,
                    &mut manifest,
                    &mut runtime_manifest,
                )?;
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

        report
            .attempts
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        report.blockers.sort();
        report.blockers.dedup();
        report.changed = report
            .attempts
            .iter()
            .filter(|attempt| attempt.changed)
            .count();
        validate_verified_manifest(&self.root_handle, &load_manifest(&self.root_handle)?)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        lock.unlock()?;
        Ok(report)
    }

    /// Применяет явное решение человека к текущим точным байтам. Подтверждение
    /// требует полной проверки декодером, затем использует тот же атомарный
    /// переход жизненного цикла, что автоматическая проверка. Её данные сохраняются.
    pub fn attest(&self, request: HumanAttestationRequest) -> Result<IngestOutcome, AssetError> {
        request
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        validate_hash(&request.expected_sha256)?;
        if request.reason.trim().is_empty() {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "решение человека требует явного основания",
            ));
        }
        let lock = self.lock_exclusive()?;
        recover_publications(&self.root_handle)?;
        recover_publications(&self.runtime_handle)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
        )?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest)?;
        validate_runtime_manifest(&self.runtime_handle, &runtime_manifest, &self.store_id)?;
        let previous = manifest
            .assets
            .iter()
            .chain(&runtime_manifest.assets)
            .find(|asset| asset.identity == request.identity)
            .cloned()
            .ok_or_else(|| {
                AssetError::new(ErrorCode::MissingAssetFile, "identity отсутствует в store")
            })?;
        if previous.sha256 != request.expected_sha256 {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "candidate изменился после review; требуется решение для текущего hash",
            ));
        }
        if request.decision == HumanDecision::Approve {
            let Some(automated_status) = previous.current_validation_status() else {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "подтверждение человеком требует автоматическую запись ValidationRecord для текущего SHA-256",
                ));
            };
            if automated_status == SemanticStatus::Corrupt {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    "подтверждение человеком не может отменить статус CORRUPT; требуется исправленный кандидат",
                ));
            }
            if previous.byte_length > MAX_MEDIA_BYTES as u64 {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    "подтверждение человеком превышает установленный предел размера изображения",
                ));
            }
            let area = if previous.lifecycle == LifecycleState::Verified {
                &self.root_handle
            } else {
                &self.runtime_handle
            };
            let bytes = read_bounded_asset_bytes(
                checked_asset_file(area, &previous)?,
                MAX_MEDIA_BYTES,
                "чтение кандидата для подтверждения человеком",
            )?;
            if sha256_hex(&bytes) != previous.sha256
                || bytes.len() as u64 != previous.byte_length
                || DetectedFormat::from_signature(&bytes) != previous.format
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "candidate изменился при чтении",
                ));
            }
            validate_image_decode(&bytes)?;
        }
        let mut record = previous.clone();
        record.human_attestation = Some(HumanAttestation {
            identity: request.identity,
            content_sha256: request.expected_sha256,
            decision: request.decision,
            reason: request.reason,
        });
        record.lifecycle = if record.effective_status() == Some(SemanticStatus::Verified) {
            LifecycleState::Verified
        } else {
            LifecycleState::Quarantined
        };
        let changed = record != previous;
        if changed {
            self.commit_lifecycle_change(&previous, &record, &mut manifest, &mut runtime_manifest)?;
        }
        lock.unlock()?;
        Ok(IngestOutcome {
            asset: record,
            previous: Some(previous),
            changed,
        })
    }

    fn commit_lifecycle_change(
        &self,
        old_record: &AssetRecord,
        new_record: &AssetRecord,
        manifest: &mut Manifest,
        runtime_manifest: &mut Manifest,
    ) -> Result<(), AssetError> {
        manifest.schema_version = MANIFEST_SCHEMA_VERSION;
        runtime_manifest.schema_version = MANIFEST_SCHEMA_VERSION;
        match (old_record.lifecycle, new_record.lifecycle) {
            (LifecycleState::Verified, LifecycleState::Verified) => {
                replace_manifest_record(manifest, new_record)?;
                save_manifest_revision(
                    &self.root_handle,
                    manifest,
                    #[cfg(test)]
                    Some(&self.fail_next_manifest_write),
                    #[cfg(not(test))]
                    None,
                )?;
            }
            (LifecycleState::Verified, LifecycleState::Quarantined) => {
                let source = checked_asset_file(&self.root_handle, old_record)?;
                let staged = stage_source(&self.runtime_handle, source)?;
                begin_transition(
                    &self.root_handle,
                    &new_record.identity,
                    TransitionTarget::Runtime,
                    &new_record.sha256,
                )?;
                let previous_runtime = runtime_manifest
                    .assets
                    .iter()
                    .find(|asset| asset.identity == new_record.identity)
                    .cloned();
                commit_asset_record(
                    &self.runtime_handle,
                    &staged,
                    previous_runtime.as_ref(),
                    new_record,
                    runtime_manifest,
                    |root, manifest| save_manifest(root, manifest, false),
                )?;
                remove_record_from_area(&self.root_handle, manifest, old_record)?;
                clear_transition(&self.root_handle)?;
            }
            (LifecycleState::Pending | LifecycleState::Quarantined, LifecycleState::Verified) => {
                let source = checked_asset_file(&self.runtime_handle, old_record)?;
                let staged = stage_source(&self.root_handle, source)?;
                begin_transition(
                    &self.root_handle,
                    &new_record.identity,
                    TransitionTarget::Canonical,
                    &new_record.sha256,
                )?;
                let previous = manifest
                    .assets
                    .iter()
                    .find(|asset| asset.identity == new_record.identity)
                    .cloned();
                commit_asset_record(
                    &self.root_handle,
                    &staged,
                    previous.as_ref(),
                    new_record,
                    manifest,
                    |root, manifest| {
                        #[cfg(test)]
                        {
                            save_manifest_with_test_hook(
                                root,
                                manifest,
                                false,
                                &self.fail_next_manifest_write,
                            )
                        }
                        #[cfg(not(test))]
                        {
                            save_manifest(root, manifest, false)
                        }
                    },
                )?;
                #[cfg(test)]
                if FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.replace(false)) {
                    return Err(AssetError::new(
                        ErrorCode::IoFailure,
                        "тестовый сбой после canonical commit",
                    ));
                }
                remove_record_from_area(&self.runtime_handle, runtime_manifest, old_record)?;
                clear_transition(&self.root_handle)?;
            }
            (
                LifecycleState::Pending | LifecycleState::Quarantined,
                LifecycleState::Quarantined,
            ) => {
                replace_manifest_record(runtime_manifest, new_record)?;
                save_manifest_revision(&self.runtime_handle, runtime_manifest, None)?;
            }
            _ => {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "недопустимый переход состояния жизненного цикла при проверке",
                ));
            }
        }
        Ok(())
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
            let _assets = ensure_dir_entry(root, ASSETS_DIR)?;
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
            ensure_dir_entry(root, ASSETS_DIR)?;
            ensure_dir_entry(root, TEMP_DIR)?;
            Ok(false)
        }
    }
}

fn open_or_initialize_runtime(root: &File, store_id: &str) -> Result<File, AssetError> {
    let runtime = ensure_dir_entry(root, RUNTIME_DIR)?;
    let names = inspect_area_top_level(&runtime, ErrorCode::StoreNotOwned, true)?;
    if names.is_empty() {
        ensure_dir_entry(&runtime, ASSETS_DIR)?;
        ensure_dir_entry(&runtime, TEMP_DIR)?;
        write_owner_marker(&runtime, store_id)?;
        save_manifest(&runtime, &Manifest::empty(store_id.to_owned()), true)?;
    } else {
        if !names.contains(OWNER_FILE) || !names.contains(MANIFEST_FILE) {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "непустой корневой каталог `.runtime` без файлов `.owner.json` и `manifest.json` не принадлежит `asset-store`",
            ));
        }
        let owner = read_owner_marker(&runtime)?;
        if owner.store_id != store_id {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "значение `store_id` в маркере владельца `.runtime` не совпадает со значением в каноническом манифесте",
            ));
        }
        let manifest = load_runtime_manifest(&runtime, store_id)?;
        check_schema(&manifest)?;
        ensure_dir_entry(&runtime, ASSETS_DIR)?;
        ensure_dir_entry(&runtime, TEMP_DIR)?;
    }
    inspect_area_top_level(&runtime, ErrorCode::UnexpectedPath, true)?;
    Ok(runtime)
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
    inspect_area_top_level(root, unknown_code, false)
}

fn inspect_area_top_level(
    root: &File,
    unknown_code: ErrorCode,
    runtime_extensions: bool,
) -> Result<BTreeSet<String>, AssetError> {
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
            LOCK_FILE | OWNER_FILE | MANIFEST_FILE | ASSETS_DIR | TEMP_DIR | RUNTIME_DIR
        ) && !(runtime_extensions && name == BATCHES_DIR)
        {
            return Err(AssetError::new(
                unknown_code,
                format!("неожиданный файл в store root: {name}"),
            ));
        }
        if runtime_extensions && name == BATCHES_DIR {
            // Открываем relative to pinned runtime fd с DIRECTORY|NOFOLLOW:
            // pathname metadata недостаточно при конкурентной подмене.
            open_directory_at(root, BATCHES_DIR)
                .map_err(|error| directory_entry_error(BATCHES_DIR, error))?;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AssetError::io("не удалось проверить store entry", error))?;
        if metadata.file_type().is_symlink() {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!("symlink запрещён в store root: {name}"),
            ));
        }
        if matches!(name.as_str(), ASSETS_DIR | TEMP_DIR | RUNTIME_DIR) && !metadata.is_dir() {
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

fn ensure_directory_empty(root: &File, name: &str) -> Result<(), AssetError> {
    let directory = match open_directory_at(root, name) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(directory_entry_error(name, error)),
    };
    let mut entries = fs::read_dir(fd_path(&directory))
        .map_err(|error| AssetError::io(format!("не удалось прочитать {name}"), error))?;
    if entries.next().is_some() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("в публикуемом хранилище остались незавершённые временные файлы в {name}"),
        ));
    }
    Ok(())
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
    load_owned_manifest(root)
}

// Recovery работает с уже проверенным canonical либо runtime directory handle.
// Имена расширений проверяет вызывающий area-specific loader, а owner/schema
// проверяются повторно при каждом чтении, включая восстановление транзакций.
fn load_owned_manifest(root: &File) -> Result<Manifest, AssetError> {
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

fn load_runtime_manifest(root: &File, store_id: &str) -> Result<Manifest, AssetError> {
    inspect_area_top_level(root, ErrorCode::UnexpectedPath, true)?;
    let owner = read_owner_marker(root)?;
    if owner.store_id != store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "значение `store_id` в маркере владельца не совпадает со значением в каноническом манифесте",
        ));
    }
    let manifest = read_manifest_file(root)?;
    check_schema(&manifest)?;
    if manifest.store_id != store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "значение `store_id` в локальном манифесте не совпадает со значением в каноническом манифесте",
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
    if schema_version != 3 && schema_version != u64::from(MANIFEST_SCHEMA_VERSION) {
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
    if manifest.schema_version != 3 && manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "manifest schema_version {} не поддерживается (ожидается {})",
                manifest.schema_version, MANIFEST_SCHEMA_VERSION
            ),
        ));
    }
    if manifest.schema_version == 3
        && manifest
            .assets
            .iter()
            .any(|asset| asset.human_attestation.is_some())
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "schema v3 не поддерживает human attestation",
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
    let mut registered_paths = BTreeSet::new();
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
        let expected_path = canonical_asset_path(&record.identity, &record.sha256, record.format);
        validate_relative_path(&record.storage_path, &expected_path)?;
        if let Some(validation) = &record.validation {
            validate_validation_record(record, validation)?;
        }
        if let Some(attestation) = &record.human_attestation {
            validate_hash(&attestation.content_sha256)?;
            if attestation.identity != record.identity
                || attestation.content_sha256 != record.sha256
                || attestation.reason.trim().is_empty()
            {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "human attestation не привязана к текущим identity/hash или не содержит основания",
                ));
            }
            if attestation.decision == HumanDecision::Approve {
                if !record.has_current_validation() {
                    return Err(AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        format!(
                            "подтверждение человеком не содержит автоматическую запись ValidationRecord для текущего SHA-256: {}",
                            record.identity
                        ),
                    ));
                }
                if record.byte_length > MAX_MEDIA_BYTES as u64 {
                    return Err(AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        format!(
                            "изображение, одобренное человеком, превышает установленный предел размера: {}",
                            record.identity
                        ),
                    ));
                }
            }
        }
        let expected_lifecycle = match record.effective_status() {
            Some(SemanticStatus::Verified) => LifecycleState::Verified,
            Some(_) => LifecycleState::Quarantined,
            None => LifecycleState::Pending,
        };
        if record.lifecycle != expected_lifecycle {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "lifecycle и effective semantic decision не согласованы для {}",
                    record.identity
                ),
            ));
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
        validate_asset(root, record)?;
        registered_paths.insert(record.storage_path.clone());
    }
    validate_asset_directory(root, &registered_paths)?;
    Ok(())
}

fn validate_verified_manifest(root: &File, manifest: &Manifest) -> Result<(), AssetError> {
    validate_manifest(root, manifest)?;
    if manifest.assets.iter().any(|asset| {
        asset.lifecycle != LifecycleState::Verified
            || asset.effective_status() != Some(SemanticStatus::Verified)
    }) {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "канонический манифест допускает только effective verified записи",
        ));
    }
    Ok(())
}

fn validate_runtime_manifest(
    root: &File,
    manifest: &Manifest,
    store_id: &str,
) -> Result<(), AssetError> {
    if manifest.store_id != store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "значение `store_id` в локальном манифесте не совпадает со значением в каноническом манифесте",
        ));
    }
    validate_manifest(root, manifest)?;
    if manifest
        .assets
        .iter()
        .any(|asset| asset.lifecycle == LifecycleState::Verified)
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "локальный манифест не может содержать записи со статусом `verified`",
        ));
    }
    Ok(())
}

fn validate_publishable_manifest(
    root: &File,
    manifest: &Manifest,
    expected_validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    validate_verified_manifest(root, manifest)?;
    for asset in &manifest.assets {
        if !asset.is_trusted_for(expected_validator) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "asset {} проверен другой версией валидатора",
                    asset.identity
                ),
            ));
        }
        if asset.identity.namespace != "kanji" || kanji_character(&asset.identity).is_none() {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "публикуемый корпус кандзи содержит чужую идентичность {}",
                    asset.identity
                ),
            ));
        }
    }
    validate_asset_directory_exact(root, manifest)?;
    Ok(())
}

fn reconcile_runtime_boundary(
    root: &File,
    runtime: &File,
    canonical_manifest: &Manifest,
    runtime_manifest: &Manifest,
) -> Result<(), AssetError> {
    let mut canonical = canonical_manifest.clone();
    let mut local = runtime_manifest.clone();
    let transition = load_transition(root)?;

    // Старые версии хранили все состояния жизненного цикла в публикуемом
    // манифесте. Сначала переносим байты в игнорируемую `.runtime`-область,
    // затем удаляем прежние записи.
    let legacy_candidates: Vec<_> = canonical
        .assets
        .iter()
        .filter(|asset| asset.lifecycle != LifecycleState::Verified)
        .cloned()
        .collect();
    for candidate in legacy_candidates {
        if local
            .assets
            .iter()
            .any(|asset| asset.identity == candidate.identity)
        {
            continue;
        }
        let source = checked_asset_file(root, &candidate)?;
        let staged = stage_source(runtime, source)?;
        commit_asset_record(
            runtime,
            &staged,
            None,
            &candidate,
            &mut local,
            |root, manifest| save_manifest(root, manifest, false),
        )?;
    }

    // Совпадающая идентичность в `.runtime` и каноническом манифесте означает,
    // что перенос в карантин или замена кандидатом прервались. Сохраняем
    // локальное состояние и завершаем удаление старой опубликованной записи.
    // Проверка публикации отклоняет такое совпадение до завершения восстановления.
    let overlaps: Vec<_> = canonical
        .assets
        .iter()
        .filter(|asset| {
            local
                .assets
                .iter()
                .any(|candidate| candidate.identity == asset.identity)
        })
        .cloned()
        .collect();
    for previous in overlaps {
        if let Some(marker) = &transition
            && marker.identity == previous.identity
            && matches!(marker.target, TransitionTarget::Canonical)
        {
            let target = canonical
                .assets
                .iter()
                .find(|asset| asset.identity == marker.identity);
            if target.is_none_or(|asset| asset.sha256 != marker.sha256) {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "transition marker не совпадает с canonical asset",
                ));
            }
            let previous_runtime = local
                .assets
                .iter()
                .find(|asset| asset.identity == previous.identity)
                .cloned()
                .ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        "runtime asset исчез при восстановлении перехода",
                    )
                })?;
            remove_record_from_area(runtime, &mut local, &previous_runtime)?;
        } else {
            remove_record_from_area(root, &mut canonical, &previous)?;
        }
    }

    if let Some(marker) = &transition {
        let target = match marker.target {
            TransitionTarget::Canonical => &canonical,
            TransitionTarget::Runtime => &local,
        };
        if let Some(asset) = target
            .assets
            .iter()
            .find(|asset| asset.identity == marker.identity)
            && asset.sha256 != marker.sha256
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "transition marker не совпадает с целевым asset",
            ));
        }
        clear_transition(root)?;
    }

    cleanup_matching_orphans(root, &canonical, &local)?;
    cleanup_matching_orphans(runtime, &local, &canonical)?;
    validate_verified_manifest(root, &canonical)?;
    validate_runtime_manifest(runtime, &local, &canonical.store_id)?;
    Ok(())
}

fn cleanup_matching_orphans(
    area: &File,
    current: &Manifest,
    other: &Manifest,
) -> Result<(), AssetError> {
    let registered: BTreeSet<_> = current
        .assets
        .iter()
        .map(|asset| asset.storage_path.as_str())
        .collect();
    let assets = open_directory_at(area, ASSETS_DIR)
        .map_err(|error| directory_entry_error(ASSETS_DIR, error))?;
    for entry in fs::read_dir(fd_path(&assets))
        .map_err(|error| AssetError::io("не удалось прочитать каталог `assets`", error))?
    {
        let entry = entry.map_err(|error| {
            AssetError::io("не удалось прочитать запись в каталоге `assets`", error)
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{ASSETS_DIR}/{name}");
        if registered.contains(path.as_str()) {
            continue;
        }
        if let Some(record) = other.assets.iter().find(|asset| asset.storage_path == path) {
            remove_if_hash(
                &assets,
                &name,
                &record.sha256,
                "запись о незавершённом переносе актива",
            )?;
        }
    }
    sync_directory(&assets)
}

fn replace_manifest_record(
    manifest: &mut Manifest,
    record: &AssetRecord,
) -> Result<(), AssetError> {
    let target = manifest
        .assets
        .iter_mut()
        .find(|asset| asset.identity == record.identity)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("манифест потерял запись {}", record.identity),
            )
        })?;
    *target = record.clone();
    sort_assets(&mut manifest.assets);
    Ok(())
}

fn save_manifest_revision(
    root: &File,
    manifest: &mut Manifest,
    #[cfg(test)] fail_before_commit: Option<&std::sync::atomic::AtomicBool>,
    #[cfg(not(test))] _fail_before_commit: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), AssetError> {
    manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "поле `revision` в манифесте переполнено",
        )
    })?;
    #[cfg(test)]
    if let Some(fail_before_commit) = fail_before_commit {
        return save_manifest_with_test_hook(root, manifest, false, fail_before_commit);
    }
    save_manifest(root, manifest, false)
}

fn remove_record_from_area(
    root: &File,
    manifest: &mut Manifest,
    record: &AssetRecord,
) -> Result<(), AssetError> {
    let Some(index) = manifest
        .assets
        .iter()
        .position(|asset| asset.identity == record.identity)
    else {
        return Ok(());
    };
    if manifest.assets[index].sha256 != record.sha256 {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            format!(
                "идентичность {} изменилась во время удаления записи жизненного цикла",
                record.identity
            ),
        ));
    }
    let assets = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| directory_entry_error(ASSETS_DIR, error))?;
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let name = asset_name(&record.storage_path)?;
    if !file_matches_hash(&assets, name, &record.sha256)? {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты удаляемой записи не совпадают с manifest",
        ));
    }
    linkat(&assets, name, &temporary, REMOVAL_BACKUP, AtFlags::empty()).map_err(|error| {
        AssetError::io(
            "не удалось сохранить bytes перед удалением записи",
            std::io::Error::from(error),
        )
    })?;
    sync_directory(&temporary)?;
    let marker = RemovalTransaction {
        schema_version: 1,
        record: record.clone(),
    };
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    write_transaction_marker(&temporary, REMOVAL_MARKER, &bytes)?;
    manifest.assets.remove(index);
    manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "поле `revision` в манифесте переполнено",
        )
    })?;
    save_manifest(root, manifest, false)?;
    #[cfg(test)]
    if FAIL_AFTER_REMOVAL_MANIFEST.with(|hook| hook.replace(false)) {
        return Err(AssetError::new(
            ErrorCode::IoFailure,
            "тестовый сбой после записи manifest удаления",
        ));
    }
    remove_if_hash(
        &assets,
        name,
        &record.sha256,
        "байты удаляемой записи жизненного цикла",
    )?;
    sync_directory(&assets)?;
    recover_removal(root)
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

/// Полный decode всех GIF frames либо PNG; magic bytes недостаточно для
/// пользовательского approval. Decoder limits ограничивают память одного frame.
pub(crate) fn validate_image_decode(bytes: &[u8]) -> Result<(), AssetError> {
    use image::{AnimationDecoder, ImageDecoder};
    let failure = |message: String| {
        AssetError::new(
            ErrorCode::InvalidTransition,
            format!(
                "подтверждение человеком требует полной проверки изображения декодером: {message}"
            ),
        )
    };
    match DetectedFormat::from_signature(bytes) {
        DetectedFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
                .map_err(|error| failure(error.to_string()))?;
            decoder
                .set_limits(image::Limits::default())
                .map_err(|error| failure(error.to_string()))?;
            let mut count = 0;
            for frame in decoder.into_frames() {
                frame.map_err(|error| failure(error.to_string()))?;
                count += 1;
            }
            if count == 0 {
                return Err(failure("GIF не содержит frames".into()));
            }
        }
        DetectedFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::with_limits(
                std::io::Cursor::new(bytes),
                image::Limits::default(),
            )
            .map_err(|error| failure(error.to_string()))?;
            if decoder
                .is_apng()
                .map_err(|error| failure(error.to_string()))?
            {
                let mut count = 0;
                for frame in decoder
                    .apng()
                    .map_err(|error| failure(error.to_string()))?
                    .into_frames()
                {
                    frame.map_err(|error| failure(error.to_string()))?;
                    count += 1;
                }
                if count == 0 {
                    return Err(failure("APNG не содержит frames".into()));
                }
            } else {
                image::DynamicImage::from_decoder(decoder)
                    .map_err(|error| failure(error.to_string()))?;
            }
        }
        _ => return Err(failure("поддерживаются GIF и PNG".into())),
    }
    Ok(())
}

fn validate_asset(root: &File, record: &AssetRecord) -> Result<(), AssetError> {
    let file = checked_asset_file(root, record)?;
    let (sha256, byte_length, format) = hash_file(file)?;
    if sha256 != record.sha256 || byte_length != record.byte_length || format != record.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            format!("файл asset {} не совпадает с manifest", record.identity),
        ));
    }
    // Approval fully decodes candidate bytes in `attest`; later manifest checks
    // only need to stream the hash. `read_verified` decodes before returning bytes.
    Ok(())
}

fn validate_asset_directory(
    root: &File,
    registered_paths: &BTreeSet<String>,
) -> Result<(), AssetError> {
    validate_asset_directory_impl(root, registered_paths, false)
}

fn validate_asset_directory_exact(root: &File, manifest: &Manifest) -> Result<(), AssetError> {
    let registered_paths = manifest
        .assets
        .iter()
        .map(|asset| asset.storage_path.clone())
        .collect();
    validate_asset_directory_impl(root, &registered_paths, true)
}

fn validate_asset_directory_impl(
    root: &File,
    registered_paths: &BTreeSet<String>,
    reject_orphans: bool,
) -> Result<(), AssetError> {
    let assets = match open_directory_at(root, ASSETS_DIR) {
        Ok(assets) => assets,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && registered_paths.is_empty() =>
        {
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "каталог assets отсутствует в program-owned store",
            ));
        }
        Err(error) => {
            return Err(AssetError::io(
                "не удалось открыть canonical asset store",
                error,
            ));
        }
    };
    let mut observed_paths = BTreeSet::new();
    for entry in fs::read_dir(fd_path(&assets))
        .map_err(|error| AssetError::io("не удалось прочитать canonical asset store", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать asset entry", error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file = open_regular_at(&assets, entry.file_name(), ErrorCode::MissingAssetFile)?;
        let (actual_hash, _, format) = hash_file(file)?;
        let storage_path = format!("{ASSETS_DIR}/{name}");
        if registered_paths.contains(&storage_path) {
            if extension_for_format(format) != name.rsplit('.').next().unwrap_or("") {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    format!("asset {name} не соответствует формату в manifest"),
                ));
            }
        } else {
            if reject_orphans {
                return Err(AssetError::new(
                    ErrorCode::UnexpectedPath,
                    format!("незарегистрированный файл в публикуемом корпусе: {name}"),
                ));
            }
            let Some(hash) = embedded_content_hash(&name) else {
                return Err(AssetError::new(
                    ErrorCode::UnexpectedPath,
                    format!("неизвестный файл в canonical asset store: {name}"),
                ));
            };
            if is_hash_suffixed_kanji_filename(&name) {
                return Err(AssetError::new(
                    ErrorCode::UnexpectedPath,
                    format!("устаревшее hash-suffixed имя kanji asset: {name}"),
                ));
            }
            validate_hash(hash)?;
            if actual_hash != hash
                || extension_for_format(format) != name.rsplit('.').next().unwrap_or("")
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    format!("asset {name} не соответствует SHA-256/формату в имени"),
                ));
            }
        }
        observed_paths.insert(storage_path);
    }
    if !registered_paths.is_subset(&observed_paths) {
        return Err(AssetError::new(
            ErrorCode::MissingAssetFile,
            "manifest ссылается на отсутствующий canonical asset",
        ));
    }
    if reject_orphans && !observed_paths.is_subset(registered_paths) {
        return Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            "в публикуемом корпусе есть незарегистрированные байты",
        ));
    }
    // Generic immutable objects can leave a verified-by-hash orphan after a
    // crash. Stable kanji paths use a transaction marker and are recovered
    // before manifest validation.
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
            "storage_path не совпадает с canonical asset layout",
        ));
    }
    Ok(())
}

fn checked_asset_file(root: &File, record: &AssetRecord) -> Result<File, AssetError> {
    let expected = canonical_asset_path(&record.identity, &record.sha256, record.format);
    validate_relative_path(&record.storage_path, &expected)?;
    let assets = open_directory_at(root, ASSETS_DIR).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                "каталог assets отсутствует в program-owned store",
            )
        } else {
            AssetError::io("не удалось открыть canonical asset store", error)
        }
    })?;
    let name = record
        .storage_path
        .strip_prefix("assets/")
        .ok_or_else(|| AssetError::new(ErrorCode::ManifestCorrupt, "storage_path вне assets/"))?;
    open_regular_at(&assets, name, ErrorCode::MissingAssetFile).map_err(|error| {
        if error.code == ErrorCode::MissingAssetFile {
            AssetError::with_details(
                error.code,
                error.message,
                serde_json::json!({
                    "identity": record.identity,
                    "storage_path": record.storage_path,
                }),
            )
        } else {
            error
        }
    })
}

fn read_bounded_asset_bytes(
    file: File,
    maximum: usize,
    operation: &str,
) -> Result<Vec<u8>, AssetError> {
    let mut bytes = Vec::new();
    file.take((maximum as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| AssetError::io(operation, error))?;
    if bytes.len() > maximum {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            format!("{operation}: изображение превышает предел {maximum} байт"),
        ));
    }
    Ok(bytes)
}

fn canonical_asset_path(identity: &AssetIdentity, hash: &str, format: DetectedFormat) -> String {
    if let Some(character) = kanji_character(identity) {
        return format!("{ASSETS_DIR}/{character}.{}", extension_for_format(format));
    }
    let prefix = {
        let key_hash = sha256_hex(identity.key.as_bytes());
        format!("{}-{}", identity.namespace, &key_hash[..16])
    };
    format!(
        "{ASSETS_DIR}/{prefix}-{hash}.{}",
        extension_for_format(format)
    )
}

fn kanji_character(identity: &AssetIdentity) -> Option<char> {
    if identity.namespace != "kanji" {
        return None;
    }
    let mut chars = identity.key.chars();
    let character = chars.next()?;
    (chars.next().is_none() && crate::kanji_domain::is_supported_han(character))
        .then_some(character)
}

fn extension_for_format(format: DetectedFormat) -> &'static str {
    match format {
        DetectedFormat::Png => "png",
        DetectedFormat::Jpeg => "jpg",
        DetectedFormat::Gif => "gif",
        DetectedFormat::Webp => "webp",
        DetectedFormat::Bmp => "bmp",
        DetectedFormat::Tiff => "tiff",
        DetectedFormat::Unknown => "bin",
    }
}

fn embedded_content_hash(filename: &str) -> Option<&str> {
    let (stem, extension) = filename.rsplit_once('.')?;
    if !matches!(
        extension,
        "png" | "jpg" | "gif" | "webp" | "bmp" | "tiff" | "bin"
    ) {
        return None;
    }
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == 64).then_some(hash)
}

fn is_hash_suffixed_kanji_filename(filename: &str) -> bool {
    let Some((stem, _)) = filename.rsplit_once('.') else {
        return false;
    };
    let Some((prefix, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    hash.len() == 64 && prefix.chars().count() == 1
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
    let sha256 = encode_lower_hex(digest.finalize());
    Ok(StagedObject {
        artifact,
        sha256,
        byte_length,
        format: DetectedFormat::from_signature(&signature),
    })
}

fn stage_bytes(root: &File, bytes: &[u8]) -> Result<StagedObject, AssetError> {
    let (artifact, mut output) = create_temp_file(root, "candidate")?;
    output
        .write_all(bytes)
        .and_then(|()| output.flush())
        .and_then(|()| output.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать candidate staging", error))?;
    let digest = Sha256::digest(bytes);
    Ok(StagedObject {
        artifact,
        sha256: encode_lower_hex(digest),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::from_signature(&bytes[..bytes.len().min(12)]),
    })
}

fn publish_object(
    staged: &StagedObject,
    root: &File,
    storage_path: &str,
) -> Result<(), AssetError> {
    let assets = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| AssetError::io("не удалось открыть canonical asset store", error))?;
    let name = storage_path.strip_prefix("assets/").ok_or_else(|| {
        AssetError::new(ErrorCode::ManifestCorrupt, "publication path вне assets/")
    })?;
    match open_regular_at(&assets, name, ErrorCode::MissingAssetFile) {
        Ok(existing) => {
            verify_staged_file(staged, existing)?;
            sync_directory(&assets)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            match linkat(
                &staged.artifact.directory,
                &staged.artifact.name,
                &assets,
                name,
                AtFlags::empty(),
            ) {
                Ok(()) => {
                    let asset = open_regular_at(&assets, name, ErrorCode::MissingAssetFile)?;
                    verify_staged_file(staged, asset)?;
                    sync_directory(&assets)
                }
                Err(link_error)
                    if std::io::Error::from(link_error).kind()
                        == std::io::ErrorKind::AlreadyExists =>
                {
                    let existing = open_regular_at(&assets, name, ErrorCode::MissingAssetFile)?;
                    verify_staged_file(staged, existing)
                }
                Err(link_error) => Err(AssetError::io(
                    "не удалось опубликовать canonical asset",
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
            "canonical asset не совпадает с вычисленным hash",
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
    let mut current = manifest.clone();
    current.schema_version = MANIFEST_SCHEMA_VERSION;
    let bytes = serde_json::to_vec_pretty(&current)
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
    let mut current = manifest.clone();
    current.schema_version = MANIFEST_SCHEMA_VERSION;
    let bytes = serde_json::to_vec_pretty(&current)
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
    create_temp_in_directory(&directory, prefix)
}

fn create_temp_in_directory(
    directory: &File,
    prefix: &str,
) -> Result<(TempArtifact, File), AssetError> {
    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!("{prefix}-{}-{counter}.tmp", std::process::id()));
        match openat(
            directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                return Ok((
                    TempArtifact {
                        directory: directory.try_clone().map_err(|error| {
                            AssetError::io("не удалось клонировать temp dir", error)
                        })?,
                        name,
                    },
                    File::from(file),
                ));
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
        encode_lower_hex(digest.finalize()),
        byte_length,
        DetectedFormat::from_signature(&signature),
    ))
}

fn commit_asset_record<F>(
    root: &File,
    staged: &StagedObject,
    previous: Option<&AssetRecord>,
    record: &AssetRecord,
    manifest: &mut Manifest,
    save: F,
) -> Result<(), AssetError>
where
    F: FnOnce(&File, &Manifest) -> Result<(), AssetError>,
{
    recover_publications(root)?;
    let next_revision = manifest.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "поле `revision` в манифесте переполнено",
        )
    })?;
    let needs_stable_publication = kanji_character(&record.identity).is_some()
        && previous.is_none_or(|asset| {
            asset.sha256 != record.sha256 || asset.storage_path != record.storage_path
        });
    let publication = if needs_stable_publication {
        Some(prepare_publication(root, staged, previous, record)?)
    } else {
        None
    };

    let result = (|| {
        if let Some(publication) = &publication {
            apply_publication(root, staged, publication)?;
        } else {
            publish_object(staged, root, &record.storage_path)?;
        }
        validate_asset(root, record)?;
        if let Some(slot) = manifest
            .assets
            .iter_mut()
            .find(|asset| asset.identity == record.identity)
        {
            *slot = record.clone();
        } else {
            manifest.assets.push(record.clone());
        }
        sort_assets(&mut manifest.assets);
        manifest.revision = next_revision;
        save(root, manifest)
    })();

    if let Err(error) = result {
        if let Some(publication) = &publication {
            recover_publication(root, &publication.marker_name)?;
        }
        return Err(error);
    }
    if let Some(publication) = publication {
        recover_publication(root, &publication.marker_name)?;
    }
    Ok(())
}

fn prepare_publication(
    root: &File,
    staged: &StagedObject,
    previous: Option<&AssetRecord>,
    next: &AssetRecord,
) -> Result<PendingPublication, AssetError> {
    if kanji_character(&next.identity).is_none() {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "публикация с постоянным именем допустима только для идентичности `kanji` из одного символа",
        ));
    }
    let assets = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| AssetError::io("не удалось открыть canonical asset store", error))?;
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    let next_name = asset_name(&next.storage_path)?;
    let previous_object = previous.map(|asset| PublicationObject {
        storage_path: asset.storage_path.clone(),
        sha256: asset.sha256.clone(),
        format: asset.format,
    });
    if let Some(previous) = previous {
        let previous_name = asset_name(&previous.storage_path)?;
        if !file_matches_hash(&assets, previous_name, &previous.sha256)? {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "предыдущий kanji asset изменился до compare-and-swap публикации",
            ));
        }
        if previous.storage_path != next.storage_path {
            ensure_asset_path_absent(&assets, next_name, &next.storage_path)?;
        }
    } else {
        ensure_asset_path_absent(&assets, next_name, &next.storage_path)?;
    }

    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("{}-{counter}", std::process::id());
        let marker_name = OsString::from(format!("publication-{id}.json"));
        let backup_name = previous_object
            .as_ref()
            .filter(|previous| previous.storage_path == next.storage_path)
            .map(|_| format!("publication-{id}.backup"));
        let transaction = PublicationTransaction {
            schema_version: 1,
            identity: next.identity.clone(),
            previous: previous_object.clone(),
            next: PublicationObject {
                storage_path: next.storage_path.clone(),
                sha256: next.sha256.clone(),
                format: next.format,
            },
            staged_name: staged.artifact.name.to_string_lossy().into_owned(),
            backup_name: backup_name.clone(),
        };
        if !write_publication_marker(&temporary, &marker_name, &transaction)? {
            continue;
        }
        let pending = PendingPublication {
            marker_name,
            transaction,
        };
        if let (Some(previous), Some(backup_name)) = (previous, backup_name) {
            let previous_name = asset_name(&previous.storage_path)?;
            if let Err(error) = linkat(
                &assets,
                previous_name,
                &temporary,
                backup_name.as_str(),
                AtFlags::empty(),
            ) {
                recover_publication(root, &pending.marker_name)?;
                return Err(AssetError::io(
                    "не удалось сохранить предыдущий kanji asset для CAS",
                    std::io::Error::from(error),
                ));
            }
            if !file_matches_hash(&temporary, &backup_name, &previous.sha256)? {
                recover_publication(root, &pending.marker_name)?;
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "backup предыдущего kanji asset не прошёл SHA-256 проверку",
                ));
            }
            sync_directory(&temporary)?;
        }
        return Ok(pending);
    }
    Err(AssetError::new(
        ErrorCode::IoFailure,
        "не удалось выбрать свободное имя publication transaction",
    ))
}

fn write_publication_marker(
    temporary: &File,
    marker_name: &OsString,
    transaction: &PublicationTransaction,
) -> Result<bool, AssetError> {
    let bytes = serde_json::to_vec_pretty(transaction)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    let (artifact, mut file) = create_temp_in_directory(temporary, "publication-state")?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать publication marker", error))?;
    drop(file);
    match linkat(
        &artifact.directory,
        &artifact.name,
        temporary,
        marker_name,
        AtFlags::empty(),
    ) {
        Ok(()) => {
            if let Err(error) = sync_directory(temporary) {
                let _ = unlink_if_exists(temporary, marker_name);
                let _ = sync_directory(temporary);
                Err(error)
            } else {
                Ok(true)
            }
        }
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(false)
        }
        Err(error) => Err(AssetError::io(
            "не удалось атомарно сохранить publication marker",
            std::io::Error::from(error),
        )),
    }
}

fn apply_publication(
    root: &File,
    staged: &StagedObject,
    publication: &PendingPublication,
) -> Result<(), AssetError> {
    let assets = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| AssetError::io("не удалось открыть canonical asset store", error))?;
    let next_name = asset_name(&publication.transaction.next.storage_path)?;
    let staged_file = open_regular_at(
        &staged.artifact.directory,
        &staged.artifact.name,
        ErrorCode::MissingAssetFile,
    )?;
    verify_staged_contents(staged, staged_file)?;
    renameat(
        &staged.artifact.directory,
        &staged.artifact.name,
        &assets,
        next_name,
    )
    .map_err(|error| {
        AssetError::io(
            "не удалось атомарно опубликовать stable kanji asset",
            std::io::Error::from(error),
        )
    })?;
    sync_directory(&staged.artifact.directory)?;
    sync_directory(&assets)?;
    let published = open_regular_at(&assets, next_name, ErrorCode::MissingAssetFile)?;
    verify_staged_file(staged, published)?;
    sync_directory(&assets)
}

fn verify_staged_contents(staged: &StagedObject, file: File) -> Result<(), AssetError> {
    let (hash, length, format) = hash_file(file)?;
    if hash != staged.sha256 || length != staged.byte_length || format != staged.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "staged kanji asset не совпадает с вычисленным SHA-256",
        ));
    }
    Ok(())
}

fn recover_publications(root: &File) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    let mut markers = Vec::new();
    for entry in fs::read_dir(fd_path(&temporary))
        .map_err(|error| AssetError::io("не удалось прочитать transaction directory", error))?
    {
        let entry = entry
            .map_err(|error| AssetError::io("не удалось прочитать transaction entry", error))?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with("publication-") && text.ends_with(".json") {
            markers.push(name);
        }
    }
    markers.sort();
    for marker in markers {
        recover_publication(root, &marker)?;
    }
    recover_removal(root)
}

fn write_transaction_marker(directory: &File, name: &str, bytes: &[u8]) -> Result<(), AssetError> {
    let (artifact, mut file) = create_temp_in_directory(directory, "transaction-state")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось сохранить transaction marker", error))?;
    drop(file);
    renameat(directory, &artifact.name, directory, name).map_err(|error| {
        AssetError::io(
            "не удалось опубликовать transaction marker",
            std::io::Error::from(error),
        )
    })?;
    sync_directory(directory)
}

fn begin_transition(
    root: &File,
    identity: &AssetIdentity,
    target: TransitionTarget,
    sha256: &str,
) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let marker = TransitionTransaction {
        schema_version: 1,
        identity: identity.clone(),
        target,
        sha256: sha256.to_owned(),
    };
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    write_transaction_marker(&temporary, TRANSITION_MARKER, &bytes)
}

fn load_transition(root: &File) -> Result<Option<TransitionTransaction>, AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let file = match open_regular_at(&temporary, TRANSITION_MARKER, ErrorCode::MissingAssetFile) {
        Ok(file) => file,
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(None),
        Err(error) => return Err(error),
    };
    let marker: TransitionTransaction = serde_json::from_reader(file)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    if marker.schema_version != 1 || marker.identity.validate().is_err() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "transition marker повреждён",
        ));
    }
    validate_hash(&marker.sha256)?;
    Ok(Some(marker))
}

fn clear_transition(root: &File) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    unlink_if_exists(&temporary, &OsString::from(TRANSITION_MARKER))?;
    sync_directory(&temporary)
}

fn recover_removal(root: &File) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let marker_file = match open_regular_at(&temporary, REMOVAL_MARKER, ErrorCode::MissingAssetFile)
    {
        Ok(file) => file,
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            unlink_if_exists(&temporary, &OsString::from(REMOVAL_BACKUP))?;
            return sync_directory(&temporary);
        }
        Err(error) => return Err(error),
    };
    let transaction: RemovalTransaction = serde_json::from_reader(marker_file)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    if transaction.schema_version != 1 || transaction.record.identity.validate().is_err() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "removal marker повреждён",
        ));
    }
    validate_hash(&transaction.record.sha256)?;
    let expected_path = canonical_asset_path(
        &transaction.record.identity,
        &transaction.record.sha256,
        transaction.record.format,
    );
    validate_relative_path(&transaction.record.storage_path, &expected_path)?;
    let assets = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| directory_entry_error(ASSETS_DIR, error))?;
    let name = asset_name(&transaction.record.storage_path)?;
    let manifest = load_owned_manifest(root)?;
    match manifest
        .assets
        .iter()
        .find(|asset| asset.identity == transaction.record.identity)
    {
        Some(current)
            if current.sha256 == transaction.record.sha256
                && current.storage_path == transaction.record.storage_path =>
        {
            match open_regular_at(&assets, name, ErrorCode::MissingAssetFile) {
                Ok(file) => {
                    if hash_file(file)?.0 != current.sha256 {
                        return Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "байты удаляемого asset изменились во время восстановления",
                        ));
                    }
                }
                Err(error) if error.code == ErrorCode::MissingAssetFile => {
                    if !file_matches_hash(&temporary, REMOVAL_BACKUP, &current.sha256)? {
                        return Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "backup удаляемого asset отсутствует",
                        ));
                    }
                    renameat(&temporary, REMOVAL_BACKUP, &assets, name).map_err(|error| {
                        AssetError::io(
                            "не удалось восстановить удаляемый asset",
                            std::io::Error::from(error),
                        )
                    })?;
                    sync_directory(&assets)?;
                }
                Err(error) => return Err(error),
            }
        }
        None => {
            remove_if_hash(&assets, name, &transaction.record.sha256, "удаляемый asset")?;
            sync_directory(&assets)?;
        }
        Some(_) => {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "identity изменилась при восстановлении удаления",
            ));
        }
    }
    remove_if_hash(
        &temporary,
        REMOVAL_BACKUP,
        &transaction.record.sha256,
        "removal backup",
    )?;
    unlink_if_exists(&temporary, &OsString::from(REMOVAL_MARKER))?;
    sync_directory(&temporary)
}

fn recover_publication(root: &File, marker_name: &OsString) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    validate_publication_names(marker_name, None, None)?;
    let marker_file = open_regular_at(&temporary, marker_name, ErrorCode::MissingAssetFile)?;
    let transaction: PublicationTransaction = serde_json::from_reader(marker_file)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    validate_publication_transaction(marker_name, &transaction)?;
    let manifest = load_owned_manifest(root)?;
    let current = manifest
        .assets
        .iter()
        .find(|asset| asset.identity == transaction.identity);
    let committed = current.is_some_and(|asset| {
        asset.sha256 == transaction.next.sha256
            && asset.storage_path == transaction.next.storage_path
    });
    let previous_state = match (&transaction.previous, current) {
        (Some(previous), Some(asset)) => {
            asset.sha256 == previous.sha256 && asset.storage_path == previous.storage_path
        }
        (None, None) => true,
        _ => false,
    };
    let assets = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| AssetError::io("не удалось открыть canonical asset store", error))?;

    if committed {
        if !file_matches_hash(
            &assets,
            asset_name(&transaction.next.storage_path)?,
            &transaction.next.sha256,
        )? {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "manifest commit ссылается на не опубликованный kanji asset",
            ));
        }
        if let Some(previous) = &transaction.previous
            && previous.storage_path != transaction.next.storage_path
        {
            remove_if_hash(
                &assets,
                asset_name(&previous.storage_path)?,
                &previous.sha256,
                "предыдущий kanji asset",
            )?;
        }
    } else if previous_state {
        if let Some(previous) = &transaction.previous {
            let previous_name = asset_name(&previous.storage_path)?;
            if previous.storage_path == transaction.next.storage_path
                && !file_matches_hash(&assets, previous_name, &previous.sha256)?
            {
                let backup_name = transaction.backup_name.as_deref().ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        "publication transaction не содержит backup для in-place CAS",
                    )
                })?;
                if !file_matches_hash(&temporary, backup_name, &previous.sha256)? {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "не удалось восстановить предыдущие kanji bytes после сбоя публикации",
                    ));
                }
                renameat(&temporary, backup_name, &assets, previous_name).map_err(|error| {
                    AssetError::io(
                        "не удалось откатить атомарную замену kanji asset",
                        std::io::Error::from(error),
                    )
                })?;
                sync_directory(&temporary)?;
                sync_directory(&assets)?;
                if !file_matches_hash(&assets, previous_name, &previous.sha256)? {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "восстановленный kanji asset не совпадает с прежним SHA-256",
                    ));
                }
            } else if !file_matches_hash(&assets, previous_name, &previous.sha256)? {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "предыдущий kanji asset отсутствует после незавершённой публикации",
                ));
            }
        }
        if transaction
            .previous
            .as_ref()
            .is_none_or(|previous| previous.storage_path != transaction.next.storage_path)
        {
            remove_if_hash(
                &assets,
                asset_name(&transaction.next.storage_path)?,
                &transaction.next.sha256,
                "неподтверждённый kanji asset",
            )?;
        }
    } else {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "publication marker не соответствует текущему manifest",
        ));
    }

    remove_if_exists(&temporary, OsString::from(transaction.staged_name.as_str()))?;
    if let (Some(backup_name), Some(previous)) = (
        transaction.backup_name.as_deref(),
        transaction.previous.as_ref(),
    ) {
        remove_if_hash(
            &temporary,
            backup_name,
            &previous.sha256,
            "publication backup",
        )?;
    }
    unlink_if_exists(&temporary, marker_name)?;
    sync_directory(&assets)?;
    sync_directory(&temporary)
}

fn validate_publication_transaction(
    marker_name: &OsString,
    transaction: &PublicationTransaction,
) -> Result<(), AssetError> {
    if transaction.schema_version != 1
        || transaction.identity.validate().is_err()
        || kanji_character(&transaction.identity).is_none()
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит неподдерживаемую kanji identity/schema",
        ));
    }
    validate_hash(&transaction.next.sha256)?;
    if transaction.next.storage_path
        != canonical_asset_path(
            &transaction.identity,
            &transaction.next.sha256,
            transaction.next.format,
        )
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит неверный новый storage_path",
        ));
    }
    if let Some(previous) = &transaction.previous {
        validate_hash(&previous.sha256)?;
        if previous.storage_path
            != canonical_asset_path(&transaction.identity, &previous.sha256, previous.format)
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "publication marker содержит неверный предыдущий storage_path",
            ));
        }
        if previous.sha256 == transaction.next.sha256
            && previous.storage_path == transaction.next.storage_path
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "publication marker не описывает изменение bytes/path",
            ));
        }
    }
    let marker = marker_name.to_string_lossy();
    let Some(id) = marker
        .strip_prefix("publication-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "неверное имя publication marker",
        ));
    };
    if !valid_numeric_id(id) || !is_temp_artifact_name(&transaction.staged_name) {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит небезопасное имя temporary file",
        ));
    }
    let expected_backup = transaction
        .previous
        .as_ref()
        .filter(|previous| previous.storage_path == transaction.next.storage_path)
        .map(|_| format!("publication-{id}.backup"));
    if transaction.backup_name != expected_backup {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит неверный backup name",
        ));
    }
    Ok(())
}

fn validate_publication_names(
    marker_name: &OsString,
    staged_name: Option<&str>,
    backup_name: Option<&str>,
) -> Result<(), AssetError> {
    let marker = marker_name.to_string_lossy();
    let Some(id) = marker
        .strip_prefix("publication-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "неверное имя publication marker",
        ));
    };
    if !valid_numeric_id(id)
        || staged_name.is_some_and(|name| !is_temp_artifact_name(name))
        || backup_name.is_some_and(|name| name != format!("publication-{id}.backup"))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит небезопасное имя temporary file",
        ));
    }
    Ok(())
}

fn valid_numeric_id(id: &str) -> bool {
    let Some((pid, counter)) = id.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !counter.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && counter.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_temp_artifact_name(name: &str) -> bool {
    ["candidate-", "ingest-"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_suffix(".tmp"))
            .is_some_and(valid_numeric_id)
    })
}

fn asset_name(storage_path: &str) -> Result<&str, AssetError> {
    storage_path
        .strip_prefix("assets/")
        .ok_or_else(|| AssetError::new(ErrorCode::ManifestCorrupt, "storage_path вне assets/"))
}

fn ensure_asset_path_absent(
    directory: &File,
    name: &str,
    storage_path: &str,
) -> Result<(), AssetError> {
    match open_regular_at(directory, name, ErrorCode::MissingAssetFile) {
        Ok(_) => Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            format!("путь нового kanji asset уже занят: {storage_path}"),
        )),
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(()),
        Err(error) => Err(error),
    }
}

fn file_matches_hash(
    directory: &File,
    name: &str,
    expected_hash: &str,
) -> Result<bool, AssetError> {
    match open_regular_at(directory, name, ErrorCode::MissingAssetFile) {
        Ok(file) => Ok(hash_file(file)?.0 == expected_hash),
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_if_hash(
    directory: &File,
    name: &str,
    expected_hash: &str,
    label: &str,
) -> Result<(), AssetError> {
    match open_regular_at(directory, name, ErrorCode::MissingAssetFile) {
        Ok(file) => {
            if hash_file(file)?.0 != expected_hash {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    format!("{label} не совпадает с transaction SHA-256"),
                ));
            }
            let owned_name = OsString::from(name);
            unlink_if_exists(directory, &owned_name)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_if_exists(directory: &File, name: OsString) -> Result<(), AssetError> {
    unlink_if_exists(directory, &name)
}

fn unlink_if_exists(directory: &File, name: &OsString) -> Result<(), AssetError> {
    match unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) => Ok(()),
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AssetError::io(
            "не удалось удалить завершённый publication artifact",
            std::io::Error::from(error),
        )),
    }
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
    after_component: impl FnMut(&Path),
) -> Result<(File, bool), AssetError> {
    open_store_root(path, true, after_component)
}

fn open_existing_store_root(path: &Path) -> Result<File, AssetError> {
    open_store_root(path, false, |_| {}).map(|(root, _)| root)
}

fn open_store_root(
    path: &Path,
    create_missing: bool,
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
                if !create_missing {
                    return Err(AssetError::new(
                        ErrorCode::StoreMissing,
                        "store root не существует",
                    ));
                }
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
    sha256_hex(material.as_bytes())
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
    fn verified_snapshot_is_read_only_and_rechecks_the_returned_bytes() {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&root)).unwrap();
        let identity = AssetIdentity::new("kanji", "一").unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity.clone(),
                    bytes: b"GIF89a-synthetic".to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "fixture".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap();
        drop(store);
        fs::remove_dir_all(root.join(RUNTIME_DIR)).unwrap();
        fs::remove_file(root.join(LOCK_FILE)).unwrap();
        let before = fs::read(root.join(MANIFEST_FILE)).unwrap();
        let read = AssetStore::read_verified(
            &root,
            std::slice::from_ref(&identity),
            &VerifiedValidator.identity(),
        )
        .unwrap();
        assert_eq!(read[0].bytes, b"GIF89a-synthetic");
        assert_eq!(fs::read(root.join(MANIFEST_FILE)).unwrap(), before);
        assert!(!root.join(RUNTIME_DIR).exists());
        assert!(!root.join(LOCK_FILE).exists());
        let error = AssetStore::read_verified_snapshot(
            &root,
            std::slice::from_ref(&identity),
            &VerifiedValidator.identity(),
            |record| {
                let path = root.join(&record.storage_path);
                fs::remove_file(&path).unwrap();
                fs::write(&path, b"GIF89a-swapped").unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::IntegrityMismatch);
    }

    #[test]
    fn verified_snapshot_rejects_symlink_replacement_after_validation() {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&root)).unwrap();
        let identity = AssetIdentity::new("kanji", "二").unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity.clone(),
                    bytes: b"GIF89a-original".to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "fixture".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap();
        let outside = temp.0.join("outside.gif");
        fs::write(&outside, b"GIF89a-original").unwrap();
        let error = AssetStore::read_verified_snapshot(
            &root,
            &[identity],
            &VerifiedValidator.identity(),
            |record| {
                let path = root.join(&record.storage_path);
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(&outside, path).unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::BoundaryViolation);
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
                expected_source_sha256: None,
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
        assert!(root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());

        let reopened = AssetStore::open(StoreOptions::new(&root)).expect("store remains readable");
        let records = reopened
            .verify_integrity()
            .expect("canonical state remains valid");
        assert!(!root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lifecycle, LifecycleState::Pending);
        assert!(records[0].validation.is_none());
    }

    #[test]
    fn removal_recovers_after_manifest_commit_before_byte_deletion() {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&root)).unwrap();
        let record = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", "日").unwrap(),
                    bytes: b"GIF89a durable removal fixture".to_vec(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "fixture.gif".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap()
            .asset
            .unwrap();
        let lock = store.lock_exclusive().unwrap();
        let mut manifest = load_manifest(&store.root_handle).unwrap();
        FAIL_AFTER_REMOVAL_MANIFEST.with(|hook| hook.set(true));
        assert_eq!(
            remove_record_from_area(&store.root_handle, &mut manifest, &record)
                .unwrap_err()
                .code,
            ErrorCode::IoFailure
        );
        assert!(root.join(&record.storage_path).exists());
        assert!(root.join(TEMP_DIR).join(REMOVAL_MARKER).exists());
        lock.unlock().unwrap();
        drop(store);

        let reopened = AssetStore::open_existing(StoreOptions::new(&root)).unwrap();
        assert!(reopened.verify_integrity().unwrap().is_empty());
        assert!(!root.join(&record.storage_path).exists());
        assert!(!root.join(TEMP_DIR).join(REMOVAL_MARKER).exists());
        assert!(!root.join(TEMP_DIR).join(REMOVAL_BACKUP).exists());
    }

    #[test]
    fn canonical_transition_recovery_keeps_verified_result() {
        for through_validation in [false, true] {
            let temp = TempDir::new();
            let root = temp.0.join("store");
            let store = AssetStore::open(StoreOptions::new(&root)).unwrap();
            let source = temp.0.join("candidate.gif");
            let bytes = b"GIF89a pending to verified fixture";
            fs::write(&source, bytes).unwrap();
            let identity = AssetIdentity::new("kanji", "日").unwrap();
            store
                .ingest(IngestRequest {
                    identity: identity.clone(),
                    source_path: source,
                    expected_source_sha256: None,
                    domain_metadata: None,
                    replace_expected_sha256: None,
                })
                .unwrap();
            FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.set(true));
            let error = if through_validation {
                store
                    .validate(SelectionMode::Full, &VerifiedValidator)
                    .unwrap_err()
            } else {
                store
                    .ingest_verified(
                        VerifiedIngestRequest {
                            identity,
                            bytes: bytes.to_vec(),
                            provenance: Provenance {
                                source_kind: "fixture".into(),
                                source_name: "candidate.gif".into(),
                            },
                            domain_metadata: None,
                            replace_expected_sha256: None,
                        },
                        &VerifiedValidator,
                    )
                    .unwrap_err()
            };
            assert_eq!(error.code, ErrorCode::IoFailure);
            assert!(root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());
            drop(store);

            let reopened = AssetStore::open_existing(StoreOptions::new(&root)).unwrap();
            let records = reopened.verify_integrity().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].lifecycle, LifecycleState::Verified);
            assert!(!root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());
        }
    }

    #[test]
    fn failed_kanji_cas_manifest_write_restores_previous_stable_bytes() {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&root)).expect("empty store opens");
        let validator = VerifiedValidator;
        let first_bytes = b"GIF89a previous fixture".to_vec();
        let first = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", "元").unwrap(),
                    bytes: first_bytes.clone(),
                    provenance: Provenance {
                        source_kind: "fixture".to_owned(),
                        source_name: "previous.gif".to_owned(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &validator,
            )
            .expect("first verified bytes publish");
        let previous = first.asset.expect("first asset exists");
        assert_eq!(previous.storage_path, "assets/元.gif");

        store.fail_next_manifest_write();
        let error = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: previous.identity.clone(),
                    bytes: b"GIF89a replacement fixture".to_vec(),
                    provenance: Provenance {
                        source_kind: "fixture".to_owned(),
                        source_name: "replacement.gif".to_owned(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: Some(previous.sha256.clone()),
                },
                &validator,
            )
            .expect_err("injected manifest failure aborts CAS publication");
        assert_eq!(error.code, ErrorCode::IoFailure);
        assert_eq!(
            fs::read(root.join("assets/元.gif")).expect("old stable path restored"),
            first_bytes
        );
        assert_eq!(
            fs::read_dir(root.join(TEMP_DIR))
                .expect("temporary directory exists")
                .count(),
            0,
            "rollback removes transaction marker, backup and staged candidate"
        );

        let reopened = AssetStore::open(StoreOptions::new(&root)).expect("store remains readable");
        let records = reopened
            .verify_integrity()
            .expect("old manifest still matches bytes");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sha256, previous.sha256);
        assert_eq!(records[0].lifecycle, LifecycleState::Verified);
    }

    #[test]
    fn reopening_after_interrupted_kanji_cas_restores_previous_verified_bytes() {
        let temp = TempDir::new();
        let root = temp.0.join("store");
        let store = AssetStore::open(StoreOptions::new(&root)).expect("empty store opens");
        let previous_bytes = b"GIF89a durable previous fixture".to_vec();
        let first = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", "元").unwrap(),
                    bytes: previous_bytes.clone(),
                    provenance: Provenance {
                        source_kind: "fixture".to_owned(),
                        source_name: "previous.gif".to_owned(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .expect("first verified bytes publish");
        let previous = first.asset.expect("first asset exists");

        let lock = store.lock_exclusive().expect("CAS lock acquired");
        let replacement_bytes = b"GIF89a interrupted replacement";
        let staged = stage_bytes(&store.root_handle, replacement_bytes).expect("new bytes staged");
        let mut next = previous.clone();
        next.sha256 = staged.sha256.clone();
        next.byte_length = staged.byte_length;
        next.format = staged.format;
        next.storage_path = canonical_asset_path(&next.identity, &next.sha256, next.format);
        next.lifecycle = LifecycleState::Pending;
        next.validation = None;

        let publication = prepare_publication(&store.root_handle, &staged, Some(&previous), &next)
            .expect("same-path CAS transaction is durable before publication");
        let marker_name = publication.marker_name.clone();
        let backup_name = publication
            .transaction
            .backup_name
            .clone()
            .expect("same-path replacement keeps an old-byte backup");
        apply_publication(&store.root_handle, &staged, &publication)
            .expect("new bytes atomically replace the stable path");
        assert_eq!(
            fs::read(root.join("assets/元.gif")).expect("replacement is published"),
            replacement_bytes
        );
        assert!(root.join(TEMP_DIR).join(&marker_name).exists());
        assert!(root.join(TEMP_DIR).join(&backup_name).exists());

        drop(staged);
        lock.unlock().expect("test releases CAS lock");
        drop(store);

        let reopened = AssetStore::open(StoreOptions::new(&root))
            .expect("open recovers interrupted in-place CAS before validation");
        let records = reopened
            .verify_integrity()
            .expect("previous verified manifest and bytes match");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sha256, previous.sha256);
        assert_eq!(records[0].lifecycle, LifecycleState::Verified);
        assert_eq!(
            fs::read(root.join("assets/元.gif")).expect("previous stable bytes restored"),
            previous_bytes
        );
        assert_eq!(
            fs::read_dir(root.join(TEMP_DIR))
                .expect("temporary directory exists")
                .count(),
            0,
            "recovery clears the marker, backup and staged artifact"
        );
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
                expected_source_sha256: None,
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
                expected_source_sha256: None,
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

#[cfg(test)]
#[path = "trust_tests.rs"]
mod trust_tests;
