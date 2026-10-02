//! Возобновляемое предметное состояние batch-получения pitch-accent.
//!
//! Файловую границу и immutable candidate blobs обслуживает [`SafeBatchRuntime`].
//! Этот модуль хранит только pitch-specific outcomes, exact identity/SHA,
//! validation evidence, выбор словарной записи и намерение publication.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::batch_runtime::{
    MAX_RUNTIME_STATE_BYTES, RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime,
    validate_batch_id, validate_hash,
};
use crate::domain::AssetDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAcquired, JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchRequest, JpdbPitchSelection,
    JpdbVocabularyCandidate,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanDecision, LifecycleState, SemanticStatus,
    ValidationRecord, ValidatorIdentity,
};
use crate::pitch_accent::{
    PITCH_ACCENT_MAX_ASSET_BYTES, PitchAccentDomainMetadata, PitchAccentDomainPolicy,
    PitchAccentImageValidator, PitchAccentResolvedForm, jpdb_readings_equivalent,
    parse_jpdb_vocabulary_route,
};
use crate::validation::SemanticValidator;

/// Версия формата предметного batch state.
pub const PITCH_BATCH_SCHEMA_VERSION: u32 = 1;
/// Максимальный размер причины targeted decision.
pub const MAX_PITCH_BATCH_REASON_BYTES: usize = 4096;

/// Runtime-состояние одного batch. Request order сохраняется для воспроизводимого CLI output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub validator: ValidatorIdentity,
    /// Смена ожидаемого validation evidence инвалидирует SafeBatchRuntime blob cache.
    pub blob_validation_context_sha256: String,
    pub items: Vec<PitchBatchItem>,
}

/// Одно состояние на exact `surface`; domain identity не содержит reading или vocabulary ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchItem {
    pub identity: AssetIdentity,
    pub request: JpdbPitchRequest,
    /// Targeted retry или explicit ambiguity selection начинает новое поколение.
    pub generation: u32,
    /// Пер-item CAS counter. Изменение соседней identity не делает acquisition stale.
    pub item_revision: u64,
    pub attempts: Vec<PitchBatchAttempt>,
    pub current_candidate_sha256: Option<String>,
    pub rejected_candidates: Vec<PitchBatchRejection>,
    /// Канонический exact SHA, подтверждённый owner snapshot, если он уже был.
    pub canonical_sha256: Option<String>,
    /// SHA записи текущей identity, наблюдавшийся в owner snapshot, без вывода о trust.
    pub owner_current_sha256: Option<String>,
    /// Owner record, которую можно повторно использовать как current VERIFIED.
    pub existing_verified_sha256: Option<String>,
    /// SHA, подтверждённый завершённым owner publication.
    pub published_sha256: Option<String>,
    /// При явном refresh новый candidate должен заменить именно этот SHA.
    pub refresh_expected_sha256: Option<String>,
    /// Завершённые publication intents, сохранённые при следующем targeted generation.
    pub publication_history: Vec<PitchBatchPublication>,
    pub publication: Option<PitchBatchPublication>,
    pub owner_conflict: Option<PitchBatchConflict>,
    pub last_action_reason: Option<String>,
}

/// Один acquisition result с query/selection, действовавшим именно для этой попытки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchAttempt {
    pub index: u32,
    pub generation: u32,
    pub request: JpdbPitchRequest,
    pub outcome: PitchBatchOutcome,
}

/// Полный typed result провайдера. Acquired bytes заменяются hash-addressed runtime reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum PitchBatchOutcome {
    Acquired {
        candidate: Box<PitchBatchCandidate>,
    },
    NoPitchAccentOnSource {
        evidence: crate::jpdb::JpdbPitchAbsenceEvidence,
    },
    AmbiguousVocabulary {
        surface: String,
        reading: Option<String>,
        candidates: Vec<JpdbVocabularyCandidate>,
    },
    VocabularyNotFound {
        surface: String,
        reading: Option<String>,
    },
    Failed {
        error: JpdbPitchFailure,
    },
}

/// Точные acquired PNG bytes и результаты текущего production validator-а.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchCandidate {
    pub blob: RuntimeBlobRef,
    pub sha256: String,
    pub byte_length: u64,
    pub metadata: PitchAccentDomainMetadata,
    pub validation: ValidationRecord,
    /// Контекст входит в runtime blob cache и меняется при изменении любого evidence.
    pub validation_context_sha256: String,
}

/// Отказ привязан только к точным байтам; последующий SHA не наследует отказ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchRejection {
    pub candidate_sha256: String,
    pub reason: String,
    pub generation: u32,
}

/// Durable intent перед owner publication. `expected_previous_sha256` задаёт CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchPublication {
    pub candidate_sha256: String,
    pub expected_previous_sha256: Option<String>,
    pub status: PitchBatchPublicationStatus,
    pub conflict_code: Option<String>,
    pub conflict_message: Option<String>,
}

/// Итог сверки publication с полным owner snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchBatchPublicationStatus {
    Pending,
    Published,
    Conflict,
}

/// Сохранённая причина, по которой owner snapshot расходится с batch state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchConflict {
    pub code: String,
    pub message: String,
}

/// UI/CLI состояние элемента, вычисляемое из typed history и owner intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchBatchItemStatus {
    Pending,
    AcquiredVerified,
    CandidateRejected,
    NoPitchAccentOnSource,
    AmbiguousVocabulary,
    VocabularyNotFound,
    TechnicalFailure,
    PublicationPending,
    Published,
    ExistingVerified,
    Conflict,
}

impl PitchBatchItemStatus {
    pub const fn is_resolved(self) -> bool {
        matches!(
            self,
            Self::NoPitchAccentOnSource | Self::Published | Self::ExistingVerified
        )
    }
}

/// CAS token, который вызывающая сторона получает перед отпусканием lock на время browser work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitchBatchItemToken {
    pub identity: AssetIdentity,
    pub generation: u32,
    pub item_revision: u64,
    pub request_fingerprint: String,
}

/// Полный immutable owner view, полученный только после успешной integrity-проверки store.
#[derive(Debug, Clone, Default)]
pub struct PitchBatchOwnerSnapshot {
    records: BTreeMap<AssetIdentity, AssetRecord>,
}

impl PitchBatchOwnerSnapshot {
    /// Строит snapshot из успешного `AssetStore::verify_integrity()` результата.
    /// Runtime/quarantine записи сохраняются для CAS, но не считаются verified assets.
    pub fn from_records(records: Vec<AssetRecord>) -> Result<Self, AssetError> {
        let mut indexed = BTreeMap::new();
        for record in records {
            PitchAccentDomainPolicy.validate_identity(&record.identity)?;
            if indexed.insert(record.identity.clone(), record).is_some() {
                return Err(identity_conflict(
                    "owner snapshot содержит повторяющуюся pitch identity",
                ));
            }
        }
        Ok(Self { records: indexed })
    }

    pub fn current(&self, identity: &AssetIdentity) -> Option<&AssetRecord> {
        self.records.get(identity)
    }
}

/// Адаптер сохранённого pitch state над общим безопасным runtime.
#[derive(Debug)]
pub struct PitchAccentBatchRuntime {
    runtime: SafeBatchRuntime,
}

