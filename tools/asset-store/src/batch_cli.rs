//! Thin CLI orchestration of kanji batch state and generic asset owner.

use super::*;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use crate::batch::{
    AggregateResolution, BatchAttemptInput, BatchCandidate, BatchItemStatus, BatchRuntime,
    BatchTrustSource, HumanBatchAction, HumanBatchDecision, KanjiBatch,
};
use crate::model::{HumanDecision, Provenance, SemanticDecision, ValidationRecord};
use crate::store::{HumanAttestationRequest, validate_image_decode};
use crate::validation::ValidatorFailure;

#[derive(Debug, Subcommand)]
pub enum BatchCommand {
    /// Создаёт persistent batch; trusted corpus reuse проверяется без acquisition.
    Start {
        #[arg(long)]
        batch_id: Option<String>,
        #[arg(required = true, num_args = 1..)]
        characters: Vec<String>,
    },
    /// Продолжает batch breadth-first, не более указанного количества раундов.
    Run {
        #[arg(long)]
        batch_id: String,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=5))]
        rounds: u32,
    },
    /// Возвращает state/evidence, не запускает acquisition или publication.
    Status {
        #[arg(long)]
        batch_id: String,
    },
    /// Создаёт локальный HTML по всем unresolved candidates после лимита.
    Review {
        #[arg(long)]
        batch_id: String,
    },
    /// Структурированное human decision для exact текущих bytes.
    Decide {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        character: String,
        #[arg(long)]
        sha256: String,
        #[arg(long, value_enum)]
        action: BatchActionArg,
        #[arg(long)]
        reason: String,
    },
    /// Targeted acquisition restart. При наличии candidate требуется exact SHA.
    Retry {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        character: String,
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BatchActionArg {
    Confirm,
    Reject,
    Reacquire,
}

impl From<BatchActionArg> for HumanBatchAction {
    fn from(action: BatchActionArg) -> Self {
        match action {
            BatchActionArg::Confirm => Self::Confirm,
            BatchActionArg::Reject => Self::Reject,
            BatchActionArg::Reacquire => Self::Reacquire,
        }
    }
}

impl BatchCommand {
    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::Start { .. } => "batch_start",
            Self::Run { .. } => "batch_run",
            Self::Status { .. } => "batch_status",
            Self::Review { .. } => "batch_review",
            Self::Decide { .. } => "batch_decide",
            Self::Retry { .. } => "batch_retry",
        }
    }

    fn id(&self) -> Option<&str> {
        match self {
            Self::Start { batch_id, .. } => batch_id.as_deref(),
            Self::Run { batch_id, .. }
            | Self::Status { batch_id }
            | Self::Review { batch_id }
            | Self::Decide { batch_id, .. }
            | Self::Retry { batch_id, .. } => Some(batch_id),
        }
    }
}

