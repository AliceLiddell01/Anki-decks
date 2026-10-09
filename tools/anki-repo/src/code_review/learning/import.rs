//! Проверенный идемпотентный импорт завершённых ревью в локальную историю.
//!
//! Доверенный путь использует существующие валидаторы пакета, очереди и
//! семантического разбора и по умолчанию требует полной AST-аутентичности
//! очереди. Ослабленный импорт возможен только по явному выбору вызывающей
//! стороны и маркируется карантином, исключающим запись из обучения.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{DomainError, ErrorCode};

use super::model::{
    CaseLinkKind, ExecutionEvidenceSummary, HistoryGeneration, ImportInputs, ImportRecord,
    ImportStatus, ObservationCounts, ReviewUnitKind, ReviewedOutcome, TrustLevel,
};
use super::store::LearningStore;
use super::{ROOT_WORKSPACE_VARIANT, SNAPSHOT_WORKSPACE_VARIANT_PREFIX};

/// Загруженные и проверенные документы одного импорта.
pub struct LoadedReview {
    /// Разобранный пакет свидетельств.
    pub pack: crate::code_review::model::ReviewPack,
    /// SHA-256 точных байтов `review.json`; не результат повторной сериализации.
    pub review_pack_sha256: String,
    /// Проверенная структурная очередь.
    pub queue: crate::code_review::review_queue::ReviewQueue,
    /// SHA-256 точных байтов очереди.
    pub queue_sha256: String,
    /// Проверенный семантический разбор вместе с digest его байтов.
    pub triage: Option<(crate::code_review::semantic_triage::SemanticTriage, String)>,
    /// Доверие к загруженному входу.
    pub trust: TrustLevel,
    /// Ограничения доверия к загруженным документам.
    pub limitations: Vec<String>,
    /// Дополнительная identity входов, если вызывающая сторона её передала.
    pub inputs_hint: ImportInputsHint,
}

/// Дополнительная identity входов, которую можно передать вместе с загрузкой.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportInputsHint {
    /// SHA-256 точных байтов `result.json` завершённого задания.
    pub execution_result_sha256: Option<String>,
    /// Версия схемы результата изолированной проверки.
    pub execution_schema_version: Option<u32>,
    /// Проверяемая сводка статуса, выхода и полноты результата.
    pub execution_evidence: Option<ExecutionEvidenceSummary>,
    /// Вариант рабочего пространства, объявленный результатом изоляции.
    pub workspace_variant: Option<String>,
}

impl std::fmt::Debug for LoadedReview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedReview")
            .field("repository_id", &self.pack.target.repository_id)
            .field("head_sha", &self.pack.target.head_sha)
            .field("trust", &self.trust)
            .field("has_triage", &self.triage.is_some())
            .finish_non_exhaustive()
    }
}

/// Вход импорта одного ревью.
#[derive(Debug, Clone, Default)]
pub struct ImportRequest {
    /// Явный вариант источника: `root` либо `snapshot-<32 hex>`.
    pub workspace_variant: String,
    /// Идентификатор каталога ревью, если он известен вызывающей стороне.
    pub workspace_label: Option<String>,
}

/// Импортирует проверенный набор свидетельств в локальную историю.
///
/// Повторный импорт того же exact source не удваивает ни случаи, ни статистику:
/// возвращается идентификатор существующей записи и явный статус
/// [`ImportStatus::NoopExisting`]. Другие байты при той же source identity
/// оформляются как аудируемая ревизия, прежняя запись сохраняется.
pub fn import_history(
    store: &LearningStore,
    loaded: &LoadedReview,
    request: &ImportRequest,
) -> Result<ImportRecord, DomainError> {
    let variant = normalize_variant(&request.workspace_variant)?;
    if let Some(declared) = loaded.inputs_hint.workspace_variant.as_deref()
        && declared != variant
    {
        return Err(DomainError::new(
            ErrorCode::BaselineMismatch,
            "result.json изолированной проверки относится к другому варианту рабочего пространства",
        ));
    }
    let inputs = inputs_of(loaded)?;
    let outcome = outcome_of(loaded);
    let identity_key = identity_key(loaded, &variant);
    let review_id = review_id(loaded, &variant);
    let observations = observation_counts(loaded);

    // Сначала ищем точный повтор вне транзакции записи: чтение дешевле.
    let repeated =
        store.read(|read| existing_exact(read, &review_id, &identity_key, &inputs, loaded))?;
    if let Some(record) = repeated {
        return Ok(record);
    }
    store.write(|write| {
        let existing = existing_exact(&write.as_tx(), &review_id, &identity_key, &inputs, loaded)?;
        if let Some(record) = existing {
            return Ok(record);
        }
        let previous = previous_head(write, &identity_key, &review_id)?;
        let revision = next_revision(write)?;
        let mut limitations = loaded.limitations.clone();
        if let Some(previous_id) = previous.as_deref() {
            limitations.push(format!(
                "Запись является аудируемой ревизией ранее импортированного ревью {previous_id}: прежняя запись сохранена и помечена как вытесненная."
            ));
            write.execute(
                "UPDATE learning_import SET superseded_by = ?1 WHERE review_id = ?2",
                params![review_id, previous_id],
            )?;
        }
        let record = ImportRecord {
            review_id: review_id.clone(),
            repository_id: loaded.pack.target.repository_id.clone(),
            base_sha: loaded.pack.target.base_sha.clone(),
            head_sha: loaded.pack.target.head_sha.clone(),
            merge_base_sha: loaded.pack.target.merge_base_sha.clone(),
            workspace_variant: variant.clone(),
            workspace_label: request.workspace_label.clone(),
            inputs: inputs.clone(),
            trust: loaded.trust,
            outcome,
            limitations,
            revision_of: previous,
            superseded_by: None,
            revision,
            imported_at: timestamp(),
            observations: observations.clone(),
        };
        insert_import(write, &record, &identity_key)?;
        write_units(write, loaded, &review_id)?;
        write_candidates(write, loaded, &review_id)?;
        write_decisions(write, loaded, &review_id)?;
        write_findings(write, loaded, &review_id)?;
        write_search(write, loaded, &review_id)?;
        if loaded.trust.participates_in_learning() {
            write_case_links(write, loaded, &review_id)?;
        }
        Ok(record)
    })
}

/// Импортирует проверенную историю и возвращает различимый итог операции.
///
/// Отличие от [`import_history`] только в возвращаемом итоге: запись,
/// идемпотентность и аудируемые ревизии определяются ровно той же логикой, без
/// второй копии правил. Итог нужен вызывающей стороне, чтобы отличить новый
/// импорт от точного повтора, от аудируемой ревизии и от карантина.
pub fn import_with_outcome(
    store: &LearningStore,
    loaded: &LoadedReview,
    request: &ImportRequest,
) -> Result<ImportOutcome, DomainError> {
    let variant = normalize_variant(&request.workspace_variant)?;
    let inputs = inputs_of(loaded)?;
    let identity_key = identity_key(loaded, &variant);
    let review_id = review_id(loaded, &variant);
    let repeated =
        store.read(|read| existing_exact(read, &review_id, &identity_key, &inputs, loaded))?;
    let record = import_history(store, loaded, request)?;
    let status = if repeated.is_some() {
        ImportStatus::NoopExisting
    } else if !record.trust.participates_in_learning() {
        ImportStatus::Quarantined
    } else if record.revision_of.is_some() {
        ImportStatus::RevisionCreated
    } else {
        ImportStatus::Imported
    };
    Ok(ImportOutcome::new(status, record))
}

