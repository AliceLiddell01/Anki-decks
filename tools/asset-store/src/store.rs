//! Program-owned filesystem store с атомарным versioned manifest.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
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
    root: PathBuf,
    store_id: String,
    initialized_on_open: bool,
    #[cfg(test)]
    fail_next_manifest_write: std::sync::atomic::AtomicBool,
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
    path: PathBuf,
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct StagedObject {
    artifact: TempArtifact,
    sha256: String,
    byte_length: u64,
    format: DetectedFormat,
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

        fs::create_dir_all(&requested_root)
            .map_err(|error| AssetError::io("не удалось создать store root", error))?;
        let canonical_root = fs::canonicalize(&requested_root)
            .map_err(|error| AssetError::io("не удалось разрешить store root", error))?;
        if canonical_root != requested_root {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "store root изменился через symlink или alias во время открытия",
            ));
        }
        ensure_directory(&canonical_root)?;
        preflight_root_ownership(&canonical_root)?;

        let lock_path = canonical_root.join(LOCK_FILE);
        ensure_lock_path(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| AssetError::io("не удалось открыть lock store", error))?;
        FileExt::lock_exclusive(&lock)
            .map_err(|error| AssetError::io("не удалось заблокировать store", error))?;

        let initialized_on_open = initialize_or_load(&canonical_root)?;
        let manifest = load_manifest(&canonical_root)?;
        validate_manifest(&canonical_root, &manifest)?;

        let store = Self {
            root: canonical_root,
            store_id: manifest.store_id,
            initialized_on_open,
            #[cfg(test)]
            fail_next_manifest_write: std::sync::atomic::AtomicBool::new(false),
        };
        FileExt::unlock(&lock)
            .map_err(|error| AssetError::io("не удалось снять lock store", error))?;
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
        let manifest = load_manifest(&self.root)?;
        validate_manifest(&self.root, &manifest)?;
        let assets = manifest.assets;
        FileExt::unlock(&lock)
            .map_err(|error| AssetError::io("не удалось снять lock store", error))?;
        Ok(assets)
    }

    /// Явно импортирует один файл, вычисляя SHA-256 по скопированным bytes.
    /// Повтор той же identity/hash — no-op; другой hash требует ожидаемый hash.
    pub fn ingest(&self, request: IngestRequest) -> Result<IngestOutcome, AssetError> {
        request
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        let lock = self.lock_exclusive()?;
        let mut manifest = load_manifest(&self.root)?;
        validate_manifest(&self.root, &manifest)?;
        let staged = stage_source(&self.root, &request.source_path)?;
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
                FileExt::unlock(&lock)
                    .map_err(|error| AssetError::io("не удалось снять lock store", error))?;
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
        let object_path = self.root.join(&storage_path);
        publish_object(&staged, &object_path)?;
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
        validate_object(&self.root, &record)?;

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
        save_manifest(&self.root, &manifest, false)?;
        drop(staged);
        FileExt::unlock(&lock)
            .map_err(|error| AssetError::io("не удалось снять lock store", error))?;
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
        let manifest = load_manifest(&self.root)?;
        validate_manifest(&self.root, &manifest)?;
        let assets = select_assets(&manifest.assets, mode, validator)
            .into_iter()
            .cloned()
            .collect();
        FileExt::unlock(&lock)
            .map_err(|error| AssetError::io("не удалось снять lock store", error))?;
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
        let mut manifest = load_manifest(&self.root)?;
        validate_manifest(&self.root, &manifest)?;
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
            let path = checked_object_path(&self.root, &record)?;
            let mut file = open_regular_file(&path, ErrorCode::MissingAssetFile)?;
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
            validate_object(&self.root, record)?;
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
            let inject_failure = self.fail_next_manifest_write.swap(false, Ordering::SeqCst);
            #[cfg(not(test))]
            let inject_failure = false;
            if inject_failure {
                return Err(AssetError::new(
                    ErrorCode::IoFailure,
                    "тестовая ошибка перед атомарной публикацией manifest",
                ));
            }
            save_manifest(&self.root, &manifest, false)?;
        }
        FileExt::unlock(&lock)
            .map_err(|error| AssetError::io("не удалось снять lock store", error))?;
        Ok(report)
    }

    fn lock_shared(&self) -> Result<File, AssetError> {
        self.lock(false)
    }

    fn lock_exclusive(&self) -> Result<File, AssetError> {
        self.lock(true)
    }

    fn lock(&self, exclusive: bool) -> Result<File, AssetError> {
        ensure_directory(&self.root)?;
        let path = self.root.join(LOCK_FILE);
        ensure_lock_path(&path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(false)
            .open(path)
            .map_err(|error| AssetError::io("не удалось открыть lock store", error))?;
        if exclusive {
            FileExt::lock_exclusive(&lock)
        } else {
            FileExt::lock_shared(&lock)
        }
        .map_err(|error| AssetError::io("не удалось заблокировать store", error))?;
        Ok(lock)
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

fn initialize_or_load(root: &Path) -> Result<bool, AssetError> {
    ensure_top_level(root)?;
    ensure_dir_entry(root, OBJECTS_DIR)?;
    ensure_dir_entry(root, TEMP_DIR)?;

    let owner_path = root.join(OWNER_FILE);
    let manifest_path = root.join(MANIFEST_FILE);
    let owner_exists = path_exists_no_symlink(&owner_path)?;
    let manifest_exists = path_exists_no_symlink(&manifest_path)?;

    if owner_exists {
        let bytes = fs::read(&owner_path)
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
        if !manifest_exists {
            return Err(AssetError::new(
                ErrorCode::ManifestMissing,
                "owner marker существует, но canonical manifest отсутствует",
            ));
        }
        let manifest = read_manifest_file(&manifest_path)?;
        check_schema(&manifest)?;
        if manifest.store_id != marker.store_id {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "store_id manifest не совпадает с owner marker",
            ));
        }
        return Ok(false);
    }

    if manifest_exists {
        let manifest = read_manifest_file(&manifest_path)?;
        check_schema(&manifest)?;
        if manifest.revision != 0 || !manifest.assets.is_empty() {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "manifest без owner marker не доказывает владение существующими assets",
            ));
        }
        write_owner_marker(root, &manifest.store_id)?;
        return Ok(true);
    }

    ensure_empty_owned_directories(root)?;
    let store_id = new_store_id();
    let manifest = Manifest::empty(store_id.clone());
    save_manifest(root, &manifest, true)?;
    write_owner_marker(root, &store_id)?;
    Ok(true)
}

