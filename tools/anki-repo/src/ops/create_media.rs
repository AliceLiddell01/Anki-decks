//! Правила явного разрешения и локальный план размещения медиафайлов для create.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use asset_store::kanji_validator::KanjiImageValidator;
use asset_store::pitch_accent::PitchAccentImageValidator;
use asset_store::store::VerifiedAssetBytes;
use asset_store::{
    AssetDomainPolicy, AssetIdentity, AssetStore, DetectedFormat, KanjiDomainPolicy,
    PitchAccentDomainPolicy, ValidatorIdentity,
};
use rustix::fs::{AtFlags, Mode, OFlags, linkat, mkdirat, openat, unlinkat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{DomainError, ErrorCode};
use crate::ops::create::MAX_REPORTED_NOTES;
use crate::{media, write::ExportLock};

pub const CONFIG_PATH: &str = ".anki-repo/create.yaml";

/// Плоский суффикс канонического имени pitch-accent для потребителя.
///
/// Совпадение с фактическим именем владельца домена проверяется тестом
/// `pitch_consumer_suffix_matches_the_domain_policy`, а не выводится из памяти.
const PITCH_CONSUMER_SUFFIX: &str = ".pitch.png";

#[derive(Debug, Default, Clone)]
pub struct MediaOptions {
    pub config: Option<PathBuf>,
    /// Проверенное хранилище изображений кандзи.
    pub asset_store: Option<PathBuf>,
    /// Проверенное хранилище pitch-accent.
    pub pitch_asset_store: Option<PathBuf>,
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
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProcessorType {
    KanjiAssets,
    PitchAccent,
}

/// Реестр поддерживаемых обработчиков.
///
/// Порядок задаёт детерминированный порядок разрешения ресурсов разных доменов
/// в одном запросе.
const PROCESSORS: &[ProcessorType] = &[ProcessorType::KanjiAssets, ProcessorType::PitchAccent];

impl ProcessorType {
    /// Имя обработчика в YAML и в диагностике.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::KanjiAssets => "kanji_assets",
            Self::PitchAccent => "pitch_accent",
        }
    }

    /// Пространство имён идентичностей, которыми владеет этот обработчик.
    pub(crate) const fn namespace(self) -> &'static str {
        match self {
            Self::KanjiAssets => "kanji",
            Self::PitchAccent => "pitch_accent",
        }
    }

    /// Путь хранилища домена по умолчанию относительно корня репозитория экспорта.
    const fn default_store(self) -> &'static str {
        match self {
            Self::KanjiAssets => ".asset-store/kanji",
            Self::PitchAccent => ".asset-store/pitch-accent",
        }
    }

    /// Ожидаемая версия семантического валидатора домена.
    fn validator(self) -> ValidatorIdentity {
        match self {
            Self::KanjiAssets => KanjiImageValidator::validator_identity(),
            Self::PitchAccent => PitchAccentImageValidator::validator_identity(),
        }
    }

    /// Политика домена: canonical location, допустимый формат и trust semantics.
    pub(crate) fn domain_policy(self) -> &'static dyn AssetDomainPolicy {
        match self {
            Self::KanjiAssets => &KanjiDomainPolicy,
            Self::PitchAccent => &PitchAccentDomainPolicy,
        }
    }

    /// Стабильная причина блокера, когда проверенного ресурса домена нет.
    pub(crate) const fn missing_reason(self) -> &'static str {
        match self {
            Self::KanjiAssets => "kanji_asset_missing",
            Self::PitchAccent => "pitch_asset_missing",
        }
    }

    /// Явный test/local override хранилища домена.
    fn store_override(self, options: &MediaOptions) -> Option<&PathBuf> {
        match self {
            Self::KanjiAssets => options.asset_store.as_ref(),
            Self::PitchAccent => options.pitch_asset_store.as_ref(),
        }
    }

    /// Обратное сопоставление: домен, которому принадлежит пространство имён.
    pub(crate) fn for_namespace(namespace: &str) -> Option<Self> {
        PROCESSORS
            .iter()
            .copied()
            .find(|kind| kind.namespace() == namespace)
    }
}