/// Возвращает действующую генерацию накопленной истории.
pub fn generation(store: &LearningStore) -> Result<HistoryGeneration, DomainError> {
    store.read(generation_of)
}

fn generation_of(read: &super::store::LearningRead<'_>) -> Result<HistoryGeneration, DomainError> {
    let (revision, trusted, quarantined): (i64, i64, i64) = read
        .transaction()
        .query_row(
            "SELECT COALESCE(MAX(revision), 0),
                    COALESCE(SUM(CASE WHEN trust = 'ast_authenticated' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN trust = 'structure_only_quarantine' THEN 1 ELSE 0 END), 0)
             FROM learning_import",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать генерацию"))?;
    let trusted_units: i64 = read
        .transaction()
        .query_row(
            "SELECT COUNT(*) FROM learning_unit AS u
             WHERE EXISTS (
                 SELECT 1 FROM learning_import AS i
                 WHERE i.review_id = u.review_id AND i.trust = 'ast_authenticated'
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать генерацию"))?;
    Ok(HistoryGeneration {
        revision: u64::try_from(revision.max(0)).unwrap_or(u64::MAX),
        trusted_reviews: usize::try_from(trusted.max(0)).unwrap_or(usize::MAX),
        quarantined_reviews: usize::try_from(quarantined.max(0)).unwrap_or(usize::MAX),
        trusted_units: usize::try_from(trusted_units.max(0)).unwrap_or(usize::MAX),
    })
}

/// Перечисляет записи истории в порядке ревизии.
pub fn list_imports(
    store: &LearningStore,
    include_quarantine: bool,
    limit: usize,
) -> Result<Vec<ImportRecord>, DomainError> {
    store.read(|read| {
        let mut statement = read
            .transaction()
            .prepare(
                "SELECT review_id, repository_id, base_sha, head_sha, merge_base_sha,
                        workspace_variant, workspace_label, review_pack_sha256, queue_sha256,
                        triage_sha256, execution_result_sha256,
                        review_schema_version, queue_schema_version, triage_schema_version,
                        execution_schema_version, execution_evidence_json, analyzer_digest, classifier_digest,
                        trust, outcome, limitations_json, revision_of, superseded_by,
                        revision, imported_at, observations_json
                 FROM learning_import
                 WHERE (?1 = 1 OR trust = 'ast_authenticated')
                 ORDER BY revision ASC, review_id ASC
                 LIMIT ?2",
            )
            .map_err(|error| super::store::map_error(&error, "не удалось прочитать историю"))?;
        let rows = statement
            .query_map(
                params![i64::from(include_quarantine), limit as i64],
                record_from_row,
            )
            .map_err(|error| super::store::map_error(&error, "не удалось прочитать историю"))?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|error| {
                super::store::map_error(&error, "не удалось прочитать историю")
            })?);
        }
        Ok(records)
    })
}

/// Возвращает одну запись истории по идентификатору.
pub fn show_import(store: &LearningStore, review_id: &str) -> Result<ImportRecord, DomainError> {
    store.read(|read| {
        read.transaction()
            .query_row(
                "SELECT review_id, repository_id, base_sha, head_sha, merge_base_sha,
                        workspace_variant, workspace_label, review_pack_sha256, queue_sha256,
                        triage_sha256, execution_result_sha256,
                        review_schema_version, queue_schema_version, triage_schema_version,
                        execution_schema_version, execution_evidence_json, analyzer_digest, classifier_digest,
                        trust, outcome, limitations_json, revision_of, superseded_by,
                        revision, imported_at, observations_json
                 FROM learning_import WHERE review_id = ?1",
                params![review_id],
                record_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => DomainError::new(
                    ErrorCode::NotFound,
                    format!("Запись истории learning не найдена: {review_id}"),
                ),
                other => super::store::map_error(&other, "не удалось прочитать запись истории"),
            })
    })
}

/// Уровень доверия по сохранённой метке.
fn trust_level_of(raw: &str) -> TrustLevel {
    if raw == TrustLevel::StructureOnlyQuarantine.as_str() {
        TrustLevel::StructureOnlyQuarantine
    } else {
        TrustLevel::AstAuthenticated
    }
}

/// Полнота рассмотрения по сохранённой метке.
fn reviewed_outcome_of(raw: &str) -> ReviewedOutcome {
    match raw {
        "fully_reviewed" => ReviewedOutcome::FullyReviewed,
        "no_triage" => ReviewedOutcome::NoTriage,
        "quarantined" => ReviewedOutcome::Quarantined,
        _ => ReviewedOutcome::PartiallyReviewed,
    }
}

/// Читает необязательную строку из транзакции записи.
pub(super) fn write_optional_row<T>(
    write: &super::store::LearningWrite<'_>,
    sql: &str,
    parameters: impl rusqlite::Params,
    mapper: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Option<T>, DomainError> {
    write
        .transaction()
        .query_row(sql, parameters, mapper)
        .optional()
        .map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать строку базы learning")
        })
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRecord> {
    let observations_json: String = row.get(25)?;
    let limitations_json: String = row.get(20)?;
    let execution_evidence_json: Option<String> = row.get(15)?;
    let trust: String = row.get(18)?;
    let outcome: String = row.get(19)?;
    Ok(ImportRecord {
        review_id: row.get(0)?,
        repository_id: row.get(1)?,
        base_sha: row.get(2)?,
        head_sha: row.get(3)?,
        merge_base_sha: row.get(4)?,
        workspace_variant: row.get(5)?,
        workspace_label: row.get(6)?,
        inputs: ImportInputs {
            review_pack_sha256: row.get(7)?,
            queue_sha256: row.get(8)?,
            triage_sha256: row.get(9)?,
            execution_result_sha256: row.get(10)?,
            review_schema_version: row.get(11)?,
            queue_schema_version: row.get(12)?,
            triage_schema_version: row.get(13)?,
            execution_schema_version: row.get(14)?,
            execution_evidence: execution_evidence_json
                .as_deref()
                .map(|raw| parse_json(raw, "сводка результата исполнения"))
                .transpose()?,
            analyzer_digest: row.get(16)?,
            classifier_digest: row.get(17)?,
        },
        trust: trust_level_of(&trust),
        outcome: reviewed_outcome_of(&outcome),
        limitations: parse_json(&limitations_json, "ограничения записи")?,
        revision_of: row.get(21)?,
        superseded_by: row.get(22)?,
        revision: u64::try_from(row.get::<_, i64>(23)?.max(0)).unwrap_or(0),
        imported_at: u64::try_from(row.get::<_, i64>(24)?.max(0)).unwrap_or(0),
        observations: parse_json(&observations_json, "единицы наблюдения")?,
    })
}