impl PitchAccentBatch {
    /// Создаёт state; одинаковые точные дубли схлопываются, но разные запросы для
    /// одной canonical identity отвергаются явно.
    pub fn new(
        batch_id: impl Into<String>,
        requests: Vec<JpdbPitchRequest>,
        validator: ValidatorIdentity,
    ) -> Result<Self, AssetError> {
        let batch_id = batch_id.into();
        validate_batch_id(&batch_id)?;
        if validator != PitchAccentImageValidator::validator_identity() {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "pitch batch требует текущую версию PitchAccentImageValidator",
            ));
        }

        let mut by_surface = BTreeMap::<String, JpdbPitchRequest>::new();
        let mut order = Vec::new();
        for request in requests {
            validate_request(&request)?;
            let surface = request.query.surface.clone();
            match by_surface.get(&surface) {
                Some(previous) if previous == &request => {}
                Some(_) => {
                    return Err(identity_conflict(format!(
                        "batch содержит несовместимые reading/selection для canonical surface {surface:?}"
                    )));
                }
                None => {
                    order.push(surface.clone());
                    by_surface.insert(surface, request);
                }
            }
        }
        if order.is_empty() {
            return Err(invalid("pitch batch должен содержать хотя бы один request"));
        }
        let items = order
            .into_iter()
            .map(|surface| {
                let request = by_surface
                    .remove(&surface)
                    .expect("каждый сохранённый surface имеет request");
                Ok(PitchBatchItem {
                    identity: AssetIdentity::new("pitch_accent", surface)
                        .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?,
                    request,
                    generation: 0,
                    item_revision: 0,
                    attempts: Vec::new(),
                    current_candidate_sha256: None,
                    rejected_candidates: Vec::new(),
                    canonical_sha256: None,
                    owner_current_sha256: None,
                    existing_verified_sha256: None,
                    published_sha256: None,
                    refresh_expected_sha256: None,
                    publication_history: Vec::new(),
                    publication: None,
                    owner_conflict: None,
                    last_action_reason: None,
                })
            })
            .collect::<Result<Vec<_>, AssetError>>()?;
        let batch = Self {
            schema_version: PITCH_BATCH_SCHEMA_VERSION,
            batch_id,
            revision: 0,
            validator,
            blob_validation_context_sha256: String::new(),
            items,
        };
        let mut batch = batch;
        batch.refresh_blob_validation_context()?;
        batch.validate()?;
        Ok(batch)
    }

    pub fn item(&self, surface: &str) -> Option<&PitchBatchItem> {
        self.items.iter().find(|item| item.identity.key == surface)
    }

    pub fn item_mut(&mut self, surface: &str) -> Option<&mut PitchBatchItem> {
        self.items
            .iter_mut()
            .find(|item| item.identity.key == surface)
    }

    pub fn is_resolved(&self) -> bool {
        self.items.iter().all(|item| item.status().is_resolved())
    }

    /// Возвращает CAS token только для элемента, который ожидает acquisition в текущем поколении.
    pub fn item_token(&self, surface: &str) -> Result<PitchBatchItemToken, AssetError> {
        let item = self
            .item(surface)
            .ok_or_else(|| invalid("surface отсутствует в pitch batch"))?;
        if item.status() != PitchBatchItemStatus::Pending {
            return Err(invalid("pitch batch item не ожидает acquisition"));
        }
        Ok(item.token())
    }

    /// Прикрепляет любой owner SHA как CAS observation, не объявляя его доверенным.
    /// Непроверенная/stale запись остаётся blocker-ом до explicit `reacquire`.
    pub fn observe_owner(&mut self, record: &AssetRecord) -> Result<(), AssetError> {
        let validator = self.validator.clone();
        let item = self
            .item_mut(&record.identity.key)
            .ok_or_else(|| invalid("owner identity отсутствует в pitch batch"))?;
        if record.identity.namespace != "pitch_accent" || item.identity != record.identity {
            return Err(identity_conflict(
                "owner record не совпадает с pitch batch identity",
            ));
        }
        validate_hash(&record.sha256)?;
        let before = (
            item.owner_current_sha256.clone(),
            item.canonical_sha256.clone(),
            item.existing_verified_sha256.clone(),
            item.published_sha256.clone(),
            item.owner_conflict.clone(),
        );
        let trusted = record_is_verified_for(record, &validator)
            && record_matches_request(record, &item.request).unwrap_or(false);
        let refresh_owner_changed = item
            .refresh_expected_sha256
            .as_deref()
            .is_some_and(|expected| expected != record.sha256);
        item.owner_current_sha256 = Some(record.sha256.clone());
        if refresh_owner_changed {
            item.canonical_sha256 = trusted.then(|| record.sha256.clone());
            item.existing_verified_sha256 = None;
            item.owner_conflict = Some(PitchBatchConflict {
                code: "identity_conflict".into(),
                message: "owner SHA изменился после фиксации refresh CAS".into(),
            });
        } else if trusted {
            item.canonical_sha256 = Some(record.sha256.clone());
            if item.refresh_expected_sha256.is_none() {
                item.existing_verified_sha256 = Some(record.sha256.clone());
            } else {
                item.existing_verified_sha256 = None;
            }
            item.owner_conflict = None;
        } else if item.refresh_expected_sha256.as_deref() == Some(record.sha256.as_str()) {
            item.canonical_sha256 = None;
            item.existing_verified_sha256 = None;
            item.owner_conflict = None;
        } else if owner_record_rejected(record) {
            item.canonical_sha256 = None;
            item.existing_verified_sha256 = None;
            item.owner_conflict = Some(PitchBatchConflict {
                code: "owner_rejected".into(),
                message: "текущая owner запись отклонена для этого exact SHA".into(),
            });
        } else {
            item.canonical_sha256 = None;
            item.existing_verified_sha256 = None;
            item.owner_conflict = Some(PitchBatchConflict {
                code: "owner_not_current_verified".into(),
                message:
                    "owner identity существует, но не подтверждена текущим validator и request"
                        .into(),
            });
        }
        let after = (
            item.owner_current_sha256.clone(),
            item.canonical_sha256.clone(),
            item.existing_verified_sha256.clone(),
            item.published_sha256.clone(),
            item.owner_conflict.clone(),
        );
        if before == after {
            return Ok(());
        }
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Сохраняет готовый verified asset из owner snapshot без JPDB запроса.
    pub fn reuse_existing(&mut self, record: &AssetRecord) -> Result<(), AssetError> {
        let identity = record.identity.clone();
        let validator = self.validator.clone();
        let item = self
            .item_mut(&identity.key)
            .ok_or_else(|| invalid("verified owner identity отсутствует в batch"))?;
        if identity.namespace != "pitch_accent" || item.identity != identity {
            return Err(identity_conflict(
                "owner asset не совпадает с pitch batch identity",
            ));
        }
        if !record_is_verified_for(record, &validator)
            || !record_matches_request(record, &item.request)?
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "существующий pitch asset не является current VERIFIED для этого request",
            ));
        }
        if item.publication.as_ref().is_some_and(|intent| {
            intent.candidate_sha256 != record.sha256
                && intent.status == PitchBatchPublicationStatus::Pending
        }) {
            return Err(identity_conflict(
                "reuse existing конфликтует с незавершённым publication intent",
            ));
        }
        if item.refresh_expected_sha256.is_some() {
            return Err(invalid("явный refresh уже запрошен для этой identity"));
        }
        item.canonical_sha256 = Some(record.sha256.clone());
        item.owner_current_sha256 = Some(record.sha256.clone());
        item.existing_verified_sha256 = Some(record.sha256.clone());
        item.refresh_expected_sha256 = None;
        item.owner_conflict = None;
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Явно ставит в очередь только эту identity для нового запроса к провайдеру.
    /// Точный наблюдавшийся SHA owner сохраняется как значение CAS публикации.
    pub fn reacquire(&mut self, surface: &str, reason: String) -> Result<(), AssetError> {
        validate_reason(&reason)?;
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("surface отсутствует в pitch batch"))?;
        if item.owner_conflict.as_ref().is_some_and(|conflict| {
            conflict.code != "owner_not_current_verified" && conflict.code != "owner_rejected"
        }) || item
            .publication
            .as_ref()
            .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "сначала разрешите owner conflict или завершающийся publication intent",
            ));
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен generation"))?;
        item.refresh_expected_sha256 = item.owner_current_sha256.clone();
        item.current_candidate_sha256 = None;
        item.owner_conflict = None;
        item.existing_verified_sha256 = None;
        item.archive_publication()?;
        item.last_action_reason = Some(reason);
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Выбирает точный candidate из последнего ambiguity inventory.
    /// Следующий provider run обязан выполнить новый JPDB search.
    pub fn select_candidate(
        &mut self,
        surface: &str,
        vocabulary_id: u64,
        detail_url: impl Into<String>,
    ) -> Result<(), AssetError> {
        let detail_url = detail_url.into();
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("surface отсутствует в pitch batch"))?;
        if item.owner_conflict.as_ref().is_some_and(|conflict| {
            conflict.code != "owner_not_current_verified" && conflict.code != "owner_rejected"
        }) || item
            .publication
            .as_ref()
            .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "сначала разрешите несовпадение owner identity или publication intent",
            ));
        }
        let Some(PitchBatchOutcome::AmbiguousVocabulary { candidates, .. }) =
            item.current_outcome()
        else {
            return Err(invalid("item не ожидает выбора ambiguous vocabulary"));
        };
        let selection = JpdbPitchSelection::new(vocabulary_id, detail_url.clone())
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        let selected_route = parse_jpdb_vocabulary_route(&selection.detail_url)
            .map_err(|_| invalid("detail route выбора невалиден"))?;
        let matches = candidates.iter().any(|candidate| {
            candidate.vocabulary_id == vocabulary_id
                && parse_jpdb_vocabulary_route(&candidate.detail_url)
                    .is_ok_and(|route| route == selected_route)
        });
        if !matches {
            return Err(identity_conflict(
                "выбранные vocabulary ID и detail route отсутствуют в последнем ambiguity inventory",
            ));
        }
        item.request.selection = Some(selection);
        // Canonical asset с другим выбором словарной записи не считается доверенным для этого запроса.
        item.canonical_sha256 = None;
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен generation"))?;
        item.current_candidate_sha256 = None;
        item.refresh_expected_sha256 = item.owner_current_sha256.clone();
        item.owner_conflict = None;
        item.existing_verified_sha256 = None;
        item.archive_publication()?;
        item.last_action_reason = Some("explicit_vocabulary_selection".into());
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Запускает новый generation только для transient provider failures.
    pub fn retry(&mut self, surface: &str, reason: String) -> Result<(), AssetError> {
        validate_reason(&reason)?;
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("surface отсутствует в pitch batch"))?;
        if item.owner_conflict.is_some()
            || item
                .publication
                .as_ref()
                .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "сначала разрешите owner conflict или завершающийся publication intent",
            ));
        }
        let Some(PitchBatchOutcome::Failed { error }) = item.current_outcome() else {
            return Err(invalid(
                "targeted retry разрешён только после технического failure",
            ));
        };
        if !retryable_failure(error) {
            return Err(invalid(
                "этот source/input/selection failure нельзя исправить повтором acquisition",
            ));
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен generation"))?;
        item.current_candidate_sha256 = None;
        item.refresh_expected_sha256 = item.owner_current_sha256.clone();
        item.existing_verified_sha256 = None;
        item.archive_publication()?;
        item.last_action_reason = Some(reason);
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Отмечает ровно один candidate SHA как отклонённый владельцем.
    pub fn reject_candidate(
        &mut self,
        surface: &str,
        candidate_sha256: &str,
        reason: String,
    ) -> Result<(), AssetError> {
        validate_hash(candidate_sha256)?;
        validate_reason(&reason)?;
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("surface отсутствует в pitch batch"))?;
        if item
            .publication
            .as_ref()
            .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "нельзя отклонить candidate при незавершённом publication intent",
            ));
        }
        if !item.candidate(candidate_sha256).is_some() {
            return Err(invalid("точный candidate SHA отсутствует в batch history"));
        }
        if item
            .rejected_candidates
            .iter()
            .any(|rejection| rejection.candidate_sha256 == candidate_sha256)
        {
            return Ok(());
        }
        if item
            .publication
            .as_ref()
            .is_some_and(|publication| publication.candidate_sha256 == candidate_sha256)
        {
            item.archive_publication()?;
        }
        item.rejected_candidates.push(PitchBatchRejection {
            candidate_sha256: candidate_sha256.into(),
            reason,
            generation: item.generation,
        });
        if item.current_candidate_sha256.as_deref() == Some(candidate_sha256) {
            item.current_candidate_sha256 = None;
        }
        if item.owner_current_sha256.as_deref() == Some(candidate_sha256) {
            item.canonical_sha256 = None;
            item.existing_verified_sha256 = None;
            item.owner_conflict = Some(PitchBatchConflict {
                code: "owner_rejected".into(),
                message: "текущая owner запись отклонена для этого exact SHA".into(),
            });
        }
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Записывает намерение publication до side effect владельца.
    /// Не принимает `REJECTED`, `CORRUPT`, incomplete или stale validator evidence.
    pub fn begin_publication(
        &mut self,
        surface: &str,
        candidate_sha256: &str,
        expected_previous_sha256: Option<String>,
    ) -> Result<(), AssetError> {
        validate_hash(candidate_sha256)?;
        if let Some(expected) = &expected_previous_sha256 {
            validate_hash(expected)?;
        }
        let validator = self.validator.clone();
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("surface отсутствует в pitch batch"))?;
        if item.current_candidate_sha256.as_deref() != Some(candidate_sha256) {
            return Err(identity_conflict(
                "publication intent относится не к текущему exact candidate",
            ));
        }
        let candidate = item
            .candidate(candidate_sha256)
            .ok_or_else(|| invalid("candidate evidence отсутствует"))?;
        if candidate.validation.status != SemanticStatus::Verified
            || candidate.validation.validator != validator
            || candidate.validation.content_sha256 != candidate.sha256
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "публикация разрешена только для current VERIFIED evidence",
            ));
        }
        if item
            .rejected_candidates
            .iter()
            .any(|rejection| rejection.candidate_sha256 == candidate_sha256)
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "точный candidate SHA был ранее отклонён",
            ));
        }
        if item.owner_current_sha256 != expected_previous_sha256
            || item.refresh_expected_sha256 != expected_previous_sha256
        {
            return Err(identity_conflict(
                "publication CAS не совпадает с текущим owner snapshot",
            ));
        }
        let requested = PitchBatchPublication {
            candidate_sha256: candidate_sha256.into(),
            expected_previous_sha256,
            status: PitchBatchPublicationStatus::Pending,
            conflict_code: None,
            conflict_message: None,
        };
        if let Some(existing) = &item.publication {
            if existing.candidate_sha256 == requested.candidate_sha256
                && existing.expected_previous_sha256 == requested.expected_previous_sha256
            {
                return Ok(());
            }
            if existing.status == PitchBatchPublicationStatus::Pending {
                return Err(identity_conflict(
                    "для identity уже сохранён другой publication intent",
                ));
            }
        }
        item.publication = Some(requested);
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Сверяет все durable owner facts с полным снимком. Отсутствие/ошибка snapshot
    /// должна обрабатываться вызывающей стороной до вызова этого метода.
    pub fn reconcile_owner(
        &mut self,
        snapshot: &PitchBatchOwnerSnapshot,
    ) -> Result<(), AssetError> {
        let validator = self.validator.clone();
        let mut batch_changed = false;
        for item in &mut self.items {
            let before = owner_state(item);
            let current = snapshot.current(&item.identity);
            item.owner_current_sha256 = current.map(|record| record.sha256.clone());

            if let Some(mut publication) = item.publication.clone() {
                let exact = current.is_some_and(|record| {
                    record.sha256 == publication.candidate_sha256
                        && record_is_verified_for(record, &validator)
                        && record_matches_request(record, &item.request).unwrap_or(false)
                });
                if exact {
                    publication.status = PitchBatchPublicationStatus::Published;
                    publication.conflict_code = None;
                    publication.conflict_message = None;
                    item.canonical_sha256 = Some(publication.candidate_sha256.clone());
                    item.existing_verified_sha256 = None;
                    item.published_sha256 = Some(publication.candidate_sha256.clone());
                    item.refresh_expected_sha256 = None;
                    item.owner_conflict = None;
                } else if current.is_some_and(owner_record_rejected) {
                    let conflict = PitchBatchConflict {
                        code: "owner_rejected".into(),
                        message: "текущая owner запись отклонена для этого exact SHA".into(),
                    };
                    publication.status = PitchBatchPublicationStatus::Conflict;
                    publication.conflict_code = Some(conflict.code.clone());
                    publication.conflict_message = Some(conflict.message.clone());
                    item.owner_conflict = Some(conflict);
                    item.canonical_sha256 = None;
                    item.existing_verified_sha256 = None;
                    item.refresh_expected_sha256 = None;
                } else {
                    let expected_unchanged =
                        match (current, publication.expected_previous_sha256.as_deref()) {
                            (None, None) => true,
                            (Some(record), Some(expected)) => record.sha256 == expected,
                            _ => false,
                        };
                    if publication.status != PitchBatchPublicationStatus::Pending
                        || !expected_unchanged
                    {
                        let conflict = PitchBatchConflict {
                            code: "identity_conflict".into(),
                            message: match current {
                                Some(record)
                                    if record.sha256 != publication.candidate_sha256 =>
                                {
                                    "owner current SHA не совпадает с candidate или ожидаемым CAS SHA".into()
                                }
                                Some(_) => "owner не подтверждает current VERIFIED для exact candidate".into(),
                                None => "owner snapshot не содержит identity publication intent".into(),
                            },
                        };
                        publication.status = PitchBatchPublicationStatus::Conflict;
                        publication.conflict_code = Some(conflict.code.clone());
                        publication.conflict_message = Some(conflict.message.clone());
                        item.owner_conflict = Some(conflict);
                        item.canonical_sha256 = None;
                        item.existing_verified_sha256 = None;
                        item.refresh_expected_sha256 = None;
                    }
                }
                item.publication = Some(publication);
            } else if let Some(record) = current {
                let reusable = record_is_verified_for(record, &validator)
                    && record_matches_request(record, &item.request).unwrap_or(false);
                let refresh_matches =
                    item.refresh_expected_sha256.as_deref() == Some(record.sha256.as_str());

                if item.refresh_expected_sha256.is_some() && !refresh_matches {
                    item.canonical_sha256 = None;
                    item.existing_verified_sha256 = None;
                    item.owner_conflict = Some(PitchBatchConflict {
                        code: "identity_conflict".into(),
                        message: "owner SHA изменился после фиксации refresh CAS".into(),
                    });
                } else if reusable {
                    item.canonical_sha256 = Some(record.sha256.clone());
                    if item.refresh_expected_sha256.is_none() {
                        if item.published_sha256.as_deref() != Some(record.sha256.as_str()) {
                            item.existing_verified_sha256 = Some(record.sha256.clone());
                        }
                    } else {
                        item.existing_verified_sha256 = None;
                    }
                    item.owner_conflict = None;
                } else if refresh_matches {
                    item.canonical_sha256 = None;
                    item.existing_verified_sha256 = None;
                    item.owner_conflict = None;
                } else if owner_record_rejected(record) {
                    item.canonical_sha256 = None;
                    item.existing_verified_sha256 = None;
                    item.owner_conflict = Some(PitchBatchConflict {
                        code: "owner_rejected".into(),
                        message: "текущая owner запись отклонена для этого exact SHA".into(),
                    });
                } else {
                    item.canonical_sha256 = None;
                    item.existing_verified_sha256 = None;
                    item.owner_conflict = Some(PitchBatchConflict {
                        code: "owner_not_current_verified".into(),
                        message: "owner identity есть, но её нельзя переиспользовать без explicit refresh".into(),
                    });
                }
            } else {
                item.canonical_sha256 = None;
                item.existing_verified_sha256 = None;
                if item.refresh_expected_sha256.is_some() {
                    item.owner_conflict = Some(PitchBatchConflict {
                        code: "identity_conflict".into(),
                        message: "expected owner SHA для refresh отсутствует в текущем snapshot"
                            .into(),
                    });
                } else {
                    item.owner_conflict = None;
                }
            }
            if owner_state(item) != before {
                item.item_revision = item
                    .item_revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("превышен item revision"))?;
                batch_changed = true;
            }
        }
        if batch_changed {
            self.bump_revision()?;
            self.validate()?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        validate_batch_id(&self.batch_id)?;
        if self.schema_version != PITCH_BATCH_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "версия pitch batch state не поддерживается",
            ));
        }
        if self.validator != PitchAccentImageValidator::validator_identity() {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "pitch batch state использует устаревший validator; automatic trust не переносится",
            ));
        }
        if self.items.is_empty() {
            return Err(invalid("pitch batch state не содержит items"));
        }
        let mut identities = BTreeSet::new();
        for item in &self.items {
            PitchAccentDomainPolicy.validate_identity(&item.identity)?;
            if !identities.insert(item.identity.clone())
                || item.request.query.surface != item.identity.key
            {
                return Err(identity_conflict(
                    "request surface не уникален или не совпадает с canonical identity",
                ));
            }
            validate_request(&item.request)?;
            if let Some(hash) = &item.current_candidate_sha256 {
                validate_hash(hash)?;
                let candidate = item
                    .candidate(hash)
                    .ok_or_else(|| invalid("current candidate отсутствует в attempt history"))?;
                if item.attempts.last().is_none_or(|attempt| {
                    attempt.generation != item.generation
                        || !matches!(&attempt.outcome, PitchBatchOutcome::Acquired { candidate: latest } if latest.sha256 == candidate.sha256)
                }) {
                    return Err(invalid(
                        "current candidate не совпадает с последним outcome текущего generation",
                    ));
                }
            }
            if let Some(hash) = &item.canonical_sha256 {
                validate_hash(hash)?;
            }
            if let Some(hash) = &item.owner_current_sha256 {
                validate_hash(hash)?;
            }
            if let Some(hash) = &item.existing_verified_sha256 {
                validate_hash(hash)?;
                if item.canonical_sha256.as_deref() != Some(hash) {
                    return Err(invalid(
                        "existing verified SHA не совпадает с canonical SHA",
                    ));
                }
            }
            if let Some(hash) = &item.published_sha256 {
                validate_hash(hash)?;
                if item.candidate(hash).is_none() {
                    return Err(invalid("published SHA отсутствует в acquisition history"));
                }
            }
            if let Some(hash) = &item.refresh_expected_sha256 {
                validate_hash(hash)?;
            }
            let mut previous_generation = None;
            for (index, attempt) in item.attempts.iter().enumerate() {
                if attempt.index != index as u32 + 1
                    || attempt.generation > item.generation
                    || previous_generation.is_some_and(|old| old >= attempt.generation)
                    || attempt.request.query.surface != item.identity.key
                {
                    return Err(invalid(
                        "attempt indexes, generation или surface нарушают batch history",
                    ));
                }
                previous_generation = Some(attempt.generation);
                validate_request(&attempt.request)?;
                validate_outcome(
                    &attempt.outcome,
                    &item.identity,
                    &attempt.request,
                    &self.validator,
                )?;
            }
            for rejection in &item.rejected_candidates {
                validate_hash(&rejection.candidate_sha256)?;
                validate_reason(&rejection.reason)?;
                if rejection.generation > item.generation
                    || item.candidate(&rejection.candidate_sha256).is_none()
                {
                    return Err(invalid(
                        "rejection не относится к сохранённому exact candidate SHA",
                    ));
                }
            }
            if let Some(publication) = &item.publication {
                validate_hash(&publication.candidate_sha256)?;
                if let Some(expected) = &publication.expected_previous_sha256 {
                    validate_hash(expected)?;
                }
                let candidate = item
                    .candidate(&publication.candidate_sha256)
                    .ok_or_else(|| invalid("publication candidate отсутствует в history"))?;
                if candidate.validation.status != SemanticStatus::Verified
                    || candidate.validation.validator != self.validator
                    || item
                        .rejected_candidates
                        .iter()
                        .any(|rejection| rejection.candidate_sha256 == candidate.sha256)
                    || publication.conflict_code.is_some() != publication.conflict_message.is_some()
                {
                    return Err(AssetError::new(
                        ErrorCode::InvalidValidationEvidence,
                        "publication intent не привязан к VERIFIED current candidate",
                    ));
                }
            }
            for publication in &item.publication_history {
                validate_hash(&publication.candidate_sha256)?;
                if publication.status == PitchBatchPublicationStatus::Pending
                    || item.candidate(&publication.candidate_sha256).is_none()
                    || publication.conflict_code.is_some() != publication.conflict_message.is_some()
                {
                    return Err(invalid(
                        "архивированный publication intent повреждён или ещё не завершён",
                    ));
                }
                if let Some(expected) = &publication.expected_previous_sha256 {
                    validate_hash(expected)?;
                }
            }
            if item.owner_conflict.as_ref().is_some_and(|conflict| {
                conflict.code.trim().is_empty() || conflict.message.trim().is_empty()
            }) {
                return Err(invalid("owner conflict содержит пустую диагностику"));
            }
            if item.last_action_reason.as_ref().is_some_and(|reason| {
                reason.trim().is_empty() || reason.len() > MAX_PITCH_BATCH_REASON_BYTES
            }) {
                return Err(invalid("причина item action некорректна"));
            }
        }
        if self.blob_validation_context_sha256 != blob_context_digest(&self.items)? {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "pitch batch blob validation context не совпадает с candidate evidence",
            ));
        }
        Ok(())
    }

    fn bump_revision(&mut self) -> Result<(), AssetError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен revision pitch batch"))?;
        Ok(())
    }

    fn refresh_blob_validation_context(&mut self) -> Result<(), AssetError> {
        self.blob_validation_context_sha256 = blob_context_digest(&self.items)?;
        Ok(())
    }
}

