//! Перенос и восстановление проверенной истории learning.
//!
//! Различаются три разные вещи:
//!
//! 1. полная локальная история и её транзакционный backup (`VACUUM INTO`) —
//!    ответственность [`super::store`];
//! 2. переносимый архив learning evidence — этот модуль;
//! 3. версионируемые утверждённые policy artifacts в Git — их материализует
//!    человек, SQLite не становится их владельцем.
//!
//! Архив содержит версионированный manifest с digest канонического тела и не
//! включает сырые execution logs, credentials, приватные артефакты, содержимое
//! временных worktrees и произвольный код. Все пути внутри архива
//! repo-relative: переносимый архив не зависит от абсолютного пути клона.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorCode};

use super::LEARNING_POLICY_VERSION;
use super::import::{parse_domain_json, sha256_hex};
use super::model::{
    ExportedCandidate, ExportedCaseLink, ExportedDecision, ExportedFinding, ExportedFindingLink,
    ExportedSearchCase, FeedbackEvent, HistoryGeneration, ImportInputs, ImportRecord,
    LearningExport, LearningExportManifest, ObservationCounts, PolicyProposal, ReviewUnitKind,
    ReviewUnitRecord,
};
use super::schema::LEARNING_SCHEMA_VERSION;
use super::store::{LearningRead, LearningStore, map_error};

/// Четвёртая версия переносит исходные решения ревьюера и их полное покрытие.
pub const EXPORT_SCHEMA_VERSION: u32 = 4;
/// Третья версия исключила source snippets из переносимой истории.
const EXPORT_SCHEMA_VERSION_WITHOUT_SNIPPETS: u32 = 3;

/// Первая версия архива: переносятся только исходные записи, без поиска.
const EXPORT_SCHEMA_VERSION_WITHOUT_SEARCH: u32 = 1;
/// Вторая версия добавила поисковые случаи и могла содержать source snippets.
const EXPORT_SCHEMA_VERSION_WITH_SEARCH: u32 = 2;

/// Результат экспорта истории.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSummary {
    /// Manifest архива.
    pub manifest: LearningExportManifest,
    /// Переносимый архив.
    pub archive: LearningExport,
}

/// Результат восстановления архива.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreSummary {
    /// Число восстановленных записей ревью.
    pub restored_reviews: usize,
    /// Число записей, уже присутствовавших без изменений.
    pub unchanged_reviews: usize,
    /// Генерация истории после восстановления.
    pub generation: HistoryGeneration,
    /// Ограничения доверия к восстановленной истории.
    pub limitations: Vec<String>,
}

fn candidate_unit_index(archive: &LearningExport) -> BTreeMap<(&str, &str), &str> {
    archive
        .candidates
        .iter()
        .map(|candidate| {
            (
                (
                    candidate.review_id.as_str(),
                    candidate.candidate_id.as_str(),
                ),
                candidate.unit_id.as_str(),
            )
        })
        .collect()
}

/// Собирает переносимый архив проверенной истории.
pub fn export_history(store: &LearningStore) -> Result<ExportSummary, DomainError> {
    let (generation, tables) = store.read(|read| {
        let generation = super::import::generation_of(read)?;
        Ok((generation, read_tables(read)?))
    })?;
    let (
        reviews,
        units,
        candidates,
        decisions,
        findings,
        links,
        case_links,
        feedback,
        proposals,
        search,
    ) = tables;
    let mut archive = LearningExport {
        manifest: LearningExportManifest {
            export_schema_version: EXPORT_SCHEMA_VERSION,
            schema_version: LEARNING_SCHEMA_VERSION,
            policy_version: LEARNING_POLICY_VERSION,
            generation,
            reviews: reviews.len(),
            units: units.len(),
            findings: findings.len(),
            feedback_events: feedback.len(),
            policy_proposals: proposals.len(),
            search_cases: search.len(),
            payload_sha256: String::new(),
        },
        reviews,
        units,
        candidates,
        decisions,
        findings,
        finding_links: links,
        case_links,
        feedback_events: feedback,
        policy_proposals: proposals,
        search,
    };
    archive.manifest.payload_sha256 = payload_digest(&archive)?;
    verify_export(&archive)?;
    Ok(ExportSummary {
        manifest: archive.manifest.clone(),
        archive,
    })
}

