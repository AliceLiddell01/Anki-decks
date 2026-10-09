//! Объяснимые версионированные структурные паттерны проверенной истории.
//!
//! Паттерн — это агрегат по **точному** ключу признаков существующих
//! классификаций очереди. Весов, похожести и эвристик «одинаковый snippet — тот
//! же дефект» здесь нет: приблизительная схожесть не имеет права выглядеть как
//! точное совпадение.
//!
//! Статистика намеренно консервативна: при недостаточной или противоречивой
//! поддержке вывод воздерживается (`insufficient_evidence`/`contradictory`),
//! а доля единиц с решением `confirmed` называется мерой согласованности
//! наблюдений, а не precision/recall и не вероятностью безопасности нового кода.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorCode};

use super::LEARNING_POLICY_VERSION;
use super::import::{generation, parse_domain_json, sha256_hex};
use super::model::{
    CaseLinkKind, CaseRef, HistoryGeneration, PatternCaseRef, PatternReport, PatternRule,
    SupportLevel, SupportSummary, TrustLevel,
};
use super::store::{LearningRead, LearningStore};

/// Версия схемы отчёта по паттернам.
pub const PATTERN_SCHEMA_VERSION: u32 = 1;

/// Минимум независимых единиц, при котором вывод по паттерну не воздерживается.
///
/// Значение выбрано консервативно: три независимые единицы — минимальная
/// поддержка, при которой наблюдение вообще имеет смысл, а не «псевдоточная»
/// вероятность. Порог участвует в ключе политики learning.
pub const MIN_SUPPORT_UNITS: usize = 3;

/// Квантиль стандартного нормального распределения для 95 % уровня.
const Z_95: f64 = 1.959_963_984_540_054;

/// Описание применённой политики агрегации для машинного вывода.
pub const PATTERN_POLICY: &str = "Точное сопоставление по ключу признаков StructuralClassification, \
CandidateOrigin, detector/source и совместимости правил классификации; независимая единица — \
единица очереди внутри точного Git-среза или доказанной линии его ревизий, а не кандидат; минимум поддержки — 3 независимые единицы; доля единиц с решением confirmed \
сопровождается нижней границей Wilson для 95 % уровня только как мера согласованности наблюдений.";

/// Точный ключ признаков единицы наблюдения.
///
/// Все различающие контексты, перечисленные в требованиях, входят в ключ
/// отдельными ячейками: `production` и `tests`, `introduced_or_changed` и
/// `pre_existing`, human-роли текста и machine-contract, роли кода и `unknown`.
#[must_use]
/// Признаки единицы наблюдения.
///
/// Ключ паттерна описывает только структуру и контекст: семантический вывод
/// ревьюера является меткой для статистики поддержки, а не частью ключа.
/// Иначе противоречивые исходы одной и той же структуры разошлись бы по разным
/// ключам и противоречие стало бы ненаблюдаемым.
pub fn unit_features(
    signature: &crate::code_review::review_queue::GroupingSignature,
) -> BTreeMap<String, String> {
    let classification = &signature.classification;
    let mut features = BTreeMap::new();
    features.insert("detector".to_owned(), signature.detector.clone());
    features.insert("source".to_owned(), signature.source.clone());
    features.insert(
        "origin".to_owned(),
        classification.origin.as_str().to_owned(),
    );
    features.insert("role".to_owned(), classification.role.as_str().to_owned());
    features.insert(
        "code_role".to_owned(),
        classification.code_role.as_str().to_owned(),
    );
    features.insert(
        "text_role".to_owned(),
        classification
            .text_role
            .map_or_else(|| "none".to_owned(), |role| role.as_str().to_owned()),
    );
    features.insert(
        "execution".to_owned(),
        classification.execution.as_ref().map_or_else(
            || "unknown".to_owned(),
            |surface| crate::code_review::review_queue::surface_name(surface).to_owned(),
        ),
    );
    let mut surfaces: Vec<&str> = classification
        .surfaces
        .iter()
        .map(crate::code_review::review_queue::surface_name)
        .collect();
    surfaces.sort_unstable();
    features.insert("surfaces".to_owned(), surfaces.join("+"));
    features.insert(
        "file_category".to_owned(),
        file_category_name(classification.file_category.as_ref()).to_owned(),
    );
    features.insert("path_family".to_owned(), signature.path_family.clone());
    features.insert(
        "structural_pattern".to_owned(),
        signature
            .structural_pattern
            .clone()
            .unwrap_or_else(|| "none".to_owned()),
    );
    features.insert(
        "classification_unknown".to_owned(),
        classification.is_unknown().to_string(),
    );
    features
}

/// Ключ признаков с доказанной совместимостью применённых правил классификации.
/// Digest описывает версии правил и форматов, а не содержимое ревью или его исход.
#[must_use]
pub fn unit_features_with_classifier(
    signature: &crate::code_review::review_queue::GroupingSignature,
    classifier_compatibility: &str,
) -> BTreeMap<String, String> {
    let mut features = unit_features(signature);
    features.insert(
        "classifier_compatibility".to_owned(),
        classifier_compatibility.to_owned(),
    );
    features
}