impl PitchBatchItem {
    fn archive_publication(&mut self) -> Result<(), AssetError> {
        let Some(publication) = self.publication.take() else {
            return Ok(());
        };
        if publication.status == PitchBatchPublicationStatus::Pending {
            self.publication = Some(publication);
            return Err(invalid(
                "нельзя завершить новое generation при незавершённом publication intent",
            ));
        }
        self.publication_history.push(publication);
        Ok(())
    }

    pub fn status(&self) -> PitchBatchItemStatus {
        if self.owner_conflict.is_some()
            || self.publication.as_ref().is_some_and(|publication| {
                publication.status == PitchBatchPublicationStatus::Conflict
            })
        {
            return PitchBatchItemStatus::Conflict;
        }
        if let Some(publication) = &self.publication {
            return match publication.status {
                PitchBatchPublicationStatus::Pending => PitchBatchItemStatus::PublicationPending,
                PitchBatchPublicationStatus::Published => PitchBatchItemStatus::Published,
                PitchBatchPublicationStatus::Conflict => PitchBatchItemStatus::Conflict,
            };
        }
        if self.canonical_sha256.is_some() && self.refresh_expected_sha256.is_none() {
            return PitchBatchItemStatus::ExistingVerified;
        }
        let Some(attempt) = self
            .attempts
            .last()
            .filter(|attempt| attempt.generation == self.generation)
        else {
            return PitchBatchItemStatus::Pending;
        };
        match &attempt.outcome {
            PitchBatchOutcome::Acquired { candidate } => {
                if self
                    .rejected_candidates
                    .iter()
                    .any(|rejection| rejection.candidate_sha256 == candidate.sha256)
                {
                    PitchBatchItemStatus::CandidateRejected
                } else if candidate.validation.status == SemanticStatus::Verified {
                    PitchBatchItemStatus::AcquiredVerified
                } else {
                    PitchBatchItemStatus::CandidateRejected
                }
            }
            PitchBatchOutcome::NoPitchAccentOnSource { .. } => {
                PitchBatchItemStatus::NoPitchAccentOnSource
            }
            PitchBatchOutcome::AmbiguousVocabulary { .. } => {
                PitchBatchItemStatus::AmbiguousVocabulary
            }
            PitchBatchOutcome::VocabularyNotFound { .. } => {
                PitchBatchItemStatus::VocabularyNotFound
            }
            PitchBatchOutcome::Failed { .. } => PitchBatchItemStatus::TechnicalFailure,
        }
    }