/// Разбирает JSON-столбец в типизированное значение для строк таблицы.
pub(super) fn parse_json<T: for<'de> Deserialize<'de>>(
    raw: &str,
    what: &str,
) -> rusqlite::Result<T> {
    serde_json::from_str(raw)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
        .map_err(|_| {
            rusqlite::Error::InvalidColumnType(0, what.to_owned(), rusqlite::types::Type::Text)
        })
}

/// Разбирает сохранённый JSON-столбец в типизированное значение или доменную ошибку.
pub(super) fn parse_domain_json<T: for<'de> Deserialize<'de>>(
    raw: &str,
    what: &str,
) -> Result<T, DomainError> {
    serde_json::from_str(raw).map_err(|error| {
        DomainError::with_details(
            ErrorCode::LearningCorrupt,
            format!("Сохранённое значение повреждено ({what}): {error}"),
            crate::details! { "column" => what.to_owned() },
        )
    })
}

/// Нормализует и проверяет явно выбранный вариант источника.
pub(crate) fn normalize_variant(raw: &str) -> Result<String, DomainError> {
    if raw.trim().is_empty() {
        return Ok(ROOT_WORKSPACE_VARIANT.to_owned());
    }
    if raw == ROOT_WORKSPACE_VARIANT {
        return Ok(ROOT_WORKSPACE_VARIANT.to_owned());
    }
    let Some(digest) = raw.strip_prefix(SNAPSHOT_WORKSPACE_VARIANT_PREFIX) else {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            format!(
                "Некорректный вариант источника learning: «{raw}»; ожидается `root` или `snapshot-<32 hex>`"
            ),
        ));
    };
    if digest.len() == 32 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(format!("{SNAPSHOT_WORKSPACE_VARIANT_PREFIX}{digest}"))
    } else {
        Err(DomainError::new(
            ErrorCode::InvalidRequest,
            format!("Некорректный digest варианта-снимка learning: «{raw}»"),
        ))
    }
}

/// Идентичность входных документов и версий схем.
pub fn inputs_of(loaded: &LoadedReview) -> Result<ImportInputs, DomainError> {
    Ok(ImportInputs {
        review_pack_sha256: loaded.review_pack_sha256.clone(),
        queue_sha256: loaded.queue_sha256.clone(),
        triage_sha256: loaded.triage.as_ref().map(|(_, digest)| digest.clone()),
        execution_result_sha256: loaded.inputs_hint.execution_result_sha256.clone(),
        review_schema_version: loaded.pack.schema_version,
        queue_schema_version: loaded.queue.schema_version,
        triage_schema_version: loaded
            .triage
            .as_ref()
            .map(|(triage, _)| triage.schema_version),
        execution_schema_version: loaded.inputs_hint.execution_schema_version,
        execution_evidence: loaded.inputs_hint.execution_evidence.clone(),
        analyzer_digest: analyzer_digest(&loaded.pack),
        classifier_digest: classifier_digest(&loaded.pack, &loaded.queue),
    })
}

/// Digest набора анализаторов из `tool_runs` пакета.
///
/// Изменение набора анализаторов при том же HEAD меняет identity записи: два
/// запуска с разными инструментами не смешиваются в один review run.
#[must_use]
pub fn analyzer_digest(pack: &crate::code_review::model::ReviewPack) -> String {
    let mut entries: Vec<String> = pack
        .tool_runs
        .iter()
        .map(|run| format!("{}={}", run.tool, run.status))
        .collect();
    entries.sort();
    entries.dedup();
    sha256_hex(entries.join("\n").as_bytes())
}

/// Digest версий правил классификации и семантики, действовавших при сборе.
#[must_use]
pub fn classifier_digest(
    pack: &crate::code_review::model::ReviewPack,
    queue: &crate::code_review::review_queue::ReviewQueue,
) -> String {
    let mut entries = [
        format!("review_schema={}", pack.schema_version),
        format!("queue_schema={}", queue.schema_version),
    ];
    entries.sort();
    sha256_hex(entries.join("\n").as_bytes())
}

/// Стабильный SHA-256 в строчных шестнадцатеричных символах.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut text = String::with_capacity(64);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn identity_key(loaded: &LoadedReview, variant: &str) -> String {
    let target = &loaded.pack.target;
    sha256_hex(
        format!(
            "{}\n{}\n{}\n{}\n{}",
            target.repository_id,
            target.merge_base_sha,
            target.head_sha,
            variant,
            analyzer_digest(&loaded.pack)
        )
        .as_bytes(),
    )
}

fn review_id(loaded: &LoadedReview, variant: &str) -> String {
    let target = &loaded.pack.target;
    let digest = sha256_hex(
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            target.repository_id,
            target.base_sha,
            target.head_sha,
            target.merge_base_sha,
            variant,
            analyzer_digest(&loaded.pack),
            classifier_digest(&loaded.pack, &loaded.queue),
            loaded.queue_sha256,
            loaded
                .triage
                .as_ref()
                .map_or("", |(_, digest)| digest.as_str()),
            loaded
                .inputs_hint
                .execution_result_sha256
                .as_deref()
                .unwrap_or("")
        )
        .as_bytes(),
    );
    format!("review-{}", &digest[..32])
}