fn file_category_name(category: Option<&crate::code_review::scope::FileCategory>) -> &'static str {
    match category {
        Some(crate::code_review::scope::FileCategory::Rust) => "rust",
        Some(crate::code_review::scope::FileCategory::Markdown) => "markdown",
        Some(crate::code_review::scope::FileCategory::Toml) => "toml",
        Some(crate::code_review::scope::FileCategory::Yaml) => "yaml",
        Some(crate::code_review::scope::FileCategory::Json) => "json",
        Some(crate::code_review::scope::FileCategory::Shell) => "shell",
        Some(crate::code_review::scope::FileCategory::Other) => "other",
        None => "unknown",
    }
}

/// Стабильная подпись ключа признаков.
#[must_use]
pub fn feature_signature(features: &BTreeMap<String, String>) -> String {
    let joined = features
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    sha256_hex(joined.as_bytes())
}

/// Запрос отчёта по паттернам.
#[derive(Debug, Clone, Default)]
pub struct PatternQuery {
    /// Верхняя граница числа правил в отчёте.
    pub limit: usize,
    /// Ограничение по детектору.
    pub detector: Option<String>,
    /// Ограничение по структурной роли.
    pub role: Option<String>,
    /// Ограничение по роли кода.
    pub code_role: Option<String>,
    /// Ограничение по происхождению сигнала.
    pub origin: Option<String>,
    /// Включать ли карантинные записи (по умолчанию они исключены).
    pub include_quarantine: bool,
    /// Опорное время в секундах Unix для оценки свежести.
    pub now: Option<u64>,
}

/// Внутренняя накопленная единица наблюдения.
#[derive(Debug, Clone)]
struct StoredUnit {
    review_id: String,
    /// Канонический корень точного Git-среза и доказанных ревизий этого случая.
    observation_lineage: String,
    /// Монотонная ревизия накопления истории: порядок импорта записей.
    revision: u64,
    unit_id: String,
    disposition: Option<String>,
    candidate_count: usize,
    representatives: Vec<String>,
    sample_path: Option<String>,
    sample_candidate: Option<String>,
    snippet: Option<String>,
    feature: BTreeMap<String, String>,
    imported_at: u64,
    trust: TrustLevel,
}

/// Поддержка и исторические случаи, загруженные одним проходом для сигнатуры.
#[derive(Debug, Clone)]
pub(super) struct SignatureEvidence {
    pub support: SupportSummary,
    pub cases: Vec<PatternCaseRef>,
}

/// Строит отчёт по паттернам на текущем снимке истории.
pub fn pattern_report(
    store: &LearningStore,
    query: &PatternQuery,
) -> Result<PatternReport, DomainError> {
    store.read(|read| {
        let generation = super::import::generation_of(read)?;
        let imported_at: i64 = read
            .transaction()
            .query_row(
                "SELECT COALESCE(MAX(imported_at), 0)
                 FROM learning_import WHERE (?1 = 1 OR trust = 'ast_authenticated')",
                [i64::from(query.include_quarantine)],
                |row| row.get(0),
            )
            .map_err(|error| {
                super::store::map_error(&error, "не удалось прочитать опорное время истории")
            })?;
        let history_reference_time = u64::try_from(imported_at.max(0)).unwrap_or(u64::MAX);
        let units = load_units(read, query)?;
        let findings = load_findings(read)?;
        let confirmed_findings = load_confirmed_findings(read)?;
        let mut groups: BTreeMap<String, Vec<StoredUnit>> = BTreeMap::new();
        for unit in units {
            groups.entry(feature_signature(&unit.feature)).or_default().push(unit);
        }
        let now = query.now.unwrap_or(history_reference_time);
        let mut rules = Vec::new();
        for (signature, members) in groups {
            let support = support_summary(&members, &findings, &confirmed_findings, now);
            let key = members[0].feature.clone();
            rules.push(PatternRule {
                key,
                signature,
                support,
            });
        }
        rules.sort_by(|a, b| {
            b.support
                .support_units
                .cmp(&a.support.support_units)
                .then_with(|| b.support.support_reviews.cmp(&a.support.support_reviews))
                .then_with(|| a.signature.cmp(&b.signature))
        });
        let limit = if query.limit == 0 { 20 } else { query.limit };
        rules.truncate(limit);
        let abstained = rules
            .iter()
            .all(|rule| rule.support.level != SupportLevel::Supported);
        let quarantine_limitation = if query.include_quarantine {
            format!(
                "Карантинные записи включены по явному --include-quarantine; всего карантинных записей в истории: {}.",
                generation.quarantined_reviews
            )
        } else {
            format!(
                "В срез входят только записи с доверием ast_authenticated; карантинных записей в истории: {}.",
                generation.quarantined_reviews
            )
        };
        Ok(PatternReport {
            schema_version: PATTERN_SCHEMA_VERSION,
            policy_version: LEARNING_POLICY_VERSION,
            generation,
            min_support_units: MIN_SUPPORT_UNITS,
            policy: PATTERN_POLICY.to_owned(),
            abstained,
            limitations: vec![
                "История собрана выборочно: доли решений отражают отбор ревью, а не свойства детектора."
                    .to_owned(),
                "Паттерн — наблюдение и материал для рекомендации, а не правило автоматического suppression."
                    .to_owned(),
                quarantine_limitation,
            ],
            rules,
        })
    })
}

