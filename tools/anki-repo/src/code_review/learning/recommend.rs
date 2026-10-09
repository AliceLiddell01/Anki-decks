//! Безопасные адаптивные рекомендации по порядку и гранулярности ревью.
//!
//! Документ — производный артефакт: он ссылается на исходные единицы и
//! кандидатов, но не меняет `review.json`, `review-queue.json` и
//! `semantic-triage.json` и не создаёт семантических решений.
//!
//! Жёсткие guardrails выполняются буквально: baseline priority не меняется
//! скрытно, ни одна единица не исключается из обязательного просмотра из-за
//! прошлой статистики, группа не объявляется семантически рассмотренной по
//! структурному сходству, а высокий исторический уровень `acceptable` не
//! трактуется как доказанная безопасность нового кода.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{DomainError, ErrorCode};

use super::import::LoadedReview;
use super::model::{
    CaseRef, HistoryGeneration, Recommendation, Recommendations, SupportLevel, SupportSummary,
    TrustLevel,
};
use super::patterns::{evidence_for_signatures, feature_signature, unit_features_with_classifier};
use super::store::LearningStore;
use super::{LEARNING_POLICY_VERSION, ROOT_WORKSPACE_VARIANT};

/// Версия схемы recommendations artifact.
pub const RECOMMENDATIONS_SCHEMA_VERSION: u32 = 2;

type RecommendationOrderKey = (u8, u8, u8, usize, String);

/// Гранулярность проверки, предлагаемая для единицы.
#[must_use]
pub fn granularity_for(
    is_group: bool,
    guardrailed: bool,
    support: Option<&SupportSummary>,
    related_repeats: usize,
) -> &'static str {
    if related_repeats > 0 {
        return "look_for_related_finding";
    }
    match support {
        Some(summary) if summary.level == SupportLevel::Supported && is_group && guardrailed => {
            "inspect_individually"
        }
        Some(summary) if summary.level == SupportLevel::Supported && is_group => {
            "inspect_each_candidate"
        }
        Some(summary)
            if summary.level == SupportLevel::Supported
                && summary.confirmed_units > 0
                && summary.unresolved_units == 0 =>
        {
            "inspect_first"
        }
        _ => "no_recommendation",
    }
}

/// Позиция единицы в предложенном порядке.
#[must_use]
pub fn suggested_position(priority: &str) -> &'static str {
    match priority {
        "high" => "first",
        "normal" => "as_queued",
        _ => "last",
    }
}

/// Порядковый ранг детерминированного приоритета очереди.
fn priority_rank(priority: &str) -> u8 {
    match priority {
        "high" => 0,
        "normal" => 1,
        _ => 2,
    }
}

fn recommendation_order_key(
    priority: &str,
    guardrailed: bool,
    support: u8,
    queue_index: usize,
    unit_id: String,
) -> RecommendationOrderKey {
    (
        priority_rank(priority),
        u8::from(!guardrailed),
        support,
        queue_index,
        unit_id,
    )
}

/// Запрос генерации рекомендаций.
#[derive(Debug, Clone)]
pub struct RecommendRequest {
    /// Явный вариант источника: `root` либо `snapshot-<32 hex>`.
    pub workspace_variant: String,
    /// Идентификатор каталога ревью, если он известен.
    pub workspace_label: Option<String>,
    /// Верхняя граница числа подсказок в документе.
    pub limit: usize,
    /// Верхняя граница числа исторических случаев на одну подсказку.
    pub case_limit: usize,
    /// Опорное время в секундах Unix для оценки свежести истории.
    pub now: u64,
    /// Ожидаемая ревизия проверяется внутри снимка, из которого строятся подсказки.
    pub history_revision: Option<u64>,
    /// Требовать непустую доверенную историю в том же снимке.
    pub require_history: bool,
}

impl Default for RecommendRequest {
    fn default() -> Self {
        Self {
            workspace_variant: ROOT_WORKSPACE_VARIANT.to_owned(),
            workspace_label: None,
            limit: 50,
            case_limit: 3,
            now: 0,
            history_revision: None,
            require_history: false,
        }
    }
}