/// Не создаёт lock-файл в существующем каталоге, пока не подтверждено, что
/// directory пуст либо имеет только файлы/каталоги, принадлежащие этому store.
fn preflight_root_ownership(root: &Path) -> Result<(), AssetError> {
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(root)
        .map_err(|error| AssetError::io("не удалось проверить store root", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось проверить store entry", error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            LOCK_FILE | OWNER_FILE | MANIFEST_FILE | OBJECTS_DIR | TEMP_DIR
        ) {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                format!("каталог не принадлежит asset store; найдено: {name}"),
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
                ErrorCode::StoreNotOwned,
                format!("{name} существует, но не является каталогом"),
            ));
        }
        if matches!(name.as_str(), LOCK_FILE | OWNER_FILE | MANIFEST_FILE) && !metadata.is_file() {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                format!("{name} существует, но не является обычным файлом"),
            ));
        }
        names.insert(name);
    }
    if !names.contains(OWNER_FILE) && !names.contains(MANIFEST_FILE) && !names.contains(LOCK_FILE) {
        for name in [OBJECTS_DIR, TEMP_DIR] {
            let path = root.join(name);
            if path.exists()
                && fs::read_dir(&path)
                    .map_err(|error| AssetError::io("не удалось проверить новый store", error))?
                    .next()
                    .is_some()
            {
                return Err(AssetError::new(
                    ErrorCode::StoreNotOwned,
                    format!("каталог без owner marker содержит данные в {name}"),
                ));
            }
        }
    }
    Ok(())
}