/// Проверяет manifest, digest архива и ссылки между его записями.
pub fn verify_export(archive: &LearningExport) -> Result<(), DomainError> {
    if !matches!(
        archive.manifest.export_schema_version,
        EXPORT_SCHEMA_VERSION
            | EXPORT_SCHEMA_VERSION_WITHOUT_SNIPPETS
            | EXPORT_SCHEMA_VERSION_WITH_SEARCH
            | EXPORT_SCHEMA_VERSION_WITHOUT_SEARCH
    ) {
        return Err(invalid(format!(
            "Неподдерживаемая версия схемы архива learning: {}",
            archive.manifest.export_schema_version
        )));
    }
    if archive.manifest.schema_version > LEARNING_SCHEMA_VERSION {
        return Err(DomainError::new(
            ErrorCode::LearningSchemaUnsupported,
            format!(
                "Архив создан более новой схемой learning ({}); эта сборка поддерживает {LEARNING_SCHEMA_VERSION}",
                archive.manifest.schema_version
            ),
        ));
    }
    if archive.manifest.export_schema_version < EXPORT_SCHEMA_VERSION
        && !archive.decisions.is_empty()
    {
        return Err(invalid(
            "Архив старой версии не может содержать решения версии 4",
        ));
    }
    let expected = payload_digest(archive)?;
    if expected != archive.manifest.payload_sha256 {
        return Err(invalid(format!(
            "Digest тела архива learning не совпадает: ожидался {}, получен {expected}",
            archive.manifest.payload_sha256
        )));
    }
    if archive.manifest.reviews != archive.reviews.len()
        || archive.manifest.units != archive.units.len()
        || archive.manifest.findings != archive.findings.len()
        || archive.manifest.feedback_events != archive.feedback_events.len()
        || archive.manifest.policy_proposals != archive.policy_proposals.len()
        || archive.manifest.search_cases != archive.search.len()
    {
        return Err(invalid(
            "Числа в manifest архива learning не совпадают с телом архива",
        ));
    }
    let review_ids: BTreeMap<&str, &ImportRecord> = archive
        .reviews
        .iter()
        .map(|record| (record.review_id.as_str(), record))
        .collect();
    if review_ids.len() != archive.reviews.len() {
        return Err(invalid("Повтор review_id в архиве learning"));
    }
    verify_review_lineage(&review_ids)?;
    let unit_ids: BTreeSet<(&str, &str)> = archive
        .units
        .iter()
        .map(|unit| (unit.review_id.as_str(), unit.unit_id.as_str()))
        .collect();
    let candidate_units = candidate_unit_index(archive);
    let finding_ids: BTreeSet<(&str, &str)> = archive
        .findings
        .iter()
        .map(|finding| (finding.review_id.as_str(), finding.finding_id.as_str()))
        .collect();
    if unit_ids.len() != archive.units.len()
        || candidate_units.len() != archive.candidates.len()
        || finding_ids.len() != archive.findings.len()
    {
        return Err(invalid(
            "Повтор ID единицы, кандидата или замечания в архиве learning",
        ));
    }
    let finding_links: BTreeSet<_> = archive
        .finding_links
        .iter()
        .map(|link| (&link.review_id, &link.candidate_id, &link.finding_id))
        .collect();
    let case_links: BTreeSet<_> = archive
        .case_links
        .iter()
        .map(|link| {
            (
                &link.review_id,
                &link.finding_id,
                &link.linked_review_id,
                &link.linked_finding_id,
                link.kind.as_str(),
            )
        })
        .collect();
    let proposal_ids: BTreeSet<_> = archive
        .policy_proposals
        .iter()
        .map(|proposal| &proposal.proposal_id)
        .collect();
    if finding_links.len() != archive.finding_links.len()
        || case_links.len() != archive.case_links.len()
        || proposal_ids.len() != archive.policy_proposals.len()
    {
        return Err(invalid(
            "Повтор связи или предложения политики в архиве learning",
        ));
    }
    for unit in &archive.units {
        if !review_ids.contains_key(unit.review_id.as_str()) {
            return Err(invalid(format!(
                "Единица архива ссылается на отсутствующую запись: {}",
                unit.review_id
            )));
        }
        if let Some(marker) = unit.feature_map().get("classifier_compatibility")
            && marker != &review_ids[unit.review_id.as_str()].inputs.classifier_digest
        {
            return Err(invalid(
                "Версия classifier в признаках единицы не совпадает с ревью",
            ));
        }
    }
    for finding in &archive.findings {
        if !review_ids.contains_key(finding.review_id.as_str()) {
            return Err(invalid(format!(
                "Замечание архива ссылается на отсутствующую запись: {}",
                finding.review_id
            )));
        }
    }
    for link in &archive.finding_links {
        if !review_ids.contains_key(link.review_id.as_str())
            || !candidate_units.contains_key(&(link.review_id.as_str(), link.candidate_id.as_str()))
            || !finding_ids.contains(&(link.review_id.as_str(), link.finding_id.as_str()))
        {
            return Err(invalid(
                "Связь кандидата и замечания архива ссылается на отсутствующую запись, кандидата или замечание",
            ));
        }
    }
    for candidate in &archive.candidates {
        // Структурная проверка пути не зависит от записей архива и идёт первой:
        // перенесённый путь обязан оставаться repo-relative сам по себе.
        reject_unportable_path(&candidate.path)?;
        if !review_ids.contains_key(candidate.review_id.as_str()) {
            return Err(invalid(format!(
                "Кандидат архива ссылается на отсутствующую запись: {}",
                candidate.review_id
            )));
        }
        if !unit_ids.contains(&(candidate.review_id.as_str(), candidate.unit_id.as_str())) {
            return Err(invalid(
                "Кандидат архива ссылается на отсутствующую единицу",
            ));
        }
        if archive.manifest.export_schema_version >= EXPORT_SCHEMA_VERSION_WITHOUT_SNIPPETS
            && candidate.snippet.is_some()
        {
            return Err(invalid(
                "Архив текущей версии не должен содержать фрагменты исходного кода",
            ));
        }
    }
    for link in &archive.case_links {
        if !review_ids.contains_key(link.review_id.as_str())
            || !review_ids.contains_key(link.linked_review_id.as_str())
            || !finding_ids.contains(&(link.review_id.as_str(), link.finding_id.as_str()))
            || !finding_ids.contains(&(
                link.linked_review_id.as_str(),
                link.linked_finding_id.as_str(),
            ))
        {
            return Err(invalid(
                "Связь случаев архива ссылается на отсутствующую запись",
            ));
        }
    }
    let mut feedback_ids = BTreeSet::new();
    for event in &archive.feedback_events {
        if !feedback_ids.insert(event.event_id.as_str()) {
            return Err(invalid(format!(
                "Повтор event_id в событиях обратной связи архива: {}",
                event.event_id
            )));
        }
        if !review_ids.contains_key(event.review_id.as_str())
            || !unit_ids.contains(&(event.review_id.as_str(), event.unit_id.as_str()))
            || event.candidate_id.as_deref().is_some_and(|candidate_id| {
                candidate_units
                    .get(&(event.review_id.as_str(), candidate_id))
                    .copied()
                    != Some(event.unit_id.as_str())
            })
        {
            return Err(invalid(
                "Событие обратной связи архива ссылается на отсутствующую запись, единицу или кандидата",
            ));
        }
    }
    verify_decisions(archive, &review_ids)?;
    super::feedback::validate_event_history(&archive.feedback_events)
        .map_err(|error| invalid(error.message))?;
    verify_search_cases(archive, &review_ids, &candidate_units)?;
    Ok(())
}

/// Преемственность ревью переносится как проверяемая связь одного Git-случая.
fn verify_review_lineage(review_ids: &BTreeMap<&str, &ImportRecord>) -> Result<(), DomainError> {
    for record in review_ids.values() {
        let mut current = *record;
        let mut seen = BTreeSet::new();
        while let Some(parent_id) = current.revision_of.as_deref() {
            if !seen.insert(current.review_id.as_str()) {
                return Err(invalid("Цикл преемственности ревью в архиве learning"));
            }
            let parent = review_ids
                .get(parent_id)
                .ok_or_else(|| invalid("Преемственность ревью ссылается на отсутствующее ревью"))?;
            if parent.repository_id != current.repository_id
                || parent.base_sha != current.base_sha
                || parent.merge_base_sha != current.merge_base_sha
                || parent.workspace_variant != current.workspace_variant
                || parent.trust != current.trust
            {
                return Err(invalid(
                    "Преемственность ревью пересекает Git-случаи или границу доверия",
                ));
            }
            current = parent;
        }
    }
    Ok(())
}

