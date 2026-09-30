//! Правила явного разрешения и локальный план размещения медиафайлов для create.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use asset_store::kanji_validator::KanjiImageValidator;
use asset_store::store::VerifiedAssetBytes;
use asset_store::{AssetIdentity, AssetStore};
use rustix::fs::{AtFlags, Mode, OFlags, linkat, mkdirat, openat, unlinkat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{DomainError, ErrorCode};
use crate::ops::create::MAX_REPORTED_NOTES;
use crate::{media, write::ExportLock};

pub const CONFIG_PATH: &str = ".anki-repo/create.yaml";

#[derive(Debug, Default, Clone)]
pub struct MediaOptions {
    pub config: Option<PathBuf>,
    pub asset_store: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    schema_version: u32,
    note_models: Vec<ModelRule>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRule {
    crowdanki_uuid: String,
    fields: BTreeMap<String, FieldRule>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldRule {
    processors: Vec<Processor>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Processor {
    #[serde(rename = "type")]
    kind: ProcessorType,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProcessorType {
    KanjiAssets,
}

pub fn blocker(code: ErrorCode, reason: &str, details: Value) -> DomainError {
    DomainError::with_details(
        code,
        format!("создание медиафайлов заблокировано: {reason}"),
        json!({"reason": reason, "evidence": details}),
    )
}

pub struct Routing {
    policy: Option<Policy>,
    store: Option<PathBuf>,
    config: Option<PathBuf>,
}
impl Routing {
    pub fn load(export: &Path, options: &MediaOptions) -> Result<Self, DomainError> {
        let absolute = std::fs::canonicalize(export).map_err(|e| {
            blocker(
                ErrorCode::InvalidRequest,
                "config_context",
                json!({"message": e.to_string()}),
            )
        })?;
        // Правила ищутся от каталога экспорта, а не от текущего каталога:
        // отдельные экспорты не наследуют правила этого репозитория.
        let repository = absolute.ancestors().find(|p| p.join(".git").exists());
        let path = options
            .config
            .clone()
            .or_else(|| repository.map(|p| p.join(CONFIG_PATH)));
        let policy = match path.clone() {
            Some(path) => match std::fs::read(&path) {
                Ok(bytes) => Some(Self::parse(&bytes)?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && options.config.is_none() => {
                    None
                }
                Err(e) => {
                    return Err(blocker(
                        ErrorCode::InvalidRequest,
                        "config_invalid",
                        json!({"message": e.to_string()}),
                    ));
                }
            },
            None => None,
        };
        Ok(Self {
            policy,
            config: path,
            store: options
                .asset_store
                .clone()
                .or_else(|| repository.map(|p| p.join(".asset-store/kanji"))),
        })
    }
    pub fn protect_artifact(&self, artifact: Option<&Path>) -> Result<(), DomainError> {
        let Some(path) = artifact else {
            return Ok(());
        };
        let absolute = crate::paths::canonical_ish(path);
        if self
            .config
            .as_ref()
            .is_some_and(|p| crate::paths::paths_alias(path, p))
            || self
                .store
                .as_ref()
                .is_some_and(|p| absolute.starts_with(crate::paths::canonical_ish(p)))
        {
            return Err(blocker(
                ErrorCode::InvalidRequest,
                "emit_resolved_protected_path",
                json!({}),
            ));
        }
        Ok(())
    }
    fn parse(bytes: &[u8]) -> Result<Policy, DomainError> {
        // Разбор YAML в Value отвергает повторяющиеся ключи во всех отображениях
        // до преобразования в структуры предметной области и BTreeMap.
        let tree: serde_yaml::Value = serde_yaml::from_slice(bytes).map_err(|e| {
            blocker(
                ErrorCode::InvalidRequest,
                "config_invalid",
                json!({"message": e.to_string()}),
            )
        })?;
        let policy: Policy = serde_yaml::from_value(tree).map_err(|e| {
            blocker(
                ErrorCode::InvalidRequest,
                "config_invalid",
                json!({"message": e.to_string()}),
            )
        })?;
        if policy.schema_version != 1 {
            return Err(blocker(
                ErrorCode::InvalidRequest,
                "config_unsupported",
                json!({"schema_version": policy.schema_version}),
            ));
        }
        let mut models = BTreeSet::new();
        for rule in &policy.note_models {
            if rule.crowdanki_uuid.trim().is_empty()
                || !models.insert(&rule.crowdanki_uuid)
                || rule.fields.is_empty()
            {
                return Err(blocker(
                    ErrorCode::InvalidRequest,
                    "config_invalid",
                    json!({"model": rule.crowdanki_uuid}),
                ));
            }
            for (name, field) in &rule.fields {
                if name.is_empty() || field.processors.len() != 1 {
                    return Err(blocker(
                        ErrorCode::InvalidRequest,
                        "config_invalid",
                        json!({"field": name}),
                    ));
                }
            }
        }
        Ok(policy)
    }
    pub fn fields(&self, uuid: &str, names: &[String]) -> Result<BTreeSet<String>, DomainError> {
        let rule = self
            .policy
            .as_ref()
            .and_then(|p| p.note_models.iter().find(|m| m.crowdanki_uuid == uuid));
        let Some(rule) = rule else {
            return Ok(BTreeSet::new());
        };
        if let Some(stale) = rule.fields.keys().find(|n| !names.contains(n)) {
            return Err(blocker(
                ErrorCode::InvalidRequest,
                "config_stale_field",
                json!({"model_uuid": uuid, "field": stale}),
            ));
        }
        Ok(rule
            .fields
            .iter()
            .filter(|(_, field)| {
                field
                    .processors
                    .iter()
                    .any(|p| matches!(p.kind, ProcessorType::KanjiAssets))
            })
            .map(|(name, _)| name.clone())
            .collect())
    }
    pub fn collect(
        &self,
        note_index: usize,
        uuid: &str,
        values: &[(String, String)],
        refs: &mut Vec<Reference>,
    ) -> Result<Vec<String>, DomainError> {
        let names = values.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        let enabled = self.fields(uuid, &names)?;
        for (field, value) in values {
            let forbidden = media::forbidden_media_references(value);
            if forbidden.is_empty() {
                continue;
            }
            if !enabled.contains(field) {
                return Err(blocker(
                    ErrorCode::MediaForbidden,
                    if self.policy.is_none() {
                        "config_absent"
                    } else {
                        "processor_not_enabled"
                    },
                    json!({"model_uuid": uuid, "field": field, "references": forbidden}),
                ));
            }
            let html = media::html_media_references(value);
            // Каждая ссылка, запрещённая по умолчанию, должна отдельно
            // соответствовать допустимому img/src. CSS, ссылки вида
            // `[sound:...]`, атрибут srcset и незакрытые конструкции не
            // считаются разрешёнными.
            let mut claimed = Vec::new();
            for reference in html {
                if reference.element != "img" || reference.attribute != "src" {
                    return Err(blocker(
                        ErrorCode::MediaForbidden,
                        "media_reference_unclaimed",
                        json!({"reference": reference.value, "field": field}),
                    ));
                }
                let Some((stem, ext)) = reference.value.rsplit_once('.') else {
                    return Err(blocker(
                        ErrorCode::MediaForbidden,
                        "media_reference_unclaimed",
                        json!({"reference": reference.value}),
                    ));
                };
                if !matches!(ext, "gif" | "png")
                    || asset_store::kanji_domain::parse_kanji_character(stem).is_err()
                {
                    return Err(blocker(
                        ErrorCode::MediaForbidden,
                        "media_reference_unclaimed",
                        json!({"reference": reference.value}),
                    ));
                }
                let identity = match AssetIdentity::new("kanji", stem) {
                    Ok(identity) => identity,
                    Err(_) => {
                        return Err(blocker(
                            ErrorCode::MediaForbidden,
                            "media_reference_unclaimed",
                            json!({"reference": &reference.value, "field": field}),
                        ));
                    }
                };
                claimed.push(reference.value.clone());
                refs.push(Reference {
                    note_index,
                    model_uuid: uuid.into(),
                    field: field.clone(),
                    filename: reference.value,
                    identity,
                });
            }
            let mut observed = forbidden.clone();
            observed.sort();
            claimed.sort();
            if observed != claimed {
                return Err(blocker(
                    ErrorCode::MediaForbidden,
                    "media_reference_unclaimed",
                    json!({"field": field, "references": forbidden}),
                ));
            }
        }
        Ok(enabled.into_iter().collect())
    }
    pub fn resolve(
        &self,
        export: &Path,
        refs: Vec<Reference>,
        pins: Option<&[Pin]>,
    ) -> Result<MediaPlan, DomainError> {
        let identities = refs
            .iter()
            .map(|r| r.identity.clone())
            .collect::<BTreeSet<_>>();
        if let Some(pins) = pins {
            let pinned_identities = pins
                .iter()
                .map(|pin| pin.identity.clone())
                .collect::<BTreeSet<_>>();
            if pinned_identities != identities {
                return Err(blocker(
                    ErrorCode::ExpectedMismatch,
                    "stale_pinned_asset",
                    json!({"expected_identities": pinned_identities, "requested_identities": identities}),
                ));
            }
        }
        if refs.is_empty() {
            return Ok(MediaPlan::default());
        }
        let identities = identities.into_iter().collect::<Vec<_>>();
        let root = self.store.as_ref().ok_or_else(|| match pins {
            Some(pins) => blocker(
                ErrorCode::ExpectedMismatch,
                "stale_pinned_asset",
                json!({"expected": pins, "identities": identities, "store_missing": true}),
            ),
            None => blocker(
                ErrorCode::InvalidRequest,
                "kanji_asset_missing",
                json!({"identities": identities}),
            ),
        })?;
        let store_path = crate::paths::canonical_ish(root);
        let export_path = crate::paths::canonical_ish(export);
        if store_path.starts_with(&export_path) || export_path.starts_with(&store_path) {
            return Err(blocker(
                ErrorCode::InvalidRequest,
                "asset_store_boundary",
                json!({}),
            ));
        }
        let assets = AssetStore::read_verified(
            root,
            &identities,
            &KanjiImageValidator::validator_identity(),
        )
        .map_err(|e| {
            let missing_pinned_identity = e
                .details
                .get("identity")
                .and_then(|identity| serde_json::from_value::<AssetIdentity>(identity.clone()).ok())
                .is_some_and(|identity| {
                    pins.is_some_and(|pins| pins.iter().any(|pin| pin.identity == identity))
                });
            if pins.is_some()
                && (e.code == asset_store::ErrorCode::StoreMissing || missing_pinned_identity)
            {
                return blocker(
                    ErrorCode::ExpectedMismatch,
                    "stale_pinned_asset",
                    json!({"expected": pins, "missing_identity": e.details.get("identity"), "asset_code": e.code.as_str(), "details": e.details, "message": e.message}),
                );
            }
            let reason = if e.code == asset_store::ErrorCode::StoreMissing
                || (e.code == asset_store::ErrorCode::MissingAssetFile
                    && e.details.get("identity").is_some()
                    && e.details.get("storage_path").is_none())
            {
                "kanji_asset_missing"
            } else {
                "asset_integrity_invalid"
            };
            blocker(
                ErrorCode::InvalidRequest,
                reason,
                json!({"asset_code": e.code.as_str(), "details": e.details, "message": e.message}),
            )
        })?;
        let mut plan = MediaPlan::default();
        for asset in assets {
            let filename = verified_asset_filename(&asset.record.storage_path)?.to_owned();
            let pin = Pin {
                identity: asset.record.identity.clone(),
                filename: filename.clone(),
                sha256: asset.record.sha256.clone(),
            };
            plan.items.push(Item {
                pin,
                action: String::new(),
                asset,
            });
        }
        let actual_pins = plan.pins();
        if pins.is_some_and(|pins| pins != actual_pins.as_slice()) {
            return Err(blocker(
                ErrorCode::ExpectedMismatch,
                "stale_pinned_asset",
                json!({"expected": pins, "actual": actual_pins}),
            ));
        }
        for item in &plan.items {
            for reference in refs.iter().filter(|r| r.identity == item.pin.identity) {
                if reference.filename != item.pin.filename {
                    return Err(blocker(
                        ErrorCode::ExpectedMismatch,
                        "canonical_filename_mismatch",
                        json!({"requested": reference.filename, "canonical_filename": item.pin.filename}),
                    ));
                }
            }
        }
        // Закреплённые значения проверяются раньше целевого файла: изменение
        // набора изображений не маскируется конфликтом или повторным
        // использованием уже размещённого файла.
        for item in &mut plan.items {
            item.action = destination(export, &item.pin.filename, &item.asset.bytes)?.into();
        }
        plan.references = refs;
        Ok(plan)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    pub note_index: usize,
    pub model_uuid: String,
    pub field: String,
    pub filename: String,
    pub identity: AssetIdentity,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub identity: AssetIdentity,
    pub filename: String,
    pub sha256: String,
}
#[derive(Debug, Clone)]
struct Item {
    pin: Pin,
    action: String,
    asset: VerifiedAssetBytes,
}
#[derive(Debug, Default, Clone)]
pub struct MediaPlan {
    pub references: Vec<Reference>,
    items: Vec<Item>,
    pub declarations_added: Vec<String>,
    pub mutations: usize,
}
impl MediaPlan {
    pub fn pins(&self) -> Vec<Pin> {
        self.items.iter().map(|i| i.pin.clone()).collect()
    }
    pub fn filenames(&self) -> Vec<String> {
        self.items.iter().map(|i| i.pin.filename.clone()).collect()
    }
    /// Число уникальных проверенных файлов в плане.
    pub fn assets_total(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn evidence(&self) -> Value {
        let references_total = self.references.len();
        json!({
            "references": self.references.iter().take(MAX_REPORTED_NOTES).collect::<Vec<_>>(),
            "references_total": references_total,
            "references_truncated": references_total > MAX_REPORTED_NOTES,
            "assets": self.items.iter().map(|i| json!({"identity": i.pin.identity, "canonical_filename": i.pin.filename, "sha256": i.pin.sha256, "destination": format!("media/{}", i.pin.filename), "action": i.action})).collect::<Vec<_>>(),
            "media_files_added": self.declarations_added,
            "mutations_planned": self.items.iter().filter(|i| i.action == "copy").count(),
            "mutations_applied": self.mutations
        })
    }
    pub fn materialize(&mut self, guard: &ExportLock) -> Result<(), DomainError> {
        let media = open_media(&guard.directory, true)?.ok_or_else(|| {
            blocker(
                ErrorCode::WriteFailed,
                "media_directory_changed",
                json!({"operation": "create_media_directory"}),
            )
        })?;
        for item in &self.items {
            if exact_state(&media, &item.pin.filename, &item.asset.bytes)? == "reuse" {
                continue;
            }
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let temp = format!(".anki-repo-media-{}-{id}", std::process::id());
            let mut file = File::from(
                openat(
                    &guard.directory,
                    temp.as_str(),
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_bits_truncate(0o644),
                )
                .map_err(io_failure)?,
            );
            let result = (|| {
                file.write_all(&item.asset.bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(io_failure)?;
                checkpoint("before_link")?;
                match linkat(
                    &guard.directory,
                    temp.as_str(),
                    &media,
                    item.pin.filename.as_str(),
                    AtFlags::empty(),
                ) {
                    Ok(()) => {
                        self.mutations += 1;
                        checkpoint("asset_copied")?;
                        media.sync_all().map_err(io_failure)?;
                    }
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(io_failure(e)),
                }
                require_present(&media, &item.pin.filename, &item.asset.bytes)?;
                Ok(())
            })();
            let cleanup =
                unlinkat(&guard.directory, temp.as_str(), AtFlags::empty()).map_err(io_failure);
            let sync_export = guard.directory.sync_all().map_err(io_failure);
            result?;
            cleanup?;
            sync_export?;
        }
        // Последняя проверка перед публикацией через те же дескрипторы,
        // открытые с `NOFOLLOW`.
        for item in &self.items {
            require_present(&media, &item.pin.filename, &item.asset.bytes)?;
        }
        let current = open_media(&guard.directory, false)?
            .ok_or_else(|| blocker(ErrorCode::WriteFailed, "media_directory_changed", json!({})))?;
        use std::os::unix::fs::MetadataExt;
        let a = current.metadata().map_err(io_failure)?;
        let b = media.metadata().map_err(io_failure)?;
        if a.ino() != b.ino() || a.dev() != b.dev() {
            return Err(blocker(
                ErrorCode::WriteFailed,
                "media_directory_changed",
                json!({}),
            ));
        }
        Ok(())
    }
}
static COUNTER: AtomicU64 = AtomicU64::new(0);
fn verified_asset_filename(storage_path: &str) -> Result<&str, DomainError> {
    storage_path
        .strip_prefix("assets/")
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            crate::ops::source::internal("проверенное хранилище вернуло путь вне assets/")
        })
}

fn io_failure(e: impl std::fmt::Display) -> DomainError {
    blocker(
        ErrorCode::WriteFailed,
        "media_materialization_failed",
        json!({"message": e.to_string()}),
    )
}
fn open_media(export: &File, create: bool) -> Result<Option<File>, DomainError> {
    if create {
        match mkdirat(export, "media", Mode::from_bits_truncate(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(io_failure(e)),
        }
    }
    match openat(
        export,
        "media",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(rustix::io::Errno::NOENT) if !create => Ok(None),
        Err(e) => Err(io_failure(e)),
    }
}
fn destination(export: &Path, name: &str, bytes: &[u8]) -> Result<&'static str, DomainError> {
    let directory = File::open(export).map_err(io_failure)?;
    match open_media(&directory, false)? {
        Some(media) => exact_state(&media, name, bytes),
        None => Ok("copy"),
    }
}
fn exact_state(directory: &File, name: &str, expected: &[u8]) -> Result<&'static str, DomainError> {
    let fd = match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok("copy"),
        Err(e) => {
            return Err(blocker(
                ErrorCode::ExpectedMismatch,
                "destination_media_conflict",
                json!({"filename": name, "message": e.to_string(), "action": "conflict"}),
            ));
        }
    };
    let mut file = File::from(fd);
    if !file.metadata().map_err(io_failure)?.is_file() {
        return Err(blocker(
            ErrorCode::ExpectedMismatch,
            "destination_media_conflict",
            json!({"filename": name, "action": "conflict"}),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_failure)?;
    if bytes != expected {
        return Err(blocker(
            ErrorCode::ExpectedMismatch,
            "destination_media_conflict",
            json!({"filename": name, "expected_sha256": format!("{:x}", Sha256::digest(expected)), "actual_sha256": format!("{:x}", Sha256::digest(&bytes)), "action": "conflict"}),
        ));
    }
    Ok("reuse")
}

fn require_present(directory: &File, name: &str, bytes: &[u8]) -> Result<(), DomainError> {
    if exact_state(directory, name, bytes)? != "reuse" {
        return Err(blocker(
            ErrorCode::WriteFailed,
            "media_disappeared",
            json!({"filename": name}),
        ));
    }
    Ok(())
}

#[cfg(test)]
type TestHook = Box<dyn FnMut(&str) -> Result<(), DomainError>>;
#[cfg(test)]
thread_local! {
    static TEST_HOOK: std::cell::RefCell<Option<TestHook>> = const { std::cell::RefCell::new(None) };
}
pub(crate) fn checkpoint(phase: &str) -> Result<(), DomainError> {
    #[cfg(test)]
    return TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(f) => f(phase),
        None => Ok(()),
    });
    #[cfg(not(test))]
    {
        let _ = phase;
        Ok(())
    }
}

#[cfg(test)]
#[path = "create_media_tests.rs"]
mod tests;