    pub fn current_outcome(&self) -> Option<&PitchBatchOutcome> {
        self.attempts
            .last()
            .filter(|attempt| attempt.generation == self.generation)
            .map(|attempt| &attempt.outcome)
    }

    pub fn candidate(&self, sha256: &str) -> Option<&PitchBatchCandidate> {
        self.attempts.iter().find_map(|attempt| {
            if let PitchBatchOutcome::Acquired { candidate } = &attempt.outcome
                && candidate.sha256 == sha256
            {
                Some(candidate.as_ref())
            } else {
                None
            }
        })
    }

    pub fn token(&self) -> PitchBatchItemToken {
        PitchBatchItemToken {
            identity: self.identity.clone(),
            generation: self.generation,
            item_revision: self.item_revision,
            request_fingerprint: request_fingerprint(&self.request),
        }
    }
}

impl PitchAccentBatchRuntime {
    pub fn open(store_root: &Path, batch_id: &str) -> Result<Self, AssetError> {
        Ok(Self {
            runtime: SafeBatchRuntime::open(store_root, batch_id)?,
        })
    }

    /// Создаёт новый runtime state и сохраняет его до возврата.
    pub fn create(store_root: &Path, batch: &PitchAccentBatch) -> Result<Self, AssetError> {
        batch.validate()?;
        let mut runtime = Self::open(store_root, &batch.batch_id)?;
        runtime.save(batch)?;
        Ok(runtime)
    }