/// Проверяет покрытие исходных решений, не выводя их из итогов единиц.
fn verify_decisions(
    archive: &LearningExport,
    review_ids: &BTreeMap<&str, &ImportRecord>,
) -> Result<(), DomainError> {
    use crate::code_review::semantic_triage::{Disposition, ReasonCode};
    let candidates: BTreeMap<(&str, &str), &ExportedCandidate> = archive
        .candidates
        .iter()
        .map(|candidate| {
            (
                (
                    candidate.review_id.as_str(),
                    candidate.candidate_id.as_str(),
                ),
                candidate,
            )
        })
        .collect();
    let mut decision_ids = BTreeSet::new();
    let mut covered_ids = BTreeSet::new();
    for decision in &archive.decisions {
        if decision.decision_id.trim().is_empty()
            || !review_ids.contains_key(decision.review_id.as_str())
            || !decision_ids.insert((decision.review_id.as_str(), decision.decision_id.as_str()))
        {
            return Err(invalid(
                "Решение архива имеет пустой/повторный ID или отсутствующее ревью",
            ));
        }
        if !matches!(decision.kind.as_str(), "individual" | "group")
            || decision.candidate_count != decision.covered_candidate_ids.len()
            || (decision.kind == "individual" && decision.candidate_count != 1)
            || (decision.kind == "group" && decision.candidate_count < 2)
        {
            return Err(invalid(
                "Вид или размер покрытия решения архива некорректен",
            ));
        }
        serde_json::from_value::<Disposition>(serde_json::Value::String(
            decision.disposition.clone(),
        ))
        .map_err(|_| invalid("Неизвестный семантический исход решения архива"))?;
        serde_json::from_value::<ReasonCode>(serde_json::Value::String(
            decision.reason_code.clone(),
        ))
        .map_err(|_| invalid("Неизвестная причина решения архива"))?;
        if decision.explanation.trim().is_empty()
            || decision.explanation.len() > super::import::MAX_STORED_TEXT_BYTES
        {
            return Err(invalid(
                "Объяснение решения архива пусто или превышает предел хранения",
            ));
        }
        for candidate_id in &decision.covered_candidate_ids {
            let key = (decision.review_id.as_str(), candidate_id.as_str());
            if !candidates.contains_key(&key) || !covered_ids.insert(key) {
                return Err(invalid(
                    "Решение архива покрывает отсутствующего или повторно покрытого кандидата",
                ));
            }
        }
    }
    Ok(())
}

/// Проверяет, что путь архива остаётся repo-relative, разбирая компоненты.
///
/// Подстрочные эвристики здесь не годятся: законный путь `tools/home/x.rs`
/// ложно отвергался бы, а `..` — не ловился. Компоненты разбираются явно:
/// корень, префикс тома (Windows) и переход к родителю запрещены, поэтому
/// перенесённый путь не может указывать за пределы клона.
fn reject_unportable_path(raw: &str) -> Result<(), DomainError> {
    // Обратные слэши нормализуются: архив может быть создан на Windows.
    let normalized = raw.replace('\\', "/");
    let mut components = Path::new(&normalized).components();
    let mut first = true;
    for component in components.by_ref() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                let drive_prefix = first
                    && name.len() == 2
                    && name.as_bytes()[1] == b':'
                    && name.as_bytes()[0].is_ascii_alphabetic();
                if drive_prefix {
                    return Err(invalid(format!(
                        "Путь архива learning содержит абсолютный префикс тома: {raw}"
                    )));
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(invalid(format!(
                    "Путь архива learning не является repo-relative: {raw}"
                )));
            }
        }
        first = false;
    }
    if raw.is_empty() || normalized.is_empty() || normalized == "/" {
        return Err(invalid(format!(
            "Путь архива learning пуст и не является repo-relative: {raw}"
        )));
    }
    Ok(())
}

/// Проверяет, что поисковые случаи ссылаются только на перенесённые записи.
fn verify_search_cases(
    archive: &LearningExport,
    review_ids: &BTreeMap<&str, &ImportRecord>,
    candidate_units: &BTreeMap<(&str, &str), &str>,
) -> Result<(), DomainError> {
    let units: BTreeSet<(&str, &str)> = archive
        .units
        .iter()
        .map(|unit| (unit.review_id.as_str(), unit.unit_id.as_str()))
        .collect();
    let findings: BTreeSet<(&str, &str)> = archive
        .findings
        .iter()
        .map(|finding| (finding.review_id.as_str(), finding.finding_id.as_str()))
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for case in &archive.search {
        if !review_ids.contains_key(case.review_id.as_str()) {
            return Err(invalid(format!(
                "Поисковый случай архива ссылается на отсутствующую запись: {}",
                case.review_id
            )));
        }
        if !seen.insert(case.case_id.as_str()) {
            return Err(invalid(format!(
                "Повтор case_id в поисковых случаях архива: {}",
                case.case_id
            )));
        }
        if !matches!(case.kind.as_str(), "finding" | "decision" | "group") {
            return Err(invalid(format!(
                "Неизвестный вид поискового случая архива: {}",
                case.kind
            )));
        }
        if !case.unit_id.is_empty()
            && !units.contains(&(case.review_id.as_str(), case.unit_id.as_str()))
        {
            return Err(invalid(format!(
                "Поисковый случай архива ссылается на отсутствующую единицу: {}",
                case.unit_id
            )));
        }
        if let Some(candidate_id) = case.candidate_id.as_deref()
            && (candidate_units
                .get(&(case.review_id.as_str(), candidate_id))
                .copied()
                .is_none_or(|unit_id| !case.unit_id.is_empty() && unit_id != case.unit_id))
        {
            return Err(invalid(format!(
                "Поисковый случай архива ссылается на отсутствующего кандидата: {candidate_id}"
            )));
        }
        if let Some(finding_id) = case.finding_id.as_deref()
            && !findings.contains(&(case.review_id.as_str(), finding_id))
        {
            return Err(invalid(format!(
                "Поисковый случай архива ссылается на отсутствующее замечание: {finding_id}"
            )));
        }
        if case.text.is_empty() || case.text.len() > super::import::MAX_STORED_TEXT_BYTES {
            return Err(invalid(
                "Текст поискового случая архива пуст или превышает предел хранения",
            ));
        }
    }
    Ok(())
}

