//! Типизированная обратная связь, аудит правок и предложения политики.
//!
//! Модуль разделяет оценку полезности рекомендации и содержательную правку
//! семантического исхода. Прошлый `semantic-triage.json` не перезаписывается:
//! правки живут отдельными аудируемыми событиями, а конфликтующие утверждения
//! для одного случая не разрешаются молча по принципу «последний записавший» —
//! требуется явный `supersede` либо возвращается `learning_conflict` со списком
//! конфликта.
//!
//! Второй слой — предложения постоянных правил: они создаются объяснимыми, но
//! никогда не применяются автоматически и не порождают suppression.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorCode};

use super::LEARNING_POLICY_VERSION;
use super::import::sha256_hex;
use super::model::{
    CaseRef, FeedbackAction, FeedbackEvent, FeedbackKind, FeedbackOutcome, FeedbackResult,
    HistoryGeneration, PolicyProposal, SupportLevel, TrustLevel,
};
use super::patterns::{cases_for_signature, feature_signature, support_for_signature};
use super::schema::LEARNING_SCHEMA_VERSION;
use super::store::{LearningStore, map_error};

/// Версия схемы событий обратной связи.
pub const FEEDBACK_SCHEMA_VERSION: u32 = 1;
/// Версия схемы предложений политики.
pub const POLICY_SCHEMA_VERSION: u32 = 1;

type EffectiveRevision = (u64, String, String);

/// Путь материализации утверждённой политики, которую коммитит человек.
pub const POLICY_ARTIFACT_PATH: &str = ".anki-repo/policies/learning-policy.json";

/// Содержательные решения, допустимые в правке семантического исхода.
const DISPOSITIONS: &[&str] = &[
    "confirmed",
    "acceptable",
    "false_positive",
    "not_applicable",
    "uncertain",
];

/// Оценки полезности рекомендации, допустимые в типизированном событии.
const USEFULNESS: &[&str] = &["useful", "not_useful", "partially_useful"];

/// Принимает типизированное событие обратной связи и возвращает действующий исход.
pub fn record_feedback(
    store: &LearningStore,
    event: &FeedbackEvent,
) -> Result<FeedbackResult, DomainError> {
    validate_event(event)?;
    let result = store.write(|write| {
        ensure_case_exists(write, &event.review_id, &event.unit_id, event.candidate_id.as_deref())?;
        match event.action {
            FeedbackAction::Append => {
                let conflicts = conflicting_events(write, event)?;
                if !conflicts.is_empty() {
                    return Err(conflict_error(event, &conflicts));
                }
                insert_event(write, event)?;
            }
            FeedbackAction::Supersede => {
                let target = event.supersedes_event_id.as_deref().ok_or_else(|| {
                    DomainError::new(
                        ErrorCode::InvalidRequest,
                        "Действие supersede требует явного supersedes_event_id",
                    )
                })?;
                ensure_supersedable(write, event, target)?;
                guard_remaining_conflicts(write, event, target)?;
                insert_event(write, event)?;
            }
            FeedbackAction::Retract => {
                let target = event.supersedes_event_id.as_deref().ok_or_else(|| {
                    DomainError::new(
                        ErrorCode::InvalidRequest,
                        "Действие retract требует явного supersedes_event_id отзываемого утверждения",
                    )
                })?;
                ensure_supersedable(write, event, target)?;
                insert_event(write, event)?;
            }
        }
        let outcome = outcome_for(&write.as_tx(), &event.review_id, &event.unit_id)?;
        Ok(FeedbackResult {
            schema_version: FEEDBACK_SCHEMA_VERSION,
            event_id: event.event_id.clone(),
            superseded_event_id: event
                .supersedes_event_id
                .clone()
                .filter(|_| event.action != FeedbackAction::Append),
            retracted_event_id: event
                .supersedes_event_id
                .clone()
                .filter(|_| event.action == FeedbackAction::Retract),
            outcome,
            limitations: vec![
                "Правка хранится аудируемо и не перезаписывает semantic-triage.json.".to_owned(),
                "Влияние отозванного утверждения учитывается при следующем пересчёте паттернов."
                    .to_owned(),
            ],
        })
    })?;
    Ok(result)
}