fn ensure_top_level(root: &Path) -> Result<(), AssetError> {
    for entry in fs::read_dir(root)
        .map_err(|error| AssetError::io("не удалось прочитать store root", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать store entry", error))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !matches!(
            name.as_ref(),
            LOCK_FILE | OWNER_FILE | MANIFEST_FILE | OBJECTS_DIR | TEMP_DIR
        ) {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
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
        if matches!(name.as_ref(), OBJECTS_DIR | TEMP_DIR) && !metadata.is_dir() {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("{name} должен быть каталогом"),
            ));
        }
        if matches!(name.as_ref(), LOCK_FILE | OWNER_FILE | MANIFEST_FILE) && !metadata.is_file() {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("{name} должен быть обычным файлом"),
            ));
        }
    }
    Ok(())
}

fn ensure_empty_owned_directories(root: &Path) -> Result<(), AssetError> {
    for name in [OBJECTS_DIR, TEMP_DIR] {
        let path = root.join(name);
        ensure_dir_entry(root, name)?;
        if fs::read_dir(&path)
            .map_err(|error| AssetError::io("не удалось проверить новый store", error))?
            .next()
            .is_some()
        {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                format!("неинициализированный каталог {name} уже содержит файлы"),
            ));
        }
    }
    Ok(())
}

fn ensure_dir_entry(root: &Path, name: &str) -> Result<(), AssetError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!("{name} должен быть обычным каталогом без symlink"),
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)
            .map_err(|error| AssetError::io(format!("не удалось создать {name}"), error)),
        Err(error) => Err(AssetError::io(
            format!("не удалось проверить {name}"),
            error,
        )),
    }
}

fn write_owner_marker(root: &Path, store_id: &str) -> Result<(), AssetError> {
    let marker = OwnerMarker {
        schema_version: OWNER_SCHEMA_VERSION,
        store_id: store_id.to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(root, &root.join(OWNER_FILE), &bytes, true)
}

fn load_manifest(root: &Path) -> Result<Manifest, AssetError> {
    ensure_top_level(root)?;
    let path = root.join(MANIFEST_FILE);
    ensure_regular_file(&path, ErrorCode::ManifestMissing)?;
    let manifest = read_manifest_file(&path)?;
    check_schema(&manifest)?;
    let owner_bytes = fs::read(root.join(OWNER_FILE))
        .map_err(|error| AssetError::io("не удалось прочитать owner marker", error))?;
    let owner: OwnerMarker = serde_json::from_slice(&owner_bytes).map_err(|error| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("owner marker невалиден: {error}"),
        )
    })?;
    if owner.schema_version != OWNER_SCHEMA_VERSION {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "неподдерживаемая версия owner marker {}",
                owner.schema_version
            ),
        ));
    }
    if owner.store_id != manifest.store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id manifest не совпадает с owner marker",
        ));
    }
    Ok(manifest)
}

fn read_manifest_file(path: &Path) -> Result<Manifest, AssetError> {
    let bytes =
        fs::read(path).map_err(|error| AssetError::io("не удалось прочитать manifest", error))?;
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

fn validate_manifest(root: &Path, manifest: &Manifest) -> Result<(), AssetError> {
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

fn validate_object(root: &Path, record: &AssetRecord) -> Result<(), AssetError> {
    let path = checked_object_path(root, record)?;
    let (sha256, byte_length, format) = hash_file(&path)?;
    if sha256 != record.sha256 || byte_length != record.byte_length || format != record.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            format!("файл asset {} не совпадает с manifest", record.identity),
        ));
    }
    Ok(())
}