/// Обработчик владеет только теми ссылками, которые сам распознал. Включение
/// обработчика на поле не разрешает остальные ссылки и не меняет HTML поля.
trait MediaProcessor {
    fn name(&self) -> &'static str;
    fn claim(&self, reference: &media::MediaReference) -> Option<AssetIdentity>;
}
impl MediaProcessor for ProcessorType {
    fn name(&self) -> &'static str {
        (*self).name()
    }
    fn claim(&self, reference: &media::MediaReference) -> Option<AssetIdentity> {
        if reference.element != "img" || reference.attribute != "src" {
            return None;
        }
        match self {
            Self::KanjiAssets => {
                let (stem, ext) = reference.value.rsplit_once('.')?;
                if !matches!(ext, "gif" | "png")
                    || asset_store::kanji_domain::parse_kanji_character(stem).is_err()
                {
                    return None;
                }
                AssetIdentity::new(self.namespace(), stem).ok()
            }
            Self::PitchAccent => {
                // Владельцем ссылки становится только точное каноническое имя
                // домена: `<surface>.pitch.png`. Произвольный PNG, `<surface>.png`,
                // URL, data URI и sound остаются незаявленными.
                let surface = reference.value.strip_suffix(PITCH_CONSUMER_SUFFIX)?;
                if surface.is_empty() {
                    return None;
                }
                let identity = AssetIdentity::new(self.namespace(), surface).ok()?;
                self.domain_policy().validate_identity(&identity).ok()?;
                Some(identity)
            }
        }
    }
}

