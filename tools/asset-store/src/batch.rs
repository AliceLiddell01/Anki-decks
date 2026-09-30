//! Resumable kanji batch: breadth-first acquisition, evidence и exact human actions.
//!
//! Это domain state, а не источник canonical trust. Интегратор сначала проверяет
//! bytes через asset owner и публикует semantic/human decision, затем фиксирует
//! результат здесь. Runtime хранится исключительно под `.runtime/batches/`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{
    AtFlags, FlockOperation, Mode, OFlags, flock, mkdirat, open, openat, renameat, unlinkat,
};
use serde::{Deserialize, Serialize};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;
use crate::model::{
    AssetIdentity, DetectedFormat, SemanticDecision, SemanticStatus, ValidationEvidence,
    ValidationRecord, ValidatorIdentity,
};

pub const BATCH_SCHEMA_VERSION: u32 = 1;
pub const AGGREGATE_POLICY_VERSION: &str = "kanji-distinct-mean-v1";
pub const MAX_ACQUISITION_ROUNDS: u32 = 5;
const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CANDIDATE_BYTES: u64 = 8 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Пороги совпадают с текущими f32-порогами kanji validator; policy pinned в state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregatePolicy {
    pub version: String,
    pub validator: ValidatorIdentity,
    pub maximum_expected_distance: f64,
    pub minimum_margin: f64,
}