/// Восстанавливает историю из переносимого архива без потери provenance.
pub fn restore_history(
    store: &LearningStore,
    archive: &LearningExport,
) -> Result<RestoreSummary, DomainError> {
    verify_export(archive)?;
    let candidate_units = candidate_unit_index(archive);
    let finding_units = finding_unit_index(archive, &candidate_units);
    let summary = store.write(|write| {
        let mut restored = 0usize;
        let mut unchanged = 0usize;
        let mut restored_ids = BTreeSet::new();
        for record in &archive.reviews {
            let existing: Option<String> = super::import::write_optional_row(
                write,
                "SELECT review_pack_sha256 FROM learning_import WHERE review_id = ?1",
                params![record.review_id],
                |row| row.get(0),
            )?;
            if let Some(existing_sha) = existing {
                if existing_sha == record.inputs.review_pack_sha256 {
                    unchanged += 1;
                    continue;
                }
                return Err(DomainError::with_details(
                    ErrorCode::LearningConflict,
                    "Архив learning содержит конфликтующую ревизию существующего review_id",
                    crate::details! {
                        "review_id" => record.review_id.clone(),
                        "existing_review_pack_sha256" => existing_sha,
                        "archive_review_pack_sha256" => record.inputs.review_pack_sha256.clone(),
                    },
                ));
            }
            insert_record(write, record)?;
            restored_ids.insert(record.review_id.as_str());
            restored += 1;
        }
        for unit in &archive.units {
            if restored_ids.contains(unit.review_id.as_str()) {
                insert_unit(write, unit)?;
            }
        }
        for candidate in &archive.candidates {
            if restored_ids.contains(candidate.review_id.as_str()) {
                insert_candidate(write, candidate)?;
            }
        }
        for decision in &archive.decisions {
            if restored_ids.contains(decision.review_id.as_str()) {
                insert_decision(write, decision)?;
            }
        }
        for finding in &archive.findings {
            if restored_ids.contains(finding.review_id.as_str()) {
                insert_finding(write, finding, &finding_units)?;
            }
        }
        for link in &archive.finding_links {
            if restored_ids.contains(link.review_id.as_str()) {
                write.execute(
                    "INSERT OR REPLACE INTO learning_finding_link (review_id, candidate_id, finding_id)
                     VALUES (?1, ?2, ?3)",
                    params![link.review_id, link.candidate_id, link.finding_id],
                )?;
            }
        }
        for link in &archive.case_links {
            if restored_ids.contains(link.review_id.as_str()) {
                write.execute(
                    "INSERT OR REPLACE INTO learning_case_link
                        (review_id, finding_id, linked_review_id, linked_finding_id, kind, basis)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        link.review_id,
                        link.finding_id,
                        link.linked_review_id,
                        link.linked_finding_id,
                        link.kind.as_str(),
                        link.basis,
                    ],
                )?;
            }
        }
        for event in &archive.feedback_events {
            if restored_ids.contains(event.review_id.as_str()) {
                write.execute(
                    "INSERT INTO learning_feedback (
                        event_id, review_id, unit_id, candidate_id, kind, action,
                        supersedes_event_id, retracted_event_id, effective_disposition, usefulness,
                        explanation, provenance, recorded_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        event.event_id,
                        event.review_id,
                        event.unit_id,
                        event.candidate_id,
                        event.kind.as_str(),
                        match event.action {
                            super::model::FeedbackAction::Retract => "retract",
                            super::model::FeedbackAction::Supersede => "supersede",
                            super::model::FeedbackAction::Append => "append",
                        },
                        event.supersedes_event_id,
                        event
                            .supersedes_event_id
                            .clone()
                            .filter(|_| event.action == super::model::FeedbackAction::Retract),
                        event.effective_disposition,
                        event.usefulness,
                        event.explanation,
                        event.provenance,
                        event.recorded_at as i64,
                    ],
                )?;
            }
        }
        for proposal in &archive.policy_proposals {
            let feature_json = super::import::serialize_json(
                &proposal.key,
                "не удалось сериализовать признаки предложения",
            )?;
            let document_json = super::import::serialize_json(
                proposal,
                "не удалось сериализовать предложение политики",
            )?;
            write.execute(
                "INSERT OR IGNORE INTO learning_policy_proposal
                    (proposal_id, rule_id, feature_json, document_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    proposal.proposal_id,
                    proposal.rule_id,
                    feature_json,
                    document_json,
                ],
            )?;
        }
        for case in &archive.search {
            if restored_ids.contains(case.review_id.as_str()) {
                let local_case: Option<String> = super::import::write_optional_row(
                    write,
                    "SELECT review_id FROM learning_search WHERE case_id = ?1",
                    params![case.case_id],
                    |row| row.get(0),
                )?;
                if local_case.is_some() {
                    return Err(invalid("Поисковый case_id архива конфликтует с локальной историей"));
                }
                super::import::insert_search(
                    write,
                    &super::import::SearchRow {
                        case_id: &case.case_id,
                        review_id: &case.review_id,
                        unit_id: &case.unit_id,
                        candidate_id: case.candidate_id.as_deref(),
                        finding_id: case.finding_id.as_deref(),
                        kind: &case.kind,
                        disposition: case.disposition.as_deref(),
                        severity: case.severity.as_deref(),
                        provenance: case.provenance.as_deref(),
                        text: &case.text,
                    },
                )?;
            }
        }
        // Проверяем объединённую историю до commit: глобальный event_id не
        // должен молча заменить локальное событие другого ревью.
        super::feedback::validate_event_history(&read_feedback(write.transaction())?)
            .map_err(|error| invalid(error.message))?;
        Ok((restored, unchanged))
    })?;
    Ok(RestoreSummary {
        restored_reviews: summary.0,
        unchanged_reviews: summary.1,
        generation: super::import::generation(store)?,
        limitations: vec![
            "Восстановление сохраняет provenance, версии схем, единицы наблюдения и статус доверия."
                .to_owned(),
            "Путь прежнего клона не требуется: архив содержит только repo-relative пути и Git identity."
                .to_owned(),
            search_limitation(archive).to_owned(),
            decisions_limitation(archive).to_owned(),
        ],
    })
}

fn insert_record(
    write: &super::store::LearningWrite<'_>,
    record: &ImportRecord,
) -> Result<(), DomainError> {
    let identity_key = super::import::identity_key_for(
        &record.repository_id,
        &record.merge_base_sha,
        &record.head_sha,
        &record.workspace_variant,
        &record.inputs.analyzer_digest,
    );
    let execution_evidence = record
        .inputs
        .execution_evidence
        .as_ref()
        .map(|evidence| {
            super::import::serialize_json(
                evidence,
                "не удалось сериализовать execution evidence из архива",
            )
        })
        .transpose()?;
    let limitations = super::import::serialize_json(
        &record.limitations,
        "не удалось сериализовать ограничения импорта из архива",
    )?;
    let observations = super::import::serialize_json(
        &record.observations,
        "не удалось сериализовать счётчики наблюдений из архива",
    )?;
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

fn insert_unit(
    write: &super::store::LearningWrite<'_>,
    unit: &ReviewUnitRecord,
) -> Result<(), DomainError> {
    let representatives = super::import::serialize_json(
        &unit.representative_candidate_ids,
        "не удалось сериализовать представителей единицы из архива",
    )?;
    let surfaces = super::import::serialize_json(
        &unit.surfaces,
        "не удалось сериализовать поверхности единицы из архива",
    )?;
    let feature_json = super::import::serialize_json(
        unit.feature_map(),
        "не удалось сериализовать признаки единицы из архива",
    )?;
    write.execute(
        "INSERT OR REPLACE INTO learning_unit (
            review_id, unit_id, kind, candidate_count, priority, representatives_json,
            disposition, reason_code, detector, source, role, code_role, surfaces_json,
            signature, feature_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            unit.review_id,
            unit.unit_id,
            unit.kind.as_str(),
            unit.candidate_count as i64,
            unit.priority,
            representatives,
            unit.disposition,
            unit.reason_code,
            unit.detector,
            unit.source,
            unit.role,
            unit.code_role,
            surfaces,
            unit.signature(),
            feature_json,
        ],
    )?;
    Ok(())
}

fn insert_decision(
    write: &super::store::LearningWrite<'_>,
    decision: &ExportedDecision,
) -> Result<(), DomainError> {
    let covered = super::import::serialize_json(
        &decision.covered_candidate_ids,
        "не удалось сериализовать покрытие решения из архива",
    )?;
    write.execute(
        "INSERT INTO learning_decision (
            review_id, decision_id, kind, disposition, reason_code, explanation,
            candidate_count, covered_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            decision.review_id,
            decision.decision_id,
            decision.kind,
            decision.disposition,
            decision.reason_code,
            decision.explanation,
            decision.candidate_count as i64,
            covered
        ],
    )?;
    Ok(())
}

