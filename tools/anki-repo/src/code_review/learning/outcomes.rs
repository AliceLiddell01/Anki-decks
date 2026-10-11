//! Единая агрегация исходных решений внутри структурной единицы.
//!
//! Метки полноты помещаются в существующий TEXT-столбец `disposition`:
//! новая схема SQLite или новый формат переносимого архива не требуются.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Transaction, params_from_iter};

use crate::error::DomainError;

use super::import::parse_domain_json;
use super::model::ImportRecord;
use super::store::map_error;

pub(super) type UnitKey = (String, String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnitOutcome {
    pub disposition: String,
    pub reason_code: Option<String>,
}

/// Только полное однородное покрытие представляет одно решение единицы.
/// Разные причины при одинаковом исходе не дают произвольного кода причины.
pub(super) fn aggregate<'a>(
    candidates: impl IntoIterator<Item = Option<(&'a str, &'a str)>>,
) -> UnitOutcome {
    let mut dispositions = BTreeSet::new();
    let mut reasons = BTreeSet::new();
    let mut unresolved = false;
    for candidate in candidates {
        if let Some((disposition, reason)) = candidate {
            dispositions.insert(disposition);
            reasons.insert(reason);
        } else {
            unresolved = true;
        }
    }
    let disposition = match (dispositions.len(), unresolved) {
        (0, _) => "unresolved",
        (1, false) => dispositions.first().copied().unwrap_or("unresolved"),
        (1, true) => "partial",
        (_, false) => "mixed",
        (_, true) => "mixed_partial",
    };
    UnitOutcome {
        disposition: disposition.to_owned(),
        reason_code: (is_homogeneous(Some(disposition)) && reasons.len() == 1)
            .then(|| reasons.first().copied().unwrap_or_default().to_owned()),
    }
}

pub(super) fn is_homogeneous(disposition: Option<&str>) -> bool {
    matches!(
        disposition,
        Some("confirmed" | "acceptable" | "false_positive" | "not_applicable" | "uncertain")
    )
}

/// Пересчитывает старые агрегаты из аудита, не меняя persistent store.
/// История без сохранённого membership остаётся в прежнем формате: данных
/// для доказательства другого исхода у неё нет. При наличии membership
/// отсутствие решений всегда означает `unresolved`, включая старые архивы.
pub(super) fn original_outcomes(
    transaction: &Transaction<'_>,
) -> Result<BTreeMap<UnitKey, UnitOutcome>, DomainError> {
    original_outcomes_filtered(transaction, None)
}

/// Пересчитывает исходы только для указанных ревью. Пакеты ограничены, чтобы
/// не зависеть от максимального числа параметров SQLite при экспорте истории.
pub(super) fn original_outcomes_for_reviews(
    transaction: &Transaction<'_>,
    reviews: &BTreeSet<&str>,
) -> Result<BTreeMap<UnitKey, UnitOutcome>, DomainError> {
    const REVIEW_BATCH_SIZE: usize = 500;
    let reviews: Vec<&str> = reviews.iter().copied().collect();
    let mut outcomes = BTreeMap::new();
    for batch in reviews.chunks(REVIEW_BATCH_SIZE) {
        outcomes.extend(original_outcomes_filtered(transaction, Some(batch))?);
    }
    Ok(outcomes)
}