/// Строит версионируемый recommendations artifact.
///
/// `store = None` включает штатный режим без learning: предложенный порядок
/// совпадает с детерминированным порядком очереди, исторических случаев нет, а
/// ограничения явно перечисляют отсутствие истории.
pub fn recommend(
    store: Option<&LearningStore>,
    loaded: &LoadedReview,
    request: &RecommendRequest,
) -> Result<Recommendations, DomainError> {
    let variant = super::import::normalize_variant(&request.workspace_variant)?;
    let classifier_compatibility = super::import::classifier_digest(&loaded.pack, &loaded.queue);
    let case_limit = request.case_limit.max(1);
    let signatures: BTreeSet<String> = loaded
        .queue
        .units
        .iter()
        .map(|unit| {
            feature_signature(&unit_features_with_classifier(
                &unit.signature,
                &classifier_compatibility,
            ))
        })
        .collect();
    let learning_disabled = store.is_none() || !loaded.trust.participates_in_learning();
    let (generation, evidence_by_signature, limitations) = match store {
        Some(store) => store.read(|read| {
            let generation = super::import::generation_of(read)?;
            require_generation(Some(&generation), request)?;
            let imported_at: i64 = read
                .transaction()
                .query_row(
                    "SELECT COALESCE(MAX(imported_at), 0)
                 FROM learning_import WHERE trust = 'ast_authenticated'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    super::store::map_error(&error, "не удалось прочитать опорное время истории")
                })?;
            let now = if request.now == 0 {
                u64::try_from(imported_at.max(0)).unwrap_or(u64::MAX)
            } else {
                request.now
            };
            let evidence = if learning_disabled {
                BTreeMap::new()
            } else {
                evidence_for_signatures(read, &signatures, case_limit, now)?
            };
            Ok((generation, evidence, Vec::new()))
        })?,
        None => {
            require_generation(None, request)?;
            (
                HistoryGeneration {
                    revision: 0,
                    trusted_reviews: 0,
                    quarantined_reviews: 0,
                    trusted_units: 0,
                },
                BTreeMap::new(),
                vec![
                    "Обычный режим без learning: рекомендации не опираются на историю.".to_owned(),
                ],
            )
        }
    };
    let mut recommendations = Vec::new();
    let mut order: Vec<RecommendationOrderKey> = Vec::new();
    for (queue_index, unit) in loaded.queue.units.iter().enumerate() {
        let features = unit_features_with_classifier(&unit.signature, &classifier_compatibility);
        let signature = feature_signature(&features);
        let is_group = unit.is_group();
        let (support, cases, repeats) = if learning_disabled {
            (None, Vec::new(), 0usize)
        } else {
            let evidence = evidence_by_signature.get(&signature);
            let support = evidence.map(|item| item.support.clone());
            let cases = evidence.map_or_else(Vec::new, |item| item.cases.clone());
            let repeats = evidence.map_or(0, |item| item.related_repeats);
            (support, cases, repeats)
        };
        let guardrailed = is_guardrailed(unit);
        let granularity = granularity_for(is_group, guardrailed, support.as_ref(), repeats);
        let mut unit_limitations = Vec::new();
        if let Some(summary) = support.as_ref() {
            match summary.level {
                SupportLevel::InsufficientEvidence => unit_limitations.push(
                    "Истории недостаточно для вывода; подсказка не влияет на обязательность проверки."
                        .to_owned(),
                ),
                SupportLevel::Contradictory => unit_limitations.push(
                    "Исторические случаи противоречат друг другу; опираться на статистику нельзя."
                        .to_owned(),
                ),
                SupportLevel::Supported => {}
            }
        } else if !learning_disabled {
            unit_limitations.push("Точных исторических случаев по этим признакам нет.".to_owned());
        }
        if unit.priority.as_str() == "high" {
            unit_limitations.push(
                "Baseline priority high сохраняется: learning не понижает обязательность проверки."
                    .to_owned(),
            );
        }
        if unit.signature.classification.is_unknown() {
            unit_limitations.push(
                "Классификация не доказана (unknown): единица не исключается из обязательного просмотра."
                    .to_owned(),
            );
        }
        let reason = reason_for(unit, support.as_ref(), granularity, guardrailed);
        let historical_cases: Vec<CaseRef> = cases.into_iter().map(|case| case.case).collect();
        order.push(recommendation_order_key(
            unit.priority.as_str(),
            guardrailed,
            support_rank(support.as_ref(), granularity),
            queue_index,
            unit.id.clone(),
        ));
        let representative_candidate_ids = unit.representative_candidate_ids().to_vec();
        let candidate_count = unit.candidate_ids().len();
        let candidate_ids = if is_group {
            representative_candidate_ids.clone()
        } else {
            unit.candidate_ids().to_vec()
        };
        recommendations.push(Recommendation {
            unit_id: unit.id.clone(),
            candidate_ids_truncated: candidate_count > candidate_ids.len(),
            candidate_count,
            candidate_ids,
            representative_candidate_ids,
            queue_priority: unit.priority.as_str().to_owned(),
            suggested_position: suggested_position(unit.priority.as_str()).to_owned(),
            granularity: granularity.to_owned(),
            reason,
            pattern_signature: Some(signature),
            support,
            historical_cases,
            limitations: unit_limitations,
            source_reference: format!(
                "{}#{}",
                variant,
                source_anchor(&loaded.pack.target.head_sha, unit.id.as_str())
            ),
        });
    }
    order.sort();
    let limit = request.limit.max(1);
    debug_assert_eq!(order.len(), recommendations.len());
    let positions: BTreeMap<String, usize> = order
        .into_iter()
        .enumerate()
        .map(|(position, (_, _, _, _, unit_id))| (unit_id, position))
        .collect();
    recommendations.sort_by_key(|item| positions.get(&item.unit_id).copied().unwrap_or(usize::MAX));
    let suggested_order: Vec<String> = recommendations
        .iter()
        .map(|item| item.unit_id.clone())
        .collect();
    let mut document_limitations = limitations;
    if recommendations.len() > limit {
        document_limitations.push(format!(
            "Подробные подсказки ограничены {limit} единицами; ещё {} единиц остаются в suggested_order и обязательном просмотре без подробной подсказки.",
            recommendations.len() - limit
        ));
        recommendations.truncate(limit);
    }
    document_limitations.push(
        "Подсказка — дополнительное ранжирование: baseline priority и порядок review-queue.json не изменяются."
            .to_owned(),
    );
    if generation.trusted_units == 0 {
        document_limitations.push(
            "Проверенная история пуста: ни одна подсказка не опирается на накопленные случаи."
                .to_owned(),
        );
    } else if generation.trusted_units < super::patterns::MIN_SUPPORT_UNITS {
        document_limitations.push(format!(
            "Проверенных независимых единиц меньше минимума поддержки {}: выводы по паттернам воздержатся.",
            super::patterns::MIN_SUPPORT_UNITS
        ));
    }
    if loaded.trust == TrustLevel::StructureOnlyQuarantine {
        document_limitations.push(
            "Текущий вход помечен карантином: история не участвует в подсказках по этому снимку."
                .to_owned(),
        );
    }
    Ok(Recommendations {
        schema_version: RECOMMENDATIONS_SCHEMA_VERSION,
        policy_version: LEARNING_POLICY_VERSION,
        generation,
        repository_id: loaded.pack.target.repository_id.clone(),
        base_sha: loaded.pack.target.base_sha.clone(),
        head_sha: loaded.pack.target.head_sha.clone(),
        merge_base_sha: loaded.pack.target.merge_base_sha.clone(),
        workspace_variant: variant,
        review_pack_sha256: loaded.review_pack_sha256.clone(),
        queue_sha256: loaded.queue_sha256.clone(),
        input_trust: loaded.trust,
        learning_disabled,
        suggested_order,
        recommendations,
        limitations: document_limitations,
    })
}

