//! Явный жизненный цикл локально сохранённой истории.
//!
//! Удаление одного review run атомарно убирает его производные записи и связи,
//! не трогая review artifacts, исходники, другие записи и утверждённые файлы
//! политики.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorCode};

use super::model::PolicyProposal;
use super::store::{LearningStore, LearningWrite};

/// Результат явного удаления одного случая истории.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetOutcome {
    /// Удалённая запись.
    pub review_id: String,
    /// Число удалённых зависимых строк по типу.
    pub removed: ForgetCounts,
    /// Ограничения удаления.
    pub limitations: Vec<String>,
}

/// Числа удалённых зависимых строк.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetCounts {
    pub reviews: usize,
    pub units: usize,
    pub candidates: usize,
    pub decisions: usize,
    pub findings: usize,
    pub finding_links: usize,
    pub case_links: usize,
    pub search_cases: usize,
    pub feedback_events: usize,
    pub policy_proposals: usize,
}

/// Удаляет одну запись истории и её производные данные в одной транзакции.
pub fn forget_review(store: &LearningStore, review_id: &str) -> Result<ForgetOutcome, DomainError> {
    if review_id.trim().is_empty() {
        return Err(DomainError::new(
            ErrorCode::InvalidRequest,
            "Идентификатор записи истории не может быть пустым",
        ));
    }
    store.write(|write| {
        let exists: Option<String> = write
            .transaction()
            .query_row(
                "SELECT review_id FROM learning_import WHERE review_id = ?1",
                params![review_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| super::store::map_error(&error, "не удалось найти запись истории"))?;
        if exists.is_none() {
            return Err(DomainError::new(
                ErrorCode::NotFound,
                format!("Запись истории learning не найдена: {review_id}"),
            ));
        }

        let proposal_ids = proposals_referencing(write, review_id)?;
        let case_links = delete_links(write, review_id)?;
        if store.fts5_available() {
            write.execute(
                "DELETE FROM learning_search_fts
                 WHERE case_id IN (SELECT case_id FROM learning_search WHERE review_id = ?1)",
                params![review_id],
            )?;
        }
        let search_cases = write.execute(
            "DELETE FROM learning_search WHERE review_id = ?1",
            params![review_id],
        )?;
        let feedback_events = write.execute(
            "DELETE FROM learning_feedback WHERE review_id = ?1",
            params![review_id],
        )?;
        let finding_links = write.execute(
            "DELETE FROM learning_finding_link WHERE review_id = ?1",
            params![review_id],
        )?;
        let findings = write.execute(
            "DELETE FROM learning_finding WHERE review_id = ?1",
            params![review_id],
        )?;
        let decisions = write.execute(
            "DELETE FROM learning_decision WHERE review_id = ?1",
            params![review_id],
        )?;
        let candidates = write.execute(
            "DELETE FROM learning_candidate WHERE review_id = ?1",
            params![review_id],
        )?;
        let units = write.execute(
            "DELETE FROM learning_unit WHERE review_id = ?1",
            params![review_id],
        )?;
        let mut policy_proposals = 0usize;
        for proposal_id in proposal_ids {
            policy_proposals += write.execute(
                "DELETE FROM learning_policy_proposal WHERE proposal_id = ?1",
                params![proposal_id],
            )?;
        }
        // Сшиваем соседей линии ревизий через удаляемую запись. Это сохраняет
        // одну текущую запись и при удалении середины цепочки A ← B ← C.
        write.execute(
            "UPDATE learning_import
             SET superseded_by = (SELECT superseded_by FROM learning_import WHERE review_id = ?1)
             WHERE superseded_by = ?1",
            params![review_id],
        )?;
        write.execute(
            "UPDATE learning_import
             SET revision_of = (SELECT revision_of FROM learning_import WHERE review_id = ?1)
             WHERE revision_of = ?1",
            params![review_id],
        )?;
        let reviews = write.execute(
            "DELETE FROM learning_import WHERE review_id = ?1",
            params![review_id],
        )?;
        Ok(ForgetOutcome {
            review_id: review_id.to_owned(),
            removed: ForgetCounts {
                reviews,
                units,
                candidates,
                decisions,
                findings,
                finding_links,
                case_links,
                search_cases,
                feedback_events,
                policy_proposals,
            },
            limitations: vec![
                "Локальные review artifacts и исходники не изменяются.".to_owned(),
                "Утверждённые policy files на диске не удаляются; предложения, ссылавшиеся на этот случай, удалены из базы.".to_owned(),
            ],
        })
    })
}

fn proposals_referencing(
    write: &LearningWrite<'_>,
    review_id: &str,
) -> Result<Vec<String>, DomainError> {
    let mut signature_statement = write
        .transaction()
        .prepare("SELECT DISTINCT signature FROM learning_unit WHERE review_id = ?1")
        .map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать подписи удаляемых единиц")
        })?;
    let signature_rows = signature_statement
        .query_map(params![review_id], |row| row.get::<_, String>(0))
        .map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать подписи удаляемых единиц")
        })?;
    let mut signatures = BTreeSet::new();
    for row in signature_rows {
        signatures.insert(row.map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать подписи удаляемых единиц")
        })?);
    }
    drop(signature_statement);

    const PAGE: i64 = 100;
    let mut offset = 0i64;
    let mut ids = Vec::new();
    loop {
        let mut statement = write
            .transaction()
            .prepare(
                "SELECT proposal_id, document_json FROM learning_policy_proposal
                 ORDER BY proposal_id ASC LIMIT ?1 OFFSET ?2",
            )
            .map_err(|error| {
                super::store::map_error(&error, "не удалось прочитать предложения политики")
            })?;
        let rows = statement
            .query_map(params![PAGE, offset], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                super::store::map_error(&error, "не удалось прочитать предложения политики")
            })?;
        let mut page = Vec::new();
        for row in rows {
            page.push(row.map_err(|error| {
                super::store::map_error(&error, "не удалось прочитать предложения политики")
            })?);
        }
        drop(statement);
        if page.is_empty() {
            break;
        }
        for (proposal_id, document) in page.iter() {
            let proposal: PolicyProposal = serde_json::from_str(document).map_err(|error| {
                DomainError::new(
                    ErrorCode::LearningCorrupt,
                    format!("Сохранённое предложение политики повреждено: {error}"),
                )
            })?;
            let references = proposal
                .supporting_cases
                .iter()
                .chain(&proposal.contradicting_cases)
                .any(|case| case.review_id == review_id)
                || signatures.contains(&super::patterns::feature_signature(&proposal.key));
            if references {
                ids.push(proposal_id.clone());
            }
        }
        offset += i64::try_from(page.len()).unwrap_or(PAGE);
    }
    Ok(ids)
}

fn delete_links(write: &LearningWrite<'_>, review_id: &str) -> Result<usize, DomainError> {
    write.execute(
        "DELETE FROM learning_case_link WHERE review_id = ?1 OR linked_review_id = ?1",
        params![review_id],
    )
}