fn original_outcomes_filtered(
    transaction: &Transaction<'_>,
    reviews: Option<&[&str]>,
) -> Result<BTreeMap<UnitKey, UnitOutcome>, DomainError> {
    let query = |select: &str| match reviews {
        Some(reviews) => format!(
            "{select} WHERE review_id IN ({})",
            vec!["?"; reviews.len()].join(",")
        ),
        None => select.to_owned(),
    };
    let parameters = || params_from_iter(reviews.unwrap_or_default().iter().copied());

    let mut statement = transaction
        .prepare(&query(
            "SELECT review_id, unit_id, disposition, reason_code FROM learning_unit",
        ))
        .map_err(|error| map_error(&error, "не удалось прочитать исходы единиц"))?;
    let rows = statement
        .query_map(parameters(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать исходы единиц"))?;
    let mut outcomes = BTreeMap::new();
    for row in rows {
        let (review, unit, disposition, reason_code) =
            row.map_err(|error| map_error(&error, "не удалось прочитать исход единицы"))?;
        outcomes.insert(
            (review, unit),
            UnitOutcome {
                disposition: disposition.unwrap_or_else(|| "unresolved".to_owned()),
                reason_code,
            },
        );
    }
    drop(statement);

    let mut statement = transaction
        .prepare(&query(
            "SELECT review_id, covered_json, disposition, reason_code FROM learning_decision",
        ))
        .map_err(|error| map_error(&error, "не удалось прочитать аудит решений"))?;
    let rows = statement
        .query_map(parameters(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать аудит решений"))?;
    let mut decisions = BTreeMap::new();
    for row in rows {
        let (review, covered, disposition, reason) =
            row.map_err(|error| map_error(&error, "не удалось прочитать аудит решения"))?;
        let covered: Vec<String> = parse_domain_json(&covered, "покрытие решения")?;
        for candidate in covered {
            decisions.insert(
                (review.clone(), candidate),
                (disposition.clone(), reason.clone()),
            );
        }
    }
    drop(statement);

    let mut statement = transaction
        .prepare(&query(
            "SELECT review_id, candidate_id, unit_id FROM learning_candidate",
        ))
        .map_err(|error| map_error(&error, "не удалось прочитать принадлежность кандидатов"))?;
    let rows = statement
        .query_map(parameters(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать принадлежность кандидатов"))?;
    let mut members: BTreeMap<UnitKey, Vec<Option<(&str, &str)>>> = BTreeMap::new();
    for row in rows {
        let (review, candidate, unit) =
            row.map_err(|error| map_error(&error, "не удалось прочитать кандидата единицы"))?;
        let decision = decisions
            .get(&(review.clone(), candidate))
            .map(|(disposition, reason)| (disposition.as_str(), reason.as_str()));
        members.entry((review, unit)).or_default().push(decision);
    }
    for (key, candidates) in members {
        outcomes.insert(key, aggregate(candidates));
    }
    Ok(outcomes)
}

/// Старые `observations_json` могли считать смешанную/частичную группу
/// рассмотренной. Публичное чтение и экспорт исправляют только этот агрегат.
pub(super) fn normalize_records(
    transaction: &Transaction<'_>,
    records: &mut [ImportRecord],
) -> Result<(), DomainError> {
    let reviews: BTreeSet<&str> = records
        .iter()
        .map(|record| record.review_id.as_str())
        .collect();
    let outcomes = original_outcomes_for_reviews(transaction, &reviews)?;
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for ((review, _), outcome) in &outcomes {
        let count = counts.entry(review.as_str()).or_default();
        if is_homogeneous(Some(&outcome.disposition)) {
            count.0 += 1;
        } else {
            count.1 += 1;
        }
    }
    for record in records {
        if let Some((reviewed, unresolved)) = counts.get(record.review_id.as_str()) {
            record.observations.reviewed_units = *reviewed;
            record.observations.unresolved_units = *unresolved;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    #[test]
    fn original_outcomes_can_be_limited_to_requested_reviews() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE learning_unit (
                    review_id TEXT, unit_id TEXT, disposition TEXT, reason_code TEXT
                );
                 CREATE TABLE learning_decision (
                    review_id TEXT, covered_json TEXT, disposition TEXT, reason_code TEXT
                );
                 CREATE TABLE learning_candidate (
                    review_id TEXT, candidate_id TEXT, unit_id TEXT
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO learning_unit VALUES (?1, ?2, ?3, ?4)",
                params![
                    "review-one",
                    "unit-one",
                    "unresolved",
                    Option::<String>::None
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO learning_unit VALUES (?1, ?2, ?3, ?4)",
                params![
                    "review-two",
                    "unit-two",
                    "unresolved",
                    Option::<String>::None
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO learning_decision VALUES (?1, ?2, ?3, ?4)",
                params![
                    "review-one",
                    "[\"candidate-one\"]",
                    "confirmed",
                    "supported"
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO learning_candidate VALUES (?1, ?2, ?3)",
                params!["review-one", "candidate-one", "unit-one"],
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();

        let filtered =
            original_outcomes_for_reviews(&transaction, &BTreeSet::from(["review-one"])).unwrap();

        assert_eq!(filtered.len(), 1);
        assert_eq!(
            filtered.get(&("review-one".to_owned(), "unit-one".to_owned())),
            Some(&UnitOutcome {
                disposition: "confirmed".to_owned(),
                reason_code: Some("supported".to_owned()),
            })
        );
    }
}