fn outcome_of(loaded: &LoadedReview) -> ReviewedOutcome {
    if !loaded.trust.participates_in_learning() {
        return ReviewedOutcome::Quarantined;
    }
    match loaded.triage.as_ref() {
        None => ReviewedOutcome::NoTriage,
        Some((triage, _)) if triage.unreviewed_candidate_ids.is_empty() => {
            ReviewedOutcome::FullyReviewed
        }
        Some(_) => ReviewedOutcome::PartiallyReviewed,
    }
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

/// Ищет точный повтор того же набора свидетельств.
fn existing_exact(
    read: &super::store::LearningTx<'_>,
    review_id: &str,
    identity_key: &str,
    inputs: &ImportInputs,
    loaded: &LoadedReview,
) -> Result<Option<ImportRecord>, DomainError> {
    let found: Option<String> = read
        .transaction()
        .query_row(
            "SELECT review_id FROM learning_import WHERE review_id = ?1 AND identity_key = ?2
             AND review_pack_sha256 = ?3 AND queue_sha256 = ?4
             AND COALESCE(triage_sha256, '') = ?5 AND analyzer_digest = ?6
             AND classifier_digest = ?7 AND trust = ?8",
            params![
                review_id,
                identity_key,
                inputs.review_pack_sha256,
                inputs.queue_sha256,
                inputs.triage_sha256.clone().unwrap_or_default(),
                inputs.analyzer_digest,
                inputs.classifier_digest,
                loaded.trust.as_str(),
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| super::store::map_error(&error, "не удалось проверить повтор импорта"))?;
    match found {
        None => Ok(None),
        Some(_) => Ok(Some(show_import_in_write(read, review_id)?)),
    }
}

fn show_import_in_write(
    read: &super::store::LearningTx<'_>,
    review_id: &str,
) -> Result<ImportRecord, DomainError> {
    read.transaction()
        .query_row(
            "SELECT review_id, repository_id, base_sha, head_sha, merge_base_sha,
                    workspace_variant, workspace_label, review_pack_sha256, queue_sha256,
                    triage_sha256, execution_result_sha256,
                    review_schema_version, queue_schema_version, triage_schema_version,
                    execution_schema_version, execution_evidence_json, analyzer_digest,
                    classifier_digest,
                    trust, outcome, limitations_json, revision_of, superseded_by,
                    revision, imported_at, observations_json
             FROM learning_import WHERE review_id = ?1",
            params![review_id],
            record_from_row,
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать запись истории"))
}

/// Предыдущая не вытесненная запись той же identity: её вытесняет новая ревизия.
fn previous_head(
    read: &super::store::LearningWrite<'_>,
    identity_key: &str,
    review_id: &str,
) -> Result<Option<String>, DomainError> {
    let mut statement = read
        .transaction()
        .prepare(
            "SELECT review_id FROM learning_import
             WHERE identity_key = ?1 AND review_id <> ?2 AND superseded_by IS NULL
             ORDER BY revision DESC LIMIT 1",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать ревизии"))?;
    let mut rows = statement
        .query(params![identity_key, review_id])
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать ревизии"))?;
    match rows
        .next()
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать ревизии"))?
    {
        Some(row) => row
            .get::<_, String>(0)
            .map(Some)
            .map_err(|error| super::store::map_error(&error, "не удалось прочитать ревизии")),
        None => Ok(None),
    }
}

fn next_revision(read: &super::store::LearningWrite<'_>) -> Result<u64, DomainError> {
    let revision: i64 = read
        .transaction()
        .query_row(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM learning_import",
            [],
            |row| row.get(0),
        )
        .map_err(|error| super::store::map_error(&error, "не удалось вычислить ревизию"))?;
    Ok(u64::try_from(revision.max(1)).unwrap_or(u64::MAX))
}

fn insert_import(
    write: &super::store::LearningWrite<'_>,
    record: &ImportRecord,
    identity_key: &str,
) -> Result<(), DomainError> {
    let limitations =
        serde_json::to_string(&record.limitations).unwrap_or_else(|_| "[]".to_owned());
    let observations =
        serde_json::to_string(&record.observations).unwrap_or_else(|_| "{}".to_owned());
    let execution_evidence = record
        .inputs
        .execution_evidence
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            DomainError::new(
                ErrorCode::Internal,
                format!("не удалось сериализовать сводку результата исполнения: {error}"),
            )
        })?;
    write.execute(
        "INSERT INTO learning_import (
            review_id, repository_id, base_sha, head_sha, merge_base_sha, workspace_variant,
            workspace_label, review_pack_sha256, queue_sha256, triage_sha256,
            execution_result_sha256, review_schema_version, queue_schema_version,
            triage_schema_version, execution_schema_version, execution_evidence_json,
            analyzer_digest, classifier_digest,
            trust, outcome, limitations_json, revision_of, superseded_by, revision, imported_at,
            observations_json, identity_key
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
            ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27
         )",
        params![
            record.review_id,
            record.repository_id,
            record.base_sha,
            record.head_sha,
            record.merge_base_sha,
            record.workspace_variant,
            record.workspace_label,
            record.inputs.review_pack_sha256,
            record.inputs.queue_sha256,
            record.inputs.triage_sha256,
            record.inputs.execution_result_sha256,
            record.inputs.review_schema_version,
            record.inputs.queue_schema_version,
            record.inputs.triage_schema_version,
            record.inputs.execution_schema_version,
            execution_evidence,
            record.inputs.analyzer_digest,
            record.inputs.classifier_digest,
            record.trust.as_str(),
            record.outcome.as_str(),
            limitations,
            record.revision_of,
            record.superseded_by,
            record.revision as i64,
            record.imported_at as i64,
            observations,
            identity_key,
        ],
    )?;
    Ok(())
}

/// Раздельные единицы наблюдения: группы не превращаются в число кандидатов.
fn observation_counts(loaded: &LoadedReview) -> ObservationCounts {
    let mut counts = ObservationCounts::default();
    let candidates = loaded.pack.all_candidates();
    counts.raw_candidates = candidates.len();
    let outcomes = outcome_map(loaded);
    for candidate in &candidates {
        match outcomes.get(candidate.id.as_str()) {
            Some(Some(disposition)) => {
                counts.covered_candidates += 1;
                match *disposition {
                    crate::code_review::semantic_triage::Disposition::Confirmed => {
                        counts.confirmed_candidates += 1;
                    }
                    crate::code_review::semantic_triage::Disposition::Acceptable => {
                        counts.acceptable_candidates += 1;
                    }
                    crate::code_review::semantic_triage::Disposition::FalsePositive => {
                        counts.false_positive_candidates += 1;
                    }
                    crate::code_review::semantic_triage::Disposition::NotApplicable => {
                        counts.not_applicable_candidates += 1;
                    }
                    crate::code_review::semantic_triage::Disposition::Uncertain => {
                        counts.uncertain_candidates += 1;
                    }
                }
            }
            // Отсутствие решения явно считается нерассмотренным, а не acceptable.
            Some(None) | None => counts.unreviewed_candidates += 1,
        }
    }
    if let Some((triage, _)) = loaded.triage.as_ref() {
        counts.individual_decisions = triage.individual_decisions.len();
        counts.group_decisions = triage.group_decisions.len();
        for finding in &triage.findings {
            counts.findings += 1;
            match finding.provenance {
                crate::code_review::semantic_triage::FindingProvenance::DirectCandidate => {
                    counts.findings_direct_candidate += 1;
                }
                crate::code_review::semantic_triage::FindingProvenance::CandidateAssisted => {
                    counts.findings_candidate_assisted += 1;
                }
                crate::code_review::semantic_triage::FindingProvenance::Independent => {
                    counts.findings_independent += 1;
                }
            }
        }
    }
    // Независимая единица — единица очереди, в которой есть решение. 1200
    // кандидатов одного GroupDecision дают ровно одну рассмотренную единицу.
    for unit in &loaded.queue.units {
        let resolved = unit
            .candidate_ids()
            .iter()
            .any(|id| matches!(outcomes.get(id.as_str()), Some(Some(_))));
        if resolved {
            counts.reviewed_units += 1;
        } else {
            counts.unresolved_units += 1;
        }
    }
    counts
}

/// Исход каждого кандидата: решение, явное отсутствие решения либо ничего.
type OutcomeMap<'a> = BTreeMap<&'a str, Option<crate::code_review::semantic_triage::Disposition>>;

fn outcome_map(loaded: &LoadedReview) -> OutcomeMap<'_> {
    let mut outcomes: OutcomeMap<'_> = BTreeMap::new();
    if let Some((triage, _)) = loaded.triage.as_ref() {
        for decision in &triage.individual_decisions {
            outcomes.insert(decision.candidate_id.as_str(), Some(decision.disposition));
        }
        for group in &triage.group_decisions {
            for id in &group.candidate_ids {
                outcomes.insert(id.as_str(), Some(group.disposition));
            }
        }
        for id in &triage.unreviewed_candidate_ids {
            outcomes.insert(id.as_str(), None);
        }
    }
    outcomes
}