/// Проверяет ограничения запроса по поколению, прочитанному в текущем снимке.
fn require_generation(
    generation: Option<&HistoryGeneration>,
    request: &RecommendRequest,
) -> Result<(), DomainError> {
    if let Some(expected) = request.history_revision {
        let actual = generation.ok_or_else(|| {
            DomainError::with_details(
                ErrorCode::SourceChanged,
                "Ожидалась конкретная ревизия истории learning, но история недоступна",
                crate::details! { "expected_revision" => expected },
            )
        })?;
        if actual.revision != expected {
            return Err(DomainError::with_details(
                ErrorCode::SourceChanged,
                format!(
                    "Ожидалась ревизия истории {expected}, действующая — {}",
                    actual.revision
                ),
                crate::details! { "expected_revision" => expected, "actual_revision" => actual.revision },
            ));
        }
    }
    if request.require_history {
        let actual = generation.ok_or_else(|| {
            DomainError::new(
                ErrorCode::NotFound,
                "Проверенная история learning недоступна, а она требуется явно",
            )
        })?;
        if actual.trusted_units == 0 {
            return Err(DomainError::with_details(
                ErrorCode::InsufficientEvidence,
                "Проверенная история пуста: рекомендации не могут опираться на накопленные случаи",
                crate::details! { "trusted_units" => actual.trusted_units },
            ));
        }
    }
    Ok(())
}