    pub fn batch_id(&self) -> &str {
        self.runtime.batch_id()
    }

    pub fn load(&mut self) -> Result<Option<PitchAccentBatch>, AssetError> {
        self.runtime.load()
    }

    pub fn save(&mut self, batch: &PitchAccentBatch) -> Result<(), AssetError> {
        self.runtime.save(batch)
    }

    /// Сохраняет one-item outcome по token CAS. Возвращает `false`, если пока
    /// шёл acquisition этот item сменил generation, request или item revision.
    pub fn record_outcome(
        &mut self,
        batch: &mut PitchAccentBatch,
        token: &PitchBatchItemToken,
        outcome: JpdbPitchOutcome,
    ) -> Result<bool, AssetError> {
        if batch.batch_id != self.batch_id() {
            return Err(invalid("batch id не совпадает с runtime directory"));
        }
        let Some(item) = batch
            .items
            .iter()
            .find(|item| item.identity == token.identity)
        else {
            return Ok(false);
        };
        if !token_matches(item, token) || item.status() != PitchBatchItemStatus::Pending {
            return Ok(false);
        }
        let stored = match outcome {
            JpdbPitchOutcome::Acquired { asset } => {
                self.store_acquired(item, *asset, &batch.validator)?
            }
            JpdbPitchOutcome::NoPitchAccentOnSource { evidence } => {
                PitchBatchOutcome::NoPitchAccentOnSource { evidence }
            }
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface,
                reading,
                candidates,
            } => PitchBatchOutcome::AmbiguousVocabulary {
                surface,
                reading,
                candidates,
            },
            JpdbPitchOutcome::VocabularyNotFound { surface, reading } => {
                PitchBatchOutcome::VocabularyNotFound { surface, reading }
            }
            JpdbPitchOutcome::Failed { error } => PitchBatchOutcome::Failed { error },
        };
        validate_outcome(&stored, &item.identity, &item.request, &batch.validator)?;
        let item = batch
            .item_mut(&token.identity.key)
            .expect("token identity проверена выше");
        item.attempts.push(PitchBatchAttempt {
            index: item.attempts.len() as u32 + 1,
            generation: item.generation,
            request: item.request.clone(),
            outcome: stored,
        });
        item.current_candidate_sha256 = item.attempts.last().and_then(|attempt| {
            if let PitchBatchOutcome::Acquired { candidate } = &attempt.outcome {
                Some(candidate.sha256.clone())
            } else {
                None
            }
        });
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен item revision"))?;
        batch.refresh_blob_validation_context()?;
        batch.bump_revision()?;
        self.save(batch)?;
        Ok(true)
    }

    pub fn read_candidate(
        &self,
        batch: &PitchAccentBatch,
        candidate: &PitchBatchCandidate,
    ) -> Result<Vec<u8>, AssetError> {
        let referenced = batch.items.iter().any(|item| {
            item.candidate(&candidate.sha256)
                .is_some_and(|stored| stored == candidate)
        });
        if !referenced {
            return Err(invalid("candidate не принадлежит этому batch state"));
        }
        let bytes = self
            .runtime
            .read_blob_with_limit(&candidate.blob, PITCH_ACCENT_MAX_ASSET_BYTES)?;
        validate_candidate_bytes(candidate, &bytes, &batch.validator)?;
        Ok(bytes)
    }

    /// Review HTML принадлежит `pitch_review`; runtime занимается только safe artifact write.
    pub fn write_review(
        &self,
        batch: &PitchAccentBatch,
        owner_records: &[AssetRecord],
    ) -> Result<PathBuf, AssetError> {
        batch.validate()?;
        if batch.batch_id != self.batch_id() {
            return Err(invalid("batch id не совпадает с runtime directory"));
        }
        let html = crate::pitch_review::render(batch, owner_records, |candidate| {
            self.read_candidate(batch, candidate)
        })?;
        self.runtime
            .write_artifact("review.html", html.as_bytes(), MAX_RUNTIME_STATE_BYTES)
    }

    fn store_acquired(
        &self,
        item: &PitchBatchItem,
        acquired: JpdbPitchAcquired,
        validator_identity: &ValidatorIdentity,
    ) -> Result<PitchBatchOutcome, AssetError> {
        validate_acquired_metadata(&item.identity, &item.request, &acquired.metadata).map_err(
            |message| {
                AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    format!("provider candidate не соответствует request: {message}"),
                )
            },
        )?;
        if acquired.bytes.len() as u64 > PITCH_ACCENT_MAX_ASSET_BYTES
            || DetectedFormat::from_signature(&acquired.bytes) != DetectedFormat::Png
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "provider acquired candidate превышает лимит или не является PNG",
            ));
        }
        let sha256 = sha256_hex(&acquired.bytes);
        let blob = self.runtime.persist_blob_with_limit(
            &acquired.bytes,
            "png",
            PITCH_ACCENT_MAX_ASSET_BYTES,
        )?;
        let validation = validate_candidate(
            &item.identity,
            &acquired.metadata,
            &acquired.bytes,
            validator_identity,
        )?;
        let candidate = PitchBatchCandidate {
            blob,
            sha256,
            byte_length: acquired.bytes.len() as u64,
            metadata: acquired.metadata,
            validation,
            validation_context_sha256: String::new(),
        };
        let candidate = with_validation_context(candidate)?;
        Ok(PitchBatchOutcome::Acquired {
            candidate: Box::new(candidate),
        })
    }
}