pub(super) fn prevalidate(command: &BatchCommand) -> Result<(), AssetError> {
    if let Some(id) = command.id() {
        crate::batch::validate_batch_id(id)?;
    }
    match command {
        BatchCommand::Start { characters, .. } => {
            for character in characters {
                parse_character(character)?;
            }
            if characters.is_empty() {
                return Err(invalid("batch не может быть пустым"));
            }
        }
        BatchCommand::Decide {
            character,
            sha256,
            reason,
            ..
        } => {
            parse_character(character)?;
            crate::batch::validate_hash(sha256)?;
            validate_reason(reason)?;
        }
        BatchCommand::Retry {
            character,
            sha256,
            reason,
            ..
        } => {
            parse_character(character)?;
            if let Some(hash) = sha256 {
                crate::batch::validate_hash(hash)?;
            }
            validate_reason(reason)?;
        }
        BatchCommand::Run { rounds, .. } if !(1..=5).contains(rounds) => {
            return Err(invalid("rounds должен быть от 1 до 5"));
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct BatchIssue {
    identity: AssetIdentity,
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct BatchCounts {
    requested: usize,
    effective_verified: usize,
    auto_verified: usize,
    human_verified: usize,
    existing_verified: usize,
    awaiting_human: usize,
    scheduled_for_reacquire: usize,
    pending_publication: usize,
    unresolved: usize,
}

#[derive(Debug, Serialize)]
struct BatchItemOutcome {
    identity: AssetIdentity,
    state: BatchItemStatus,
    effective_verified: bool,
    candidate_sha256: Option<String>,
    published_sha256: Option<String>,
    publication_source: Option<BatchTrustSource>,
    acquisition_attempts: usize,
    generation: u32,
    distinct_valid_hashes: usize,
    item_outcome: &'static str,
}

#[derive(Debug, Serialize)]
struct BatchResponse {
    schema_version: u32,
    operation: String,
    store: StoreSummary,
    batch_id: String,
    outcome: &'static str,
    changed: bool,
    counts: BatchCounts,
    items: Vec<BatchItemOutcome>,
    issues: Vec<BatchIssue>,
    blockers: Vec<String>,
    review_artifact: Option<String>,
    /// Domain source of truth remains the persistent owner state.
    batch: KanjiBatch,
}

pub(super) fn execute(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    output: OutputFormat,
    allow_insecure_tls: bool,
) -> CliOutput {
    match execute_command(store, summary.clone(), command, allow_insecure_tls) {
        Ok((response, exit_code)) => match output {
            OutputFormat::Json => CliOutput {
                stdout: format!(
                    "{}\n",
                    serde_json::to_string_pretty(&response).expect("finite batch response")
                ),
                stderr: String::new(),
                exit_code,
            },
            OutputFormat::Human => {
                let mut text = format!(
                    "{}: {} (batch={})\n",
                    response.operation, response.outcome, response.batch_id
                );
                for item in &response.items {
                    text.push_str(&format!(
                        "{}  {}  {}  attempts={} distinct={}\n",
                        item.identity,
                        item.item_outcome,
                        item.candidate_sha256.as_deref().unwrap_or("-"),
                        item.acquisition_attempts,
                        item.distinct_valid_hashes
                    ));
                }
                if let Some(path) = response.review_artifact {
                    text.push_str(&format!("review: {path}\n"));
                }
                for issue in &response.issues {
                    text.push_str(&format!(
                        "{}: {}: {}\n",
                        issue.identity, issue.code, issue.message
                    ));
                }
                CliOutput {
                    stdout: text,
                    stderr: String::new(),
                    exit_code,
                }
            }
        },
        Err(error) => super::render_error(
            command.operation().into(),
            None,
            error,
            summary,
            output,
            store.initialized_on_open(),
        ),
    }
}

/// One checked full-corpus read. Indexed records are reused for the whole
/// boundary; mutations keep this local view current via exact owner outcomes.
struct OwnerSnapshot {
    records: BTreeMap<AssetIdentity, AssetRecord>,
    error: Option<AssetError>,
}

impl OwnerSnapshot {
    fn from_result(result: Result<Vec<AssetRecord>, AssetError>) -> Self {
        match result {
            Ok(records) => Self {
                records: records
                    .into_iter()
                    .map(|record| (record.identity.clone(), record))
                    .collect(),
                error: None,
            },
            Err(error) => Self {
                records: BTreeMap::new(),
                error: Some(error),
            },
        }
    }
    fn checked(&self) -> Result<(), AssetError> {
        if let Some(error) = &self.error {
            return Err(copy_error(error));
        }
        Ok(())
    }
    fn current(&self, identity: &AssetIdentity) -> Option<&AssetRecord> {
        self.records.get(identity)
    }
    fn committed(&mut self, record: AssetRecord) {
        self.records.insert(record.identity.clone(), record);
    }
}

trait OwnerSnapshotReader {
    fn capture(&mut self, store: &AssetStore) -> OwnerSnapshot;
}
struct StoreSnapshotReader;
impl OwnerSnapshotReader for StoreSnapshotReader {
    fn capture(&mut self, store: &AssetStore) -> OwnerSnapshot {
        OwnerSnapshot::from_result(store.verify_integrity())
    }
}
fn copy_error(error: &AssetError) -> AssetError {
    AssetError::with_details(error.code, error.message.clone(), error.details.clone())
}

fn execute_command(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    allow_insecure_tls: bool,
) -> Result<(BatchResponse, u8), AssetError> {
    execute_command_with_snapshots(
        store,
        summary,
        command,
        allow_insecure_tls,
        &mut StoreSnapshotReader,
    )
}

fn execute_command_with_snapshots(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    allow_insecure_tls: bool,
    reader: &mut impl OwnerSnapshotReader,
) -> Result<(BatchResponse, u8), AssetError> {
    prevalidate(command)?;
    let operation = command.operation();
    match command {
        BatchCommand::Start {
            batch_id,
            characters,
        } => {
            let identities: Vec<_> = characters
                .iter()
                .map(|character| parse_character(character).map(|character| character.identity()))
                .collect::<Result<_, _>>()?;
            let batch_id = match batch_id {
                Some(id) => id.clone(),
                None => generated_batch_id()?,
            };
            let mut runtime = BatchRuntime::open(store.root(), &batch_id)?;
            if let Some(mut state) = runtime.load()? {
                let requested: BTreeSet<_> = identities.into_iter().collect();
                let existing: BTreeSet<_> = state
                    .items
                    .iter()
                    .map(|item| item.identity.clone())
                    .collect();
                if requested != existing
                    || state.policy.validator != KanjiImageValidator::validator_identity()
                {
                    return Err(invalid(
                        "batch_id уже принадлежит другому requested set/validator",
                    ));
                }
                let revision = state.revision;
                let mut issues = Vec::new();
                let snapshot = reader.capture(store);
                reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
                if state.revision != revision {
                    runtime.save(&state)?;
                }
                let changed = state.revision != revision;
                return Ok((
                    batch_response(
                        operation,
                        summary,
                        state,
                        changed,
                        issues,
                        None,
                        "already_started",
                    ),
                    0,
                ));
            }
            let mut state = KanjiBatch::new(
                batch_id,
                identities,
                KanjiImageValidator::validator_identity(),
            )?;
            let snapshot = reader.capture(store);
            reuse_existing(&snapshot, &mut state)?;
            runtime.save(&state)?;
            Ok((
                batch_response(operation, summary, state, true, Vec::new(), None, "started"),
                0,
            ))
        }
        BatchCommand::Run { batch_id, rounds } => {
            let (state, changed, issues) = run_batch_with_snapshots(
                store,
                batch_id,
                *rounds,
                |characters| acquire_many(characters, allow_insecure_tls),
                reader,
            )?;
            let resolved = state.is_resolved();
            let outcome = if resolved {
                "resolved"
            } else if state.review_queue().is_empty() {
                "partial_progress"
            } else {
                "awaiting_human"
            };
            let artifact = if state.review_queue().is_empty() {
                None
            } else {
                let runtime = BatchRuntime::open(store.root(), batch_id)?;
                Some(runtime.write_review(&state)?.display().to_string())
            };
            Ok((
                batch_response(
                    operation, summary, state, changed, issues, artifact, outcome,
                ),
                if resolved { 0 } else { 3 },
            ))
        }
        BatchCommand::Status { batch_id } | BatchCommand::Review { batch_id } => {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            let revision = state.revision;
            let mut issues = Vec::new();
            let snapshot = reader.capture(store);
            reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
            if state.revision != revision {
                runtime.save(&state)?;
            }
            let changed = state.revision != revision;
            let artifact = if matches!(command, BatchCommand::Review { .. }) {
                Some(runtime.write_review(&state)?.display().to_string())
            } else {
                None
            };
            Ok((
                batch_response(operation, summary, state, changed, issues, artifact, "ok"),
                0,
            ))
        }
        BatchCommand::Decide {
            batch_id,
            character,
            sha256,
            action,
            reason,
        } => {
            let decision = HumanBatchDecision {
                identity: parse_character(character)?.identity(),
                candidate_sha256: sha256.clone(),
                action: (*action).into(),
                reason: reason.clone(),
            };
            let (state, issues) = decide_exact(store, batch_id, decision)?;
            let code = if issues.is_empty() { 0 } else { 3 };
            Ok((
                batch_response(
                    operation,
                    summary,
                    state,
                    true,
                    issues,
                    None,
                    if code == 0 {
                        "decision_applied"
                    } else {
                        "publication_blocked"
                    },
                ),
                code,
            ))
        }
        BatchCommand::Retry {
            batch_id,
            character,
            sha256,
            reason,
        } => {
            let identity = parse_character(character)?.identity();
            let (state, issues) = if let Some(sha256) = sha256 {
                decide_exact(
                    store,
                    batch_id,
                    HumanBatchDecision {
                        identity,
                        candidate_sha256: sha256.clone(),
                        action: HumanBatchAction::Reacquire,
                        reason: reason.clone(),
                    },
                )?
            } else {
                let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
                let mut state = load_required(&mut runtime)?;
                check_current_validator(&state)?;
                state.retry_acquisition(&identity, reason.clone())?;
                runtime.save(&state)?;
                (state, Vec::new())
            };
            let code = if issues.is_empty() { 0 } else { 3 };
            Ok((
                batch_response(
                    operation,
                    summary,
                    state,
                    true,
                    issues,
                    None,
                    "reacquire_scheduled",
                ),
                code,
            ))
        }
    }
}

fn load_required(runtime: &mut BatchRuntime) -> Result<KanjiBatch, AssetError> {
    runtime.load()?.ok_or_else(|| {
        AssetError::new(
            ErrorCode::MissingAssetFile,
            "batch отсутствует; сначала выполните batch start",
        )
    })
}

#[cfg(test)]
fn run_batch<F>(
    store: &AssetStore,
    batch_id: &str,
    rounds: u32,
    acquire: F,
) -> Result<(KanjiBatch, bool, Vec<BatchIssue>), AssetError>
where
    F: FnMut(&[String]) -> Result<Vec<Result<AcquiredMedia, String>>, String>,
{
    run_batch_with_snapshots(store, batch_id, rounds, acquire, &mut StoreSnapshotReader)
}

fn run_batch_with_snapshots<F>(
    store: &AssetStore,
    batch_id: &str,
    rounds: u32,
    mut acquire: F,
    reader: &mut impl OwnerSnapshotReader,
) -> Result<(KanjiBatch, bool, Vec<BatchIssue>), AssetError>
where
    F: FnMut(&[String]) -> Result<Vec<Result<AcquiredMedia, String>>, String>,
{
    let mut issues = Vec::new();
    let initial_revision;
    {
        let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
        let mut state = load_required(&mut runtime)?;
        check_current_validator(&state)?;
        initial_revision = state.revision;
        let mut snapshot = reader.capture(store);
        reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
        runtime.save(&state)?;
        snapshot.checked()?;
        // Durable intents resume before reuse; exact owner mutation outcomes
        // update the snapshot without scanning neighboring corpus files again.
        publish_pending(store, &mut runtime, &mut state, &mut issues, &mut snapshot)?;
        reuse_existing(&snapshot, &mut state)?;
        runtime.save(&state)?;
    }
    for _ in 0..rounds {
        let (frontier, generations) = {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let state = load_required(&mut runtime)?;
            let frontier = state.next_round();
            let generations: Vec<_> = frontier
                .iter()
                .map(|identity| {
                    let item = state
                        .items
                        .iter()
                        .find(|item| &item.identity == identity)
                        .expect("frontier identity");
                    (item.generation, item.attempts.len())
                })
                .collect();
            (frontier, generations)
        };
        if frontier.is_empty() {
            break;
        }
        let characters: Vec<_> = frontier
            .iter()
            .map(|identity| identity.key.clone())
            .collect();
        // Runtime locks are released during network/browser acquisition.
        let acquired = acquire(&characters);
        let mut snapshot = reader.capture(store);
        {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
            runtime.save(&state)?;
            snapshot.checked()?;
            reuse_existing(&snapshot, &mut state)?;
            runtime.save(&state)?;
        }
        // Один runtime на весь frontier: exclusive lock удерживается от первого
        // до последнего item, а state читается один раз вместо повторного
        // чтения и перепроверки candidates на каждый item.
        {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            for (index, identity) in frontier.iter().enumerate() {
                let item = state
                    .items
                    .iter()
                    .find(|item| &item.identity == identity)
                    .expect("frontier requested identity");
                if (item.generation, item.attempts.len()) != generations[index]
                    || !state.next_round().contains(identity)
                {
                    continue;
                }
                let input = match acquired.as_ref() {
                    Ok(results) => match results.get(index) {
                        Some(Ok(media)) => validated_candidate(&runtime, identity, media),
                        Some(Err(message)) => Ok(failure("acquisition_failed", message)),
                        None => Ok(failure(
                            "provider_outcome_mismatch",
                            "provider outcome count mismatch",
                        )),
                    },
                    Err(message) => Ok(failure("acquisition_failed", message)),
                };
                match input {
                    Ok(input) => state.record_attempt(identity, input)?,
                    Err(error) if error.code.exit_code() != 4 => {
                        issues.push(issue(identity, &error));
                        state.record_attempt(
                            identity,
                            failure(error.code.as_str(), &error.message),
                        )?;
                    }
                    Err(error) => return Err(error),
                }
                // Persist each exact outcome; no full owner snapshot inside this loop.
                runtime.save(&state)?;
            }
        }
        {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            // A newly acquired hash may already be explicitly rejected by owner.
            reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
            runtime.save(&state)?;
            publish_pending(store, &mut runtime, &mut state, &mut issues, &mut snapshot)?;
            runtime.save(&state)?;
        }
    }
    let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
    let mut state = load_required(&mut runtime)?;
    let snapshot = reader.capture(store);
    reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
    runtime.save(&state)?;
    let changed = state.revision != initial_revision;
    Ok((state, changed, issues))
}

fn validated_candidate(
    runtime: &BatchRuntime,
    identity: &AssetIdentity,
    media: &AcquiredMedia,
) -> Result<BatchAttemptInput, AssetError> {
    // Provider evidence хранит Unicode статьи как hex code point, а identity —
    // сам character. Сравнение идёт с точным code point requested identity.
    let expected_code = match parse_kanji_character(&identity.key) {
        Ok(character) => format!("{:X}", u32::from(character)),
        Err(message) => return Ok(failure("source_identity_mismatch", &message)),
    };
    if media.character != identity.key
        || !media.article_unicode.eq_ignore_ascii_case(&expected_code)
        || !media
            .evidence
            .article_unicode
            .eq_ignore_ascii_case(&expected_code)
    {
        return Ok(failure(
            "source_identity_mismatch",
            "Yarxi character/article Unicode не совпадает с requested identity",
        ));
    }
    if let Err(message) = validate_selected_format(media.selection, &media.bytes) {
        return Ok(failure("media_format_mismatch", &message));
    }
    let sha256 = sha256_hex(&media.bytes);
    let record = candidate_asset_record(identity, &media.bytes, Some(&media.evidence));
    let validator = KanjiImageValidator::new();
    let decision = validator
        .validate(&record, &mut Cursor::new(media.bytes.as_slice()))
        .map_err(|failure| AssetError::new(ErrorCode::ValidatorFailure, failure.message))?;
    let technically_valid =
        decision.status != SemanticStatus::Corrupt && validate_image_decode(&media.bytes).is_ok();
    let automated = ValidationRecord {
        status: decision.status,
        validator: validator.identity(),
        content_sha256: sha256,
        evidence: decision.evidence,
    };
    let mut candidate = runtime.persist_candidate(&media.bytes, automated, technically_valid)?;
    candidate.acquisition = Some(Box::new(media.evidence.clone()));
    Ok(BatchAttemptInput::Candidate { candidate })
}

fn reuse_existing(snapshot: &OwnerSnapshot, state: &mut KanjiBatch) -> Result<(), AssetError> {
    snapshot.checked()?;
    for item in state.items.clone() {
        if item.is_ready() || item.status.semantic_resolved() || !item.human_decisions.is_empty() {
            continue;
        }
        let Some(record) = snapshot.current(&item.identity).filter(|record| {
            record.lifecycle == LifecycleState::Verified
                && record.is_trusted_for(&state.policy.validator)
        }) else {
            continue;
        };
        state.mark_existing_ready(&item.identity, &record.sha256)?;
    }
    Ok(())
}

/// Snapshot integrity was checked once by the owner. Readiness uses indexed
/// exact identity/hash/current trust, never per-item full-manifest reads.
fn reconcile_owner_trust(
    snapshot: &OwnerSnapshot,
    state: &mut KanjiBatch,
    issues: &mut Vec<BatchIssue>,
) -> Result<(), AssetError> {
    for item in state.items.clone() {
        let current = snapshot.current(&item.identity);
        let owner_rejection = current
            .filter(|record| record.current_human_decision() == Some(HumanDecision::Reject))
            .map(|record| HumanBatchDecision {
                identity: record.identity.clone(),
                candidate_sha256: record.sha256.clone(),
                action: HumanBatchAction::Reject,
                reason: record
                    .human_attestation
                    .as_ref()
                    .expect("current human decision")
                    .reason
                    .chars()
                    .take(900)
                    .collect(),
            });
        let rejected_auto_candidate = item.status == BatchItemStatus::AutoVerified
            && owner_rejection.as_ref().is_some_and(|decision| {
                item.current_sha256.as_deref() == Some(decision.candidate_sha256.as_str())
            });
        if !item.is_ready() && !rejected_auto_candidate {
            continue;
        }
        if snapshot.error.is_none()
            && current.is_some_and(|record| {
                record.lifecycle == LifecycleState::Verified
                    && record.is_trusted_for(&state.policy.validator)
                    && item.published_sha256.as_deref() == Some(record.sha256.as_str())
            })
            && !rejected_auto_candidate
        {
            continue;
        }
        let error = if let Some(error) = &snapshot.error {
            copy_error(error)
        } else if let Some(record) = current {
            if record.sha256 != item.published_sha256.as_deref().unwrap_or("") {
                AssetError::new(
                    ErrorCode::IdentityConflict,
                    "saved ready SHA отличается от exact current owner bytes",
                )
            } else {
                AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "owner не подтверждает expected canonical trust",
                )
            }
        } else {
            AssetError::new(
                ErrorCode::MissingAssetFile,
                "ready identity отсутствует в checked owner snapshot",
            )
        };
        state.invalidate_owner_trust(
            &item.identity,
            format!("owner trust утрачен: {}", error.code.as_str()),
            owner_rejection,
        )?;
        issues.push(issue(&item.identity, &error));
    }
    Ok(())
}

fn publish_pending(
    store: &AssetStore,
    runtime: &mut BatchRuntime,
    state: &mut KanjiBatch,
    issues: &mut Vec<BatchIssue>,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    snapshot.checked()?;
    for item in state.items.clone() {
        if let Some(decision) = item.human_decisions.last()
            && decision.action == HumanBatchAction::Reject
            && !item.observed_owner_rejections.contains(decision)
        {
            let current = snapshot.current(&item.identity);
            let already_rejected = current.is_some_and(|record| {
                record.sha256 == decision.candidate_sha256
                    && record.current_human_decision() == Some(HumanDecision::Reject)
            });
            let may_apply = current.is_none_or(|record| record.sha256 == decision.candidate_sha256);
            if !already_rejected
                && may_apply
                && let Some(candidate) = candidate_by_hash(&item, &decision.candidate_sha256)
            {
                let bytes = runtime.read_candidate(candidate)?;
                if let Err(error) = apply_rejection(
                    store,
                    runtime,
                    &item.identity,
                    candidate,
                    &bytes,
                    &decision.reason,
                    snapshot,
                ) {
                    issues.push(issue(&item.identity, &error));
                }
            }
        }
        if item.is_ready() {
            continue;
        }
        let result = match item.status {
            BatchItemStatus::AutoVerified => {
                publish_automated(store, runtime, state, &item.identity, snapshot)
            }
            BatchItemStatus::HumanVerified => publish_human(store, runtime, &item, snapshot),
            _ => continue,
        };
        match result {
            Ok((sha256, source)) => {
                state.mark_published_ready(&item.identity, &sha256, source)?;
                runtime.save(state)?;
            }
            Err(error) if error.code.exit_code() != 4 => issues.push(issue(&item.identity, &error)),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn publish_automated(
    store: &AssetStore,
    runtime: &BatchRuntime,
    state: &KanjiBatch,
    identity: &AssetIdentity,
    snapshot: &mut OwnerSnapshot,
) -> Result<(String, BatchTrustSource), AssetError> {
    let item = state
        .items
        .iter()
        .find(|item| &item.identity == identity)
        .expect("batch identity");
    let candidate = current_candidate(item)?;
    let bytes = runtime.read_candidate(candidate)?;
    let request = verified_request(snapshot, identity, bytes, candidate)?;
    let (outcome, source) = if candidate.automated.status == SemanticStatus::Verified {
        (
            store.ingest_verified(request, &KanjiImageValidator::new())?,
            BatchTrustSource::Automated,
        )
    } else {
        let resolution = state
            .aggregate_decision(identity)?
            .ok_or_else(|| invalid("auto candidate не имеет положительного aggregate evidence"))?;
        (
            store.ingest_verified(request, &AggregateValidator { resolution })?,
            BatchTrustSource::Aggregate,
        )
    };
    let record = outcome
        .asset
        .ok_or_else(|| invalid("owner не опубликовал batch candidate"))?;
    if outcome.status != SemanticStatus::Verified
        || outcome.sha256 != candidate.sha256
        || record.identity != *identity
        || record.sha256 != candidate.sha256
        || record.lifecycle != LifecycleState::Verified
        || !record.is_trusted_for(&state.policy.validator)
    {
        return Err(invalid(
            "exact owner commit не подтверждает batch candidate trust",
        ));
    }
    // Owner just checked/staged/committed these exact bytes under CAS. A final
    // boundary snapshot handles later concurrent edits; no full reread per item.
    snapshot.committed(record);
    Ok((candidate.sha256.clone(), source))
}

fn publish_human(
    store: &AssetStore,
    runtime: &BatchRuntime,
    item: &crate::batch::BatchItem,
    snapshot: &mut OwnerSnapshot,
) -> Result<(String, BatchTrustSource), AssetError> {
    let decision = item
        .human_decisions
        .last()
        .ok_or_else(|| invalid("human candidate не имеет decision"))?;
    let candidate = current_candidate(item)?;
    if decision.action != HumanBatchAction::Confirm || decision.candidate_sha256 != candidate.sha256
    {
        return Err(invalid("human intent относится к stale candidate"));
    }
    let bytes = runtime.read_candidate(candidate)?;
    materialize_candidate(
        store,
        runtime,
        &item.identity,
        candidate,
        &bytes,
        true,
        snapshot,
    )?;
    let outcome = store.attest(HumanAttestationRequest {
        identity: item.identity.clone(),
        expected_sha256: candidate.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: decision.reason.clone(),
    })?;
    if outcome.asset.identity != item.identity
        || outcome.asset.sha256 != candidate.sha256
        || outcome.asset.current_human_decision() != Some(HumanDecision::Approve)
        || outcome.asset.lifecycle != LifecycleState::Verified
        || !outcome
            .asset
            .is_trusted_for(&KanjiImageValidator::validator_identity())
    {
        return Err(invalid(
            "exact human owner commit не подтверждает candidate trust",
        ));
    }
    snapshot.committed(outcome.asset);
    Ok((candidate.sha256.clone(), BatchTrustSource::Human))
}

fn decide_exact(
    store: &AssetStore,
    batch_id: &str,
    decision: HumanBatchDecision,
) -> Result<(KanjiBatch, Vec<BatchIssue>), AssetError> {
    let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
    let mut state = load_required(&mut runtime)?;
    // Решение публикует соседние items текущим validator'ом, поэтому pinned
    // validator проверяется до любой мутации, а не после неё.
    check_current_validator(&state)?;
    let mut snapshot = StoreSnapshotReader.capture(store);
    snapshot.checked()?;
    let identity = decision.identity.clone();
    let item = state
        .items
        .iter()
        .find(|item| item.identity == identity)
        .ok_or_else(|| invalid("identity не входит в batch"))?;
    let candidate = if item.status == BatchItemStatus::ExistingVerified {
        // A reused canonical item has no runtime attempt. Pin owner bytes and
        // append local evidence so explicit rejection can still be audited.
        let verified = AssetStore::read_verified(
            store.root(),
            std::slice::from_ref(&identity),
            &state.policy.validator,
        )?;
        let record = &verified[0].record;
        if record.sha256 != decision.candidate_sha256 {
            return Err(invalid(
                "existing canonical candidate изменился после review",
            ));
        }
        // Достижимо только через повреждённый извне manifest: все writer'ы
        // (ingest/ingest_verified/validate_exact) записывают automated evidence
        // для Verified записи, а schema v3 не допускает human attestation.
        let automated = record
            .validation
            .clone()
            .ok_or_else(|| invalid("existing canonical candidate не имеет automated evidence"))?;
        runtime.persist_candidate(&verified[0].bytes, automated, true)?
    } else {
        current_candidate(item)?.clone()
    };
    if state
        .items
        .iter()
        .any(|item| item.identity == identity && item.status == BatchItemStatus::ExistingVerified)
    {
        state.retain_existing_candidate(&identity, candidate.clone())?;
    }
    let bytes = runtime.read_candidate(&candidate)?;
    // No human action is inferred in Rust. This only validates an explicit action.
    state.decide(decision.clone(), &bytes)?;
    runtime.save(&state)?;
    let mut issues = Vec::new();
    if decision.action == HumanBatchAction::Confirm {
        publish_pending(store, &mut runtime, &mut state, &mut issues, &mut snapshot)?;
    } else if decision.action == HumanBatchAction::Reject {
        // Retain both semantic evidence and the human rejection in generic owner.
        if let Err(error) = apply_rejection(
            store,
            &runtime,
            &identity,
            &candidate,
            &bytes,
            &decision.reason,
            &mut snapshot,
        ) {
            issues.push(issue(&identity, &error));
        }
    }
    let snapshot = StoreSnapshotReader.capture(store);
    reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
    runtime.save(&state)?;
    Ok((state, issues))
}

fn materialize_candidate(
    store: &AssetStore,
    runtime: &BatchRuntime,
    identity: &AssetIdentity,
    candidate: &BatchCandidate,
    bytes: &[u8],
    attach_automated: bool,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    snapshot.checked()?;
    if sha256_hex(bytes) != candidate.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "materialization bytes/hash mismatch",
        ));
    }
    let current = snapshot.current(identity).cloned();
    if current
        .as_ref()
        .is_some_and(|record| record.sha256 == candidate.sha256 && record.validation.is_some())
    {
        return Ok(());
    }
    let path = store
        .root()
        .join(".runtime/batches")
        .join(runtime.batch_id())
        .join(&candidate.storage_path);
    let outcome = store.ingest(IngestRequest {
        identity: identity.clone(),
        source_path: path,
        expected_source_sha256: Some(candidate.sha256.clone()),
        domain_metadata: Some(candidate_metadata(identity, candidate)),
        replace_expected_sha256: current.map(|record| record.sha256),
    })?;
    snapshot.committed(outcome.asset);
    if attach_automated {
        validate_exact_snapshot(store, identity, &candidate.sha256, snapshot)?;
    }
    Ok(())
}

fn validate_exact_snapshot(
    store: &AssetStore,
    identity: &AssetIdentity,
    sha256: &str,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    let report = store.validate_exact(identity, sha256, &KanjiImageValidator::new())?;
    if !report.blockers.is_empty() {
        return Err(AssetError::new(
            ErrorCode::ValidatorFailure,
            "owner exact automated validation завершилась техническим отказом",
        ));
    }
    let attempt = report
        .attempts
        .iter()
        .find(|attempt| &attempt.identity == identity && attempt.content_sha256 == sha256)
        .ok_or_else(|| invalid("owner exact validation не вернула ожидаемый outcome"))?;
    let status = attempt
        .status
        .ok_or_else(|| invalid("owner exact validation отсутствует semantic status"))?;
    let mut record = snapshot
        .current(identity)
        .filter(|record| record.sha256 == sha256)
        .cloned()
        .ok_or_else(|| invalid("snapshot не содержит exact validated record"))?;
    record.validation = Some(ValidationRecord {
        status,
        validator: report.validator,
        content_sha256: sha256.into(),
        evidence: attempt.evidence.clone(),
    });
    record.lifecycle = attempt.to_state;
    snapshot.committed(record);
    Ok(())
}

fn apply_rejection(
    store: &AssetStore,
    runtime: &BatchRuntime,
    identity: &AssetIdentity,
    candidate: &BatchCandidate,
    bytes: &[u8],
    reason: &str,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    snapshot.checked()?;
    if sha256_hex(bytes) != candidate.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "rejection bytes/hash mismatch",
        ));
    }
    let current = snapshot.current(identity).cloned();
    if current
        .as_ref()
        .is_some_and(|record| record.sha256 != candidate.sha256)
    {
        return Ok(());
    }
    let attach_automated = current
        .as_ref()
        .is_none_or(|record| record.validation.is_none());
    if current.is_none() {
        // CAS=None still protects against an intervening distinct owner B.
        let outcome = store.ingest(IngestRequest {
            identity: identity.clone(),
            source_path: store
                .root()
                .join(".runtime/batches")
                .join(runtime.batch_id())
                .join(&candidate.storage_path),
            expected_source_sha256: Some(candidate.sha256.clone()),
            domain_metadata: Some(candidate_metadata(identity, candidate)),
            replace_expected_sha256: None,
        })?;
        snapshot.committed(outcome.asset);
    }
    let outcome = store.attest(HumanAttestationRequest {
        identity: identity.clone(),
        expected_sha256: candidate.sha256.clone(),
        decision: HumanDecision::Reject,
        reason: reason.into(),
    })?;
    snapshot.committed(outcome.asset);
    if attach_automated {
        validate_exact_snapshot(store, identity, &candidate.sha256, snapshot)?;
    }
    Ok(())
}