fn insert_candidate(
    write: &super::store::LearningWrite<'_>,
    candidate: &ExportedCandidate,
) -> Result<(), DomainError> {
    let execution = classification_execution(&candidate.classification_json);
    write.execute(
        "INSERT OR REPLACE INTO learning_candidate (
            review_id, candidate_id, unit_id, detector, source, path, path_family, origin,
            execution, snippet, classification_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            candidate.review_id,
            candidate.candidate_id,
            candidate.unit_id,
            candidate.detector,
            candidate.source,
            candidate.path,
            candidate.path_family,
            candidate.origin,
            execution,
            Option::<String>::None,
            candidate.classification_json,
        ],
    )?;
    Ok(())
}

fn classification_execution(classification_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(classification_json)
        .ok()
        .and_then(|value| {
            value
                .get("execution")
                .and_then(|execution| execution.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn insert_finding(
    write: &super::store::LearningWrite<'_>,
    finding: &ExportedFinding,
    finding_units: &BTreeMap<(&str, &str), Vec<&str>>,
) -> Result<(), DomainError> {
    let mut units: Vec<String> = finding_units
        .get(&(finding.review_id.as_str(), finding.finding_id.as_str()))
        .into_iter()
        .flatten()
        .map(|unit_id| (*unit_id).to_owned())
        .collect();
    units.sort();
    units.dedup();
    let linked_units = super::import::serialize_json(
        &units,
        "не удалось сериализовать связанные единицы замечания из архива",
    )?;
    write.execute(
        "INSERT OR REPLACE INTO learning_finding (
            review_id, finding_id, severity, provenance, title, description, signature,
            linked_unit_ids_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            finding.review_id,
            finding.finding_id,
            finding.severity,
            finding.provenance,
            finding.title,
            finding.description,
            finding.signature,
            linked_units,
        ],
    )?;
    Ok(())
}

fn finding_unit_index<'a>(
    archive: &'a LearningExport,
    candidate_units: &BTreeMap<(&'a str, &'a str), &'a str>,
) -> BTreeMap<(&'a str, &'a str), Vec<&'a str>> {
    let mut finding_units = BTreeMap::new();
    for link in &archive.finding_links {
        if let Some(unit_id) =
            candidate_units.get(&(link.review_id.as_str(), link.candidate_id.as_str()))
        {
            finding_units
                .entry((link.review_id.as_str(), link.finding_id.as_str()))
                .or_insert_with(Vec::new)
                .push(*unit_id);
        }
    }
    finding_units
}

fn payload_digest(archive: &LearningExport) -> Result<String, DomainError> {
    #[derive(Serialize)]
    struct Body<'a> {
        reviews: &'a [ImportRecord],
        units: &'a [ReviewUnitRecord],
        candidates: &'a [ExportedCandidate],
        #[serde(skip_serializing_if = "Option::is_none")]
        decisions: Option<&'a [ExportedDecision]>,
        findings: &'a [ExportedFinding],
        finding_links: &'a [ExportedFindingLink],
        case_links: &'a [ExportedCaseLink],
        feedback_events: &'a [FeedbackEvent],
        policy_proposals: &'a [PolicyProposal],
        search: &'a [ExportedSearchCase],
    }
    let body = Body {
        reviews: &archive.reviews,
        units: &archive.units,
        candidates: &archive.candidates,
        decisions: (archive.manifest.export_schema_version >= EXPORT_SCHEMA_VERSION)
            .then_some(archive.decisions.as_slice()),
        findings: &archive.findings,
        finding_links: &archive.finding_links,
        case_links: &archive.case_links,
        feedback_events: &archive.feedback_events,
        policy_proposals: &archive.policy_proposals,
        search: &archive.search,
    };
    let bytes = serde_json::to_vec(&body).map_err(|error| {
        DomainError::new(
            ErrorCode::Internal,
            format!("не удалось сериализовать тело архива learning: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

#[allow(clippy::type_complexity)]
fn read_tables(
    read: &LearningRead<'_>,
) -> Result<
    (
        Vec<ImportRecord>,
        Vec<ReviewUnitRecord>,
        Vec<ExportedCandidate>,
        Vec<ExportedDecision>,
        Vec<ExportedFinding>,
        Vec<ExportedFindingLink>,
        Vec<ExportedCaseLink>,
        Vec<FeedbackEvent>,
        Vec<PolicyProposal>,
        Vec<ExportedSearchCase>,
    ),
    DomainError,
> {
    let transaction = read.transaction();
    let mut statement = transaction
        .prepare(
            "SELECT review_id, repository_id, base_sha, head_sha, merge_base_sha, workspace_variant,
                    workspace_label, review_pack_sha256, queue_sha256, triage_sha256,
                    execution_result_sha256, review_schema_version, queue_schema_version,
                    triage_schema_version, execution_schema_version, execution_evidence_json,
                    analyzer_digest,
                    classifier_digest, trust, outcome, limitations_json, revision_of,
                    superseded_by, revision, imported_at, observations_json
             FROM learning_import ORDER BY revision ASC, review_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать записи истории"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, String>(16)?,
                row.get::<_, String>(17)?,
                row.get::<_, String>(18)?,
                row.get::<_, String>(19)?,
                row.get::<_, String>(20)?,
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                row.get::<_, i64>(23)?,
                row.get::<_, i64>(24)?,
                row.get::<_, String>(25)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать записи истории"))?;
    let mut raw_reviews = Vec::new();
    for row in rows {
        raw_reviews
            .push(row.map_err(|error| map_error(&error, "не удалось прочитать записи истории"))?);
    }
    drop(statement);
    let mut reviews = Vec::new();
    for row in raw_reviews {
        reviews.push(ImportRecord {
            review_id: row.0,
            repository_id: row.1,
            base_sha: row.2,
            head_sha: row.3,
            merge_base_sha: row.4,
            workspace_variant: row.5,
            workspace_label: row.6,
            inputs: ImportInputs {
                review_pack_sha256: row.7,
                queue_sha256: row.8,
                triage_sha256: row.9,
                execution_result_sha256: row.10,
                review_schema_version: u32::try_from(row.11.max(0)).unwrap_or(0),
                queue_schema_version: u32::try_from(row.12.max(0)).unwrap_or(0),
                triage_schema_version: row.13.map(|value| u32::try_from(value.max(0)).unwrap_or(0)),
                execution_schema_version: row
                    .14
                    .map(|value| u32::try_from(value.max(0)).unwrap_or(0)),
                execution_evidence: row
                    .15
                    .as_deref()
                    .map(|raw| parse_domain_json(raw, "сводка результата исполнения"))
                    .transpose()?,
                analyzer_digest: row.16,
                classifier_digest: row.17,
            },
            trust: super::import::trust_level_of(&row.18).ok_or_else(|| {
                DomainError::new(
                    ErrorCode::LearningCorrupt,
                    format!("Неизвестный уровень доверия записи истории: {}", row.18),
                )
            })?,
            outcome: super::import::reviewed_outcome_of(&row.19).ok_or_else(|| {
                DomainError::new(
                    ErrorCode::LearningCorrupt,
                    format!("Неизвестный исход записи истории: {}", row.19),
                )
            })?,
            limitations: parse_domain_json(&row.20, "ограничения записи")?,
            revision_of: row.21,
            superseded_by: row.22,
            revision: u64::try_from(row.23.max(0)).unwrap_or(0),
            imported_at: u64::try_from(row.24.max(0)).unwrap_or(0),
            observations: parse_domain_json::<ObservationCounts>(&row.25, "единицы наблюдения")?,
        });
    }
    let units = read_units(transaction)?;
    let candidates = read_candidates(transaction)?;
    let decisions = read_decisions(transaction)?;
    let findings = read_findings(transaction)?;
    let finding_links = read_finding_links(transaction)?;
    let case_links = read_case_links(transaction)?;
    let feedback_events = read_feedback(transaction)?;
    let policy_proposals = read_proposals(transaction)?;
    let mut search_statement = transaction
        .prepare(
            "SELECT case_id, review_id, unit_id, candidate_id, finding_id, kind, disposition,
                    severity, provenance, text
             FROM learning_search ORDER BY case_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать поисковые случаи"))?;
    let search_rows = search_statement
        .query_map([], |row| {
            Ok(ExportedSearchCase {
                case_id: row.get(0)?,
                review_id: row.get(1)?,
                unit_id: row.get(2)?,
                candidate_id: row.get(3)?,
                finding_id: row.get(4)?,
                kind: row.get(5)?,
                disposition: row.get(6)?,
                severity: row.get(7)?,
                provenance: row.get(8)?,
                text: row.get(9)?,
            })
        })
        .map_err(|error| map_error(&error, "не удалось прочитать поисковые случаи"))?;
    let mut search = Vec::new();
    for row in search_rows {
        search
            .push(row.map_err(|error| map_error(&error, "не удалось прочитать поисковые случаи"))?);
    }
    drop(search_statement);
    Ok((
        reviews,
        units,
        candidates,
        decisions,
        findings,
        finding_links,
        case_links,
        feedback_events,
        policy_proposals,
        search,
    ))
}

/// Честная формулировка ограничения переноса поиска для версии архива.
fn search_limitation(archive: &LearningExport) -> &'static str {
    if archive.manifest.export_schema_version < EXPORT_SCHEMA_VERSION_WITH_SEARCH {
        "Поисковые случаи в архив этой версии не входят: локальный поиск после восстановления наполняется заново только повторным импортом triage."
    } else {
        "Поисковые случаи переносятся вместе с архивом как есть, включая тексты объяснений ревьюера; индекс FTS5 пересобирается из них."
    }
}

/// Старые архивы не содержали learning_decision; потерю нельзя исправлять догадкой.
fn decisions_limitation(archive: &LearningExport) -> &'static str {
    if archive.manifest.export_schema_version < EXPORT_SCHEMA_VERSION {
        "Архив версии 1–3 не содержит исходных semantic decisions и покрытых ID: решения не фабрикуются; подтверждённые структурные паттерны могут быть неполны до повторного импорта исходного triage."
    } else {
        "Исходные индивидуальные и групповые semantic decisions переносятся вместе с полным покрытием candidate IDs."
    }
}

fn read_decisions(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<ExportedDecision>, DomainError> {
    let mut statement = transaction
        .prepare(
            "SELECT review_id, decision_id, kind, disposition, reason_code, explanation,
                candidate_count, covered_json
         FROM learning_decision ORDER BY review_id ASC, decision_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать решения истории"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать решения истории"))?;
    let mut decisions = Vec::new();
    for row in rows {
        let (review_id, decision_id, kind, disposition, reason_code, explanation, count, covered) =
            row.map_err(|error| map_error(&error, "не удалось прочитать решение истории"))?;
        let candidate_count = usize::try_from(count)
            .map_err(|_| invalid("Некорректное число кандидатов в сохранённом решении"))?;
        decisions.push(ExportedDecision {
            review_id,
            decision_id,
            kind,
            disposition,
            reason_code,
            explanation,
            candidate_count,
            covered_candidate_ids: parse_domain_json(&covered, "покрытие решения истории")?,
        });
    }
    Ok(decisions)
}

fn read_units(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<ReviewUnitRecord>, DomainError> {
    let mut statement = transaction
        .prepare(
            "SELECT review_id, unit_id, kind, candidate_count, priority, representatives_json,
                    disposition, reason_code, detector, source, role, code_role, surfaces_json,
                    feature_json
             FROM learning_unit ORDER BY review_id ASC, unit_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать единицы истории"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
            ))
        })
        .map_err(|error| map_error(&error, "не удалось прочитать единицы истории"))?;
    let mut raw = Vec::new();
    for row in rows {
        raw.push(row.map_err(|error| map_error(&error, "не удалось прочитать единицы истории"))?);
    }
    let mut units = Vec::new();
    for row in raw {
        let feature: BTreeMap<String, String> = parse_domain_json(&row.13, "признаки единицы")?;
        units.push(ReviewUnitRecord {
            review_id: row.0,
            unit_id: row.1,
            kind: if row.2 == "group" {
                ReviewUnitKind::Group
            } else {
                ReviewUnitKind::Individual
            },
            candidate_count: usize::try_from(row.3.max(0)).unwrap_or(0),
            priority: row.4,
            representative_candidate_ids: parse_domain_json(&row.5, "представители единицы")?,
            disposition: row.6,
            reason_code: row.7,
            detector: row.8,
            source: row.9,
            role: row.10,
            code_role: row.11,
            surfaces: parse_domain_json(&row.12, "поверхности единицы")?,
            feature_map: feature,
        });
    }
    Ok(units)
}

fn read_candidates(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<ExportedCandidate>, DomainError> {
    let mut statement = transaction
        .prepare(
            "SELECT review_id, candidate_id, unit_id, detector, source, path, path_family, origin,
                    classification_json
             FROM learning_candidate ORDER BY review_id ASC, candidate_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать кандидатов истории"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ExportedCandidate {
                review_id: row.get(0)?,
                candidate_id: row.get(1)?,
                unit_id: row.get(2)?,
                detector: row.get(3)?,
                source: row.get(4)?,
                path: row.get(5)?,
                path_family: row.get(6)?,
                origin: row.get(7)?,
                // Текст кандидата остаётся только в локальной базе; переноситcя
                // ссылка на evidence, но не фрагмент исходного кода.
                snippet: None,
                classification_json: row.get(8)?,
            })
        })
        .map_err(|error| map_error(&error, "не удалось прочитать кандидатов истории"))?;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(
            row.map_err(|error| map_error(&error, "не удалось прочитать кандидатов истории"))?,
        );
    }
    Ok(candidates)
}