/// Возвращает ровно один паттерн по точному ключу признаков.
pub fn pattern_catalog(
    store: &LearningStore,
    key: &BTreeMap<String, String>,
    now: u64,
) -> Result<Option<PatternRule>, DomainError> {
    let signature = feature_signature(key);
    store.read(|read| {
        let query = PatternQuery::default();
        let units = load_units(read, &query)?;
        let findings = load_findings(read)?;
        let confirmed_findings = load_confirmed_findings(read)?;
        let members: Vec<StoredUnit> = units
            .into_iter()
            .filter(|unit| feature_signature(&unit.feature) == signature)
            .collect();
        if members.is_empty() {
            return Ok(None);
        }
        let support = support_summary(&members, &findings, &confirmed_findings, now);
        Ok(Some(PatternRule {
            key: key.clone(),
            signature,
            support,
        }))
    })
}

fn load_units(
    read: &LearningRead<'_>,
    query: &PatternQuery,
) -> Result<Vec<StoredUnit>, DomainError> {
    let lineages = observation_lineages(read, query.include_quarantine)?;
    let mut statement = read
        .transaction()
        .prepare(
            "SELECT u.review_id, u.unit_id, u.disposition, u.candidate_count,
                    u.representatives_json, u.feature_json, i.imported_at, i.trust,
                    i.classifier_digest, i.revision
             FROM learning_unit AS u
             JOIN learning_import AS i ON i.review_id = u.review_id
             WHERE (?1 = 1 OR i.trust = 'ast_authenticated')
             ORDER BY u.review_id ASC, u.unit_id ASC",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать единицы истории"))?;
    let rows = statement
        .query_map(params![i64::from(query.include_quarantine)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать единицы истории"))?;
    let mut rows_data = Vec::new();
    for row in rows {
        rows_data.push(row.map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать единицы истории")
        })?);
    }
    drop(statement);
    // Пересчёт видит действующие исходы: аудируемая правка заменяет решение,
    // а отозванная — перестаёт на него влиять.
    let effective = super::feedback::effective_dispositions(read)?;
    let mut units = Vec::new();
    for (
        review_id,
        unit_id,
        disposition,
        candidate_count,
        representatives,
        feature,
        imported_at,
        trust,
        classifier_digest,
        revision,
    ) in rows_data
    {
        let disposition = effective
            .get(&(review_id.clone(), unit_id.clone()))
            .cloned()
            .unwrap_or(disposition);
        let mut feature: BTreeMap<String, String> =
            parse_domain_json(&feature, "признаки единицы")?;
        feature.remove("disposition");
        // Старую историю не объявляем совместимой с текущими правилами: берём
        // версию из её сохранённой provenance, независимо от текущей сборки.
        if feature
            .get("classifier_compatibility")
            .is_some_and(|stamp| stamp != &classifier_digest)
        {
            return Err(DomainError::new(
                ErrorCode::LearningCorrupt,
                "Метка совместимости признаков не соответствует сохранённой версии классификатора",
            ));
        }
        feature
            .entry("classifier_compatibility".to_owned())
            .or_insert(classifier_digest);
        if !matches_filters(&feature, query) {
            continue;
        }
        let representatives: Vec<String> =
            parse_domain_json(&representatives, "представители единицы")?;
        let (sample_path, sample_candidate, snippet) = sample_of(
            read,
            &review_id,
            representatives.first().map(String::as_str),
        )?;
        let trust = super::import::trust_level_of(&trust).ok_or_else(|| {
            DomainError::new(
                ErrorCode::LearningCorrupt,
                "В базе learning обнаружена неизвестная метка доверия",
            )
        })?;
        units.push(StoredUnit {
            review_id: review_id.clone(),
            observation_lineage: lineages.get(&review_id).cloned().ok_or_else(|| {
                DomainError::new(
                    ErrorCode::LearningCorrupt,
                    "Единица истории не связана с записью импорта",
                )
            })?,
            revision: u64::try_from(revision.max(0)).unwrap_or(0),
            unit_id,
            disposition,
            candidate_count: usize::try_from(candidate_count.max(0)).unwrap_or(0),
            representatives,
            sample_path,
            sample_candidate,
            snippet,
            feature,
            imported_at: u64::try_from(imported_at.max(0)).unwrap_or(0),
            trust,
        });
    }
    Ok(units)
}