impl AggregatePolicy {
    pub fn current(validator: ValidatorIdentity) -> Self {
        Self {
            version: AGGREGATE_POLICY_VERSION.into(),
            validator,
            maximum_expected_distance: f64::from(0.020_f32),
            minimum_margin: f64::from(0.004_f32),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSummary {
    pub mean: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub count: u32,
}

impl MetricSummary {
    fn calculate(values: impl Iterator<Item = f64>) -> Option<Self> {
        let values: Vec<_> = values.collect();
        if values.is_empty() {
            return None;
        }
        Some(Self {
            mean: values.iter().sum::<f64>() / values.len() as f64,
            minimum: values.iter().copied().fold(f64::INFINITY, f64::min),
            maximum: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            count: values.len() as u32,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KanjiMetrics {
    pub expected_distance: f64,
    pub margin: f64,
    pub nearest_other: Option<String>,
}

impl KanjiMetrics {
    /// Читает только фактическое evidence текущего CV owner.
    pub fn from_validation(record: &ValidationRecord) -> Option<Self> {
        record.evidence.iter().find_map(|evidence| {
            if evidence.kind != "pixel_reference_comparison" {
                return None;
            }
            let details = evidence.details.as_ref()?;
            let metrics = Self {
                expected_distance: details.get("expected_distance")?.as_f64()?,
                margin: details.get("nearest_margin")?.as_f64()?,
                nearest_other: details
                    .get("nearest_other")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
            };
            metrics.valid().then_some(metrics)
        })
    }

    fn valid(&self) -> bool {
        self.expected_distance.is_finite()
            && self.expected_distance >= 0.0
            && self.margin.is_finite()
    }
}

/// Exact candidate. Reference всегда относительна batch directory и hash-derived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchCandidate {
    pub sha256: String,
    pub storage_path: String,
    pub format: DetectedFormat,
    /// Только успешная integrity/format/decode проверка owner разрешает `true`.
    pub technically_valid: bool,
    pub automated: ValidationRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<Box<crate::yarxi::AcquisitionEvidence>>,
}

impl BatchCandidate {
    pub fn metrics(&self) -> Option<KanjiMetrics> {
        self.technically_valid
            .then(|| KanjiMetrics::from_validation(&self.automated))
            .flatten()
    }
}

/// Все реальные acquisition outcomes сохраняются, включая network failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchAttemptInput {
    Candidate { candidate: BatchCandidate },
    Failed { code: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchAttempt {
    pub index: u32,
    /// Новый пользовательский reacquire начинает поколение, сохраняя историю.
    pub generation: u32,
    pub round: u32,
    pub result: BatchAttemptInput,
    /// Exact duplicate записан как воспроизводимость, а не независимый sample.
    pub duplicate_sha256: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregateEvidence {
    pub policy_version: String,
    pub distinct_valid_hashes: Vec<String>,
    pub expected_distance: Option<MetricSummary>,
    pub margin: Option<MetricSummary>,
    pub accepted: bool,
    pub selected_sha256: Option<String>,
    pub selection_reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchItemStatus {
    Unresolved,
    AwaitingHuman,
    Reacquire,
    AutoVerified,
    HumanVerified,
    ExistingVerified,
}

impl BatchItemStatus {
    /// Semantic outcome; canonical publication отдельно проверяет `BatchItem::is_ready`.
    pub fn semantic_resolved(self) -> bool {
        matches!(
            self,
            Self::AutoVerified | Self::HumanVerified | Self::ExistingVerified
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchTrustSource {
    Existing,
    Automated,
    Aggregate,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanBatchAction {
    Confirm,
    Reject,
    Reacquire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanBatchDecision {
    pub identity: AssetIdentity,
    pub candidate_sha256: String,
    pub action: HumanBatchAction,
    /// Указываются сообщение пользователя или однозначная semantic интерпретация.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItem {
    pub identity: AssetIdentity,
    pub status: BatchItemStatus,
    pub generation: u32,
    pub attempts: Vec<BatchAttempt>,
    pub aggregate: AggregateEvidence,
    pub current_sha256: Option<String>,
    pub published_sha256: Option<String>,
    pub existing_candidate: Option<BatchCandidate>,
    pub publication_source: Option<BatchTrustSource>,
    pub human_decisions: Vec<HumanBatchDecision>,
    /// Уже применённые exact owner Reject: resume не материализует их заново.
    #[serde(default)]
    pub observed_owner_rejections: Vec<HumanBatchDecision>,
    pub review_reason: Option<String>,
}

impl BatchItem {
    pub fn is_ready(&self) -> bool {
        self.status.semantic_resolved()
            && self.current_sha256.is_some()
            && self.published_sha256 == self.current_sha256
            && self.publication_source.is_some()
    }
}

/// Runtime state не является manifest и не должен попадать в corpus commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KanjiBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub policy: AggregatePolicy,
    pub revision: u64,
    pub items: Vec<BatchItem>,
}

/// Выбранные bytes доступны через `BatchRuntime::read_candidate`, decision — owner API.
#[derive(Debug, Clone)]
pub struct AggregateResolution {
    pub identity: AssetIdentity,
    pub candidate: BatchCandidate,
    pub decision: SemanticDecision,
    pub validator: ValidatorIdentity,
}

impl KanjiBatch {
    pub fn new(
        batch_id: String,
        identities: Vec<AssetIdentity>,
        validator: ValidatorIdentity,
    ) -> Result<Self, AssetError> {
        validate_batch_id(&batch_id)?;
        ValidatorIdentity::new(validator.id.clone(), validator.version.clone()).map_err(invalid)?;
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        for identity in identities {
            validate_identity(&identity)?;
            if !seen.insert(identity.clone()) {
                continue;
            }
            items.push(BatchItem {
                identity,
                status: BatchItemStatus::Unresolved,
                generation: 0,
                attempts: Vec::new(),
                aggregate: empty_aggregate(),
                current_sha256: None,
                published_sha256: None,
                existing_candidate: None,
                publication_source: None,
                human_decisions: Vec::new(),
                observed_owner_rejections: Vec::new(),
                review_reason: None,
            });
        }
        if items.is_empty() {
            return Err(invalid("batch должен содержать хотя бы одну identity"));
        }
        Ok(Self {
            schema_version: BATCH_SCHEMA_VERSION,
            batch_id,
            policy: AggregatePolicy::current(validator),
            revision: 0,
            items,
        })
    }

    /// Frontiers не допускают retry первого item до первого прохода всех остальных.
    /// Caller сохраняет state после каждого результата, а не только целого раунда.
    pub fn next_round(&self) -> Vec<AssetIdentity> {
        let unresolved = self.items.iter().filter(|item| {
            matches!(
                item.status,
                BatchItemStatus::Unresolved | BatchItemStatus::Reacquire
            )
        });
        let minimum = unresolved.clone().map(generation_attempt_count).min();
        let Some(minimum) = minimum else {
            return Vec::new();
        };
        if minimum >= MAX_ACQUISITION_ROUNDS {
            return Vec::new();
        }
        unresolved
            .filter(|item| generation_attempt_count(item) == minimum)
            .map(|item| item.identity.clone())
            .collect()
    }

    pub fn is_resolved(&self) -> bool {
        self.items.iter().all(BatchItem::is_ready)
    }

    pub fn review_queue(&self) -> Vec<&BatchItem> {
        self.items
            .iter()
            .filter(|item| item.status == BatchItemStatus::AwaitingHuman)
            .collect()
    }

    /// Existing effective trust устанавливает caller после `AssetStore::read_verified`.
    pub fn mark_existing_ready(
        &mut self,
        identity: &AssetIdentity,
        sha256: &str,
    ) -> Result<(), AssetError> {
        validate_hash(sha256)?;
        let item = self.item_mut(identity)?;
        if !item.human_decisions.is_empty() {
            return Err(invalid(
                "reuse после human decision требует explicit exact resolution",
            ));
        }
        item.status = BatchItemStatus::ExistingVerified;
        item.current_sha256 = Some(sha256.into());
        item.published_sha256 = Some(sha256.into());
        item.publication_source = Some(BatchTrustSource::Existing);
        self.revision += 1;
        Ok(())
    }

    /// Сохраняет bytes/evidence canonical reuse для будущего exact human review.
    /// Это не acquisition attempt и не увеличивает независимый count.
    pub fn retain_existing_candidate(
        &mut self,
        identity: &AssetIdentity,
        candidate: BatchCandidate,
    ) -> Result<(), AssetError> {
        validate_attempt_input(
            &BatchAttemptInput::Candidate {
                candidate: candidate.clone(),
            },
            &self.policy,
        )?;
        let item = self.item_mut(identity)?;
        if item.status != BatchItemStatus::ExistingVerified
            || item.current_sha256.as_deref() != Some(candidate.sha256.as_str())
        {
            return Err(invalid(
                "existing review candidate не соответствует reused canonical SHA",
            ));
        }
        item.existing_candidate = Some(candidate);
        self.revision += 1;
        Ok(())
    }

    /// После canonical owner commit. До вызова exact candidate уже сохранён
    /// и resume повторяет publication без нового acquisition.
    pub fn mark_published_ready(
        &mut self,
        identity: &AssetIdentity,
        sha256: &str,
        source: BatchTrustSource,
    ) -> Result<(), AssetError> {
        validate_hash(sha256)?;
        let item = self.item_mut(identity)?;
        if item.current_sha256.as_deref() != Some(sha256) || !item.status.semantic_resolved() {
            return Err(invalid(
                "publication относится к stale/unresolved candidate",
            ));
        }
        check_publication_source(item, source)?;
        item.published_sha256 = Some(sha256.into());
        item.publication_source = Some(source);
        self.revision += 1;
        Ok(())
    }

    /// Owner snapshot больше не подтверждает saved readiness. Exact owner Reject
    /// сохраняется отдельно и исключает этот SHA из будущей auto aggregation.
    pub fn invalidate_owner_trust(
        &mut self,
        identity: &AssetIdentity,
        reason: String,
        owner_rejection: Option<HumanBatchDecision>,
    ) -> Result<(), AssetError> {
        if reason.trim().is_empty() || reason.len() > 4096 {
            return Err(invalid("owner reconciliation требует bounded reason"));
        }
        if let Some(decision) = &owner_rejection {
            validate_hash(&decision.candidate_sha256)?;
            if &decision.identity != identity
                || decision.action != HumanBatchAction::Reject
                || decision.reason.trim().is_empty()
                || decision.reason.len() > 4096
            {
                return Err(invalid(
                    "owner rejection не является exact identity/SHA semantic Reject",
                ));
            }
        }
        let item = self.item_mut(identity)?;
        if !item.status.semantic_resolved() {
            return Err(invalid(
                "owner trust invalidation требует ранее resolved item",
            ));
        }
        if let Some(decision) = owner_rejection {
            if !item.observed_owner_rejections.contains(&decision) {
                item.observed_owner_rejections.push(decision.clone());
            }
            if !item.human_decisions.contains(&decision) {
                item.human_decisions.push(decision);
            }
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("generation overflow"))?;
        item.status = BatchItemStatus::Reacquire;
        item.current_sha256 = None;
        item.published_sha256 = None;
        item.publication_source = None;
        item.aggregate = empty_aggregate();
        item.review_reason = Some(reason);
        self.revision += 1;
        Ok(())
    }

    /// Restart технического acquisition после пяти failures без candidate.
    /// Не принимает semantic решения и не может заменить exact candidate review.
    pub fn retry_acquisition(
        &mut self,
        identity: &AssetIdentity,
        reason: String,
    ) -> Result<(), AssetError> {
        if reason.trim().is_empty() || reason.len() > 4096 {
            return Err(invalid("reacquire требует bounded reason"));
        }
        let item = self.item_mut(identity)?;
        if item.status != BatchItemStatus::AwaitingHuman || item.current_sha256.is_some() {
            return Err(invalid(
                "candidate-less retry разрешён только после технических failures",
            ));
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("generation overflow"))?;
        item.status = BatchItemStatus::Reacquire;
        item.review_reason = Some(reason);
        item.aggregate = empty_aggregate();
        self.revision += 1;
        Ok(())
    }

    pub fn record_attempt(
        &mut self,
        identity: &AssetIdentity,
        result: BatchAttemptInput,
    ) -> Result<(), AssetError> {
        if !self.next_round().contains(identity) {
            return Err(invalid(
                "identity не входит в текущий breadth-first frontier",
            ));
        }
        validate_attempt_input(&result, &self.policy)?;
        let policy = self.policy.clone();
        let item = self.item_mut(identity)?;
        let duplicate_sha256 = match &result {
            BatchAttemptInput::Candidate { candidate } => item.attempts.iter().any(|attempt| matches!(&attempt.result, BatchAttemptInput::Candidate { candidate: old } if old.sha256 == candidate.sha256)),
            BatchAttemptInput::Failed { .. } => false,
        };
        item.attempts.push(BatchAttempt {
            index: item.attempts.len() as u32 + 1,
            generation: item.generation,
            round: generation_attempt_count(item) + 1,
            result,
            duplicate_sha256,
        });
        refresh_item(item, &policy);
        self.revision += 1;
        Ok(())
    }

    /// Сохраняет exact human intent до owner attestation; Confirm становится
    /// ready только после `mark_published_ready`. Bytes повторно сверяются с SHA.
    pub fn decide(
        &mut self,
        decision: HumanBatchDecision,
        exact_current_bytes: &[u8],
    ) -> Result<(), AssetError> {
        validate_hash(&decision.candidate_sha256)?;
        if decision.reason.trim().is_empty() || decision.reason.len() > 4096 {
            return Err(invalid("human decision требует bounded непустое reason"));
        }
        if sha256_hex(exact_current_bytes) != decision.candidate_sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "bytes human decision не совпадают с exact SHA-256",
            ));
        }
        let item = self.item_mut(&decision.identity)?;
        if item.current_sha256.as_deref() != Some(&decision.candidate_sha256) {
            return Err(invalid("human decision относится к stale/чужому candidate"));
        }
        let repeated = item.human_decisions.last() == Some(&decision);
        if repeated && decision.action != HumanBatchAction::Confirm {
            return Ok(());
        }
        if decision.action == HumanBatchAction::Confirm {
            let candidate = find_candidate(item, &decision.candidate_sha256)
                .ok_or_else(|| invalid("candidate bytes/evidence отсутствуют"))?;
            if !candidate.technically_valid || candidate.automated.status == SemanticStatus::Corrupt
            {
                return Err(invalid(
                    "technical/corrupt candidate нельзя подтвердить вручную",
                ));
            }
            item.status = BatchItemStatus::HumanVerified;
            // Existing/automated publication не заменяет explicit human owner
            // attestation. Durable intent должен пройти publish_human при resume.
            item.published_sha256 = None;
            item.publication_source = None;
            item.review_reason = None;
        } else {
            item.status = BatchItemStatus::Reacquire;
            item.generation = item
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("generation overflow"))?;
            item.review_reason = Some(decision.reason.clone());
            item.current_sha256 = None;
            item.published_sha256 = None;
            item.publication_source = None;
            item.aggregate = empty_aggregate();
        }
        if !repeated {
            item.human_decisions.push(decision);
        }
        self.revision += 1;
        Ok(())
    }

    /// Exact hash и versioned aggregate evidence для публикации owner'ом.
    /// Individual classifier status при этом никогда не переписывается в state.
    pub fn aggregate_decision(
        &self,
        identity: &AssetIdentity,
    ) -> Result<Option<AggregateResolution>, AssetError> {
        let item = self
            .items
            .iter()
            .find(|item| &item.identity == identity)
            .ok_or_else(|| invalid("identity не входит в batch"))?;
        if item.status != BatchItemStatus::AutoVerified || !item.aggregate.accepted {
            return Ok(None);
        }
        let hash = item
            .current_sha256
            .as_deref()
            .ok_or_else(|| invalid("aggregate не имеет exact selected hash"))?;
        let candidate = find_candidate(item, hash)
            .ok_or_else(|| invalid("aggregate candidate отсутствует"))?
            .clone();
        let evidence = ValidationEvidence {
            kind: "kanji_batch_aggregate".into(),
            summary: "Средние метрики distinct technically valid SHA-256 без REJECTED удовлетворяют неизменённым CV порогам; exact canonical candidate выбран детерминированно".into(),
            details: Some(serde_json::json!({
                "batch_id": self.batch_id,
                "identity": identity,
                "content_sha256": hash,
                "aggregate": item.aggregate,
                "policy": self.policy,
                "generation": item.generation,
                // Aggregate не подменяет индивидуальный automated статус
                // выбранного candidate, поэтому он сохраняется рядом.
                "selected_individual_status": candidate.automated.status,
                "selected_individual_validator": candidate.automated.validator,
                "selected_individual_evidence": candidate.automated.evidence,
            })),
        };
        Ok(Some(AggregateResolution {
            identity: identity.clone(),
            candidate,
            decision: SemanticDecision::new(SemanticStatus::Verified, vec![evidence]),
            validator: self.policy.validator.clone(),
        }))
    }

    fn item_mut(&mut self, identity: &AssetIdentity) -> Result<&mut BatchItem, AssetError> {
        self.items
            .iter_mut()
            .find(|item| &item.identity == identity)
            .ok_or_else(|| invalid("identity не входит в batch"))
    }

    /// Fail closed при подмене derived evidence, unknown policy/schema или refs.
    pub fn validate(&self) -> Result<(), AssetError> {
        validate_batch_id(&self.batch_id)?;
        if self.schema_version != BATCH_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "неподдерживаемая версия batch state",
            ));
        }
        ValidatorIdentity::new(
            self.policy.validator.id.clone(),
            self.policy.validator.version.clone(),
        )
        .map_err(invalid)?;
        if self.policy != AggregatePolicy::current(self.policy.validator.clone()) {
            return Err(invalid(
                "aggregate policy/thresholds не соответствуют зарегистрированной версии",
            ));
        }
        if self.items.is_empty() {
            return Err(invalid("пустой batch state"));
        }
        let mut identities = BTreeSet::new();
        for item in &self.items {
            validate_identity(&item.identity)?;
            if let Some(candidate) = &item.existing_candidate {
                validate_attempt_input(
                    &BatchAttemptInput::Candidate {
                        candidate: candidate.clone(),
                    },
                    &self.policy,
                )?;
            }
            if !identities.insert(&item.identity) {
                return Err(invalid("duplicate requested identity"));
            }
            let mut hashes = BTreeSet::new();
            let mut generations = std::collections::BTreeMap::<u32, u32>::new();
            for (index, attempt) in item.attempts.iter().enumerate() {
                validate_attempt_input(&attempt.result, &self.policy)?;
                let count = generations.entry(attempt.generation).or_default();
                *count += 1;
                if attempt.index != index as u32 + 1
                    || attempt.generation > item.generation
                    || attempt.round != *count
                    || *count > MAX_ACQUISITION_ROUNDS
                {
                    return Err(invalid("нарушен acquisition index/round/generation limit"));
                }
                let duplicate = match &attempt.result {
                    BatchAttemptInput::Candidate { candidate } => {
                        !hashes.insert(candidate.sha256.clone())
                    }
                    BatchAttemptInput::Failed { .. } => false,
                };
                if duplicate != attempt.duplicate_sha256 {
                    return Err(invalid(
                        "duplicate evidence не совпадает с реальными hashes",
                    ));
                }
            }
            for decision in item
                .human_decisions
                .iter()
                .chain(&item.observed_owner_rejections)
            {
                if decision.identity != item.identity
                    || decision.reason.trim().is_empty()
                    || decision.reason.len() > 4096
                {
                    return Err(invalid("human decision identity/reason невалидны"));
                }
                validate_hash(&decision.candidate_sha256)?;
            }
            if item.observed_owner_rejections.iter().any(|decision| {
                decision.action != HumanBatchAction::Reject
                    || !item.human_decisions.contains(decision)
            }) {
                return Err(invalid(
                    "observed owner rejection не имеет exact semantic Reject evidence",
                ));
            }
            if let Some(hash) = &item.current_sha256 {
                validate_hash(hash)?;
                if item.status != BatchItemStatus::ExistingVerified
                    && find_candidate(item, hash).is_none()
                {
                    return Err(invalid("current candidate не имеет сохранённого evidence"));
                }
            }
            if item.published_sha256.is_some() != item.publication_source.is_some() {
                return Err(invalid("publication hash/source не согласованы"));
            }
            if let Some(source) = item.publication_source {
                if !item.is_ready() {
                    return Err(invalid(
                        "published state относится к stale/unresolved candidate",
                    ));
                }
                check_publication_source(item, source)?;
            }
            if item.status == BatchItemStatus::ExistingVerified {
                if item.current_sha256.is_none() || !item.human_decisions.is_empty() {
                    return Err(invalid("невалидный existing verified state"));
                }
            } else if item.status == BatchItemStatus::HumanVerified {
                let decision = item
                    .human_decisions
                    .last()
                    .ok_or_else(|| invalid("human verified без decision"))?;
                let candidate = item
                    .current_sha256
                    .as_deref()
                    .and_then(|hash| find_candidate(item, hash))
                    .ok_or_else(|| invalid("human verified без candidate"))?;
                if decision.action != HumanBatchAction::Confirm
                    || decision.candidate_sha256 != candidate.sha256
                    || !candidate.technically_valid
                    || candidate.automated.status == SemanticStatus::Corrupt
                {
                    return Err(invalid(
                        "human verification не привязан к valid exact bytes",
                    ));
                }
            } else {
                let mut derived = item.clone();
                refresh_item(&mut derived, &self.policy);
                if item.aggregate != derived.aggregate
                    || item.current_sha256 != derived.current_sha256
                    || item.status != derived.status
                {
                    return Err(invalid(
                        "batch derived status/aggregate/candidate evidence подменены",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn check_publication_source(item: &BatchItem, source: BatchTrustSource) -> Result<(), AssetError> {
    let allowed = match source {
        BatchTrustSource::Existing => item.status == BatchItemStatus::ExistingVerified,
        BatchTrustSource::Human => item.status == BatchItemStatus::HumanVerified,
        BatchTrustSource::Aggregate => {
            item.status == BatchItemStatus::AutoVerified && item.aggregate.accepted
        }
        BatchTrustSource::Automated => {
            item.status == BatchItemStatus::AutoVerified
                && item
                    .current_sha256
                    .as_deref()
                    .and_then(|hash| find_candidate(item, hash))
                    .is_some_and(|candidate| {
                        candidate.technically_valid
                            && candidate.automated.status == SemanticStatus::Verified
                    })
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(invalid(
            "publication trust source не подтверждён domain evidence",
        ))
    }
}

fn empty_aggregate() -> AggregateEvidence {
    AggregateEvidence {
        policy_version: AGGREGATE_POLICY_VERSION.into(),
        distinct_valid_hashes: Vec::new(),
        expected_distance: None,
        margin: None,
        accepted: false,
        selected_sha256: None,
        selection_reason: "нет distinct технически валидных semantic samples".into(),
    }
}

fn generation_attempt_count(item: &BatchItem) -> u32 {
    item.attempts
        .iter()
        .filter(|attempt| attempt.generation == item.generation)
        .count() as u32
}

fn find_candidate<'a>(item: &'a BatchItem, hash: &str) -> Option<&'a BatchCandidate> {
    item.attempts
        .iter()
        .rev()
        .find_map(|attempt| match &attempt.result {
            BatchAttemptInput::Candidate { candidate } if candidate.sha256 == hash => {
                Some(candidate)
            }
            _ => None,
        })
        .or_else(|| {
            item.existing_candidate
                .as_ref()
                .filter(|candidate| candidate.sha256 == hash)
        })
}

fn refresh_item(item: &mut BatchItem, policy: &AggregatePolicy) {
    if generation_attempt_count(item) == 0 {
        item.aggregate = empty_aggregate();
        item.current_sha256 = None;
        item.status = if item.generation > 0 {
            BatchItemStatus::Reacquire
        } else {
            BatchItemStatus::Unresolved
        };
        return;
    }
    let rejected: BTreeSet<_> = item
        .human_decisions
        .iter()
        .filter(|decision| decision.action == HumanBatchAction::Reject)
        .map(|decision| decision.candidate_sha256.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    let mut samples: Vec<(&BatchCandidate, KanjiMetrics)> = Vec::new();
    let mut display_candidates = Vec::new();
    for attempt in item
        .attempts
        .iter()
        .filter(|attempt| attempt.generation == item.generation)
    {
        let BatchAttemptInput::Candidate { candidate } = &attempt.result else {
            continue;
        };
        if rejected.contains(candidate.sha256.as_str()) {
            continue;
        }
        display_candidates.push(candidate);
        // REJECTED означает, что заметно ближе другой эталон Unicode, то есть
        // изображение, вероятно, изображает другой символ. Это отрицательное
        // свидетельство: такой candidate не голосует за среднее и не может быть
        // выбран как canonical.
        if matches!(
            candidate.automated.status,
            SemanticStatus::Verified | SemanticStatus::Uncertain
        ) && let Some(metrics) = candidate.metrics()
            && seen.insert(&candidate.sha256)
        {
            samples.push((candidate, metrics));
        }
    }
    samples.sort_by(|(left, lm), (right, rm)| {
        lm.expected_distance
            .total_cmp(&rm.expected_distance)
            .then_with(|| rm.margin.total_cmp(&lm.margin))
            .then_with(|| left.sha256.cmp(&right.sha256))
    });
    let expected_distance =
        MetricSummary::calculate(samples.iter().map(|(_, metrics)| metrics.expected_distance));
    let margin = MetricSummary::calculate(samples.iter().map(|(_, metrics)| metrics.margin));
    let accepted = expected_distance
        .as_ref()
        .is_some_and(|metric| metric.mean <= policy.maximum_expected_distance)
        && margin
            .as_ref()
            .is_some_and(|metric| metric.mean >= policy.minimum_margin);
    // Individual VERIFIED выигрывает как уже достаточный evidence; aggregate
    // может выбрать любой distinct valid sample только при положительном mean.
    let individually_verified = display_candidates
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.technically_valid && candidate.automated.status == SemanticStatus::Verified
        })
        .min_by(|left, right| left.sha256.cmp(&right.sha256));
    let selected = individually_verified
        .or_else(|| samples.first().map(|(candidate, _)| *candidate))
        .or_else(|| {
            // Fallback нужен только для review: пока есть любой другой candidate,
            // REJECTED не выдвигается как текущий, но полностью отвергнутый item
            // остаётся видимым и доступным для решения человека.
            let rejected_rank = |candidate: &&BatchCandidate| {
                usize::from(candidate.automated.status == SemanticStatus::Rejected)
            };
            display_candidates.iter().copied().min_by(|left, right| {
                rejected_rank(left)
                    .cmp(&rejected_rank(right))
                    .then_with(|| left.sha256.cmp(&right.sha256))
            })
        });
    let selected_hash = selected.map(|candidate| candidate.sha256.clone());
    let mut distinct_valid_hashes: Vec<_> = samples
        .iter()
        .map(|(candidate, _)| candidate.sha256.clone())
        .collect();
    distinct_valid_hashes.sort();
    item.aggregate = AggregateEvidence {
        policy_version: policy.version.clone(), distinct_valid_hashes, expected_distance, margin,
        accepted, selected_sha256: selected_hash.clone(),
        selection_reason: if individually_verified.is_some() { "individual VERIFIED; tie-break lowercase SHA-256" } else { "minimum expected_distance, maximum margin, затем lowercase SHA-256; только distinct valid SHA" }.into(),
    };
    item.current_sha256 = selected_hash;
    if individually_verified.is_some() || accepted {
        item.status = BatchItemStatus::AutoVerified;
        item.review_reason = None;
    } else if generation_attempt_count(item) >= MAX_ACQUISITION_ROUNDS {
        item.status = BatchItemStatus::AwaitingHuman;
        item.review_reason = Some(if samples.is_empty() { "пять acquisition rounds завершены без пригодных independent semantic samples" } else { "пять acquisition rounds завершены; distinct mean не достигает неизменённых CV thresholds" }.into());
    } else {
        item.status = if item.generation > 0 {
            BatchItemStatus::Reacquire
        } else {
            BatchItemStatus::Unresolved
        };
    }
}

fn validate_attempt_input(
    result: &BatchAttemptInput,
    policy: &AggregatePolicy,
) -> Result<(), AssetError> {
    match result {
        BatchAttemptInput::Failed { code, message } => {
            if code.is_empty() || code.len() > 256 || message.len() > 4096 {
                return Err(invalid(
                    "failure evidence должно быть bounded и содержать code",
                ));
            }
        }
        BatchAttemptInput::Candidate { candidate } => {
            validate_hash(&candidate.sha256)?;
            if candidate.storage_path != candidate_path(&candidate.sha256, candidate.format)?
                || candidate.automated.content_sha256 != candidate.sha256
                || candidate.automated.validator != policy.validator
                || candidate.automated.evidence.is_empty()
            {
                return Err(invalid(
                    "candidate path/hash/validator/evidence не совпадают",
                ));
            }
            let encoded =
                serde_json::to_vec(candidate).map_err(|error| invalid(error.to_string()))?;
            if encoded.len() > 128 * 1024 {
                return Err(invalid("candidate automated evidence превышает 128 KiB"));
            }
            if candidate.automated.status == SemanticStatus::Corrupt && candidate.technically_valid
            {
                return Err(invalid("CORRUPT не является technically valid sample"));
            }
        }
    }
    Ok(())
}

fn validate_identity(identity: &AssetIdentity) -> Result<(), AssetError> {
    identity.validate().map_err(invalid)?;
    if identity.namespace != "kanji" {
        return Err(invalid("batch поддерживает только namespace kanji"));
    }
    parse_kanji_character(&identity.key).map_err(invalid)?;
    Ok(())
}

pub(crate) fn validate_batch_id(batch_id: &str) -> Result<(), AssetError> {
    if batch_id.is_empty()
        || batch_id.len() > 128
        || !batch_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(invalid(
            "batch_id должен содержать до 128 символов a-zA-Z0-9, '-', '_'",
        ));
    }
    Ok(())
}

pub(crate) fn validate_hash(hash: &str) -> Result<(), AssetError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("SHA-256 должен содержать 64 lowercase hex символа"));
    }
    Ok(())
}

fn candidate_path(hash: &str, format: DetectedFormat) -> Result<String, AssetError> {
    validate_hash(hash)?;
    let extension = match format {
        DetectedFormat::Gif => "gif",
        DetectedFormat::Png => "png",
        _ => return Err(invalid("kanji batch candidate должен иметь GIF/PNG format")),
    };
    Ok(format!("candidates/{hash}.{extension}"))
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

#[cfg(test)]
thread_local! {
    /// Наблюдаемость регрессионного теста: сколько раз candidate-файл был реально
    /// прочитан. Thread-local, поэтому параллельные тесты друг друга не считают.
    pub(crate) static CANDIDATE_FILE_READS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// FD-relative runtime persistence. Lock освобождается при process exit, включая crash.
/// Держать открытым во время load/mutate/save, чтобы два процесса не теряли updates.
#[derive(Debug)]
pub struct BatchRuntime {
    directory: File,
    candidates: File,
    batch_id: String,
    loaded_revision: Option<u64>,
    directory_path: PathBuf,
    /// Candidate files content-addressed и неизменяемы, а exclusive lock
    /// удерживается на весь lifetime instance, поэтому каждая проверяемая
    /// комбинация (path, hash, format) читается не более одного раза: иначе
    /// per-item save обходил бы весь набор заново.
    verified_candidate_keys: BTreeSet<String>,
}

impl BatchRuntime {
    /// Store root должен быть уже открыт/проверен владельцем `AssetStore`.
    /// Здесь root/descendants открываются NOFOLLOW, runtime не попадает в assets/.
    pub fn open(store_root: &Path, batch_id: &str) -> Result<Self, AssetError> {
        validate_batch_id(batch_id)?;
        let root = File::from(
            open(
                store_root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(boundary_io)?,
        );
        let runtime = ensure_directory(&root, ".runtime")?;
        let batches = ensure_directory(&runtime, "batches")?;
        let directory = ensure_directory(&batches, batch_id)?;
        flock(&directory, FlockOperation::LockExclusive).map_err(boundary_io)?;
        let candidates = ensure_directory(&directory, "candidates")?;
        Ok(Self {
            directory,
            candidates,
            batch_id: batch_id.into(),
            loaded_revision: None,
            directory_path: store_root.join(".runtime").join("batches").join(batch_id),
            verified_candidate_keys: BTreeSet::new(),
        })
    }

    pub fn batch_id(&self) -> &str {
        &self.batch_id
    }

    pub fn load(&mut self) -> Result<Option<KanjiBatch>, AssetError> {
        let bytes = match read_file(&self.directory, "state.json", MAX_STATE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.code == ErrorCode::MissingAssetFile => {
                self.loaded_revision = None;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let state: KanjiBatch = serde_json::from_slice(&bytes).map_err(|error| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("batch JSON повреждён: {error}"),
            )
        })?;
        state.validate()?;
        if state.batch_id != self.batch_id {
            return Err(invalid("batch identity не совпадает с runtime directory"));
        }
        // Every saved candidate remains recoverable, including rejected/corrupt bytes.
        self.verify_referenced_candidates(&state)?;
        self.loaded_revision = Some(state.revision);
        Ok(Some(state))
    }

    /// Проверяет каждый referenced candidate не более одного раза за instance.
    fn verify_referenced_candidates(&mut self, state: &KanjiBatch) -> Result<(), AssetError> {
        for item in &state.items {
            if let Some(candidate) = &item.existing_candidate {
                self.verify_candidate_once(candidate)?;
            }
            for attempt in &item.attempts {
                if let BatchAttemptInput::Candidate { candidate } = &attempt.result {
                    self.verify_candidate_once(candidate)?;
                }
            }
        }
        Ok(())
    }

    fn verify_candidate_once(&mut self, candidate: &BatchCandidate) -> Result<(), AssetError> {
        // Ключ покрывает всё, что проверяет read_candidate, поэтому две ссылки с
        // одинаковым ключом проверяются одинаково и второе чтение избыточно.
        let key = format!(
            "{}|{}|{:?}",
            candidate.storage_path, candidate.sha256, candidate.format
        );
        if self.verified_candidate_keys.contains(&key) {
            return Ok(());
        }
        self.read_candidate(candidate)?;
        self.verified_candidate_keys.insert(key);
        Ok(())
    }

    pub fn save(&mut self, state: &KanjiBatch) -> Result<(), AssetError> {
        state.validate()?;
        if state.batch_id != self.batch_id {
            return Err(invalid("batch identity не совпадает с runtime directory"));
        }
        if let Some(revision) = self.loaded_revision {
            if state.revision < revision {
                return Err(invalid("batch revision нельзя откатить"));
            }
        } else {
            match read_file(&self.directory, "state.json", MAX_STATE_BYTES) {
                Ok(_) => return Err(invalid("существующий batch необходимо load перед save")),
                Err(error) if error.code == ErrorCode::MissingAssetFile => {}
                Err(error) => return Err(error),
            }
        }
        // `save` всегда следует за `load` того же instance, а load уже проверил
        // каждый referenced candidate; здесь достаточно дешёвой инкрементальной
        // проверки, а не повторного обхода всего набора на каждый item.
        self.verify_referenced_candidates(state)?;
        let bytes = serde_json::to_vec_pretty(state).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid("batch state превышает bounded runtime limit"));
        }
        atomic_write(&self.directory, "state.json", &bytes)?;
        self.loaded_revision = Some(state.revision);
        Ok(())
    }

    /// Escaped local review UI. Каждый distinct candidate SHA проверяется перед
    /// выдачей relative image reference; GIF сохраняет полную анимацию.
    /// Review artifact never changes the machine-readable source of truth.
    pub fn write_review(&self, state: &KanjiBatch) -> Result<PathBuf, AssetError> {
        state.validate()?;
        if state.batch_id != self.batch_id {
            return Err(invalid(
                "review batch identity не совпадает с runtime directory",
            ));
        }
        let mut html = String::from(
            "<!doctype html><html lang=\"ru\"><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file:; style-src 'unsafe-inline'\"><title>Проверка kanji assets</title><style>body{font-family:system-ui;max-width:1100px;margin:auto;padding:1rem}article{border:1px solid #999;padding:1rem;margin:1rem 0}img{max-width:300px;max-height:300px}pre{white-space:pre-wrap;overflow-wrap:anywhere}code{overflow-wrap:anywhere}</style><h1>Проверка kanji assets</h1>",
        );
        write!(
            &mut html,
            "<p>Batch: <code>{}</code>; policy: <code>{}</code>; revision: {}</p>",
            html_escape(&state.batch_id),
            html_escape(&state.policy.version),
            state.revision
        )
        .expect("String write");
        for item in state.review_queue() {
            write!(&mut html, "<article><h2>{} · U+{:04X}</h2><p>State: {:?}; acquisition attempts: {}; generation: {}; independent valid SHA: {}</p>", html_escape(&item.identity.key), u32::from(parse_kanji_character(&item.identity.key).map_err(invalid)?), item.status, item.attempts.len(), item.generation, item.aggregate.distinct_valid_hashes.len()).expect("String write");
            if let Some(hash) = &item.current_sha256 {
                let candidate = find_candidate(item, hash)
                    .ok_or_else(|| invalid("review candidate отсутствует"))?;
                self.read_candidate(candidate)?;
                write!(&mut html, "<img alt=\"{}\" src=\"{}\"><p>Exact SHA-256: <code>{}</code>; format: {:?}; automated: {:?}; technical valid: {}</p>", html_escape(&item.identity.key), html_escape(&candidate.storage_path), candidate.sha256, candidate.format, candidate.automated.status, candidate.technically_valid).expect("String write");
                if let Some(metrics) = candidate.metrics() {
                    write!(
                        &mut html,
                        "<p>Ближайший competing Unicode: {}</p>",
                        html_escape(metrics.nearest_other.as_deref().unwrap_or("нет"))
                    )
                    .expect("String write");
                }
            } else {
                html.push_str("<p>Нет доступного текущего candidate: acquisition завершился техническим отказом. Нужен новый acquisition, semantic approval недоступен.</p>");
            }
            write!(&mut html, "<p>Причина review: {}</p><h3>Aggregate mean / min / max / count</h3><pre>{}</pre><h3>Все acquisition attempts</h3>", html_escape(item.review_reason.as_deref().unwrap_or("")), html_escape(&serde_json::to_string_pretty(&item.aggregate).map_err(|error| invalid(error.to_string()))?)).expect("String write");
            let mut shown = BTreeSet::new();
            if let Some(hash) = &item.current_sha256 {
                shown.insert(hash.clone());
            }
            for attempt in &item.attempts {
                if let BatchAttemptInput::Candidate { candidate } = &attempt.result
                    && shown.insert(candidate.sha256.clone())
                {
                    self.read_candidate(candidate)?;
                    write!(&mut html, "<figure><img alt=\"{}\" src=\"{}\"><figcaption>Distinct candidate SHA-256: <code>{}</code>; format: {:?}; automated: {:?}; technical valid: {}</figcaption></figure>", html_escape(&item.identity.key), html_escape(&candidate.storage_path), candidate.sha256, candidate.format, candidate.automated.status, candidate.technically_valid).expect("String write");
                }
            }
            for attempt in &item.attempts {
                write!(&mut html, "<details><summary>Attempt {} · round {} · generation {} · duplicate SHA: {}</summary>", attempt.index, attempt.round, attempt.generation, attempt.duplicate_sha256).expect("String write");
                let description = serde_json::to_string_pretty(&attempt.result)
                    .map_err(|error| invalid(error.to_string()))?;
                let bounded: String = description.chars().take(6000).collect();
                write!(&mut html, "<pre>{}</pre></details>", html_escape(&bounded))
                    .expect("String write");
            }
            html.push_str("</article>");
        }
        html.push_str("</html>");
        if html.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid(
                "review HTML превышает bounded runtime limit; разделите batch",
            ));
        }
        atomic_write(&self.directory, "review.html", html.as_bytes())?;
        Ok(self.directory_path.join("review.html"))
    }

    /// Bytes persist ДО state entry: interruption оставит безопасный orphan,
    /// который не становится trusted и не блокирует resume предыдущего state.
    pub fn persist_candidate(
        &self,
        bytes: &[u8],
        automated: ValidationRecord,
        technically_valid: bool,
    ) -> Result<BatchCandidate, AssetError> {
        if bytes.len() as u64 > MAX_CANDIDATE_BYTES {
            return Err(invalid("candidate превышает лимит размера"));
        }
        let hash = sha256_hex(bytes);
        if automated.content_sha256 != hash {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "automated record не соответствует candidate bytes",
            ));
        }
        let format = DetectedFormat::from_signature(bytes);
        let storage_path = candidate_path(&hash, format)?;
        let name = storage_path
            .strip_prefix("candidates/")
            .expect("hash-derived path");
        match read_file(&self.candidates, name, MAX_CANDIDATE_BYTES) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "существующий candidate path содержит другие bytes",
                ));
            }
            Err(error) if error.code == ErrorCode::MissingAssetFile => {
                atomic_write(&self.candidates, name, bytes)?
            }
            Err(error) => return Err(error),
        }
        Ok(BatchCandidate {
            sha256: hash,
            storage_path,
            format,
            technically_valid,
            automated,
            acquisition: None,
        })
    }