impl RuntimeBatchState for PitchAccentBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        PitchAccentBatch::validate(self)
    }

    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.items
            .iter()
            .flat_map(|item| item.attempts.iter())
            .filter_map(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } => Some(candidate.blob.clone()),
                _ => None,
            })
            .collect()
    }

    fn maximum_blob_bytes(&self) -> u64 {
        PITCH_ACCENT_MAX_ASSET_BYTES
    }

    fn blob_validation_context(&self, blob: &RuntimeBlobRef) -> Option<&str> {
        self.items
            .iter()
            .flat_map(|item| item.attempts.iter())
            .any(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } => candidate.blob == *blob,
                _ => false,
            })
            .then_some(self.blob_validation_context_sha256.as_str())
    }

    fn validate_blob_bytes(&self, blob: &RuntimeBlobRef, bytes: &[u8]) -> Result<(), AssetError> {
        let mut found = false;
        for candidate in self
            .items
            .iter()
            .flat_map(|item| item.attempts.iter())
            .filter_map(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } if candidate.blob == *blob => {
                    Some(candidate)
                }
                _ => None,
            })
        {
            found = true;
            validate_candidate_bytes(candidate, bytes, &self.validator)?;
        }
        if !found {
            return Err(invalid(
                "runtime state ссылается на неизвестный pitch candidate",
            ));
        }
        Ok(())
    }
}

fn token_matches(item: &PitchBatchItem, token: &PitchBatchItemToken) -> bool {
    item.identity == token.identity
        && item.generation == token.generation
        && item.item_revision == token.item_revision
        && request_fingerprint(&item.request) == token.request_fingerprint
}

type OwnerItemState = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<PitchBatchConflict>,
    Option<PitchBatchPublication>,
);