/// Канонические компоненты связности истории: повтор точного диапазона либо
/// явная аудируемая ревизия. Общая база разных head не доказывает одну линию.
fn observation_lineages(
    read: &LearningRead<'_>,
    include_quarantine: bool,
) -> Result<BTreeMap<String, String>, DomainError> {
    let mut statement = read.transaction().prepare(
        "SELECT review_id, repository_id, base_sha, head_sha, merge_base_sha, revision_of, trust
         FROM learning_import WHERE (?1 = 1 OR trust = 'ast_authenticated') ORDER BY review_id",
    ).map_err(|error| super::store::map_error(&error, "не удалось прочитать линии наблюдения"))?;
    let rows = statement
        .query_map([i64::from(include_quarantine)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать линии наблюдения")
        })?;
    let records = rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
        super::store::map_error(&error, "не удалось прочитать линии наблюдения")
    })?;
    let repositories: BTreeMap<&str, (&str, &str)> = records
        .iter()
        .map(|record| (record.0.as_str(), (record.1.as_str(), record.6.as_str())))
        .collect();
    let mut parents: BTreeMap<String, String> = records
        .iter()
        .map(|record| (record.0.clone(), record.0.clone()))
        .collect();
    let mut ranges: BTreeMap<(&str, &str, &str, &str, &str), &str> = BTreeMap::new();
    for (id, repository, base, head, merge_base, previous, trust) in &records {
        let range = (
            repository.as_str(),
            base.as_str(),
            head.as_str(),
            merge_base.as_str(),
            trust.as_str(),
        );
        if let Some(first) = ranges.insert(range, id) {
            join_lineages(&mut parents, id, first);
        }
        if let Some(previous) = previous {
            if repositories.get(previous.as_str()).copied()
                != Some((repository.as_str(), trust.as_str()))
            {
                return Err(DomainError::new(
                    ErrorCode::LearningCorrupt,
                    "Ревизия истории ссылается на отсутствующий или чужой Git-репозиторий",
                ));
            }
            join_lineages(&mut parents, id, previous);
        }
    }
    Ok(parents
        .keys()
        .map(|id| (id.clone(), lineage_root(&parents, id)))
        .collect())
}

fn lineage_root(parents: &BTreeMap<String, String>, id: &str) -> String {
    let mut root = id;
    while let Some(parent) = parents.get(root) {
        if parent == root {
            break;
        }
        root = parent;
    }
    root.to_owned()
}

fn join_lineages(parents: &mut BTreeMap<String, String>, left: &str, right: &str) {
    let left = lineage_root(parents, left);
    let right = lineage_root(parents, right);
    if left < right {
        parents.insert(right, left);
    } else if right < left {
        parents.insert(left, right);
    }
}

fn matches_filters(feature: &BTreeMap<String, String>, query: &PatternQuery) -> bool {
    let matches = |name: &str, expected: &Option<String>| match expected {
        None => true,
        Some(value) => feature.get(name).map(String::as_str) == Some(value.as_str()),
    };
    matches("detector", &query.detector)
        && matches("role", &query.role)
        && matches("code_role", &query.code_role)
        && matches("origin", &query.origin)
}

/// Путь, сниппет и категория файла образца: части одного представления кандидата.
type MinoritySample = (Option<String>, Option<String>, Option<String>);

fn sample_of(
    read: &LearningRead<'_>,
    review_id: &str,
    candidate_id: Option<&str>,
) -> Result<MinoritySample, DomainError> {
    let Some(candidate_id) = candidate_id else {
        return Ok((None, None, None));
    };
    read.transaction()
        .query_row(
            "SELECT path, snippet FROM learning_candidate WHERE review_id = ?1 AND candidate_id = ?2",
            params![review_id, candidate_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .map(|(path, snippet)| (path, Some(candidate_id.to_owned()), snippet))
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok((None, Some(candidate_id.to_owned()), None)),
            other => Err(super::store::map_error(
                &other,
                "не удалось прочитать образец случая",
            )),
        })
}

/// Связанное замечание с его структурной сигнатурой и признаками.
#[derive(Debug, Clone)]
struct StoredFinding {
    review_id: String,
    finding_id: String,
    provenance: String,
    units: Vec<String>,
}