fn validate_event(event: &FeedbackEvent) -> Result<(), DomainError> {
    if event.schema_version != FEEDBACK_SCHEMA_VERSION {
        return Err(DomainError::new(
            ErrorCode::ReviewArtifactInvalid,
            "Неподдерживаемая версия схемы события обратной связи",
        ));
    }
    for (value, field) in [
        (&event.event_id, "event_id"),
        (&event.review_id, "review_id"),
        (&event.unit_id, "unit_id"),
        (&event.provenance, "provenance"),
        (&event.explanation, "explanation"),
    ] {
        if value.trim().is_empty() {
            return Err(DomainError::new(
                ErrorCode::InvalidRequest,
                format!("В событии обратной связи не задано поле: {field}"),
            ));
        }
    }
    match event.kind {
        FeedbackKind::RecommendationUsefulness => {
            let usefulness = event.usefulness.as_deref().ok_or_else(|| {
                DomainError::new(
                    ErrorCode::InvalidRequest,
                    "Для оценки полезности рекомендации нужно указать usefulness",
                )
            })?;
            if !USEFULNESS.contains(&usefulness) {
                return Err(DomainError::new(
                    ErrorCode::InvalidRequest,
                    format!("Недопустимое значение usefulness: «{usefulness}»"),
                ));
            }
            if event.effective_disposition.is_some() {
                return Err(DomainError::new(
                    ErrorCode::InvalidRequest,
                    "Оценка полезности не меняет семантический исход: effective_disposition запрещён",
                ));
            }
        }
        FeedbackKind::SemanticOutcomeRevision => {
            // Отзыв снимает прежнюю правку и не назначает новый исход, поэтому
            // явный effective_disposition требуется только для содержательной правки.
            if let Some(disposition) = event.effective_disposition.as_deref() {
                if !DISPOSITIONS.contains(&disposition) {
                    return Err(DomainError::new(
                        ErrorCode::InvalidRequest,
                        format!("Недопустимое значение effective_disposition: «{disposition}»"),
                    ));
                }
            } else if event.action != FeedbackAction::Retract {
                return Err(DomainError::new(
                    ErrorCode::InvalidRequest,
                    "Содержательная правка требует явного effective_disposition",
                ));
            }
            if event.usefulness.is_some() {
                return Err(DomainError::new(
                    ErrorCode::InvalidRequest,
                    "Содержательная правка не является оценкой полезности: usefulness запрещён",
                ));
            }
        }
    }
    if event.action == FeedbackAction::Retract && event.effective_disposition.is_some() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Отзыв не назначает новый исход: effective_disposition должен быть пуст",
        ));
    }
    Ok(())
}