fn verified_request(
    snapshot: &OwnerSnapshot,
    identity: &AssetIdentity,
    bytes: Vec<u8>,
    candidate: &BatchCandidate,
) -> Result<VerifiedIngestRequest, AssetError> {
    snapshot.checked()?;
    Ok(VerifiedIngestRequest {
        identity: identity.clone(),
        bytes,
        provenance: candidate_provenance(candidate),
        domain_metadata: Some(candidate_metadata(identity, candidate)),
        replace_expected_sha256: snapshot
            .current(identity)
            .map(|record| record.sha256.clone()),
    })
}

fn candidate_provenance(candidate: &BatchCandidate) -> Provenance {
    Provenance {
        source_kind: candidate
            .acquisition
            .as_ref()
            .map_or("kanji_batch_runtime", |evidence| evidence.provider.as_str())
            .into(),
        source_name: match candidate.format {
            DetectedFormat::Gif => "batch-candidate.gif",
            _ => "batch-candidate.png",
        }
        .into(),
    }
}

fn candidate_metadata(identity: &AssetIdentity, candidate: &BatchCandidate) -> serde_json::Value {
    let mut metadata = KanjiCharacter(identity.key.clone()).metadata();
    if let Some(evidence) = &candidate.acquisition {
        metadata["yarxi"] = serde_json::to_value(evidence).expect("typed acquisition evidence");
    }
    metadata
}