fn write_units(
    write: &super::store::LearningWrite<'_>,
    loaded: &LoadedReview,
    review_id: &str,
) -> Result<(), DomainError> {
    let dispositions = decision_index(loaded);
    for unit in &loaded.queue.units {
        let kind = if unit.is_group() {
            ReviewUnitKind::Group
        } else {
            ReviewUnitKind::Individual
        };
        let (disposition, reason_code) = unit
            .candidate_ids()
            .iter()
            .find_map(|id| dispositions.get(id.as_str()).cloned())
            .unwrap_or((None, None));
        let features = super::patterns::unit_features(&unit.signature);
        write.execute(
            "INSERT INTO learning_unit (
                review_id, unit_id, kind, candidate_count, priority, representatives_json,
                disposition, reason_code, detector, source, role, code_role, surfaces_json,
                signature, feature_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                review_id,
                unit.id,
                kind.as_str(),
                unit.candidate_ids().len() as i64,
                unit.priority.as_str(),
                serde_json::to_string(unit.representative_candidate_ids())
                    .unwrap_or_else(|_| "[]".to_owned()),
                disposition,
                reason_code,
                unit.signature.detector,
                unit.signature.source,
                unit.signature.classification.role.as_str(),
                unit.signature.classification.code_role.as_str(),
                serde_json::to_string(
                    &unit
                        .signature
                        .classification
                        .surfaces
                        .iter()
                        .map(crate::code_review::review_queue::surface_name)
                        .collect::<Vec<_>>()
                )
                .unwrap_or_else(|_| "[]".to_owned()),
                super::patterns::feature_signature(&features),
                serde_json::to_string(&features).unwrap_or_else(|_| "{}".to_owned()),
            ],
        )?;
    }
    Ok(())
}

fn decision_index(loaded: &LoadedReview) -> BTreeMap<&str, (Option<String>, Option<String>)> {
    let mut index = BTreeMap::new();
    if let Some((triage, _)) = loaded.triage.as_ref() {
        for decision in &triage.individual_decisions {
            index.insert(
                decision.candidate_id.as_str(),
                (
                    Some(decision.disposition.as_str().to_owned()),
                    Some(decision.reason_code.as_str().to_owned()),
                ),
            );
        }
        for group in &triage.group_decisions {
            for id in &group.candidate_ids {
                index.insert(
                    id.as_str(),
                    (
                        Some(group.disposition.as_str().to_owned()),
                        Some(group.reason_code.as_str().to_owned()),
                    ),
                );
            }
        }
    }
    index
}

fn write_candidates(
    write: &super::store::LearningWrite<'_>,
    loaded: &LoadedReview,
    review_id: &str,
) -> Result<(), DomainError> {
    let owner = unit_of_candidate(loaded);
    for candidate in loaded.pack.all_candidates() {
        let unit_id = owner.get(candidate.id.as_str()).copied().unwrap_or("");
        let classification = loaded
            .queue
            .classifications
            .get(&candidate.id)
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned()))
            .unwrap_or_else(|| "{}".to_owned());
        let execution = loaded
            .queue
            .classifications
            .get(&candidate.id)
            .and_then(|value| value.execution.as_ref())
            .map_or("unknown", crate::code_review::review_queue::surface_name);
        write.execute(
            "INSERT INTO learning_candidate (
                review_id, candidate_id, unit_id, detector, source, path, path_family, origin,
                execution, snippet, classification_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                review_id,
                candidate.id,
                unit_id,
                candidate.detector,
                candidate.source,
                sanitize_path(&candidate.path),
                path_family(&candidate.path),
                candidate.origin.as_str(),
                execution,
                sanitize_snippet(candidate.snippet.as_deref()),
                classification,
            ],
        )?;
    }
    Ok(())
}

fn unit_of_candidate(loaded: &LoadedReview) -> BTreeMap<&str, &str> {
    let mut owner = BTreeMap::new();
    for unit in &loaded.queue.units {
        for id in unit.candidate_ids() {
            owner.insert(id.as_str(), unit.id.as_str());
        }
    }
    owner
}

fn write_decisions(
    write: &super::store::LearningWrite<'_>,
    loaded: &LoadedReview,
    review_id: &str,
) -> Result<(), DomainError> {
    let Some((triage, _)) = loaded.triage.as_ref() else {
        return Ok(());
    };
    for decision in &triage.individual_decisions {
        write.execute(
            "INSERT INTO learning_decision (
                review_id, decision_id, kind, disposition, reason_code, explanation,
                candidate_count, covered_json
             ) VALUES (?1, ?2, 'individual', ?3, ?4, ?5, 1, ?6)",
            params![
                review_id,
                format!("individual-{}", decision.candidate_id),
                decision.disposition.as_str(),
                decision.reason_code.as_str(),
                sanitize_text(&decision.explanation),
                serde_json::to_string(std::slice::from_ref(&decision.candidate_id))
                    .unwrap_or_else(|_| "[]".to_owned()),
            ],
        )?;
    }
    for group in &triage.group_decisions {
        write.execute(
            "INSERT INTO learning_decision (
                review_id, decision_id, kind, disposition, reason_code, explanation,
                candidate_count, covered_json
             ) VALUES (?1, ?2, 'group', ?3, ?4, ?5, ?6, ?7)",
            params![
                review_id,
                group.id,
                group.disposition.as_str(),
                group.reason_code.as_str(),
                sanitize_text(&group.explanation),
                group.candidate_ids.len() as i64,
                serde_json::to_string(&group.candidate_ids).unwrap_or_else(|_| "[]".to_owned()),
            ],
        )?;
    }
    Ok(())
}

fn write_findings(
    write: &super::store::LearningWrite<'_>,
    loaded: &LoadedReview,
    review_id: &str,
) -> Result<(), DomainError> {
    let Some((triage, _)) = loaded.triage.as_ref() else {
        return Ok(());
    };
    let owner = unit_of_candidate(loaded);
    let features = unit_features_by_candidate(loaded);
    for finding in &triage.findings {
        let units: BTreeSet<String> = finding
            .candidate_ids
            .iter()
            .filter_map(|id| owner.get(id.as_str()).map(|unit| (*unit).to_owned()))
            .collect();
        let signature = finding_signature(finding, &features);
        write.execute(
            "INSERT INTO learning_finding (
                review_id, finding_id, severity, provenance, title, description, signature,
                linked_unit_ids_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                review_id,
                finding.id,
                finding.severity.as_str(),
                finding.provenance.as_str(),
                sanitize_text(&finding.title),
                sanitize_text(&finding.description),
                signature,
                serde_json::to_string(&units).unwrap_or_else(|_| "[]".to_owned()),
            ],
        )?;
        for candidate_id in &finding.candidate_ids {
            write.execute(
                "INSERT INTO learning_finding_link (review_id, candidate_id, finding_id)
                 VALUES (?1, ?2, ?3)",
                params![review_id, candidate_id, finding.id],
            )?;
        }
    }
    Ok(())
}

/// Точный ключ признаков связанных единиц для структурного сопоставления.
fn unit_features_by_candidate(loaded: &LoadedReview) -> BTreeMap<&str, String> {
    let dispositions = decision_index(loaded);
    let mut features = BTreeMap::new();
    for unit in &loaded.queue.units {
        let (_disposition, _) = unit
            .candidate_ids()
            .iter()
            .find_map(|id| dispositions.get(id.as_str()).cloned())
            .unwrap_or((None, None));
        let unit_features = super::patterns::unit_features(&unit.signature);
        let signature = super::patterns::feature_signature(&unit_features);
        for id in unit.candidate_ids() {
            features.insert(id.as_str(), signature.clone());
        }
    }
    features
}