    /// Доступ к bytes для GIF/PNG review или owner attestation. Проверяет path,
    /// regular file, bounds, format и SHA перед возвращением фактических bytes.
    pub fn read_candidate(&self, candidate: &BatchCandidate) -> Result<Vec<u8>, AssetError> {
        let expected = candidate_path(&candidate.sha256, candidate.format)?;
        if candidate.storage_path != expected {
            return Err(AssetError::new(
                ErrorCode::PathTraversal,
                "candidate reference не совпадает с hash-derived relative path",
            ));
        }
        let name = expected
            .strip_prefix("candidates/")
            .expect("hash-derived path");
        #[cfg(test)]
        CANDIDATE_FILE_READS.with(|reads| reads.set(reads.get() + 1));
        let bytes = read_file(&self.candidates, name, MAX_CANDIDATE_BYTES)?;
        if sha256_hex(&bytes) != candidate.sha256
            || DetectedFormat::from_signature(&bytes) != candidate.format
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "candidate bytes/hash/format изменились",
            ));
        }
        Ok(bytes)
    }
}

fn ensure_directory(parent: &File, name: &str) -> Result<File, AssetError> {
    match mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::EXIST => {}
        Err(error) => return Err(boundary_io(error)),
    }
    Ok(File::from(
        openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(boundary_io)?,
    ))
}