fn candidate_asset_record(
    identity: &AssetIdentity,
    bytes: &[u8],
    acquisition: Option<&crate::yarxi::AcquisitionEvidence>,
) -> AssetRecord {
    AssetRecord {
        identity: identity.clone(),
        storage_path: "candidate".into(),
        sha256: sha256_hex(bytes),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::from_signature(bytes),
        provenance: Provenance {
            source_kind: acquisition
                .map_or("kanji_batch_runtime", |evidence| evidence.provider.as_str())
                .into(),
            source_name: "candidate".into(),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: Some(KanjiCharacter(identity.key.clone()).metadata()),
    }
}

struct AggregateValidator {
    resolution: AggregateResolution,
}

impl SemanticValidator for AggregateValidator {
    fn identity(&self) -> ValidatorIdentity {
        self.resolution.validator.clone()
    }

    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut actual = Vec::new();
        bytes
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut actual)
            .map_err(|error| ValidatorFailure::new("aggregate_read_failure", error.to_string()))?;
        if asset.identity != self.resolution.identity
            || asset.sha256 != self.resolution.candidate.sha256
            || sha256_hex(&actual) != self.resolution.candidate.sha256
            || self.identity() != KanjiImageValidator::validator_identity()
        {
            return Err(ValidatorFailure::new(
                "aggregate_candidate_mismatch",
                "aggregate decision не соответствует exact identity/hash/current validator",
            ));
        }
        validate_image_decode(&actual)
            .map_err(|error| ValidatorFailure::new("aggregate_decode_failure", error.message))?;
        let selected = KanjiImageValidator::new().validate(asset, &mut Cursor::new(actual))?;
        if selected.status == SemanticStatus::Corrupt {
            return Err(ValidatorFailure::new(
                "aggregate_corrupt_candidate",
                "CORRUPT candidate не может получить aggregate trust",
            ));
        }
        if selected.status == SemanticStatus::Rejected {
            return Err(ValidatorFailure::new(
                "aggregate_rejected_candidate",
                "REJECTED candidate не может получить aggregate trust",
            ));
        }
        let mut decision = self.resolution.decision.clone();
        // Keep single-candidate automated evidence beside explicit aggregate policy.
        decision.evidence.extend(selected.evidence);
        Ok(decision)
    }
}