fn owner_state(item: &PitchBatchItem) -> OwnerItemState {
    (
        item.owner_current_sha256.clone(),
        item.canonical_sha256.clone(),
        item.existing_verified_sha256.clone(),
        item.published_sha256.clone(),
        item.refresh_expected_sha256.clone(),
        item.owner_conflict.clone(),
        item.publication.clone(),
    )
}

fn owner_record_rejected(record: &AssetRecord) -> bool {
    record.current_human_decision() == Some(HumanDecision::Reject)
}

fn request_fingerprint(request: &JpdbPitchRequest) -> String {
    let bytes = serde_json::to_vec(request).expect("JpdbPitchRequest сериализуемый");
    let mut context = b"pitch-accent-request-v1\0".to_vec();
    context.extend_from_slice(&bytes);
    sha256_hex(&context)
}

fn validate_request(request: &JpdbPitchRequest) -> Result<(), AssetError> {
    let query = &request.query;
    if query.surface.trim().is_empty()
        || query.surface != query.surface.trim()
        || query.surface.chars().any(char::is_control)
        || query.reading.as_deref().is_some_and(|reading| {
            reading.trim().is_empty() || reading.chars().any(char::is_control)
        })
    {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "surface должен быть exact непустым значением без краевых пробелов; reading — непустым без управляющих символов",
        ));
    }
    PitchAccentDomainPolicy.validate_identity(
        &AssetIdentity::new("pitch_accent", query.surface.clone())
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?,
    )?;
    if let Some(selection) = &request.selection {
        JpdbPitchSelection::new(selection.vocabulary_id, selection.detail_url.clone())
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
    }
    Ok(())
}

fn validate_outcome(
    outcome: &PitchBatchOutcome,
    identity: &AssetIdentity,
    request: &JpdbPitchRequest,
    validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    match outcome {
        PitchBatchOutcome::Acquired { candidate } => {
            validate_candidate_record(candidate, identity, request, validator)?;
        }
        PitchBatchOutcome::NoPitchAccentOnSource { evidence } => {
            if evidence.surface != identity.key
                || request
                    .query
                    .reading
                    .as_deref()
                    .is_some_and(|reading| !jpdb_readings_equivalent(&evidence.reading, reading))
                || evidence.jpdb_vocabulary_id == 0
                || evidence.source_url.trim().is_empty()
                || !evidence.base_page_contract_valid
                || evidence.pitch_section_present
                || evidence.pitch_marker_count != 0
                || !contains_form(
                    &evidence.resolved_forms,
                    &identity.key,
                    Some(&evidence.reading),
                )
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "evidence отсутствия pitch accent противоречит exact request или контракту страницы",
                ));
            }
            let selection =
                JpdbPitchSelection::new(evidence.jpdb_vocabulary_id, evidence.source_url.clone())
                    .map_err(|message| {
                    AssetError::new(ErrorCode::InvalidValidationEvidence, message)
                })?;
            let _ = selection;
        }
        PitchBatchOutcome::AmbiguousVocabulary {
            surface,
            reading,
            candidates,
        } => {
            if surface != &identity.key
                || request.query.reading.as_deref().is_some_and(|expected| {
                    reading
                        .as_deref()
                        .is_none_or(|actual| !jpdb_readings_equivalent(actual, expected))
                })
                || candidates.is_empty()
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "ambiguity inventory не соответствует exact request",
                ));
            }
            let mut ids = BTreeSet::new();
            for candidate in candidates {
                if candidate.vocabulary_id == 0
                    || !ids.insert(candidate.vocabulary_id)
                    || !contains_form(&candidate.resolved_forms, &identity.key, reading.as_deref())
                {
                    return Err(AssetError::new(
                        ErrorCode::InvalidValidationEvidence,
                        "ambiguity inventory содержит повторный ID или неподтверждённую форму",
                    ));
                }
                JpdbPitchSelection::new(candidate.vocabulary_id, candidate.detail_url.clone())
                    .map_err(|message| {
                        AssetError::new(ErrorCode::InvalidValidationEvidence, message)
                    })?;
            }
        }
        PitchBatchOutcome::VocabularyNotFound { surface, reading } => {
            if surface != &identity.key
                || request.query.reading.as_deref().is_some_and(|expected| {
                    reading
                        .as_deref()
                        .is_none_or(|actual| !jpdb_readings_equivalent(actual, expected))
                })
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "not-found outcome не соответствует exact request",
                ));
            }
        }
        PitchBatchOutcome::Failed { .. } => {}
    }
    if let PitchBatchOutcome::Acquired { candidate } = outcome {
        validate_acquired_metadata(identity, request, &candidate.metadata)?;
        if candidate.validation.validator != *validator {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "candidate evidence использует не текущий validator",
            ));
        }
    }
    Ok(())
}

fn validate_candidate_record(
    candidate: &PitchBatchCandidate,
    identity: &AssetIdentity,
    request: &JpdbPitchRequest,
    validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    validate_hash(&candidate.sha256)?;
    if candidate.blob.sha256 != candidate.sha256
        || candidate.blob.storage_path != format!("candidates/{}.png", candidate.sha256)
        || candidate.byte_length == 0
        || candidate.byte_length > PITCH_ACCENT_MAX_ASSET_BYTES
        || candidate.metadata.surface != identity.key
        || candidate.validation.content_sha256 != candidate.sha256
        || candidate.validation.validator != *validator
        || !candidate.validation.is_valid_for_sha(&candidate.sha256)
        || candidate.validation_context_sha256 != candidate_context(candidate)?
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "pitch candidate blob, SHA, metadata, validator или context не согласованы",
        ));
    }
    validate_acquired_metadata(identity, request, &candidate.metadata)
}

fn validate_acquired_metadata(
    identity: &AssetIdentity,
    request: &JpdbPitchRequest,
    metadata: &PitchAccentDomainMetadata,
) -> Result<(), AssetError> {
    if metadata.surface != identity.key
        || !contains_form(
            &metadata.evidence.resolved_forms,
            &identity.key,
            request.query.reading.as_deref(),
        )
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "candidate metadata не доказывает requested surface/reading",
        ));
    }
    if let Some(selection) = &request.selection {
        let requested_route = parse_jpdb_vocabulary_route(&selection.detail_url)
            .map_err(|_| identity_conflict("request selection route invalid"))?;
        let actual_route = parse_jpdb_vocabulary_route(&metadata.evidence.source_url)
            .map_err(|_| identity_conflict("candidate source route invalid"))?;
        if selection.vocabulary_id != metadata.jpdb_vocabulary_id || requested_route != actual_route
        {
            return Err(identity_conflict(
                "candidate source не совпадает с explicit vocabulary selection",
            ));
        }
    }
    Ok(())
}

fn contains_form(forms: &[PitchAccentResolvedForm], surface: &str, reading: Option<&str>) -> bool {
    forms.iter().any(|form| {
        form.surface == surface
            && reading.is_none_or(|expected| jpdb_readings_equivalent(&form.reading, expected))
    })
}

