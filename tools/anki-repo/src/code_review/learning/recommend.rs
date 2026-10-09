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

use crate::error::DomainError;

use super::import::LoadedReview;
use super::model::{
    CaseRef, HistoryGeneration, Recommendation, Recommendations, SupportLevel, SupportSummary,
    TrustLevel,
};
use super::patterns::{
    cases_for_signature, feature_signature, support_for_signature, unit_features,
};
use super::store::LearningStore;
use super::{LEARNING_POLICY_VERSION, ROOT_WORKSPACE_VARIANT};

/// Версия схемы recommendations artifact.
pub const RECOMMENDATIONS_SCHEMA_VERSION: u32 = 1;

/// Гранулярность проверки, предлагаемая для единицы.
#[must_use]
pub fn granularity_for(
    is_group: bool,
    support: Option<&SupportSummary>,
    related_repeats: usize,
) -> &'static str {
    if related_repeats > 0 {
        return "look_for_related_finding";
    }
    match support {
        Some(summary) if summary.level == SupportLevel::Supported && is_group => "inspect_group",
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
}

impl Default for RecommendRequest {
    fn default() -> Self {
        Self {
            workspace_variant: ROOT_WORKSPACE_VARIANT.to_owned(),
            workspace_label: None,
            limit: 50,
            case_limit: 3,
            now: 0,
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
    let now = if request.now == 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default()
    } else {
        request.now
    };
    let (generation, limitations, learning_disabled) = match store {
        Some(store) => (
            super::import::generation(store)?,
            Vec::new(),
            !loaded.trust.participates_in_learning(),
        ),
        None => (
            HistoryGeneration {
                revision: 0,
                trusted_reviews: 0,
                quarantined_reviews: 0,
                trusted_units: 0,
            },
            vec!["Обычный режим без learning: рекомендации не опираются на историю.".to_owned()],
            true,
        ),
    };
    let case_limit = request.case_limit.max(1);
    let mut recommendations = Vec::new();
    let mut order: Vec<(u8, u8, String)> = Vec::new();
    for unit in &loaded.queue.units {
        let features = unit_features(&unit.signature);
        let signature = feature_signature(&features);
        let is_group = unit.is_group();
        let (support, cases, repeats) = if learning_disabled {
            (None, Vec::new(), 0usize)
        } else if let Some(store) = store {
            let support = support_for_signature(store, &signature, now)?;
            let cases = cases_for_signature(store, &signature, case_limit, now)?;
            let repeats = cases
                .iter()
                .filter(|case| case.link == super::model::CaseLinkKind::StructuralRepeat)
                .count();
            (support, cases, repeats)
        } else {
            (None, Vec::new(), 0usize)
        };
        let granularity = granularity_for(is_group, support.as_ref(), repeats);
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
        let guardrailed = is_guardrailed(unit);
        let reason = reason_for(unit, support.as_ref(), granularity, guardrailed);
        let historical_cases: Vec<CaseRef> = cases.into_iter().map(|case| case.case).collect();
        order.push((
            priority_rank(unit.priority.as_str()),
            support_rank(support.as_ref(), granularity),
            unit.id.clone(),
        ));
        recommendations.push(Recommendation {
            unit_id: unit.id.clone(),
            candidate_ids: unit.candidate_ids().to_vec(),
            representative_candidate_ids: unit.representative_candidate_ids().to_vec(),
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
    let suggested_order: Vec<String> = order
        .into_iter()
        .take(limit)
        .map(|(_, _, unit_id)| unit_id)
        .collect();
    recommendations.truncate(limit);
    let mut document_limitations = limitations;
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

/// Уровень приоритета подсказки: меньше — раньше.
fn support_rank(support: Option<&SupportSummary>, granularity: &str) -> u8 {
    match (support, granularity) {
        (Some(summary), _) if summary.level == SupportLevel::Supported => 0,
        (Some(summary), _) if summary.level == SupportLevel::Contradictory => 2,
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
            "В истории есть связанный повтор того же структурного дефекта: сначала проверьте, не повторяется ли он в этом снимке."
        }
        "inspect_group" => {
            "Единица структурно однородна и имеет достаточную поддержку в истории: проверьте представителей и распространите вывод на группу."
        }
        "inspect_first" => {
            "В истории по этим признакам есть подтверждённые случаи: имеет смысл посмотреть единицу раньше обычного порядка."
        }
        _ => {
            "Недостаточно проверенной истории по этим признакам: единица остаётся в обычном порядке без подсказки."
        }
    };
    let mut reason = base.to_owned();
    if let Some(summary) = support {
        reason.push_str(&format!(
            " Поддержка: независимых единиц {}, записей {}, unresolved {}.",
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