fn finding_signature(
    finding: &crate::code_review::semantic_triage::SemanticFinding,
    features: &BTreeMap<&str, String>,
) -> String {
    let mut unit_signatures: Vec<&str> = finding
        .candidate_ids
        .iter()
        .filter_map(|id| features.get(id.as_str()).map(String::as_str))
        .collect();
    unit_signatures.sort_unstable();
    unit_signatures.dedup();
    sha256_hex(
        format!(
            "{}\n{}\n{}",
            finding.severity.as_str(),
            finding.provenance.as_str(),
            unit_signatures.join(",")
        )
        .as_bytes(),
    )
}

fn write_search(
    write: &super::store::LearningWrite<'_>,
    loaded: &LoadedReview,
    review_id: &str,
) -> Result<(), DomainError> {
    let Some((triage, _)) = loaded.triage.as_ref() else {
        return Ok(());
    };
    let owner = unit_of_candidate(loaded);
    // Вывод ревьюера привязывается к случаю: случай замечания наследует исход
    // решений, которые на него ссылаются, поэтому точный фильтр «текст + исход»
    // остаётся одним случаем, а не склейкой двух независимых строк.
    let mut disposition_of_finding: BTreeMap<&str, &str> = BTreeMap::new();
    for decision in &triage.individual_decisions {
        for finding_id in &decision.finding_ids {
            disposition_of_finding
                .entry(finding_id.as_str())
                .or_insert(decision.disposition.as_str());
        }
    }
    for group in &triage.group_decisions {
        for finding_id in &group.finding_ids {
            disposition_of_finding
                .entry(finding_id.as_str())
                .or_insert(group.disposition.as_str());
        }
    }
    for finding in &triage.findings {
        let case_id = format!("finding:{review_id}:{}", finding.id);
        let body = sanitize_text(&format!("{} {}", finding.title, finding.description));
        insert_search(
            write,
            &SearchRow {
                case_id: &case_id,
                review_id,
                unit_id: "",
                candidate_id: None,
                finding_id: Some(&finding.id),
                kind: "finding",
                disposition: disposition_of_finding.get(finding.id.as_str()).copied(),
                severity: Some(finding.severity.as_str()),
                provenance: Some(finding.provenance.as_str()),
                text: &body,
            },
        )?;
    }
    let findings_by_id: BTreeMap<&str, &crate::code_review::semantic_triage::SemanticFinding> =
        triage
            .findings
            .iter()
            .map(|finding| (finding.id.as_str(), finding))
            .collect();
    let severity_of = |finding_ids: &[String]| -> Option<&'static str> {
        finding_ids
            .iter()
            .filter_map(|id| findings_by_id.get(id.as_str()))
            .map(|finding| finding.severity.as_str())
            .max_by_key(|severity| severity_rank(severity))
    };
    let provenance_of = |finding_ids: &[String]| -> Option<&'static str> {
        finding_ids
            .iter()
            .filter_map(|id| findings_by_id.get(id.as_str()))
            .map(|finding| finding.provenance.as_str())
            .max()
    };
    for decision in &triage.individual_decisions {
        let case_id = format!("decision:{review_id}:{}", decision.candidate_id);
        let body = sanitize_text(&decision.explanation);
        let unit_id = owner
            .get(decision.candidate_id.as_str())
            .copied()
            .unwrap_or("");
        insert_search(
            write,
            &SearchRow {
                case_id: &case_id,
                review_id,
                unit_id,
                candidate_id: Some(&decision.candidate_id),
                finding_id: None,
                kind: "decision",
                disposition: Some(decision.disposition.as_str()),
                severity: severity_of(&decision.finding_ids),
                provenance: provenance_of(&decision.finding_ids),
                text: &body,
            },
        )?;
    }
    for group in &triage.group_decisions {
        let case_id = format!("group:{review_id}:{}", group.id);
        let body = sanitize_text(&group.explanation);
        let unit_id = group
            .candidate_ids
            .iter()
            .find_map(|id| owner.get(id.as_str()).copied())
            .unwrap_or("");
        insert_search(
            write,
            &SearchRow {
                case_id: &case_id,
                review_id,
                unit_id,
                candidate_id: None,
                finding_id: None,
                kind: "group",
                disposition: Some(group.disposition.as_str()),
                severity: severity_of(&group.finding_ids),
                provenance: provenance_of(&group.finding_ids),
                text: &body,
            },
        )?;
    }
    Ok(())
}

/// Один поисковый случай вместе с машинными метками фильтров.
pub(crate) struct SearchRow<'a> {
    pub(crate) case_id: &'a str,
    pub(crate) review_id: &'a str,
    pub(crate) unit_id: &'a str,
    pub(crate) candidate_id: Option<&'a str>,
    pub(crate) finding_id: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) disposition: Option<&'a str>,
    pub(crate) severity: Option<&'a str>,
    pub(crate) provenance: Option<&'a str>,
    pub(crate) text: &'a str,
}

/// Ранг серьёзности для выбора самой значимой связанной оценки.
fn severity_rank(severity: &str) -> u8 {
    match severity {
        "critical" => 3,
        "major" => 2,
        "minor" => 1,
        _ => 0,
    }
}

pub(crate) fn insert_search(
    write: &super::store::LearningWrite<'_>,
    row: &SearchRow<'_>,
) -> Result<(), DomainError> {
    write.execute(
        "INSERT OR REPLACE INTO learning_search
            (case_id, review_id, unit_id, candidate_id, finding_id, kind, disposition, severity,
             provenance, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            row.case_id,
            row.review_id,
            row.unit_id,
            row.candidate_id,
            row.finding_id,
            row.kind,
            row.disposition,
            row.severity,
            row.provenance,
            row.text
        ],
    )?;
    let fts_present = write.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = 'learning_search_fts'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if fts_present > 0 {
        // Повторная запись того же случая не должна удваивать строки индекса:
        // у FTS5 нет первичного ключа, поэтому старая строка снимается явно.
        write.execute(
            "DELETE FROM learning_search_fts WHERE case_id = ?1",
            params![row.case_id],
        )?;
        write.execute(
            "INSERT INTO learning_search_fts (case_id, body) VALUES (?1, ?2)",
            params![row.case_id, row.text],
        )?;
    }
    Ok(())
}