fn candidate_by_hash<'a>(
    item: &'a crate::batch::BatchItem,
    hash: &str,
) -> Option<&'a BatchCandidate> {
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

fn current_candidate(item: &crate::batch::BatchItem) -> Result<&BatchCandidate, AssetError> {
    let hash = item
        .current_sha256
        .as_deref()
        .ok_or_else(|| invalid("item не имеет current candidate"))?;
    candidate_by_hash(item, hash).ok_or_else(|| invalid("exact candidate evidence отсутствует"))
}

fn batch_response(
    operation: &str,
    store: StoreSummary,
    batch: KanjiBatch,
    changed: bool,
    issues: Vec<BatchIssue>,
    artifact: Option<String>,
    outcome: &'static str,
) -> BatchResponse {
    let ready = |status| {
        batch
            .items
            .iter()
            .filter(|item| item.status == status && item.is_ready())
            .count()
    };
    let counts = BatchCounts {
        requested: batch.items.len(),
        effective_verified: batch.items.iter().filter(|item| item.is_ready()).count(),
        auto_verified: ready(BatchItemStatus::AutoVerified),
        human_verified: ready(BatchItemStatus::HumanVerified),
        existing_verified: ready(BatchItemStatus::ExistingVerified),
        awaiting_human: batch.review_queue().len(),
        scheduled_for_reacquire: batch
            .items
            .iter()
            .filter(|item| item.status == BatchItemStatus::Reacquire)
            .count(),
        pending_publication: batch
            .items
            .iter()
            .filter(|item| item.status.semantic_resolved() && !item.is_ready())
            .count(),
        unresolved: batch
            .items
            .iter()
            .filter(|item| !item.status.semantic_resolved())
            .count(),
    };
    let items = batch
        .items
        .iter()
        .map(|item| BatchItemOutcome {
            identity: item.identity.clone(),
            state: item.status,
            effective_verified: item.is_ready(),
            candidate_sha256: item.current_sha256.clone(),
            published_sha256: item.published_sha256.clone(),
            publication_source: item.publication_source,
            acquisition_attempts: item.attempts.len(),
            generation: item.generation,
            distinct_valid_hashes: item.aggregate.distinct_valid_hashes.len(),
            item_outcome: if item.is_ready() {
                "effective_verified"
            } else if item.status.semantic_resolved() {
                "pending_publication"
            } else if item.status == BatchItemStatus::AwaitingHuman {
                "awaiting_human"
            } else if item.status == BatchItemStatus::Reacquire {
                "reacquire_scheduled"
            } else {
                "unresolved"
            },
        })
        .collect();
    let blockers = issues
        .iter()
        .map(|issue| format!("{}:{}", issue.identity, issue.code))
        .collect();
    BatchResponse {
        schema_version: 1,
        operation: operation.into(),
        store,
        batch_id: batch.batch_id.clone(),
        outcome,
        changed,
        counts,
        items,
        issues,
        blockers,
        review_artifact: artifact,
        batch,
    }
}