/// Цепочка исполняется в порядке YAML. План публикуется только после проверки
/// единственного владельца каждой ссылки и полного fail-closed media-гейта.
fn collect_processor_chain(
    processors: &[&dyn MediaProcessor],
    note_index: usize,
    uuid: &str,
    field: &str,
    value: &str,
) -> Result<Vec<Reference>, DomainError> {
    let html = media::html_media_references(value);
    let mut owners: Vec<Option<(&str, AssetIdentity)>> = vec![None; html.len()];
    for processor in processors {
        for (reference, owner) in html.iter().zip(&mut owners) {
            let Some(identity) = processor.claim(reference) else {
                continue;
            };
            if let Some((previous, previous_identity)) = owner {
                return Err(blocker(
                    ErrorCode::MediaForbidden,
                    "media_reference_conflict",
                    json!({"field": field, "reference": reference.value,
                        "processors": [previous, processor.name()],
                        "identities": [previous_identity, &identity]}),
                ));
            }
            *owner = Some((processor.name(), identity));
        }
    }
    let mut refs = Vec::new();
    for (reference, owner) in html.into_iter().zip(owners) {
        let Some((_, identity)) = owner else {
            return Err(blocker(
                ErrorCode::MediaForbidden,
                "media_reference_unclaimed",
                json!({"field": field, "reference": reference.value}),
            ));
        };
        refs.push(Reference {
            note_index,
            model_uuid: uuid.into(),
            field: field.into(),
            filename: reference.value,
            identity,
        });
    }
    let mut observed = media::forbidden_media_references(value);
    let mut claimed = refs.iter().map(|r| r.filename.clone()).collect::<Vec<_>>();
    observed.sort();
    claimed.sort();
    if observed != claimed {
        return Err(blocker(
            ErrorCode::MediaForbidden,
            "media_reference_unclaimed",
            json!({"field": field, "references": observed}),
        ));
    }
    Ok(refs)
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
    config: Option<PathBuf>,
    /// Разрешённые корни хранилищ по пространству имён доменов.
    ///
    /// Domain ownership остаётся явным: один обработчик читает ровно своё
    /// хранилище и не получает неявного доступа к чужим.
    stores: BTreeMap<&'static str, PathBuf>,
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
        let mut stores = BTreeMap::new();
        for kind in PROCESSORS {
            let root = kind
                .store_override(options)
                .cloned()
                .or_else(|| repository.map(|p| p.join(kind.default_store())));
            if let Some(root) = root {
                stores.insert(kind.namespace(), root);
            }
        }
        Ok(Self {
            policy,
            config: path,
            stores,
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
        {
            return Err(blocker(
                ErrorCode::InvalidRequest,
                "emit_resolved_protected_path",
                json!({}),
            ));
        }
        // Артефакт не может попасть ни в одно хранилище, которым пользуется запрос.
        for (namespace, root) in &self.stores {
            if absolute.starts_with(crate::paths::canonical_ish(root)) {
                return Err(blocker(
                    ErrorCode::InvalidRequest,
                    "emit_resolved_protected_path",
                    json!({"domain": namespace}),
                ));
            }
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
                if name.is_empty() || field.processors.is_empty() {
                    return Err(blocker(
                        ErrorCode::InvalidRequest,
                        "config_invalid",
                        json!({"field": name}),
                    ));
                }
                let mut processors = BTreeSet::new();
                for processor in &field.processors {
                    if !processors.insert(&processor.kind) {
                        return Err(blocker(
                            ErrorCode::InvalidRequest,
                            "config_invalid",
                            json!({"field": name, "duplicate_processor": processor.kind.name()}),
                        ));
                    }
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
        Ok(rule.fields.keys().cloned().collect())
    }
    /// Владеет ли это поле обработчиком указанного домена.
    ///
    /// Нужно миграции legacy consumer filename: ссылку на старое имя вправе
    /// переписать только то поле, которому принадлежит каноническое имя домена.
    /// Включение обработчика на поле — часть конфигурации, а не догадка по имени
    /// поля, поэтому ответ берётся из разобранной policy.
    pub(crate) fn owns_field(&self, uuid: &str, field: &str, kind: ProcessorType) -> bool {
        self.policy
            .as_ref()
            .and_then(|policy| {
                policy
                    .note_models
                    .iter()
                    .find(|rule| rule.crowdanki_uuid == uuid)
            })
            .and_then(|rule| rule.fields.get(field))
            .is_some_and(|rule| {
                rule.processors
                    .iter()
                    .any(|processor| processor.kind == kind)
            })
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
            // Выбор строго по UUID и точному полю уже проверен через fields().
            let chain = &self
                .policy
                .as_ref()
                .and_then(|p| p.note_models.iter().find(|m| m.crowdanki_uuid == uuid))
                .and_then(|m| m.fields.get(field))
                .expect("для включённого поля настроена цепочка обработчиков")
                .processors;
            let processors = chain
                .iter()
                .map(|p| &p.kind as &dyn MediaProcessor)
                .collect::<Vec<_>>();
            refs.extend(collect_processor_chain(
                &processors,
                note_index,
                uuid,
                field,
                value,
            )?);
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
        // Каждый домен читается только своим обработчиком: своё хранилище, своя
        // policy, свой ожидаемый validator. Порядок доменов детерминирован
        // реестром обработчиков.
        let mut by_domain = BTreeMap::<ProcessorType, BTreeSet<AssetIdentity>>::new();
        for identity in identities {
            let kind = ProcessorType::for_namespace(&identity.namespace).ok_or_else(|| {
                blocker(
                    ErrorCode::MediaForbidden,
                    "media_domain_unknown",
                    json!({"identity": identity}),
                )
            })?;
            by_domain.entry(kind).or_default().insert(identity);
        }
        let mut plan = MediaPlan::default();
        for (kind, domain_identities) in by_domain {
            let domain_identities = domain_identities.into_iter().collect::<Vec<_>>();
            for asset in self.read_domain(export, kind, &domain_identities, pins)? {
                let filename = verified_asset_filename(&asset)?.to_owned();
                let pin = Pin {
                    identity: asset.record.identity.clone(),
                    filename,
                    sha256: asset.record.sha256.clone(),
                };
                plan.items.push(Item {
                    pin,
                    action: String::new(),
                    asset,
                });
            }
        }
        let actual_pins = plan.pins();
        if pins.is_some_and(|pins| pins != actual_pins.as_slice()) {
            return Err(blocker(
                ErrorCode::ExpectedMismatch,
                "stale_pinned_asset",
                json!({"expected": pins, "actual": actual_pins}),
            ));
        }
        // Плоское пространство имён media в CrowdAnki общее для всех доменов:
        // одно имя не может одновременно означать разные байты.
        check_filename_ownership(&plan.items)?;
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

    /// Читает проверенные байты одного домена через его policy и validator.
    pub(crate) fn read_domain(
        &self,
        export: &Path,
        kind: ProcessorType,
        identities: &[AssetIdentity],
        pins: Option<&[Pin]>,
    ) -> Result<Vec<VerifiedAssetBytes>, DomainError> {
        let Some(root) = self.stores.get(kind.namespace()) else {
            return Err(match pins {
                Some(pins) => blocker(
                    ErrorCode::ExpectedMismatch,
                    "stale_pinned_asset",
                    json!({"expected": pins, "identities": identities, "store_missing": true, "domain": kind.namespace()}),
                ),
                None => blocker(
                    ErrorCode::InvalidRequest,
                    kind.missing_reason(),
                    json!({"identities": identities, "domain": kind.namespace()}),
                ),
            });
        };
        let store_path = crate::paths::canonical_ish(root);
        let export_path = crate::paths::canonical_ish(export);
        if store_path.starts_with(&export_path) || export_path.starts_with(&store_path) {
            return Err(blocker(
                ErrorCode::InvalidRequest,
                "asset_store_boundary",
                json!({"domain": kind.namespace()}),
            ));
        }
        AssetStore::read_verified_with_policy(
            root,
            identities,
            &kind.validator(),
            kind.domain_policy(),
        )
        .map_err(|error| domain_read_error(kind, error, pins))
    }
}

/// Отказывает, если два домена претендуют на одно плоское имя media с разными
/// bytes. Совпадение имени при одинаковом SHA безвредно: потребитель видит одни
/// и те же данные.
fn check_filename_ownership(items: &[Item]) -> Result<(), DomainError> {
    let mut owners = BTreeMap::<&str, (&AssetIdentity, &str)>::new();
    for item in items {
        match owners.entry(item.pin.filename.as_str()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((&item.pin.identity, item.pin.sha256.as_str()));
            }
            std::collections::btree_map::Entry::Occupied(slot) => {
                let (owner_identity, owner_sha256) = *slot.get();
                if owner_sha256 != item.pin.sha256 {
                    return Err(blocker(
                        ErrorCode::ExpectedMismatch,
                        "media_filename_collision",
                        json!({
                            "filename": item.pin.filename,
                            "identities": [owner_identity, &item.pin.identity],
                            "sha256": [owner_sha256, &item.pin.sha256],
                        }),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Блокер отказа чтения проверенного корпуса конкретного домена.
///
/// Причина остаётся доменной (`<domain>_asset_missing`), поэтому отсутствие
/// pitch-ресурса не маскируется под отсутствие kanji-ресурса.
fn domain_read_error(
    kind: ProcessorType,
    error: asset_store::AssetError,
    pins: Option<&[Pin]>,
) -> DomainError {
    let missing_pinned_identity = error
        .details
        .get("identity")
        .and_then(|identity| serde_json::from_value::<AssetIdentity>(identity.clone()).ok())
        .is_some_and(|identity| {
            pins.is_some_and(|pins| pins.iter().any(|pin| pin.identity == identity))
        });
    if pins.is_some()
        && (error.code == asset_store::ErrorCode::StoreMissing || missing_pinned_identity)
    {
        return blocker(
            ErrorCode::ExpectedMismatch,
            "stale_pinned_asset",
            json!({"expected": pins, "missing_identity": error.details.get("identity"), "asset_code": error.code.as_str(), "details": error.details, "message": error.message, "domain": kind.namespace()}),
        );
    }
    let reason = if error.code == asset_store::ErrorCode::StoreMissing
        || (error.code == asset_store::ErrorCode::MissingAssetFile
            && error.details.get("identity").is_some()
            && error.details.get("storage_path").is_none())
    {
        kind.missing_reason()
    } else {
        "asset_integrity_invalid"
    };
    blocker(
        ErrorCode::InvalidRequest,
        reason,
        json!({"asset_code": error.code.as_str(), "details": error.details, "message": error.message, "domain": kind.namespace()}),
    )
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
    /// Действие размещения единственного файла плана.
    pub(crate) fn action(&self) -> Option<String> {
        self.items.first().map(|item| item.action.clone())
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
/// План размещения ровно одного проверенного файла.
///
/// Миграция legacy consumer filename переиспользует тот же fail-closed placement,
/// что и `create`: второй реализации записи файла в `media/` в toolkit'е нет.
impl MediaPlan {
    pub(crate) fn for_asset(asset: VerifiedAssetBytes, export: &Path) -> Result<Self, DomainError> {
        let filename = verified_asset_filename(&asset)?.to_owned();
        let action = destination(export, &filename, &asset.bytes)?.to_owned();
        let pin = Pin {
            identity: asset.record.identity.clone(),
            filename,
            sha256: asset.record.sha256.clone(),
        };
        Ok(Self {
            references: Vec::new(),
            items: vec![Item { pin, action, asset }],
            declarations_added: Vec::new(),
            mutations: 0,
        })
    }
}

/// Длина и SHA-256 физического файла `media/<name>`; `None`, если файла нет.
///
/// Читается через тот же `NOFOLLOW`-дескриптор, что и размещение: подмена
/// legacy-имени симлинком — это отказ, а не «файл, который можно удалить».
pub(crate) fn media_file_digest(
    export: &Path,
    name: &str,
) -> Result<Option<(usize, String)>, DomainError> {
    let directory = File::open(export).map_err(io_failure)?;
    let Some(media) = open_media(&directory, false)? else {
        return Ok(None);
    };
    let fd = match openat(
        &media,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(io_failure(e)),
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
    Ok(Some((bytes.len(), format!("{:x}", Sha256::digest(&bytes)))))
}

/// Удаляет файл из `media/` экспорта.
///
/// Отсутствие файла — не ошибка: миграция обязана сходиться и после обрыва между
/// публикацией `deck.json` и удалением освободившегося имени.
pub(crate) fn remove_media_file(guard: &ExportLock, name: &str) -> Result<(), DomainError> {
    let Some(media) = open_media(&guard.directory, false)? else {
        return Ok(());
    };
    match unlinkat(&media, name, AtFlags::empty()) {
        Ok(()) => media.sync_all().map_err(io_failure),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(io_failure(e)),
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);
fn verified_asset_filename(asset: &VerifiedAssetBytes) -> Result<&str, DomainError> {
    let filename = asset.record.consumer_filename.as_str();
    let detected_format = DetectedFormat::from_signature(&asset.bytes);
    if asset.record.format == DetectedFormat::Unknown
        || asset_store::domain::validate_safe_consumer_filename(filename, asset.record.format)
            .is_err()
        || detected_format != asset.record.format
    {
        return Err(crate::ops::source::internal(
            "проверенное хранилище вернуло небезопасное или несогласованное имя файла для потребителя",
        ));
    }
    Ok(filename)
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