fn read_file(parent: &File, name: &str, maximum: u64) -> Result<Vec<u8>, AssetError> {
    let descriptor = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == rustix::io::Errno::NOENT {
            AssetError::new(ErrorCode::MissingAssetFile, "runtime file отсутствует")
        } else {
            boundary_io(error)
        }
    })?;
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| AssetError::io("runtime metadata", error))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "runtime entry должен быть bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("runtime read", error))?;
    if bytes.len() as u64 > maximum {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "runtime entry вырос сверх лимита",
        ));
    }
    Ok(bytes)
}

fn atomic_write(parent: &File, name: &str, bytes: &[u8]) -> Result<(), AssetError> {
    // Fixed destination must not be symlink/special file before replacement.
    match read_file(parent, name, MAX_STATE_BYTES) {
        Ok(_) => {}
        Err(error) if error.code == ErrorCode::MissingAssetFile => {}
        Err(error) => return Err(error),
    }
    let temporary = format!(
        ".tmp-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let descriptor = openat(
        parent,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(boundary_io)?;
    let result = (|| {
        let mut file = File::from(descriptor);
        file.write_all(bytes)
            .map_err(|error| AssetError::io("runtime write", error))?;
        file.sync_all()
            .map_err(|error| AssetError::io("runtime sync", error))?;
        renameat(parent, temporary.as_str(), parent, name).map_err(boundary_io)?;
        parent
            .sync_all()
            .map_err(|error| AssetError::io("runtime directory sync", error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(parent, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn boundary_io(error: rustix::io::Errno) -> AssetError {
    if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
        AssetError::new(
            ErrorCode::BoundaryViolation,
            "runtime path содержит symlink или не является каталогом/regular file",
        )
    } else {
        AssetError::io("batch runtime filesystem", std::io::Error::from(error))
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