fn generated_batch_id() -> Result<String, AssetError> {
    let mut entropy = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut entropy))
        .map_err(|error| AssetError::io("не удалось получить entropy для batch identity", error))?;
    Ok(format!("kanji-{}", &sha256_hex(entropy)[..32]))
}

fn check_current_validator(state: &KanjiBatch) -> Result<(), AssetError> {
    if state.policy.validator != KanjiImageValidator::validator_identity() {
        return Err(AssetError::new(
            ErrorCode::InvalidValidatorIdentity,
            "batch pinned validator отличается от production; требуется новый batch",
        ));
    }
    Ok(())
}
fn parse_character(value: &str) -> Result<KanjiCharacter, AssetError> {
    value
        .parse()
        .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))
}
fn validate_reason(reason: &str) -> Result<(), AssetError> {
    if reason.trim().is_empty() || reason.len() > 4096 {
        Err(invalid(
            "reason должно быть непустым и не длиннее 4096 bytes",
        ))
    } else {
        Ok(())
    }
}
fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}
fn issue(identity: &AssetIdentity, error: &AssetError) -> BatchIssue {
    BatchIssue {
        identity: identity.clone(),
        code: error.code.as_str().into(),
        message: error.message.chars().take(1000).collect(),
    }
}
fn failure(code: &str, message: &str) -> BatchAttemptInput {
    BatchAttemptInput::Failed {
        code: code.chars().take(256).collect(),
        message: message.chars().take(1000).collect(),
    }
}