/// Уровень приоритета подсказки: меньше — раньше.
fn support_rank(support: Option<&SupportSummary>, granularity: &str) -> u8 {
    match (support, granularity) {
        (Some(summary), "inspect_first" | "inspect_each_candidate" | "inspect_individually")
            if summary.level == SupportLevel::Supported =>
        {
            0
        }
        (Some(_), "look_for_related_finding") => 1,
        (Some(summary), _) if summary.level == SupportLevel::Contradictory => 2,
        (Some(summary), _) if summary.level == SupportLevel::Supported => 4,
        (Some(_), _) => 3,
        (None, _) => 4,
    }
}

/// Считается ли единица защищённой guardrails и обязательной к просмотру.
fn is_guardrailed(unit: &crate::code_review::review_queue::ReviewUnit) -> bool {
    let classification = &unit.signature.classification;
    classification.is_unknown()
        || unit.priority.as_str() == "high"
        || matches!(
            classification.role,
            crate::code_review::review_queue::StructuralRole::Security
                | crate::code_review::review_queue::StructuralRole::Suppression
                | crate::code_review::review_queue::StructuralRole::ErrorPath
        )
        || classification.code_role == crate::code_review::review_queue::CodeRole::RuntimeBoundary
}

fn reason_for(
    unit: &crate::code_review::review_queue::ReviewUnit,
    support: Option<&SupportSummary>,
    granularity: &str,
    guardrailed: bool,
) -> String {
    let base = match granularity {
        "look_for_related_finding" => {
            "В истории есть структурно похожий случай: проверьте его контекст и возможную аналогию в этом снимке. Структурная схожесть не доказывает повтор той же ошибки."
        }
        "inspect_each_candidate" => {
            "По этим структурным признакам есть достаточная поддержка истории: представителей можно использовать для навигации, но проверьте каждого кандидата; сходство не доказывает семантическую однородность."
        }
        "inspect_individually" => {
            "Защищённую единицу нужно проверить целиком и по каждому кандидату отдельно; представители помогают найти контекст, но не доказывают однородность группы."
        }
        "inspect_first" => {
            "В истории по этим признакам есть подтверждённые случаи: имеет смысл посмотреть единицу раньше обычного порядка."
        }
        _ => {
            "История не даёт положительной подсказки к порядку: единица остаётся в обычной очереди."
        }
    };
    let mut reason = base.to_owned();
    if let Some(summary) = support {
        reason.push_str(&format!(
            " Поддержка: независимых единиц {}, записей {}, без решения {}.",
            summary.support_units, summary.support_reviews, summary.unresolved_units
        ));
    }
    reason.push_str(&format!(
        " Кандидатов в единице: {}.",
        unit.candidate_ids().len()
    ));
    if guardrailed {
        reason.push_str(
            " Единица защищена guardrails (unknown/security/suppression/error_path/runtime_boundary/high): прошлая статистика не может исключить её из обязательного просмотра.",
        );
    }
    reason
}

/// Детерминированный анкер к первичному контексту единицы.
fn source_anchor(head_sha: &str, unit_id: &str) -> String {
    let short = &head_sha[..head_sha.len().min(12)];
    format!("{short}/{unit_id}")
}