fn validate_candidate(
    identity: &AssetIdentity,
    metadata: &PitchAccentDomainMetadata,
    bytes: &[u8],
    validator_identity: &ValidatorIdentity,
) -> Result<ValidationRecord, AssetError> {
    let sha256 = sha256_hex(bytes);
    let location =
        PitchAccentDomainPolicy.canonical_location(identity, &sha256, DetectedFormat::Png)?;
    let asset = AssetRecord {
        identity: identity.clone(),
        storage_path: location.storage_path,
        consumer_filename: location.consumer_filename,
        sha256: sha256.clone(),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::Png,
        provenance: crate::model::Provenance {
            source_kind: "jpdb_browser_render".into(),
            source_name: format!("jpdb-vocabulary-{}.png", metadata.jpdb_vocabulary_id),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: Some(
            serde_json::to_value(metadata)
                .map_err(|error| invalid(format!("не удалось сериализовать metadata: {error}")))?,
        ),
    };
    let validator = PitchAccentImageValidator;
    if validator.identity() != *validator_identity {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            "candidate acquisition использует не текущий validator",
        ));
    }
    let decision = validator
        .validate(&asset, &mut Cursor::new(bytes))
        .map_err(|failure| {
            AssetError::new(
                ErrorCode::ValidatorFailure,
                format!(
                    "PitchAccentImageValidator завершился ошибкой: {}",
                    failure.message
                ),
            )
        })?;
    Ok(ValidationRecord {
        status: decision.status,
        validator: validator.identity(),
        content_sha256: sha256,
        evidence: decision.evidence,
    })
}

fn validate_candidate_bytes(
    candidate: &PitchBatchCandidate,
    bytes: &[u8],
    validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    if bytes.len() as u64 != candidate.byte_length
        || sha256_hex(bytes) != candidate.sha256
        || DetectedFormat::from_signature(bytes) != DetectedFormat::Png
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "pitch candidate bytes, длина или PNG signature не совпадают с state",
        ));
    }
    if candidate.validation_context_sha256 != candidate_context(candidate)? {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "pitch candidate validation context не совпадает с metadata/evidence",
        ));
    }
    let identity = AssetIdentity::new("pitch_accent", candidate.metadata.surface.clone())
        .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
    let fresh = validate_candidate(&identity, &candidate.metadata, bytes, validator)?;
    if fresh != candidate.validation {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "PitchAccentImageValidator result не совпадает с сохранённым exact evidence",
        ));
    }
    Ok(())
}

fn with_validation_context(
    mut candidate: PitchBatchCandidate,
) -> Result<PitchBatchCandidate, AssetError> {
    candidate.validation_context_sha256 = candidate_context(&candidate)?;
    Ok(candidate)
}

fn candidate_context(candidate: &PitchBatchCandidate) -> Result<String, AssetError> {
    #[derive(Serialize)]
    struct Context<'a> {
        sha256: &'a str,
        byte_length: u64,
        metadata: &'a PitchAccentDomainMetadata,
        validation: &'a ValidationRecord,
    }
    let bytes = serde_json::to_vec(&Context {
        sha256: &candidate.sha256,
        byte_length: candidate.byte_length,
        metadata: &candidate.metadata,
        validation: &candidate.validation,
    })
    .map_err(|error| {
        invalid(format!(
            "не удалось сериализовать candidate context: {error}"
        ))
    })?;
    Ok(sha256_hex(&bytes))
}

fn blob_context_digest(items: &[PitchBatchItem]) -> Result<String, AssetError> {
    let contexts = items
        .iter()
        .flat_map(|item| item.attempts.iter())
        .filter_map(|attempt| match &attempt.outcome {
            PitchBatchOutcome::Acquired { candidate } => Some((
                candidate.blob.sha256.clone(),
                candidate.validation_context_sha256.clone(),
            )),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let bytes = serde_json::to_vec(&contexts)
        .map_err(|error| invalid(format!("не удалось сериализовать blob context: {error}")))?;
    Ok(sha256_hex(&bytes))
}

fn record_is_verified_for(record: &AssetRecord, validator: &ValidatorIdentity) -> bool {
    record.lifecycle == LifecycleState::Verified
        && record.format == DetectedFormat::Png
        && record.validation.as_ref().is_some_and(|validation| {
            validation.status == SemanticStatus::Verified
                && validation.validator == *validator
                && validation.content_sha256 == record.sha256
                && validation.is_valid_for_sha(&record.sha256)
        })
        && record.is_trusted_for(validator)
}

fn record_matches_request(
    record: &AssetRecord,
    request: &JpdbPitchRequest,
) -> Result<bool, AssetError> {
    let Some(value) = &record.domain_metadata else {
        return Ok(false);
    };
    let metadata: PitchAccentDomainMetadata =
        serde_json::from_value(value.clone()).map_err(|_| {
            AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "owner pitch metadata имеет неизвестную форму",
            )
        })?;
    if metadata.surface != request.query.surface
        || !contains_form(
            &metadata.evidence.resolved_forms,
            &request.query.surface,
            request.query.reading.as_deref(),
        )
    {
        return Ok(false);
    }
    if let Some(selection) = &request.selection {
        let expected = parse_jpdb_vocabulary_route(&selection.detail_url)
            .map_err(|_| identity_conflict("request selection route invalid"))?;
        let actual = parse_jpdb_vocabulary_route(&metadata.evidence.source_url)
            .map_err(|_| identity_conflict("owner metadata source route invalid"))?;
        return Ok(metadata.jpdb_vocabulary_id == selection.vocabulary_id && actual == expected);
    }
    Ok(true)
}

fn retryable_failure(error: &JpdbPitchFailure) -> bool {
    match error {
        JpdbPitchFailure::Timeout { .. }
        | JpdbPitchFailure::BrowserSetup { .. }
        | JpdbPitchFailure::Telemetry { .. }
        | JpdbPitchFailure::SessionFailure { .. }
        | JpdbPitchFailure::Screenshot { .. } => true,
        JpdbPitchFailure::Navigation { message, .. } => contains_retryable_network_error(message),
        JpdbPitchFailure::BrowserEvaluation { .. }
        | JpdbPitchFailure::BrowserConfiguration { .. }
        | JpdbPitchFailure::InvalidQuery { .. }
        | JpdbPitchFailure::PageContract { .. }
        | JpdbPitchFailure::DetailIdentityMismatch { .. }
        | JpdbPitchFailure::InvalidSelection { .. }
        | JpdbPitchFailure::ExplicitSelectionMismatch { .. }
        | JpdbPitchFailure::DarkThemeUnverified { .. }
        | JpdbPitchFailure::CaptureContract { .. }
        | JpdbPitchFailure::InvalidPng { .. } => false,
    }
}

fn contains_retryable_network_error(message: &str) -> bool {
    let upper = message.to_ascii_uppercase();
    [
        "NET::ERR_TIMED_OUT",
        "ERR_TIMED_OUT",
        "NET::ERR_CONNECTION_RESET",
        "ERR_CONNECTION_RESET",
        "NET::ERR_CONNECTION_CLOSED",
        "ERR_CONNECTION_CLOSED",
        "NET::ERR_CONNECTION_REFUSED",
        "ERR_CONNECTION_REFUSED",
        "NET::ERR_NETWORK_CHANGED",
        "ERR_NETWORK_CHANGED",
        "NET::ERR_ABORTED",
        "ERR_ABORTED",
    ]
    .iter()
    .any(|token| upper.contains(token))
        || (upper.contains("429") || (500..=599).any(|status| upper.contains(&status.to_string())))
}

fn validate_reason(reason: &str) -> Result<(), AssetError> {
    if reason.trim().is_empty() || reason.len() > MAX_PITCH_BATCH_REASON_BYTES {
        return Err(invalid(format!(
            "reason должен быть непустым и не длиннее {MAX_PITCH_BATCH_REASON_BYTES} байт"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn identity_conflict(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::IdentityConflict, message)
}