fn validate_object_directory(root: &Path) -> Result<(), AssetError> {
    let path = root.join(OBJECTS_DIR);
    ensure_directory(&path)?;
    for entry in fs::read_dir(&path)
        .map_err(|error| AssetError::io("не удалось прочитать object store", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать object entry", error))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AssetError::io("не удалось проверить object entry", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "object store содержит symlink или вложенный каталог",
            ));
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(hash) = name.strip_suffix(".blob") else {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("неожиданный файл в object store: {name}"),
            ));
        };
        validate_hash(hash)?;
        let (actual_hash, _, _) = hash_file(&entry.path())?;
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

fn checked_object_path(root: &Path, record: &AssetRecord) -> Result<PathBuf, AssetError> {
    let expected = object_relative_path(&record.sha256);
    validate_relative_path(&record.storage_path, &expected)?;
    let objects = root.join(OBJECTS_DIR);
    ensure_directory(&objects)?;
    Ok(root.join(expected))
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

fn stage_source(root: &Path, source: &Path) -> Result<StagedObject, AssetError> {
    ensure_directory(&root.join(TEMP_DIR))?;
    let source_metadata = fs::symlink_metadata(source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::SourceMissing,
                format!("explicit source file отсутствует: {}", source.display()),
            )
        } else {
            AssetError::io("не удалось проверить source file", error)
        }
    })?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "explicit source должен быть обычным файлом без symlink",
        ));
    }
    let mut input = File::open(source)
        .map_err(|error| AssetError::io("не удалось открыть source file", error))?;
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

fn publish_object(staged: &StagedObject, destination: &Path) -> Result<(), AssetError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AssetError::new(ErrorCode::InvalidStoreRoot, "object path без parent"))?;
    ensure_directory(parent)?;
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    "content-addressed destination должен быть обычным файлом",
                ));
            }
            let (hash, length, format) = hash_file(destination)?;
            if hash != staged.sha256 || length != staged.byte_length || format != staged.format {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "существующий object path не совпадает с вычисленным content hash",
                ));
            }
            make_readonly(destination)?;
            sync_directory(parent)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::hard_link(&staged.artifact.path, destination) {
                Ok(()) => {
                    make_readonly(destination)?;
                    sync_directory(parent)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let (hash, length, format) = hash_file(destination)?;
                    if hash == staged.sha256
                        && length == staged.byte_length
                        && format == staged.format
                    {
                        Ok(())
                    } else {
                        Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "object path создан конкурентным процессом с другими bytes",
                        ))
                    }
                }
                Err(error) => Err(AssetError::io("не удалось опубликовать object", error)),
            }
        }
        Err(error) => Err(AssetError::io(
            "не удалось проверить object destination",
            error,
        )),
    }
}

fn make_readonly(path: &Path) -> Result<(), AssetError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AssetError::io("не удалось проверить object permissions", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "object должен быть обычным файлом",
        ));
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|error| AssetError::io("не удалось защитить object от случайной записи", error))?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать object metadata", error))
}

fn save_manifest(root: &Path, manifest: &Manifest, initial: bool) -> Result<(), AssetError> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(root, &root.join(MANIFEST_FILE), &bytes, initial)
}

fn atomic_write(
    root: &Path,
    destination: &Path,
    bytes: &[u8],
    initial: bool,
) -> Result<(), AssetError> {
    ensure_directory(&root.join(TEMP_DIR))?;
    let (temporary, mut file) = create_temp_file(root, "state")?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать state file", error))?;
    drop(file);
    match fs::rename(&temporary.path, destination) {
        Ok(()) => sync_directory(root),
        Err(error) if initial && error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "state file уже существует при инициализации нового store",
            ))
        }
        Err(error) => Err(AssetError::io(
            "не удалось атомарно опубликовать state file",
            error,
        )),
    }
}