/// Пустая заготовка документа без обращения к хранилищу.
#[must_use]
pub fn empty_document(loaded: &LoadedReview) -> Recommendations {
    Recommendations {
        schema_version: RECOMMENDATIONS_SCHEMA_VERSION,
        policy_version: LEARNING_POLICY_VERSION,
        generation: HistoryGeneration {
            revision: 0,
            trusted_reviews: 0,
            quarantined_reviews: 0,
            trusted_units: 0,
        },
        repository_id: loaded.pack.target.repository_id.clone(),
        base_sha: loaded.pack.target.base_sha.clone(),
        head_sha: loaded.pack.target.head_sha.clone(),
        merge_base_sha: loaded.pack.target.merge_base_sha.clone(),
        workspace_variant: ROOT_WORKSPACE_VARIANT.to_owned(),
        review_pack_sha256: loaded.review_pack_sha256.clone(),
        queue_sha256: loaded.queue_sha256.clone(),
        input_trust: loaded.trust,
        learning_disabled: true,
        suggested_order: Vec::new(),
        recommendations: Vec::new(),
        limitations: vec!["Документ не заполнен.".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supported_summary() -> SupportSummary {
        SupportSummary {
            support_units: 3,
            support_reviews: 3,
            confirmed_units: 3,
            acceptable_units: 0,
            false_positive_units: 0,
            not_applicable_units: 0,
            uncertain_units: 0,
            unresolved_units: 0,
            confirmed_findings: 0,
            findings_by_provenance: BTreeMap::new(),
            freshest_age_days: 0,
            oldest_age_days: 0,
            revised_units: 0,
            level: SupportLevel::Supported,
            confirmed_share_lower_bound: Some(0.4),
            explanation: String::new(),
            contradicting_unit_ids: Vec::new(),
        }
    }

    #[test]
    fn grouped_recommendations_never_transfer_a_decision_to_guardrailed_units() {
        let support = supported_summary();
        assert_eq!(
            granularity_for(true, true, Some(&support), 0),
            "inspect_individually"
        );
        assert_eq!(
            granularity_for(true, false, Some(&support), 0),
            "inspect_each_candidate"
        );
        assert_eq!(
            granularity_for(true, true, Some(&support), 1),
            "look_for_related_finding"
        );
    }

    #[test]
    fn guardrails_precede_history_and_equal_ranks_keep_queue_order() {
        let mut entries = [
            recommendation_order_key("normal", false, 0, 0, "supported".to_owned()),
            recommendation_order_key("normal", false, 4, 1, "a-later".to_owned()),
            recommendation_order_key("normal", false, 4, 0, "z-earlier".to_owned()),
            recommendation_order_key("normal", true, 4, 50, "protected-tail".to_owned()),
        ];
        entries.sort();
        assert_eq!(
            entries
                .iter()
                .map(|(_, _, _, _, id)| id.as_str())
                .collect::<Vec<_>>(),
            ["protected-tail", "supported", "z-earlier", "a-later"]
        );

        let mut long_queue: Vec<RecommendationOrderKey> = (0..50)
            .map(|index| {
                recommendation_order_key("normal", false, 0, index, format!("supported-{index}"))
            })
            .collect();
        long_queue.push(recommendation_order_key(
            "normal",
            true,
            4,
            50,
            "protected-tail".to_owned(),
        ));
        long_queue.sort();
        assert_eq!(long_queue[0].4, "protected-tail");
        assert!(
            long_queue
                .into_iter()
                .take(50)
                .any(|entry| entry.4 == "protected-tail"),
            "низкая поддержка истории не должна вытеснять защищённую единицу из лимита"
        );
    }

    #[test]
    fn supported_history_without_a_positive_hint_does_not_outrank_no_history() {
        let mut support = supported_summary();
        support.confirmed_units = 0;
        support.acceptable_units = 3;
        support.confirmed_share_lower_bound = None;
        assert_eq!(support_rank(Some(&support), "no_recommendation"), 4);
        assert_eq!(support_rank(None, "no_recommendation"), 4);

        let mut entries = [
            recommendation_order_key(
                "normal",
                false,
                support_rank(Some(&support), "no_recommendation"),
                1,
                "supported-but-neutral".to_owned(),
            ),
            recommendation_order_key(
                "normal",
                false,
                support_rank(None, "no_recommendation"),
                0,
                "no-history".to_owned(),
            ),
        ];
        entries.sort();
        assert_eq!(entries[0].4, "no-history");
    }
}