#[cfg(test)]
#[path = "batch_cli_tests.rs"]
mod tests;

#[cfg(test)]
mod snapshot_cost_tests {
    use super::*;

    struct CountedSnapshots {
        records: Vec<AssetRecord>,
        captures: usize,
    }
    impl OwnerSnapshotReader for CountedSnapshots {
        fn capture(&mut self, _store: &AssetStore) -> OwnerSnapshot {
            self.captures += 1;
            OwnerSnapshot::from_result(Ok(self.records.clone()))
        }
    }

    /// Exercise the production command and round paths, with the checked owner
    /// snapshot boundary counted explicitly. The physical corpus is empty, so a
    /// regression to per-item read_verified cannot silently pass this fixture.
    #[test]
    fn thousand_ready_identities_use_bounded_full_owner_snapshots() {
        let directory = std::env::temp_dir().join(generated_batch_id().unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        let store = AssetStore::open(StoreOptions::new(directory.join("corpus"))).unwrap();
        let summary = StoreSummary {
            path: store.root().display().to_string(),
            store_id: Some(store.store_id().into()),
        };
        let characters: Vec<_> = (0x4e00..0x4e00 + 1000)
            .map(|code| char::from_u32(code).unwrap().to_string())
            .collect();
        let records = characters
            .iter()
            .map(|character| {
                let identity = parse_character(character).unwrap().identity();
                let sha256 = sha256_hex(character.as_bytes());
                AssetRecord {
                    identity,
                    storage_path: format!("assets/{sha256}.png"),
                    sha256: sha256.clone(),
                    byte_length: 1,
                    format: DetectedFormat::Png,
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "synthetic.png".into(),
                    },
                    lifecycle: LifecycleState::Verified,
                    validation: Some(ValidationRecord {
                        status: SemanticStatus::Verified,
                        validator: KanjiImageValidator::validator_identity(),
                        content_sha256: sha256,
                        evidence: Vec::new(),
                    }),
                    human_attestation: None,
                    domain_metadata: None,
                }
            })
            .collect();
        let mut reader = CountedSnapshots {
            records,
            captures: 0,
        };
        let start = BatchCommand::Start {
            batch_id: Some("thousand-ready".into()),
            characters,
        };
        let (response, _) =
            execute_command_with_snapshots(&store, summary.clone(), &start, false, &mut reader)
                .unwrap();
        assert_eq!(response.counts.effective_verified, 1000);
        assert_eq!(reader.captures, 1, "start uses one full snapshot");
        for command in [
            start,
            BatchCommand::Status {
                batch_id: "thousand-ready".into(),
            },
            BatchCommand::Review {
                batch_id: "thousand-ready".into(),
            },
        ] {
            reader.captures = 0;
            let (response, _) = execute_command_with_snapshots(
                &store,
                summary.clone(),
                &command,
                false,
                &mut reader,
            )
            .unwrap();
            assert_eq!(response.counts.effective_verified, 1000);
            assert_eq!(
                reader.captures, 1,
                "idempotent start/status/review uses one full snapshot"
            );
        }
        reader.captures = 0;
        let (state, changed, issues) = run_batch_with_snapshots(
            &store,
            "thousand-ready",
            5,
            |_| panic!("trusted corpus must not trigger acquisition"),
            &mut reader,
        )
        .unwrap();
        assert_eq!(
            state.items.iter().filter(|item| item.is_ready()).count(),
            1000
        );
        assert!(!changed);
        assert!(issues.is_empty());
        assert_eq!(
            reader.captures, 2,
            "ready run uses initial and final snapshots"
        );
        // The acquisition frontier also pays one boundary capture per round,
        // independent of the number of item results saved inside that round.
        reader.records.clear();
        reader.captures = 0;
        execute_command_with_snapshots(
            &store,
            summary,
            &BatchCommand::Start {
                batch_id: Some("unresolved-frontier".into()),
                characters: (0x4e00..0x4e00 + 12)
                    .map(|code| char::from_u32(code).unwrap().to_string())
                    .collect(),
            },
            false,
            &mut reader,
        )
        .unwrap();
        assert_eq!(reader.captures, 1);
        reader.captures = 0;
        let (state, _, _) = run_batch_with_snapshots(
            &store,
            "unresolved-frontier",
            1,
            |characters| {
                Ok(characters
                    .iter()
                    .map(|_| Err("synthetic failure".into()))
                    .collect())
            },
            &mut reader,
        )
        .unwrap();
        assert_eq!(state.items.len(), 12);
        assert!(state.items.iter().all(|item| item.attempts.len() == 1));
        assert_eq!(
            reader.captures, 3,
            "initial + one frontier boundary + final"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod candidate_cost_tests {
    use super::*;
    use crate::batch::CANDIDATE_FILE_READS;

    /// Round-trip цикл сохраняет состояние на каждый item, поэтому повторная
    /// проверка всех candidate-файлов внутри цикла давала квадратичный обход.
    /// Фикстура держит по одному реальному candidate на identity и падает, если
    /// стоимость чтений вернётся к O(N^2).
    #[test]
    fn frontier_round_reads_each_candidate_a_bounded_number_of_times() {
        const ITEMS: u32 = 200;
        let directory = std::env::temp_dir().join(generated_batch_id().unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        let store = AssetStore::open(StoreOptions::new(directory.join("corpus"))).unwrap();
        let summary = StoreSummary {
            path: store.root().display().to_string(),
            store_id: Some(store.store_id().into()),
        };
        let characters: Vec<_> = (0x4e00..0x4e00 + ITEMS)
            .map(|code| char::from_u32(code).unwrap().to_string())
            .collect();
        execute_command(
            &store,
            summary.clone(),
            &BatchCommand::Start {
                batch_id: Some("candidate-cost".into()),
                characters: characters.clone(),
            },
            false,
        )
        .unwrap();
        let mut runtime = BatchRuntime::open(store.root(), "candidate-cost").unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        for (index, character) in characters.iter().enumerate() {
            let identity = parse_character(character).unwrap().identity();
            let bytes = synthetic_png(index as u8);
            let sha256 = sha256_hex(&bytes);
            let record = ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: KanjiImageValidator::validator_identity(),
                content_sha256: sha256.clone(),
                evidence: vec![ValidationEvidence {
                    kind: "pixel_reference_comparison".into(),
                    summary: "synthetic independent metrics".into(),
                    details: Some(serde_json::json!({
                        "expected_distance": 0.5,
                        "nearest_margin": -0.5,
                        "nearest_other": "漠",
                    })),
                }],
            };
            let candidate = runtime.persist_candidate(&bytes, record, true).unwrap();
            state
                .record_attempt(&identity, BatchAttemptInput::Candidate { candidate })
                .unwrap();
        }
        runtime.save(&state).unwrap();
        drop(runtime);
        CANDIDATE_FILE_READS.with(|reads| reads.set(0));
        let (state, _, _) = run_batch(&store, "candidate-cost", 1, |characters| {
            Ok(characters
                .iter()
                .map(|_| Err("synthetic failure".into()))
                .collect())
        })
        .unwrap();
        let reads = CANDIDATE_FILE_READS.with(std::cell::Cell::get);
        assert_eq!(state.items.len(), ITEMS as usize);
        assert!(state.items.iter().all(|item| item.attempts.len() == 2));
        // Линейный обход укладывается в несколько чтений на identity; прежний
        // квадратичный путь давал ITEMS^2 = 40000.
        assert!(
            reads <= 8 * ITEMS as usize,
            "candidate-файлы перечитаны {reads} раз при {ITEMS} identity"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn synthetic_png(index: u8) -> Vec<u8> {
        let mut image = image::RgbaImage::new(8, 8);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([index, x as u8, 0, 255]);
        }
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }
}