fn ensure_case_exists(
    write: &super::store::LearningWrite<'_>,
    review_id: &str,
    unit_id: &str,
    candidate_id: Option<&str>,
) -> Result<(), DomainError> {
    let found: Option<String> = write
        .transaction()
        .query_row(
            "SELECT unit_id FROM learning_unit WHERE review_id = ?1 AND unit_id = ?2",
            params![review_id, unit_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| map_error(&error, "не удалось проверить существование случая"))?;
    if found.is_none() {
        return Err(DomainError::new(
            ErrorCode::NotFound,
            format!("Случай learning не найден: {review_id}/{unit_id}"),
        ));
    }
    if let Some(candidate_id) = candidate_id {
        let found: Option<String> = write
            .transaction()
            .query_row(
                "SELECT candidate_id FROM learning_candidate
                 WHERE review_id = ?1 AND candidate_id = ?2",
                params![review_id, candidate_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| map_error(&error, "не удалось проверить существование кандидата"))?;
        if found.is_none() {
            return Err(DomainError::new(
                ErrorCode::NotFound,
                format!("Кандидат learning не найден: {candidate_id}"),
            ));
        }
    }
    Ok(())
}

fn conflicting_events(
    write: &super::store::LearningWrite<'_>,
    event: &FeedbackEvent,
) -> Result<Vec<String>, DomainError> {
    let mut statement = write
        .transaction()
        .prepare(
            "SELECT event.event_id FROM learning_feedback AS event
             WHERE event.review_id = ?1 AND event.unit_id = ?2 AND event.kind = ?3
               AND event.action <> 'retract'
               AND NOT EXISTS (
                   SELECT 1 FROM learning_feedback AS correction
                   WHERE correction.review_id = event.review_id
                     AND correction.unit_id = event.unit_id
                     AND correction.kind = event.kind
                     AND (
                         (correction.action = 'retract'
                          AND correction.retracted_event_id = event.event_id)
                         OR
                         (correction.action = 'supersede'
                          AND correction.supersedes_event_id = event.event_id)
                     )
               )
             ORDER BY event.recorded_at ASC, event.event_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать прежние утверждения"))?;
    let rows = statement
        .query_map(
            params![event.review_id, event.unit_id, event.kind.as_str()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| map_error(&error, "не удалось прочитать прежние утверждения"))?;
    let mut conflicting = Vec::new();
    for row in rows {
        conflicting.push(
            row.map_err(|error| map_error(&error, "не удалось прочитать прежние утверждения"))?,
        );
    }
    Ok(conflicting)
}

fn ensure_supersedable(
    write: &super::store::LearningWrite<'_>,
    event: &FeedbackEvent,
    target: &str,
) -> Result<(), DomainError> {
    let exists: Option<String> = write
        .transaction()
        .query_row(
            "SELECT event_id FROM learning_feedback
             WHERE event_id = ?1 AND review_id = ?2 AND unit_id = ?3 AND kind = ?4
               AND action <> 'retract'
               AND NOT EXISTS (
                   SELECT 1 FROM learning_feedback AS correction
                   WHERE correction.review_id = learning_feedback.review_id
                     AND correction.unit_id = learning_feedback.unit_id
                     AND correction.kind = learning_feedback.kind
                     AND (
                         (correction.action = 'retract'
                          AND correction.retracted_event_id = learning_feedback.event_id)
                         OR
                         (correction.action = 'supersede'
                          AND correction.supersedes_event_id = learning_feedback.event_id)
                     )
               )",
            params![target, event.review_id, event.unit_id, event.kind.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| map_error(&error, "не удалось проверить заменяемое утверждение"))?;
    if exists.is_none() {
        return Err(DomainError::new(
            ErrorCode::NotFound,
            format!("Заменяемое утверждение learning не найдено или уже не действует: {target}"),
        ));
    }
    Ok(())
}

fn guard_remaining_conflicts(
    write: &super::store::LearningWrite<'_>,
    event: &FeedbackEvent,
    target: &str,
) -> Result<(), DomainError> {
    let remaining: Vec<String> = conflicting_events(write, event)?
        .into_iter()
        .filter(|event_id| event_id != target)
        .collect();
    if !remaining.is_empty() && event.action == FeedbackAction::Supersede {
        return Err(conflict_error(event, &remaining));
    }
    Ok(())
}

fn conflict_error(event: &FeedbackEvent, conflicting: &[String]) -> DomainError {
    DomainError::with_details(
        ErrorCode::LearningConflict,
        format!(
            "Конфликтующее утверждение для того же случая: требуется явный supersede или retract; найдено {} утверждений",
            conflicting.len()
        ),
        crate::details! {
            "review_id" => event.review_id.clone(),
            "unit_id" => event.unit_id.clone(),
            "kind" => event.kind.as_str(),
            "conflicting_event_ids" => conflicting,
        },
    )
}

fn insert_event(
    write: &super::store::LearningWrite<'_>,
    event: &FeedbackEvent,
) -> Result<(), DomainError> {
    write.execute(
        "INSERT INTO learning_feedback (
            event_id, review_id, unit_id, candidate_id, kind, action, supersedes_event_id,
            retracted_event_id, effective_disposition, usefulness, explanation, provenance,
            recorded_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            event.event_id,
            event.review_id,
            event.unit_id,
            event.candidate_id,
            event.kind.as_str(),
            action_name(event.action),
            event.supersedes_event_id,
            match event.action {
                FeedbackAction::Retract => event.supersedes_event_id.clone(),
                _ => None,
            },
            event.effective_disposition,
            event.usefulness,
            super::import::sanitize_text(&event.explanation),
            event.provenance,
            event.recorded_at as i64,
        ],
    )?;
    Ok(())
}

fn action_name(action: FeedbackAction) -> &'static str {
    match action {
        FeedbackAction::Append => "append",
        FeedbackAction::Retract => "retract",
        FeedbackAction::Supersede => "supersede",
    }
}

/// Действующий исход случая с полным аудитом отозванных утверждений.
pub fn outcome(
    store: &LearningStore,
    review_id: &str,
    unit_id: &str,
) -> Result<FeedbackOutcome, DomainError> {
    store.read(|read| outcome_for(read, review_id, unit_id))
}

/// Проверяет, сохраняет ли событие действующий эффект в аудируемом журнале.
pub fn is_event_active(store: &LearningStore, event_id: &str) -> Result<bool, DomainError> {
    store.read(|read| {
        let active: Option<(String, bool)> = read
            .transaction()
            .query_row(
                "SELECT event.action,
                        NOT EXISTS (
                            SELECT 1 FROM learning_feedback AS correction
                            WHERE correction.review_id = event.review_id
                              AND correction.unit_id = event.unit_id
                              AND correction.kind = event.kind
                              AND (
                                  (correction.action = 'retract'
                                   AND correction.retracted_event_id = event.event_id)
                                  OR
                                  (correction.action = 'supersede'
                                   AND correction.supersedes_event_id = event.event_id)
                              )
                        )
                 FROM learning_feedback AS event
                 WHERE event.event_id = ?1",
                params![event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| map_error(&error, "не удалось проверить действие события"))?;
        match active {
            Some((action, active)) => Ok(action == "retract" || active),
            None => Err(DomainError::new(
                ErrorCode::NotFound,
                format!("Событие обратной связи не найдено: {event_id}"),
            )),
        }
    })
}

/// Публичное чтение события по идентификатору.
pub fn show_event(store: &LearningStore, event_id: &str) -> Result<FeedbackEvent, DomainError> {
    store.read(|read| {
        read.transaction()
            .query_row(
                "SELECT event_id, review_id, unit_id, candidate_id, kind, action,
                        supersedes_event_id, effective_disposition, usefulness, explanation,
                        provenance, recorded_at
                 FROM learning_feedback WHERE event_id = ?1",
                params![event_id],
                event_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => DomainError::new(
                    ErrorCode::NotFound,
                    format!("Событие обратной связи не найдено: {event_id}"),
                ),
                other => map_error(&other, "не удалось прочитать событие обратной связи"),
            })
    })
}

/// Перечисляет события обратной связи конкретного случая.
pub fn list_events(
    store: &LearningStore,
    review_id: &str,
    unit_id: &str,
) -> Result<Vec<FeedbackEvent>, DomainError> {
    store.read(|read| {
        let mut statement = read
            .transaction()
            .prepare(
                "SELECT event_id, review_id, unit_id, candidate_id, kind, action,
                        supersedes_event_id, effective_disposition, usefulness, explanation,
                        provenance, recorded_at
                 FROM learning_feedback WHERE review_id = ?1 AND unit_id = ?2
                 ORDER BY recorded_at ASC, event_id ASC",
            )
            .map_err(|error| map_error(&error, "не удалось прочитать события обратной связи"))?;
        let rows = statement
            .query_map(params![review_id, unit_id], event_from_row)
            .map_err(|error| map_error(&error, "не удалось прочитать события обратной связи"))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|error| {
                map_error(&error, "не удалось прочитать события обратной связи")
            })?);
        }
        Ok(events)
    })
}