fn create_temp_file(root: &Path, prefix: &str) -> Result<(TempArtifact, File), AssetError> {
    let directory = root.join(TEMP_DIR);
    ensure_directory(&directory)?;
    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("{prefix}-{}-{counter}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((TempArtifact { path }, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(AssetError::io("не удалось создать временный файл", error)),
        }
    }
    Err(AssetError::new(
        ErrorCode::IoFailure,
        "не удалось выбрать свободное имя временного файла",
    ))
}

fn hash_file(path: &Path) -> Result<(String, u64, DetectedFormat), AssetError> {
    let mut file = open_regular_file(path, ErrorCode::MissingAssetFile)?;
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

fn open_regular_file(path: &Path, missing_code: ErrorCode) -> Result<File, AssetError> {
    ensure_regular_file(path, missing_code)?;
    File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                missing_code,
                format!("файл исчез перед чтением: {}", path.display()),
            )
        } else {
            AssetError::io("не удалось открыть файл", error)
        }
    })
}

fn ensure_regular_file(path: &Path, missing_code: ErrorCode) -> Result<(), AssetError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            format!("symlink запрещён внутри asset store: {}", path.display()),
        )),
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            format!("ожидался обычный файл: {}", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(AssetError::new(
            missing_code,
            format!("файл отсутствует: {}", path.display()),
        )),
        Err(error) => Err(AssetError::io("не удалось проверить файл", error)),
    }
}

fn ensure_directory(path: &Path) -> Result<(), AssetError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!(
                    "каталог должен быть обычным и без symlink: {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("каталог store отсутствует: {}", path.display()),
        )),
        Err(error) => Err(AssetError::io("не удалось проверить каталог", error)),
    }
}

fn ensure_lock_path(path: &Path) -> Result<(), AssetError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "lock path должен быть обычным файлом без symlink",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AssetError::io("не удалось проверить lock path", error)),
    }
}

fn path_exists_no_symlink(path: &Path) -> Result<bool, AssetError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            format!("symlink запрещён: {}", path.display()),
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AssetError::io("не удалось проверить путь", error)),
    }
}

fn resolve_store_root(root: &Path) -> Result<PathBuf, AssetError> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить cwd", error))?
            .join(root)
    };
    let components: Vec<_> = absolute.components().collect();
    let mut normalized = PathBuf::new();
    for (position, component) in components.iter().enumerate() {
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
        match fs::symlink_metadata(&normalized) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    format!(
                        "store root проходит через symlink: {}",
                        normalized.display()
                    ),
                ));
            }
            Ok(metadata) if !metadata.is_dir() && normalized != absolute => {
                return Err(AssetError::new(
                    ErrorCode::InvalidStoreRoot,
                    format!(
                        "родитель store root не является каталогом: {}",
                        normalized.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for remaining in &components[position + 1..] {
                    match remaining {
                        Component::Normal(part) => normalized.push(part),
                        Component::CurDir => {}
                        _ => {
                            return Err(AssetError::new(
                                ErrorCode::InvalidStoreRoot,
                                "store root должен состоять из обычных path components",
                            ));
                        }
                    }
                }
                break;
            }
            Err(error) => return Err(AssetError::io("не удалось проверить store root", error)),
        }
    }
    Ok(normalized)
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

fn sync_directory(path: &Path) -> Result<(), AssetError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| AssetError::io("не удалось синхронизировать каталог store", error))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

        store.fail_next_manifest_write();
        let error = store
            .validate(SelectionMode::Full, &VerifiedValidator)
            .expect_err("injected publication failure is surfaced");
        assert_eq!(error.code, ErrorCode::IoFailure);
        drop(store);

        let reopened = AssetStore::open(StoreOptions::new(&root)).expect("store remains readable");
        let records = reopened
            .verify_integrity()
            .expect("canonical state remains valid");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lifecycle, LifecycleState::Pending);
        assert!(records[0].validation.is_none());
    }
}