fn write_case_links(
    write: &super::store::LearningWrite<'_>,
    loaded: &LoadedReview,
    review_id: &str,
) -> Result<(), DomainError> {
    let target = &loaded.pack.target;
    let mut statement = write
        .transaction()
        .prepare(
            "SELECT f.review_id, f.finding_id, f.signature, i.head_sha, i.base_sha
             FROM learning_finding AS f
             JOIN learning_import AS i ON i.review_id = f.review_id
             WHERE i.repository_id = ?1 AND i.trust = 'ast_authenticated' AND f.review_id <> ?2
             ORDER BY f.review_id ASC, f.finding_id ASC",
        )
        .map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать прежние замечания")
        })?;
    let rows = statement
        .query_map(params![target.repository_id, review_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать прежние замечания")
        })?;
    let mut prior = Vec::new();
    for row in rows {
        prior.push(row.map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать прежние замечания")
        })?);
    }
    drop(statement);
    let current = current_findings(write, review_id)?;
    for (finding_id, signature) in &current {
        for (prior_review, prior_finding, prior_signature, prior_head, prior_base) in &prior {
            if prior_signature != signature {
                continue;
            }
            let (kind, basis) = if prior_head == &target.head_sha {
                (
                    CaseLinkKind::SameIteration,
                    "совпадает точный ключ структурных признаков и один и тот же head: повторное чтение неизменного случая",
                )
            } else if prior_base == &target.base_sha
                || prior_head == &target.base_sha
                || prior_base == &target.head_sha
            {
                (
                    CaseLinkKind::StructuralRepeat,
                    "совпадает точный ключ структурных признаков и связана история версий одного диапазона: тот же структурный дефект в другой версии",
                )
            } else {
                (CaseLinkKind::Unknown, "связь истории версий не доказана")
            };
            if kind == CaseLinkKind::Unknown {
                continue;
            }
            write.execute(
                "INSERT OR IGNORE INTO learning_case_link (
                    review_id, finding_id, linked_review_id, linked_finding_id, kind, basis
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    review_id,
                    finding_id,
                    prior_review,
                    prior_finding,
                    kind.as_str(),
                    basis
                ],
            )?;
        }
    }
    Ok(())
}

fn current_findings(
    write: &super::store::LearningWrite<'_>,
    review_id: &str,
) -> Result<Vec<(String, String)>, DomainError> {
    let mut statement = write
        .transaction()
        .prepare(
            "SELECT finding_id, signature FROM learning_finding WHERE review_id = ?1
             ORDER BY finding_id ASC",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать замечания"))?;
    let rows = statement
        .query_map(params![review_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать замечания"))?;
    let mut findings = Vec::new();
    for row in rows {
        findings.push(
            row.map_err(|error| super::store::map_error(&error, "не удалось прочитать замечания"))?,
        );
    }
    Ok(findings)
}

/// Ограничивает сохранённый текст: история — данные, а не исполнимая инструкция.
#[must_use]
pub fn sanitize_text(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len().min(MAX_STORED_TEXT_BYTES));
    for line in raw.lines() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line.trim());
        if text.len() >= MAX_STORED_TEXT_BYTES {
            break;
        }
    }
    let mut text: String = text.chars().filter(|c| !c.is_control()).collect();
    if text.len() > MAX_STORED_TEXT_BYTES {
        let mut end = MAX_STORED_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Верхняя граница сохранённого текста одного поля.
pub const MAX_STORED_TEXT_BYTES: usize = 1024;
/// Верхняя граница сохранённого сниппета.
pub const MAX_STORED_SNIPPET_BYTES: usize = 240;

/// Ограничивает сниппет и убирает управляющие символы.
#[must_use]
pub fn sanitize_snippet(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let text = sanitize_text(raw);
    if text.is_empty() {
        return None;
    }
    if text.len() <= MAX_STORED_SNIPPET_BYTES {
        return Some(text);
    }
    // Границы UTF-8 сохраняются: обрезка идёт по байтам с проверкой границы.
    let mut limit = MAX_STORED_SNIPPET_BYTES;
    while limit > 0 && !text.is_char_boundary(limit) {
        limit -= 1;
    }
    Some(text[..limit].to_owned())
}

/// Приводит путь к repo-relative виду без абсолютных и временных компонентов.
#[must_use]
pub fn sanitize_path(raw: &str) -> String {
    let mut text = raw.replace('\\', "/");
    text = text.strip_prefix('/').map_or(text.clone(), str::to_owned);
    let normalized = text
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect::<Vec<_>>()
        .join("/");
    let noisy = normalized.is_empty()
        || normalized.contains("home/")
        || normalized.starts_with("tmp/")
        || normalized.starts_with("Users/")
        || normalized.contains("AppData/")
        || normalized.contains(':');
    if noisy {
        return safe_file_name(&normalized);
    }
    normalized
}

fn safe_file_name(normalized: &str) -> String {
    normalized
        .rsplit('/')
        .find(|part| !part.is_empty() && !is_noise_component(part))
        .unwrap_or("unknown")
        .to_owned()
}

fn is_noise_component(part: &str) -> bool {
    matches!(
        part,
        "home" | "tmp" | "Users" | "AppData" | "Local" | "Roaming"
    )
}

/// Извлекает семейство признаков пути без абсолютных компонентов.
#[must_use]
pub fn path_family(raw: &str) -> String {
    let path = sanitize_path(raw);
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let directory = path
        .rsplit_once('/')
        .map_or_else(String::new, |(directory, _)| directory.to_owned());
    if extension.is_empty() {
        directory
    } else if directory.is_empty() {
        extension
    } else {
        format!("{directory}.{extension}")
    }
}

/// Явный статус проверки при ослабленном импорте.
#[must_use]
pub fn structure_only_limitations() -> Vec<String> {
    vec![
        "Импорт выполнен по явно запрошенной структурной проверке: классификация очереди не подтверждена точными Git-образами."
            .to_owned(),
        "Запись помечена карантином и исключена из обучения, статистики, паттернов и рекомендаций."
            .to_owned(),
    ]
}

/// Итог операции импорта для машинного вывода CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportOutcome {
    /// Итог операции.
    pub status: ImportStatus,
    /// Запись истории.
    pub record: ImportRecord,
}

impl ImportOutcome {
    /// Собирает итог операции из статуса и записи.
    #[must_use]
    pub fn new(status: ImportStatus, record: ImportRecord) -> Self {
        Self { status, record }
    }
}

/// Загружает и проверяет набор артефактов одного ревью для импорта.
///
/// Доверенный путь (`structure_only = false`) требует полной AST-аутентичности
/// очереди и при недоступности точных Git-образов возвращает явный
/// `syntax_authenticity_unavailable`. Ослабленный путь возможен только по
/// явному запросу вызывающей стороны и маркируется карантином.
pub fn load_review(
    pack_path: &std::path::Path,
    queue_path: &std::path::Path,
    triage_path: Option<&std::path::Path>,
    structure_only: bool,
) -> Result<LoadedReview, DomainError> {
    // Сначала сверяются сами документы: несовпадающий digest источника — это
    // отдельная различимая ошибка, а не «недоступность точных Git-образов».
    let pack_bytes = std::fs::read(pack_path).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось прочитать review.json: {error}"),
        )
    })?;
    let review_pack_sha256 = sha256_hex(&pack_bytes);
    let declared_queue = read_queue_document(queue_path)?;
    verify_declared_source(&declared_queue, &review_pack_sha256)?;

    // Дальше очередь проверяется ровно той же логикой, что и в CLI: молчаливого
    // ослабления до структурной проверки при недоступных образах Git нет.
    let (pack, pack_bytes, queue, queue_bytes, _summary, authenticity, _contexts) =
        crate::code_review::workflow::load_validated_review_queue_with_contexts(
            pack_path,
            queue_path,
            structure_only,
        )?;
    let review_pack_sha256 = {
        let recomputed = sha256_hex(&pack_bytes);
        debug_assert_eq!(recomputed, review_pack_sha256);
        recomputed
    };
    let mut limitations = Vec::new();
    let trust = match authenticity {
        crate::code_review::workflow::SyntaxAuthenticityStatus::Verified => {
            TrustLevel::AstAuthenticated
        }
        crate::code_review::workflow::SyntaxAuthenticityStatus::StructureOnly => {
            limitations.extend(structure_only_limitations());
            TrustLevel::StructureOnlyQuarantine
        }
    };
    let triage = match triage_path {
        None => None,
        Some(path) => Some(load_triage(path, &pack, &review_pack_sha256)?),
    };
    if let Some((triage, _)) = triage.as_ref() {
        verify_triage_identity(triage, &pack, &review_pack_sha256)?;
    }
    let queue_sha256 = sha256_hex(&queue_bytes);
    Ok(LoadedReview {
        pack,
        review_pack_sha256,
        queue,
        queue_sha256,
        triage,
        trust,
        limitations,
        inputs_hint: ImportInputsHint::default(),
    })
}