fn load_findings(read: &LearningRead<'_>) -> Result<Vec<StoredFinding>, DomainError> {
    let mut statement = read
        .transaction()
        .prepare(
            "SELECT f.review_id, f.finding_id, f.provenance, f.linked_unit_ids_json
             FROM learning_finding AS f
             JOIN learning_import AS i ON i.review_id = f.review_id
             WHERE i.trust = 'ast_authenticated'
             ORDER BY f.review_id ASC, f.finding_id ASC",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать замечания"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать замечания"))?;
    let mut data = Vec::new();
    for row in rows {
        data.push(
            row.map_err(|error| super::store::map_error(&error, "не удалось прочитать замечания"))?,
        );
    }
    drop(statement);
    let mut findings = Vec::new();
    for (review_id, finding_id, provenance, units) in data {
        let units: Vec<String> = parse_domain_json(&units, "связанные единицы")?;
        findings.push(StoredFinding {
            review_id,
            finding_id,
            provenance,
            units,
        });
    }
    Ok(findings)
}

fn support_summary(
    members: &[StoredUnit],
    findings: &[StoredFinding],
    confirmed_findings: &BTreeSet<(String, String)>,
    now: u64,
) -> SupportSummary {
    // Структурный unit_id уникален лишь внутри очереди. Независимая линия
    // определяется точным Git-срезом и сохранёнными доказанными revision_of,
    // поэтому одинаковая структура независимых PR не схлопывается.
    let mut representatives: BTreeMap<(String, String), &StoredUnit> = BTreeMap::new();
    for unit in members {
        representatives
            .entry((unit.observation_lineage.clone(), unit.unit_id.clone()))
            .and_modify(|current| {
                // Линию представляет последнее наблюдение: если ревьюер изменил
                // решение в следующей версии, действует именно оно. Порядок
                // задают время импорта и монотонная ревизия истории, поэтому
                // выбор детерминирован даже для импортов одной секунды.
                // Идентификатор записи сравнивается последним и в обратном
                // порядке: при полном совпадении времени и ревизии выбор всё
                // равно остаётся детерминированным.
                let newer = (unit.imported_at, unit.revision)
                    > (current.imported_at, current.revision)
                    || ((unit.imported_at, unit.revision)
                        == (current.imported_at, current.revision)
                        && unit.review_id < current.review_id);
                if newer {
                    *current = unit;
                }
            })
            .or_insert(unit);
    }
    let independent: Vec<&StoredUnit> = representatives.values().copied().collect();
    let mut by_disposition: BTreeMap<&str, usize> = BTreeMap::new();
    // Число записей ревью, наблюдавших срез: сырые запуски, а не независимые
    // линии. Пара к `support_units` объясняет разницу между «сколько раз
    // смотрели» и «сколько независимых наблюдений получилось».
    let reviews: BTreeSet<&str> = members.iter().map(|unit| unit.review_id.as_str()).collect();
    let mut confirmed_units = 0usize;
    let mut reviewed_units = 0usize;
    let mut unresolved_units = 0usize;
    // Находки относятся к срезу только через независимые единицы: замечание,
    // привязанное исключительно к свёрнутому повтору того же случая, не
    // добавляет срезу новой информации и в статистику не попадает.
    let unit_ids: BTreeSet<(&str, &str)> = independent
        .iter()
        .map(|unit| (unit.review_id.as_str(), unit.unit_id.as_str()))
        .collect();
    for unit in &independent {
        match unit.disposition.as_deref() {
            Some(disposition) => {
                *by_disposition.entry(disposition).or_default() += 1;
                reviewed_units += 1;
                if disposition == "confirmed" {
                    confirmed_units += 1;
                }
            }
            // Отсутствие решения — не позиция, а нехватка информации: в
            // распределение решений оно не попадает и противоречия не создаёт.
            None => unresolved_units += 1,
        }
    }
    // Свежесть наблюдений считается по всем единицам среза: это свойство
    // наблюдений, а не решений, и сворачивание повторов его не меняет.
    let ages: Vec<u64> = members
        .iter()
        .map(|unit| now.saturating_sub(unit.imported_at) / 86_400)
        .collect();
    let mut findings_total = 0usize;
    let mut findings_by_provenance: BTreeMap<String, usize> = BTreeMap::new();
    let mut confirmed_linked_findings = 0usize;
    for finding in findings {
        // Принадлежность срезу определяется связью находки с его единицами
        // (`linked_unit_ids_json`), а не совпадением хешей: подпись находки
        // хеширует серьёзность, происхождение и подписи единиц — это другой
        // домен, чем подпись ключа признаков.
        if !finding
            .units
            .iter()
            .any(|unit| unit_ids.contains(&(finding.review_id.as_str(), unit.as_str())))
        {
            continue;
        }
        findings_total += 1;
        *findings_by_provenance
            .entry(finding.provenance.clone())
            .or_default() += 1;
        // Подтверждённость — свойство решения ревьюера, а не серьёзности:
        // замечание считается подтверждённым, только если на него ссылается
        // решение с исходом `confirmed`.
        if confirmed_findings.contains(&(finding.review_id.clone(), finding.finding_id.clone())) {
            confirmed_linked_findings += 1;
        }
    }
    let support_units = independent.len();
    let revised_units = members.len().saturating_sub(support_units);
    let minority: Vec<String> = if by_disposition.len() > 1 {
        let (majority, _) = by_disposition
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map_or(("", &0), |(name, count)| (*name, count));
        independent
            .iter()
            .filter(|unit| match unit.disposition.as_deref() {
                Some(disposition) => disposition != majority,
                None => false,
            })
            .map(|unit| unit.unit_id.clone())
            .collect()
    } else {
        Vec::new()
    };
    let covered_candidates: usize = independent.iter().map(|unit| unit.candidate_count).sum();
    let (level, lower_bound, explanation) = classify_support(
        support_units,
        reviewed_units,
        confirmed_units,
        covered_candidates,
        revised_units,
        unresolved_units,
        &minority,
    );
    let mut provenance = findings_by_provenance;
    provenance.insert("findings_total".to_owned(), findings_total);
    SupportSummary {
        support_units,
        support_reviews: reviews.len(),
        confirmed_units,
        acceptable_units: *by_disposition.get("acceptable").unwrap_or(&0),
        false_positive_units: *by_disposition.get("false_positive").unwrap_or(&0),
        not_applicable_units: *by_disposition.get("not_applicable").unwrap_or(&0),
        uncertain_units: *by_disposition.get("uncertain").unwrap_or(&0),
        unresolved_units,
        confirmed_findings: confirmed_linked_findings,
        findings_by_provenance: provenance,
        freshest_age_days: ages.iter().copied().min().unwrap_or_default(),
        oldest_age_days: ages.iter().copied().max().unwrap_or_default(),
        revised_units,
        level,
        confirmed_share_lower_bound: lower_bound,
        explanation,
        contradicting_unit_ids: minority,
    }
}

fn classify_support(
    support_units: usize,
    reviewed_units: usize,
    confirmed_units: usize,
    covered_candidates: usize,
    revised_units: usize,
    unresolved_units: usize,
    minority: &[String],
) -> (SupportLevel, Option<f64>, String) {
    if support_units < MIN_SUPPORT_UNITS {
        return (
            SupportLevel::InsufficientEvidence,
            None,
            format!(
                "Недостаточно независимых единиц: {support_units} < {MIN_SUPPORT_UNITS}; вывод воздерживается. Повторных наблюдений того же случая исключено: {revised_units}, без решения: {unresolved_units}."
            ),
        );
    }
    if !minority.is_empty() {
        return (
            SupportLevel::Contradictory,
            Some(wilson_lower_bound(confirmed_units, reviewed_units)),
            format!(
                "Поддерживающие единицы противоречат друг другу: из {reviewed_units} рассмотренных независимых единиц {} имеют отличающееся решение; не рассмотрено: {unresolved_units}.",
                minority.len()
            ),
        );
    }
    if reviewed_units < MIN_SUPPORT_UNITS {
        return (
            SupportLevel::InsufficientEvidence,
            None,
            format!(
                "Независимых единиц: {support_units}, но решений ревьюера только {reviewed_units} < {MIN_SUPPORT_UNITS}; вывод воздерживается. Без решения: {unresolved_units}, повторных наблюдений исключено: {revised_units}."
            ),
        );
    }
    let lower = wilson_lower_bound(confirmed_units, reviewed_units);
    (
        SupportLevel::Supported,
        Some(lower),
        format!(
            "Независимых единиц: {support_units}, покрытых кандидатов: {covered_candidates}, из них с решением: {reviewed_units} (без решения: {unresolved_units}); повторных наблюдений того же случая исключено: {revised_units}; нижняя граница доли confirmed для 95 % уровня: {lower:.6}. Это мера согласованности наблюдений, а не вероятность безопасности нового кода."
        ),
    )
}

/// Находки, на которые ссылается хотя бы одно решение с исходом `confirmed`.
///
/// В базе связь «находка → решение» не материализована: решение хранит
/// покрытых кандидатов (`covered_json`), а находка — своих кандидатов
/// (`learning_finding_link`). Подтверждённость выводится пересечением этих
/// множеств в пределах одной записи, а серьёзность находки в неё не входит.
fn load_confirmed_findings(
    read: &LearningRead<'_>,
) -> Result<BTreeSet<(String, String)>, DomainError> {
    let mut decisions_statement = read
        .transaction()
        .prepare(
            "SELECT review_id, covered_json FROM learning_decision
             WHERE disposition = 'confirmed'
             ORDER BY review_id ASC, decision_id ASC",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать решения"))?;
    let decisions = decisions_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать решения"))?;
    let mut confirmed_decisions = Vec::new();
    for row in decisions {
        confirmed_decisions.push(
            row.map_err(|error| super::store::map_error(&error, "не удалось прочитать решения"))?,
        );
    }
    drop(decisions_statement);
    let mut links_statement = read
        .transaction()
        .prepare(
            "SELECT review_id, candidate_id, finding_id FROM learning_finding_link
             ORDER BY review_id ASC, finding_id ASC, candidate_id ASC",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать связи замечаний"))?;
    let links = links_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать связи замечаний"))?;
    let mut findings_of_candidate: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for row in links {
        let (review_id, candidate_id, finding_id) = row.map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать связи замечаний")
        })?;
        findings_of_candidate
            .entry((review_id, candidate_id))
            .or_default()
            .push(finding_id);
    }
    drop(links_statement);
    let mut confirmed = BTreeSet::new();
    for (review_id, covered) in confirmed_decisions {
        let covered: Vec<String> = parse_domain_json(&covered, "покрытые кандидаты решения")?;
        for candidate_id in covered {
            if let Some(finding_ids) = findings_of_candidate.get(&(review_id.clone(), candidate_id))
            {
                for finding_id in finding_ids {
                    confirmed.insert((review_id.clone(), finding_id.clone()));
                }
            }
        }
    }
    Ok(confirmed)
}