fn read_findings(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<ExportedFinding>, DomainError> {
    let mut statement = transaction
        .prepare(
            "SELECT review_id, finding_id, severity, provenance, title, description, signature
             FROM learning_finding ORDER BY review_id ASC, finding_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать замечания истории"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ExportedFinding {
                review_id: row.get(0)?,
                finding_id: row.get(1)?,
                severity: row.get(2)?,
                provenance: row.get(3)?,
                title: row.get(4)?,
                description: row.get(5)?,
                signature: row.get(6)?,
            })
        })
        .map_err(|error| map_error(&error, "не удалось прочитать замечания истории"))?;
    let mut findings = Vec::new();
    for row in rows {
        findings.push(
            row.map_err(|error| map_error(&error, "не удалось прочитать замечания истории"))?,
        );
    }
    Ok(findings)
}

fn read_finding_links(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<ExportedFindingLink>, DomainError> {
    let mut statement = transaction
        .prepare(
            "SELECT review_id, candidate_id, finding_id FROM learning_finding_link
             ORDER BY review_id ASC, candidate_id ASC, finding_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать связи замечаний"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ExportedFindingLink {
                review_id: row.get(0)?,
                candidate_id: row.get(1)?,
                finding_id: row.get(2)?,
            })
        })
        .map_err(|error| map_error(&error, "не удалось прочитать связи замечаний"))?;
    let mut links = Vec::new();
    for row in rows {
        links.push(row.map_err(|error| map_error(&error, "не удалось прочитать связи замечаний"))?);
    }
    Ok(links)
}