/// Загружает артефакты ревью вместе с необязательным результатом изоляции.
///
/// Результат изолированной проверки принимается только как завершённое задание
/// с той же identity источника: незавершённый или чужой результат отвергается,
/// а не становится частью истории наблюдений.
pub fn load_review_with_execution(
    pack_path: &std::path::Path,
    queue_path: &std::path::Path,
    triage_path: Option<&std::path::Path>,
    execution_path: Option<&std::path::Path>,
    structure_only: bool,
) -> Result<LoadedReview, DomainError> {
    let mut loaded = load_review(pack_path, queue_path, triage_path, structure_only)?;
    let Some(execution_path) = execution_path else {
        return Ok(loaded);
    };
    if execution_path.file_name().and_then(|name| name.to_str()) != Some("result.json") {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "--execution должен указывать на result.json завершённого задания",
        ));
    }
    let job_directory = execution_path.parent().ok_or_else(|| {
        DomainError::new(
            ErrorCode::InvalidRequest,
            "у result.json отсутствует каталог задания",
        )
    })?;
    let inspection = crate::code_review::execution::inspect_job(job_directory)?;
    if inspection.lifecycle != crate::code_review::execution::LifecycleStatus::Completed {
        return Err(DomainError::new(
            ErrorCode::ExecutionNotCompleted,
            "Незавершённый result.json нельзя включить в историю learning",
        ));
    }
    let validated = inspection.result.ok_or_else(|| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            "Завершённое задание не содержит проверенного result.json",
        )
    })?;
    let metadata = std::fs::symlink_metadata(execution_path).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось проверить result.json изолированной проверки: {error}"),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            "result.json изолированной проверки должен быть обычным файлом",
        ));
    }
    let bytes = std::fs::read(execution_path).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось прочитать result.json изолированной проверки: {error}"),
        )
    })?;
    let parsed: crate::code_review::execution::ExecutionResult = serde_json::from_slice(&bytes)
        .map_err(|error| {
            DomainError::new(
                ErrorCode::ReviewArtifactInvalid,
                format!("некорректный JSON в result.json изолированной проверки: {error}"),
            )
        })?;
    if parsed != validated {
        return Err(DomainError::new(
            ErrorCode::SourceChanged,
            "байты result.json изменились после проверки по манифесту задания",
        ));
    }
    let result = validated;
    if result.source.review_pack_sha256 != loaded.review_pack_sha256 {
        return Err(DomainError::new(
            ErrorCode::SourceChanged,
            "result.json изолированной проверки относится к другим байтам review.json",
        ));
    }
    if result.source.snapshot != loaded.pack.target {
        return Err(DomainError::new(
            ErrorCode::BaselineMismatch,
            "Git-снимок изолированной проверки не соответствует исходному review.json",
        ));
    }
    loaded.inputs_hint = ImportInputsHint {
        execution_result_sha256: Some(sha256_hex(&bytes)),
        execution_schema_version: Some(result.schema_version),
        execution_evidence: Some(ExecutionEvidenceSummary::from_result(&result)),
        // Вариант рабочего пространства сверяется там, где известен явно
        // выбранный вариант импорта: в `import_history`.
        workspace_variant: result
            .source
            .workspace_variant
            .as_deref()
            .map(normalize_variant)
            .transpose()?,
    };
    Ok(loaded)
}

/// Сверяет объявленный в очереди digest точных байтов пакета.
fn verify_declared_source(
    queue: &crate::code_review::review_queue::ReviewQueue,
    review_pack_sha256: &str,
) -> Result<(), DomainError> {
    if queue.source.review_pack_sha256 != review_pack_sha256 {
        return Err(DomainError::new(
            ErrorCode::SourceChanged,
            "review-queue.json заявляет digest, не совпадающий с точными байтами переданного review.json",
        ));
    }
    Ok(())
}

/// Читает документ структурной очереди.
fn read_queue_document(
    path: &std::path::Path,
) -> Result<crate::code_review::review_queue::ReviewQueue, DomainError> {
    let bytes = std::fs::read(path).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось прочитать review-queue.json: {error}"),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            format!("некорректный JSON в review-queue.json: {error}"),
        )
    })
}

/// Проверяет identity документа семантического разбора.
fn verify_triage_identity(
    triage: &crate::code_review::semantic_triage::SemanticTriage,
    pack: &crate::code_review::model::ReviewPack,
    review_pack_sha256: &str,
) -> Result<(), DomainError> {
    if triage.source.review_pack_sha256 != review_pack_sha256 {
        return Err(DomainError::new(
            ErrorCode::SourceChanged,
            "semantic-triage.json заявляет digest, не совпадающий с точными байтами переданного review.json",
        ));
    }
    if triage.source.snapshot
        != crate::code_review::semantic_triage::TriageSnapshot::from(&pack.target)
    {
        return Err(DomainError::new(
            ErrorCode::BaselineMismatch,
            "Git-снимок семантического разбора не соответствует исходному review.json",
        ));
    }
    Ok(())
}

/// Читает и проверяет документ семантического разбора против пакета.
fn load_triage(
    path: &std::path::Path,
    pack: &crate::code_review::model::ReviewPack,
    review_pack_sha256: &str,
) -> Result<(crate::code_review::semantic_triage::SemanticTriage, String), DomainError> {
    let bytes = std::fs::read(path).map_err(|error| {
        DomainError::new(
            ErrorCode::InputUnreadable,
            format!("не удалось прочитать semantic-triage.json: {error}"),
        )
    })?;
    let triage: crate::code_review::semantic_triage::SemanticTriage =
        serde_json::from_slice(&bytes).map_err(|error| {
            DomainError::new(
                ErrorCode::ReviewArtifactInvalid,
                format!("некорректный JSON в semantic-triage.json: {error}"),
            )
        })?;
    crate::code_review::semantic_triage::validate(&triage, pack, review_pack_sha256)?;
    Ok((triage, sha256_hex(&bytes)))
}