/// Разбор строки события для переносимого архива.
pub(super) fn event_from_row_for_export(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<FeedbackEvent> {
    event_from_row(row)
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeedbackEvent> {
    let kind: String = row.get(4)?;
    let action: String = row.get(5)?;
    Ok(FeedbackEvent {
        schema_version: FEEDBACK_SCHEMA_VERSION,
        event_id: row.get(0)?,
        review_id: row.get(1)?,
        unit_id: row.get(2)?,
        candidate_id: row.get(3)?,
        kind: if kind == "semantic_outcome_revision" {
            FeedbackKind::SemanticOutcomeRevision
        } else {
            FeedbackKind::RecommendationUsefulness
        },
        action: match action.as_str() {
            "retract" => FeedbackAction::Retract,
            "supersede" => FeedbackAction::Supersede,
            _ => FeedbackAction::Append,
        },
        supersedes_event_id: row.get(6)?,
        effective_disposition: row.get(7)?,
        usefulness: row.get(8)?,
        explanation: row.get(9)?,
        provenance: row.get(10)?,
        recorded_at: u64::try_from(row.get::<_, i64>(11)?.max(0)).unwrap_or(0),
    })
}

fn outcome_for(
    read: &super::store::LearningTx<'_>,
    review_id: &str,
    unit_id: &str,
) -> Result<FeedbackOutcome, DomainError> {
    let transaction = read.transaction();
    let mut statement = transaction
        .prepare(
            "SELECT event_id, kind, action, supersedes_event_id, retracted_event_id,
                    effective_disposition
             FROM learning_feedback WHERE review_id = ?1 AND unit_id = ?2
             ORDER BY recorded_at ASC, event_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать исход случая"))?;
    let rows = statement
        .query_map(params![review_id, unit_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать исход случая"))?;
    let mut data = Vec::new();
    for row in rows {
        data.push(row.map_err(|error| map_error(&error, "не удалось прочитать исход случая"))?);
    }
    drop(statement);
    let retracted: BTreeSet<&str> = data
        .iter()
        .filter_map(|(_, _, action, _, retracted, _)| {
            (action == "retract")
                .then_some(retracted.as_deref())
                .flatten()
        })
        .collect();
    let superseded: BTreeSet<&str> = data
        .iter()
        .filter_map(|(_, _, action, supersedes, _, _)| {
            (action == "supersede")
                .then_some(supersedes.as_deref())
                .flatten()
        })
        .collect();
    let original = original_disposition(transaction, review_id, unit_id)?;
    let mut effective: Vec<(String, String)> = Vec::new();
    for (event_id, kind, action, _supersedes, _retracted_event, disposition) in &data {
        if kind != FeedbackKind::SemanticOutcomeRevision.as_str() {
            continue;
        }
        if action == "retract"
            || retracted.contains(event_id.as_str())
            || superseded.contains(event_id.as_str())
        {
            continue;
        }
        if let Some(value) = disposition {
            effective.push((event_id.clone(), value.clone()));
        }
    }
    // Конфликт не разрешается молча: он либо отвергается при записи,
    // либо отмечается здесь, если утверждение было отозвано после создания
    // второго действующего утверждения.
    let has_conflict = effective.len() > 1;
    let effective_disposition = effective.last().map(|(_, value)| value.clone());
    Ok(FeedbackOutcome {
        original_disposition: original,
        effective_disposition,
        effective_event_ids: effective.iter().map(|(id, _)| id.clone()).collect(),
        retracted_event_ids: retracted.iter().map(|id| (*id).to_owned()).collect(),
        has_conflict,
    })
}

fn original_disposition(
    transaction: &rusqlite::Transaction<'_>,
    review_id: &str,
    unit_id: &str,
) -> Result<Option<String>, DomainError> {
    transaction
        .query_row(
            "SELECT disposition FROM learning_unit WHERE review_id = ?1 AND unit_id = ?2",
            params![review_id, unit_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| map_error(&error, "не удалось прочитать исходное решение случая"))
        .map(|value: Option<Option<String>>| value.flatten())
}

/// Действующие исходы всех случаев с учётом аудируемых правок.
///
/// Пересчёт аналитики обязан видеть актуальные утверждения: содержательная
/// правка заменяет исход, а отозванная правка перестаёт на него влиять.
/// Отсутствие действующей правки означает, что сохраняется исход разбора.
pub(super) fn effective_dispositions(
    read: &super::store::LearningTx<'_>,
) -> Result<BTreeMap<(String, String), Option<String>>, DomainError> {
    let transaction = read.transaction();
    let mut original = BTreeMap::new();
    {
        let mut statement = transaction
            .prepare("SELECT review_id, unit_id, disposition FROM learning_unit")
            .map_err(|error| map_error(&error, "не удалось прочитать исходы единиц"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|error| map_error(&error, "не удалось прочитать исходы единиц"))?;
        for row in rows {
            let (review_id, unit_id, disposition) =
                row.map_err(|error| map_error(&error, "не удалось прочитать исходы единиц"))?;
            original.insert((review_id, unit_id), disposition);
        }
    }
    let mut inactive: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut revisions: BTreeMap<(String, String), Vec<EffectiveRevision>> = BTreeMap::new();
    {
        let mut statement = transaction
            .prepare(
                "SELECT review_id, unit_id, event_id, kind, action, supersedes_event_id,
                        retracted_event_id, effective_disposition, recorded_at
                 FROM learning_feedback ORDER BY recorded_at ASC, event_id ASC",
            )
            .map_err(|error| map_error(&error, "не удалось прочитать правки исходов"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(|error| map_error(&error, "не удалось прочитать правки исходов"))?;
        for row in rows {
            let (
                review_id,
                unit_id,
                event_id,
                kind,
                action,
                superseded_event,
                retracted_event,
                disposition,
                recorded_at,
            ) = row.map_err(|error| map_error(&error, "не удалось прочитать правки исходов"))?;
            let key = (review_id, unit_id);
            if action == "retract" {
                if let Some(retracted_event) = retracted_event {
                    inactive
                        .entry(key.clone())
                        .or_default()
                        .insert(retracted_event);
                }
            } else if action == "supersede"
                && let Some(superseded_event) = superseded_event
            {
                inactive
                    .entry(key.clone())
                    .or_default()
                    .insert(superseded_event);
            }
            if kind != FeedbackKind::SemanticOutcomeRevision.as_str() || action == "retract" {
                continue;
            }
            if let Some(disposition) = disposition {
                revisions.entry(key.clone()).or_default().push((
                    u64::try_from(recorded_at.max(0)).unwrap_or(0),
                    event_id,
                    disposition,
                ));
            }
        }
    }
    let mut effective = BTreeMap::new();
    for (key, original_disposition) in original {
        let inactive_for_case = inactive.remove(&key).unwrap_or_default();
        let active = revisions
            .remove(&key)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, event_id, _)| !inactive_for_case.contains(event_id))
            .map(|(_, _, disposition)| disposition)
            .next_back();
        effective.insert(key, active.or(original_disposition));
    }
    Ok(effective)
}

/// Строит объяснимое предложение постоянного правила.
///
/// Предложение никогда не применяется автоматически: оно перечисляет
/// подтверждающие и противоречащие случаи и требует явного утверждения
/// человеком через обычный контроль изменений.
pub fn propose_policy(
    store: &LearningStore,
    signature: &str,
    rule_id: &str,
    now: u64,
) -> Result<PolicyProposal, DomainError> {
    if rule_id.trim().is_empty() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Идентификатор предлагаемого правила не задан",
        ));
    }
    let generation = super::import::generation(store)?;
    let support = support_for_signature(store, signature, now)?;
    let cases = cases_for_signature(store, signature, 10, now)?;
    let (key, supporting, contradicting, cautions) = match support {
        None => (
            BTreeMap::new(),
            Vec::new(),
            Vec::new(),
            vec!["По этому ключу признаков история пуста: предложение непригодно.".to_owned()],
        ),
        Some(summary) => {
            let contradictory: BTreeSet<&str> = summary
                .contradicting_unit_ids
                .iter()
                .map(String::as_str)
                .collect();
            let mut supporting = Vec::new();
            let mut contradicting = Vec::new();
            for case in &cases {
                let reference: CaseRef = case.case.clone();
                if contradictory.contains(reference.unit_id.as_str()) {
                    contradicting.push(reference);
                } else {
                    supporting.push(reference);
                }
            }
            let mut cautions = vec![
                "Предложение не применяется автоматически и не выполняет suppression.".to_owned(),
                "Включение правила возможно только через утверждение человеком и обычный контроль изменений."
                    .to_owned(),
            ];
            match summary.level {
                SupportLevel::InsufficientEvidence => cautions.push(
                    "Поддержки недостаточно: предложение носит предварительный характер."
                        .to_owned(),
                ),
                SupportLevel::Contradictory => cautions.push(
                    "Есть противоречащие случаи: правило требует ручного разбора.".to_owned(),
                ),
                SupportLevel::Supported => {}
            }
            if summary.confirmed_units > 0 {
                cautions.push(format!(
                    "Среди поддерживающих единиц есть {} с решением confirmed: автоматическое правило недопустимо.",
                    summary.confirmed_units
                ));
            }
            let key = key_for_signature(store, signature)?;
            (key, supporting, contradicting, cautions)
        }
    };
    let proposal_id = sha256_hex(format!("{rule_id}\n{signature}").as_bytes());
    let document = PolicyProposal {
        schema_version: POLICY_SCHEMA_VERSION,
        policy_version: LEARNING_POLICY_VERSION,
        generation,
        proposal_id: format!("proposal-{}", &proposal_id[..32]),
        rule_id: rule_id.to_owned(),
        key,
        supporting_cases: supporting,
        contradicting_cases: contradicting,
        cautions,
        artifact_path: POLICY_ARTIFACT_PATH.to_owned(),
        auto_applied: false,
    };
    let _ = schema_version();
    store.write(|write| {
        write.execute(
            "INSERT OR REPLACE INTO learning_policy_proposal
                (proposal_id, rule_id, feature_json, document_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                document.proposal_id,
                document.rule_id,
                serde_json::to_string(&document.key).unwrap_or_else(|_| "{}".to_owned()),
                serde_json::to_string(&document).unwrap_or_else(|_| "{}".to_owned()),
            ],
        )?;
        Ok(())
    })?;
    Ok(document)
}

fn key_for_signature(
    store: &LearningStore,
    signature: &str,
) -> Result<BTreeMap<String, String>, DomainError> {
    store.read(|read| {
        let mut statement = read
            .transaction()
            .prepare(
                "SELECT u.feature_json FROM learning_unit AS u
                 JOIN learning_import AS i ON i.review_id = u.review_id
                 WHERE i.trust = 'ast_authenticated'
                 ORDER BY u.review_id ASC, u.unit_id ASC",
            )
            .map_err(|error| map_error(&error, "не удалось прочитать ключ признаков политики"))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| map_error(&error, "не удалось прочитать ключ признаков политики"))?;
        for row in rows {
            let feature_json = row.map_err(|error| {
                map_error(&error, "не удалось прочитать ключ признаков политики")
            })?;
            let key: BTreeMap<String, String> =
                super::import::parse_domain_json(&feature_json, "признаки единицы")?;
            if super::patterns::feature_signature(&key) == signature {
                return Ok(key);
            }
        }
        Ok(BTreeMap::new())
    })
}

/// Возвращает точный ключ признаков сохранённого предложения.
pub fn proposal_key_map(
    store: &LearningStore,
    proposal_id: &str,
) -> Result<BTreeMap<String, String>, DomainError> {
    let proposal = show_proposal(store, proposal_id)?;
    Ok(proposal.key)
}

/// Читает сохранённое предложение политики.
pub fn show_proposal(
    store: &LearningStore,
    proposal_id: &str,
) -> Result<PolicyProposal, DomainError> {
    store.read(|read| {
        let document: String = read
            .transaction()
            .query_row(
                "SELECT document_json FROM learning_policy_proposal WHERE proposal_id = ?1",
                params![proposal_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => DomainError::new(
                    ErrorCode::NotFound,
                    format!("Предложение политики не найдено: {proposal_id}"),
                ),
                other => map_error(&other, "не удалось прочитать предложение политики"),
            })?;
        serde_json::from_str(&document).map_err(|error| {
            DomainError::new(
                ErrorCode::LearningCorrupt,
                format!("Сохранённое предложение политики повреждено: {error}"),
            )
        })
    })
}

/// Перечисляет сохранённые предложения политики.
pub fn list_proposals(
    store: &LearningStore,
    limit: usize,
) -> Result<Vec<PolicyProposal>, DomainError> {
    store.read(|read| {
        let mut statement = read
            .transaction()
            .prepare(
                "SELECT document_json FROM learning_policy_proposal
                 ORDER BY proposal_id ASC LIMIT ?1",
            )
            .map_err(|error| map_error(&error, "не удалось прочитать предложения политики"))?;
        let rows = statement
            .query_map(params![limit as i64], |row| row.get::<_, String>(0))
            .map_err(|error| map_error(&error, "не удалось прочитать предложения политики"))?;
        let mut documents = Vec::new();
        for row in rows {
            let document = row
                .map_err(|error| map_error(&error, "не удалось прочитать предложения политики"))?;
            let proposal: PolicyProposal = serde_json::from_str(&document).map_err(|error| {
                DomainError::new(
                    ErrorCode::LearningCorrupt,
                    format!("Сохранённое предложение политики повреждено: {error}"),
                )
            })?;
            documents.push(proposal);
        }
        Ok(documents)
    })
}

/// Версия схемы предложений политики.
#[must_use]
pub const fn schema_version() -> u32 {
    LEARNING_SCHEMA_VERSION
}

/// Сериализуемое представление аудита по одному случаю.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseAudit {
    /// Запись ревью.
    pub review_id: String,
    /// Единица очереди.
    pub unit_id: String,
    /// Действующий исход и история правок.
    pub outcome: FeedbackOutcome,
    /// Полная история утверждений по случаю.
    pub events: Vec<FeedbackEvent>,
}

/// Возвращает полный аудит случая.
pub fn audit_case(
    store: &LearningStore,
    review_id: &str,
    unit_id: &str,
) -> Result<CaseAudit, DomainError> {
    Ok(CaseAudit {
        review_id: review_id.to_owned(),
        unit_id: unit_id.to_owned(),
        outcome: outcome(store, review_id, unit_id)?,
        events: list_events(store, review_id, unit_id)?,
    })
}

/// Проверяет, что доверие записи позволяет опираться на неё при пересчёте.
pub fn trusted(trust: TrustLevel) -> bool {
    trust.participates_in_learning()
}

/// Стабильный идентификатор события по содержанию.
#[must_use]
pub fn event_id_for(seed: &str) -> String {
    let digest = sha256_hex(seed.as_bytes());
    format!("event-{}", &digest[..32])
}

/// Пустая история правок для отчёта без обращений к хранилищу.
#[must_use]
pub fn empty_outcome() -> FeedbackOutcome {
    FeedbackOutcome {
        original_disposition: None,
        effective_disposition: None,
        effective_event_ids: Vec::new(),
        retracted_event_ids: Vec::new(),
        has_conflict: false,
    }
}

/// Подпись признаков, к которой относится предложение.
pub fn signature_of_key(key: &BTreeMap<String, String>) -> String {
    feature_signature(key)
}

/// Действующее распределение исходов по генерации истории.
pub fn outcome_distribution(store: &LearningStore) -> Result<BTreeMap<String, usize>, DomainError> {
    store.read(|read| {
        // Распределение считается по действующим исходам: правка и её отзыв
        // меняют статистику, а не только журнал событий.
        let effective = effective_dispositions(read)?;
        let mut statement = read
            .transaction()
            .prepare(
                "SELECT u.review_id, u.unit_id
                 FROM learning_unit AS u
                 JOIN learning_import AS i ON i.review_id = u.review_id
                 WHERE i.trust = 'ast_authenticated'",
            )
            .map_err(|error| map_error(&error, "не удалось прочитать распределение исходов"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                map_error(&error, "не удалось выполнить чтение распределения исходов")
            })?;
        let mut distribution: BTreeMap<String, usize> = BTreeMap::new();
        for row in rows {
            let (review_id, unit_id) = row
                .map_err(|error| map_error(&error, "не удалось прочитать распределение исходов"))?;
            let disposition = effective
                .get(&(review_id, unit_id))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "unreviewed".to_owned());
            *distribution.entry(disposition).or_default() += 1;
        }
        Ok(distribution)
    })
}

pub fn generation_of(store: &LearningStore) -> Result<HistoryGeneration, DomainError> {
    super::import::generation(store)
}