fn read_case_links(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<ExportedCaseLink>, DomainError> {
    let mut statement = transaction
        .prepare(
            "SELECT review_id, finding_id, linked_review_id, linked_finding_id, kind, basis
             FROM learning_case_link
             ORDER BY review_id ASC, finding_id ASC, linked_review_id ASC, linked_finding_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать связи случаев"))?;
    let rows = statement
        .query_map([], |row| {
            let kind: String = row.get(4)?;
            Ok(ExportedCaseLink {
                review_id: row.get(0)?,
                finding_id: row.get(1)?,
                linked_review_id: row.get(2)?,
                linked_finding_id: row.get(3)?,
                kind: match kind.as_str() {
                    "structural_repeat" => super::model::CaseLinkKind::StructuralRepeat,
                    "same_iteration" => super::model::CaseLinkKind::SameIteration,
                    _ => super::model::CaseLinkKind::Unknown,
                },
                basis: row.get(5)?,
            })
        })
        .map_err(|error| map_error(&error, "не удалось прочитать связи случаев"))?;
    let mut links = Vec::new();
    for row in rows {
        links.push(row.map_err(|error| map_error(&error, "не удалось прочитать связи случаев"))?);
    }
    Ok(links)
}

fn read_feedback(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<FeedbackEvent>, DomainError> {
    let invalid_time: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM learning_feedback WHERE recorded_at < 0)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| map_error(&error, "не удалось проверить время событий обратной связи"))?;
    if invalid_time {
        return Err(DomainError::new(
            ErrorCode::LearningCorrupt,
            "История обратной связи содержит отрицательное recorded_at",
        ));
    }
    let mut statement = transaction
        .prepare(
            "SELECT event_id, review_id, unit_id, candidate_id, kind, action,
                    supersedes_event_id, effective_disposition, usefulness, explanation,
                    provenance, recorded_at
             FROM learning_feedback ORDER BY event_id ASC",
        )
        .map_err(|error| map_error(&error, "не удалось прочитать события обратной связи"))?;
    let rows = statement
        .query_map([], super::feedback::event_from_row_for_export)
        .map_err(|error| map_error(&error, "не удалось прочитать события обратной связи"))?;
    let mut events = Vec::new();
    for row in rows {
        events.push(
            row.map_err(|error| map_error(&error, "не удалось прочитать события обратной связи"))?,
        );
    }
    Ok(events)
}

fn read_proposals(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<Vec<PolicyProposal>, DomainError> {
    let mut statement = transaction
        .prepare("SELECT document_json FROM learning_policy_proposal ORDER BY proposal_id ASC")
        .map_err(|error| map_error(&error, "не удалось прочитать предложения политики"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| map_error(&error, "не удалось прочитать предложения политики"))?;
    let mut proposals = Vec::new();
    for row in rows {
        let document =
            row.map_err(|error| map_error(&error, "не удалось прочитать предложения политики"))?;
        let proposal: PolicyProposal = serde_json::from_str(&document).map_err(|error| {
            DomainError::new(
                ErrorCode::LearningCorrupt,
                format!("Сохранённое предложение политики повреждено: {error}"),
            )
        })?;
        proposals.push(proposal);
    }
    Ok(proposals)
}

fn invalid(message: impl Into<String>) -> DomainError {
    DomainError::new(ErrorCode::LearningExportInvalid, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_review::learning::model::{ReviewedOutcome, TrustLevel};

    #[test]
    fn export_rejects_unknown_persisted_trust_and_outcome() {
        use super::super::store::{LearningStore, StoreOptions};

        let directory = std::env::temp_dir().join(format!(
            "anki-repo-transfer-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let store = LearningStore::open(StoreOptions::at(directory.join("state.sqlite"))).unwrap();
        let record = archive_with_valid_references().reviews.remove(0);
        store.write(|write| insert_record(write, &record)).unwrap();

        for (column, value) in [("trust", "future_trust"), ("outcome", "complete")] {
            store
                .write(|write| {
                    write
                        .execute(
                            &format!(
                                "UPDATE learning_import SET {column} = ?1 WHERE review_id = ?2"
                            ),
                            params![value, record.review_id],
                        )
                        .map(|_| ())
                })
                .unwrap();
            let error = export_history(&store).unwrap_err();
            assert_eq!(error.code, ErrorCode::LearningCorrupt, "column: {column}");
            store
                .write(|write| {
                    write
                        .execute(
                            &format!(
                                "UPDATE learning_import SET {column} = ?1 WHERE review_id = ?2"
                            ),
                            params![
                                if column == "trust" {
                                    TrustLevel::AstAuthenticated.as_str()
                                } else {
                                    ReviewedOutcome::FullyReviewed.as_str()
                                },
                                record.review_id
                            ],
                        )
                        .map(|_| ())
                })
                .unwrap();
        }
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Кандидат архива с заданным путём: остальные поля для проверки не важны.
    fn candidate_with_path(path: &str) -> ExportedCandidate {
        ExportedCandidate {
            review_id: "review-1".into(),
            candidate_id: "candidate-1".into(),
            unit_id: "unit-1".into(),
            detector: "error_path".into(),
            source: "synthetic_detector".into(),
            path: path.into(),
            path_family: "src".into(),
            origin: "introduced_or_changed".into(),
            snippet: None,
            classification_json: "{}".into(),
        }
    }

    /// Архив без записей и с одним кандидатом: digest пересчитывается честно.
    fn archive_with_candidate(path: &str) -> LearningExport {
        let mut archive = LearningExport {
            manifest: LearningExportManifest {
                export_schema_version: EXPORT_SCHEMA_VERSION,
                schema_version: super::super::LEARNING_SCHEMA_VERSION,
                policy_version: super::super::LEARNING_POLICY_VERSION,
                generation: HistoryGeneration {
                    revision: 0,
                    trusted_reviews: 0,
                    quarantined_reviews: 0,
                    trusted_units: 0,
                },
                reviews: 0,
                units: 0,
                findings: 0,
                feedback_events: 0,
                policy_proposals: 0,
                search_cases: 0,
                payload_sha256: String::new(),
            },
            reviews: Vec::new(),
            units: Vec::new(),
            candidates: vec![candidate_with_path(path)],
            decisions: Vec::new(),
            findings: Vec::new(),
            finding_links: Vec::new(),
            case_links: Vec::new(),
            feedback_events: Vec::new(),
            policy_proposals: Vec::new(),
            search: Vec::new(),
        };
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        archive
    }

    fn archive_with_valid_references() -> LearningExport {
        let mut archive = archive_with_candidate("src/lib.rs");
        archive.reviews.push(ImportRecord {
            review_id: "review-1".into(),
            repository_id: "repository-1".into(),
            base_sha: "base".into(),
            head_sha: "head".into(),
            merge_base_sha: "merge-base".into(),
            workspace_variant: "root".into(),
            workspace_label: None,
            inputs: ImportInputs::default(),
            trust: TrustLevel::AstAuthenticated,
            outcome: ReviewedOutcome::FullyReviewed,
            limitations: Vec::new(),
            revision_of: None,
            superseded_by: None,
            revision: 1,
            imported_at: 1,
            observations: ObservationCounts::default(),
        });
        archive.units.push(ReviewUnitRecord {
            unit_id: "unit-1".into(),
            review_id: "review-1".into(),
            kind: ReviewUnitKind::Individual,
            candidate_count: 1,
            priority: "normal".into(),
            representative_candidate_ids: vec!["candidate-1".into()],
            disposition: None,
            reason_code: None,
            detector: "error_path".into(),
            source: "synthetic".into(),
            role: "production".into(),
            code_role: "implementation".into(),
            surfaces: vec!["production".into()],
            feature_map: BTreeMap::new(),
        });
        archive.findings.push(ExportedFinding {
            review_id: "review-1".into(),
            finding_id: "finding-1".into(),
            severity: "minor".into(),
            provenance: "independent".into(),
            title: "Finding".into(),
            description: "Finding description".into(),
            signature: "signature".into(),
        });
        archive.finding_links.push(ExportedFindingLink {
            review_id: "review-1".into(),
            candidate_id: "candidate-1".into(),
            finding_id: "finding-1".into(),
        });
        archive.feedback_events.push(FeedbackEvent {
            schema_version: super::super::feedback::FEEDBACK_SCHEMA_VERSION,
            event_id: "event-1".into(),
            review_id: "review-1".into(),
            unit_id: "unit-1".into(),
            candidate_id: Some("candidate-1".into()),
            kind: super::super::model::FeedbackKind::SemanticOutcomeRevision,
            action: super::super::model::FeedbackAction::Append,
            supersedes_event_id: None,
            effective_disposition: Some("confirmed".into()),
            usefulness: None,
            explanation: "Reviewer's explanation".into(),
            provenance: "reviewer".into(),
            recorded_at: 1,
        });
        archive.manifest.reviews = archive.reviews.len();
        archive.manifest.units = archive.units.len();
        archive.manifest.findings = archive.findings.len();
        archive.manifest.feedback_events = archive.feedback_events.len();
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        archive
    }

    #[test]
    fn archive_paths_are_validated_by_components_not_substrings() {
        // Законные repo-relative пути, включая компонент `home`.
        for path in [
            "src/lib.rs",
            "tools/home/x.rs",
            "src/deep/nested/file.rs",
            "./src/lib.rs",
        ] {
            assert!(
                reject_unportable_path(path).is_ok(),
                "законный путь отвергнут: {path}"
            );
        }
        // Абсолютные пути, переход к родителю и префикс тома запрещены.
        for path in [
            "/etc/passwd",
            "../escape.rs",
            "src/../../escape.rs",
            "C:/windows/system32/x.rs",
            "C:\\windows\\system32\\x.rs",
            "\\\\server\\share\\x.rs",
            "",
        ] {
            let error = reject_unportable_path(path).unwrap_err();
            assert_eq!(error.code, ErrorCode::LearningExportInvalid, "путь: {path}");
        }
    }

    #[test]
    fn archive_rejects_dangling_findings_links_and_feedback_references() {
        let valid = archive_with_valid_references();
        verify_export(&valid).unwrap();

        let mut archive = valid.clone();
        archive.findings[0].review_id = "missing-review".into();
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        assert_eq!(
            verify_export(&archive).unwrap_err().code,
            ErrorCode::LearningExportInvalid
        );

        let mut archive = valid.clone();
        archive.finding_links[0].candidate_id = "missing-candidate".into();
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        assert_eq!(
            verify_export(&archive).unwrap_err().code,
            ErrorCode::LearningExportInvalid
        );

        let mut archive = valid.clone();
        archive.finding_links[0].finding_id = "missing-finding".into();
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        assert_eq!(
            verify_export(&archive).unwrap_err().code,
            ErrorCode::LearningExportInvalid
        );

        let mut archive = valid.clone();
        archive.feedback_events[0].review_id = "missing-review".into();
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        assert_eq!(
            verify_export(&archive).unwrap_err().code,
            ErrorCode::LearningExportInvalid
        );

        let mut archive = valid.clone();
        archive.feedback_events[0].unit_id = "missing-unit".into();
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        assert_eq!(
            verify_export(&archive).unwrap_err().code,
            ErrorCode::LearningExportInvalid
        );

        let mut archive = valid;
        archive.feedback_events[0].candidate_id = Some("missing-candidate".into());
        archive.manifest.payload_sha256 = payload_digest(&archive).unwrap();
        assert_eq!(
            verify_export(&archive).unwrap_err().code,
            ErrorCode::LearningExportInvalid
        );
    }

    #[test]
    fn verify_export_rejects_a_relative_escape_but_accepts_a_home_component() {
        let escaped = archive_with_candidate("../escape.rs");
        let error = verify_export(&escaped).unwrap_err();
        assert_eq!(error.code, ErrorCode::LearningExportInvalid);
        assert!(
            error.message.contains("repo-relative"),
            "отказ обязан называть причину: {}",
            error.message
        );

        // Компонент `home` внутри репозитория больше не считается абсолютным
        // путём: архив отвергается только из-за отсутствующей записи.
        let legitimate = archive_with_candidate("tools/home/x.rs");
        let error = verify_export(&legitimate).unwrap_err();
        assert!(
            !error.message.contains("repo-relative"),
            "законный путь не может считаться абсолютным: {}",
            error.message
        );
        assert!(error.message.contains("отсутствующую запись"));
    }
}