/// Вид связи повторяющегося случая для каждой единицы, установленный импортом.
///
/// Читаются явные записи `learning_case_link`, а не подстрока JSON: вид связи
/// доказан импортом, а принадлежность единицы берётся из разобранного
/// `linked_unit_ids_json`.
fn load_case_links(
    read: &LearningRead<'_>,
) -> Result<BTreeMap<(String, String), CaseLinkKind>, DomainError> {
    let mut statement = read
        .transaction()
        .prepare(
            "SELECT l.review_id, l.kind, f.linked_unit_ids_json
             FROM learning_case_link AS l
             JOIN learning_finding AS f
               ON f.review_id = l.review_id AND f.finding_id = l.finding_id
             ORDER BY l.review_id ASC, l.finding_id ASC, l.linked_review_id ASC, l.linked_finding_id ASC",
        )
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать связи случаев"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| super::store::map_error(&error, "не удалось прочитать связи случаев"))?;
    let mut data = Vec::new();
    for row in rows {
        data.push(row.map_err(|error| {
            super::store::map_error(&error, "не удалось прочитать связи случаев")
        })?);
    }
    drop(statement);
    let mut links: BTreeMap<(String, String), CaseLinkKind> = BTreeMap::new();
    for (review_id, kind, units) in data {
        let kind = match kind.as_str() {
            "structural_repeat" => CaseLinkKind::StructuralRepeat,
            "same_iteration" => CaseLinkKind::SameIteration,
            _ => CaseLinkKind::Unknown,
        };
        let units: Vec<String> = parse_domain_json(&units, "связанные единицы повтора")?;
        for unit_id in units {
            let entry = links.entry((review_id.clone(), unit_id)).or_insert(kind);
            // Структурный аналог полезен для навигации, но не доказывает тот же дефект.
            if kind == CaseLinkKind::StructuralRepeat {
                *entry = kind;
            }
        }
    }
    Ok(links)
}

/// Нижняя граница доли по интервалу Уилсона для 95 % уровня.
///
/// Формула выбрана как прозрачная консервативная оценка при малой выборке:
/// при `n < 3` она не вычисляется вовсе, а срез воздерживается. Она никогда не
/// называется precision/recall и не используется как вероятность безопасности.
#[must_use]
pub fn wilson_lower_bound(successes: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z2 = Z_95 * Z_95;
    let denominator = 1.0 + z2 / n;
    let center = p + z2 / (2.0 * n);
    let spread = Z_95 * ((p * (1.0 - p) / n) + z2 / (4.0 * n * n)).sqrt();
    let lower = (center - spread) / denominator;
    round6(lower.clamp(0.0, 1.0))
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// Срез поддержки по точному ключу признаков для указанной генерации.
pub fn support_for_signature(
    store: &LearningStore,
    signature: &str,
    now: u64,
) -> Result<Option<SupportSummary>, DomainError> {
    store.read(|read| {
        let query = PatternQuery::default();
        let units = load_units(read, &query)?;
        let findings = load_findings(read)?;
        let confirmed_findings = load_confirmed_findings(read)?;
        let members: Vec<StoredUnit> = units
            .into_iter()
            .filter(|unit| feature_signature(&unit.feature) == signature)
            .collect();
        if members.is_empty() {
            return Ok(None);
        }
        Ok(Some(support_summary(
            &members,
            &findings,
            &confirmed_findings,
            now,
        )))
    })
}

/// Ссылки на исторические случаи по точному ключу признаков.
pub fn cases_for_signature(
    store: &LearningStore,
    signature: &str,
    limit: usize,
    now: u64,
) -> Result<Vec<PatternCaseRef>, DomainError> {
    store.read(|read| {
        let query = PatternQuery::default();
        let units = load_units(read, &query)?;
        let links = load_case_links(read)?;
        let mut cases = Vec::new();
        for unit in units
            .into_iter()
            .filter(|unit| feature_signature(&unit.feature) == signature)
        {
            if cases.len() >= limit {
                break;
            }
            let link = links
                .get(&(unit.review_id.clone(), unit.unit_id.clone()))
                .copied()
                .unwrap_or(CaseLinkKind::Unknown);
            cases.push(PatternCaseRef {
                case: CaseRef {
                    review_id: unit.review_id,
                    unit_id: unit.unit_id,
                    candidate_ids: if unit.sample_candidate.is_some() {
                        // Образец противоречащего случая — тот кандидат, который
                        // действительно дал наблюдение, а не весь список единицы.
                        std::slice::from_ref(&unit.sample_candidate)
                            .iter()
                            .flatten()
                            .cloned()
                            .collect()
                    } else {
                        unit.representatives
                    },
                    path: unit.sample_path,
                    snippet: unit.snippet,
                    disposition: unit.disposition,
                    trust: unit.trust,
                    age_days: now.saturating_sub(unit.imported_at) / 86_400,
                },
                link,
            });
        }
        Ok(cases)
    })
}

/// Загружает историю для набора сигнатур за один проход по каждому источнику.
/// Рекомендации строятся для всей очереди разом, поэтому отдельный полный запрос
/// на каждую единицу делал стоимость пропорциональной `очередь × история`.
pub(super) fn evidence_for_signatures(
    read: &LearningRead<'_>,
    signatures: &BTreeSet<String>,
    case_limit: usize,
    now: u64,
) -> Result<BTreeMap<String, SignatureEvidence>, DomainError> {
    if signatures.is_empty() {
        return Ok(BTreeMap::new());
    }
    let units = load_units(read, &PatternQuery::default())?;
    let findings = load_findings(read)?;
    let confirmed_findings = load_confirmed_findings(read)?;
    let links = load_case_links(read)?;
    let mut members_by_signature: BTreeMap<String, Vec<StoredUnit>> = BTreeMap::new();
    let mut signatures_by_unit: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for unit in units {
        let signature = feature_signature(&unit.feature);
        if signatures.contains(&signature) {
            signatures_by_unit
                .entry((unit.review_id.clone(), unit.unit_id.clone()))
                .or_default()
                .insert(signature.clone());
            members_by_signature
                .entry(signature)
                .or_default()
                .push(unit);
        }
    }

    let mut findings_by_signature: BTreeMap<String, Vec<StoredFinding>> = BTreeMap::new();
    for finding in findings {
        let linked_signatures: BTreeSet<&str> = finding
            .units
            .iter()
            .filter_map(|unit_id| {
                signatures_by_unit.get(&(finding.review_id.clone(), unit_id.clone()))
            })
            .flat_map(|values| values.iter().map(String::as_str))
            .collect();
        for signature in linked_signatures {
            findings_by_signature
                .entry(signature.to_owned())
                .or_default()
                .push(finding.clone());
        }
    }

    let mut evidence = BTreeMap::new();
    for (signature, members) in members_by_signature {
        let support = support_summary(
            &members,
            findings_by_signature
                .get(&signature)
                .map_or(&[], Vec::as_slice),
            &confirmed_findings,
            now,
        );
        let cases = members
            .into_iter()
            .take(case_limit)
            .map(|unit| {
                let link = links
                    .get(&(unit.review_id.clone(), unit.unit_id.clone()))
                    .copied()
                    .unwrap_or(CaseLinkKind::Unknown);
                PatternCaseRef {
                    case: CaseRef {
                        review_id: unit.review_id,
                        unit_id: unit.unit_id,
                        candidate_ids: if unit.sample_candidate.is_some() {
                            unit.sample_candidate.into_iter().collect()
                        } else {
                            unit.representatives
                        },
                        path: unit.sample_path,
                        snippet: unit.snippet,
                        disposition: unit.disposition,
                        trust: unit.trust,
                        age_days: now.saturating_sub(unit.imported_at) / 86_400,
                    },
                    link,
                }
            })
            .collect();
        evidence.insert(signature, SignatureEvidence { support, cases });
    }
    Ok(evidence)
}

/// Пустая заготовка отчёта без обращений к хранилищу.
#[must_use]
pub fn empty_report() -> PatternReport {
    PatternReport {
        schema_version: PATTERN_SCHEMA_VERSION,
        policy_version: LEARNING_POLICY_VERSION,
        generation: HistoryGeneration {
            revision: 0,
            trusted_reviews: 0,
            quarantined_reviews: 0,
            trusted_units: 0,
        },
        min_support_units: MIN_SUPPORT_UNITS,
        policy: PATTERN_POLICY.to_owned(),
        abstained: true,
        limitations: vec!["История пуста: обучение воздерживается.".to_owned()],
        rules: Vec::new(),
    }
}

/// Проверка, что отчёт опирается на действующую версию схемы.
pub fn require_supported(report: &PatternReport) -> Result<(), DomainError> {
    if report.schema_version != PATTERN_SCHEMA_VERSION
        || report.policy_version != LEARNING_POLICY_VERSION
    {
        return Err(DomainError::new(
            ErrorCode::InsufficientEvidence,
            "Отчёт по паттернам построен по несовместимой версии схемы или политики",
        ));
    }
    Ok(())
}

/// Сериализуемое представление связи случая с текущим ревью.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseLinkView {
    /// Идентификатор случая истории.
    pub review_id: String,
    /// Идентификатор единицы.
    pub unit_id: String,
    /// Вид связи.
    pub kind: String,
    /// Основание связи.
    pub basis: String,
}

/// Возвращает пропуск в поддержке для явного вывода о недостаточности истории.
pub fn insufficient_evidence(store: &LearningStore) -> Result<bool, DomainError> {
    let generation = generation(store)?;
    Ok(generation.trusted_units < MIN_SUPPORT_UNITS)
}
