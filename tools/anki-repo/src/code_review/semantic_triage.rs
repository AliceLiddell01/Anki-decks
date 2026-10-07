//! Внешние семантические решения для точного неизменяемого пакета свидетельств.
//!
//! Этот модуль не собирает свидетельства, не меняет исходники и не выводит
//! решения из эвристик. JSON — источник решений; Markdown — его представление.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, ErrorCode};

use super::model::ReviewPack;
use super::scope::GitTarget;

/// Версия отдельного контракта семантического разбора.
pub const TRIAGE_SCHEMA_VERSION: u32 = 1;

/// Состояние кандидата после внешнего семантического рассмотрения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Confirmed,
    Acceptable,
    FalsePositive,
    NotApplicable,
    Uncertain,
}

impl Disposition {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Acceptable => "acceptable",
            Self::FalsePositive => "false_positive",
            Self::NotApplicable => "not_applicable",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Устойчивые причины; пояснение сохраняет конкретный контекст решения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    PreExisting,
    OutOfScope,
    TestFixture,
    MachineContract,
    TechnicalIdentifier,
    DocumentedInvariant,
    ExpectedFailurePath,
    InsufficientEvidence,
    DuplicateSignal,
    Other,
}

impl ReasonCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreExisting => "pre_existing",
            Self::OutOfScope => "out_of_scope",
            Self::TestFixture => "test_fixture",
            Self::MachineContract => "machine_contract",
            Self::TechnicalIdentifier => "technical_identifier",
            Self::DocumentedInvariant => "documented_invariant",
            Self::ExpectedFailurePath => "expected_failure_path",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::DuplicateSignal => "duplicate_signal",
            Self::Other => "other",
        }
    }
}

/// Уровни серьёзности в порядке политики независимого ревью.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    Major,
    Minor,
    Trivial,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Trivial => "trivial",
        }
    }
}

/// Вклад кандидата в нахождение замечания задаёт ревьюер, а не детектор.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingProvenance {
    DirectCandidate,
    CandidateAssisted,
    Independent,
}

impl FindingProvenance {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectCandidate => "direct_candidate",
            Self::CandidateAssisted => "candidate_assisted",
            Self::Independent => "independent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageSource {
    /// Полная переносимая идентичность Git без пути к локальному клону.
    pub snapshot: TriageSnapshot,
    /// SHA-256 точных байтов исходного review.json в виде строчных шестнадцатеричных символов.
    pub review_pack_sha256: String,
    /// Число уникальных кандидатов в общей выборке статического анализа и языковой проверки.
    pub candidate_count: usize,
}

/// Строгая форма Git-снимка внутри семантического разбора.
///
/// `GitTarget` также используется внутри старого `review.json`, где неизвестные
/// поля сохраняют совместимость формата. Новый контракт разбора закрыт для
/// неизвестных полей и поэтому использует отдельный тип с теми же данными.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageSnapshot {
    pub repository_id: String,
    pub base_sha: String,
    pub head_sha: String,
    pub merge_base_sha: String,
}

impl From<&GitTarget> for TriageSnapshot {
    fn from(target: &GitTarget) -> Self {
        Self {
            repository_id: target.repository_id.clone(),
            base_sha: target.base_sha.clone(),
            head_sha: target.head_sha.clone(),
            merge_base_sha: target.merge_base_sha.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDecision {
    pub candidate_id: String,
    pub disposition: Disposition,
    pub reason_code: ReasonCode,
    pub explanation: String,
    pub finding_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupDecision {
    pub id: String,
    /// Точное множество покрытых IDs; каждый получает решение всей группы.
    pub candidate_ids: Vec<String>,
    /// Рассмотренные представители входят в покрытое множество.
    pub representative_candidate_ids: Vec<String>,
    pub disposition: Disposition,
    pub reason_code: ReasonCode,
    pub explanation: String,
    pub finding_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFinding {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    /// Место, нарушенное ожидание, сценарий и последствия описывает ревьюер.
    pub description: String,
    pub provenance: FindingProvenance,
    pub candidate_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticTriage {
    pub schema_version: u32,
    pub source: TriageSource,
    pub individual_decisions: Vec<CandidateDecision>,
    pub group_decisions: Vec<GroupDecision>,
    pub findings: Vec<SemanticFinding>,
    /// Отсутствие решения явно сохраняется, включая частично завершённое ревью.
    pub unreviewed_candidate_ids: Vec<String>,
}

/// Распределение именно единиц решений, без подмены групп отдельными решениями.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionCounts {
    pub decision_count: usize,
    pub by_disposition: BTreeMap<Disposition, usize>,
    pub by_reason_code: BTreeMap<ReasonCode, usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupReviewCounts {
    pub decisions: DecisionCounts,
    pub covered_candidate_ids: usize,
    pub representative_candidate_ids: usize,
}

/// Распределение покрытия детектора, включая происхождение каждого решения.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectorSummary {
    pub total_candidates: usize,
    pub reviewed_candidates: usize,
    pub unreviewed_candidates: usize,
    pub individually_reviewed_candidates: usize,
    pub group_reviewed_candidates: usize,
    pub finding_linked_candidates: usize,
    /// Числа покрытых IDs, а не числа отдельных решений.
    pub covered_ids_by_disposition: BTreeMap<Disposition, usize>,
    pub covered_ids_by_reason_code: BTreeMap<ReasonCode, usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingSummary {
    pub total_findings: usize,
    pub by_severity: BTreeMap<Severity, usize>,
    pub by_provenance: BTreeMap<FindingProvenance, usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageSummary {
    pub schema_version: u32,
    pub total_candidates: usize,
    pub reviewed_candidates: usize,
    pub unreviewed_candidates: usize,
    pub individual_review: DecisionCounts,
    pub group_review: GroupReviewCounts,
    pub by_detector: BTreeMap<String, DetectorSummary>,
    pub findings: FindingSummary,
}

/// Создаёт незавершённое ревью без автоматически присвоенных решений.
#[must_use]
pub fn initialize(pack: &ReviewPack, source_pack_sha256: &str) -> SemanticTriage {
    let ids: BTreeSet<_> = pack.all_candidates().into_iter().map(|c| c.id).collect();
    SemanticTriage {
        schema_version: TRIAGE_SCHEMA_VERSION,
        source: source_identity(pack, source_pack_sha256),
        individual_decisions: Vec::new(),
        group_decisions: Vec::new(),
        findings: Vec::new(),
        unreviewed_candidate_ids: ids.into_iter().collect(),
    }
}

/// Создаёт общую для производных review-artifacts идентичность точного пакета.
#[must_use]
pub fn source_identity(pack: &ReviewPack, source_pack_sha256: &str) -> TriageSource {
    let candidate_count = pack
        .all_candidates()
        .into_iter()
        .map(|candidate| candidate.id)
        .collect::<BTreeSet<_>>()
        .len();
    TriageSource {
        snapshot: TriageSnapshot::from(&pack.target),
        review_pack_sha256: source_pack_sha256.to_owned(),
        candidate_count,
    }
}

/// Проверяет общую границу неизменного источника для семантического разбора и очереди ревью.
///
/// Дайджест должен быть вычислен по точным байтам `review.json`, а не по
/// повторно сериализованной структуре. Проверка не назначает кандидатам
/// семантический статус.
pub fn validate_source(
    source: &TriageSource,
    pack: &ReviewPack,
    source_pack_sha256: &str,
) -> Result<(), DomainError> {
    validate_source_for(
        source,
        pack,
        source_pack_sha256,
        "производного review-artifact",
    )
}

fn validate_source_for(
    source: &TriageSource,
    pack: &ReviewPack,
    source_pack_sha256: &str,
    artifact_label: &str,
) -> Result<(), DomainError> {
    let valid_digest = source_pack_sha256.len() == 64
        && source_pack_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid_digest || source.review_pack_sha256 != source_pack_sha256 {
        return Err(invalid(format!(
            "SHA-256 {artifact_label} не соответствует точным байтам исходного review.json"
        )));
    }
    if source.snapshot != TriageSnapshot::from(&pack.target) {
        return Err(DomainError::new(
            ErrorCode::BaselineMismatch,
            format!("Git-снимок {artifact_label} не соответствует исходному review.json"),
        ));
    }
    let all_candidates = pack.all_candidates();
    let candidates: BTreeSet<_> = all_candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    if all_candidates.len() != candidates.len() || candidates.iter().any(|id| id.trim().is_empty())
    {
        return Err(invalid(
            "В исходном review.json есть пустые или повторяющиеся ID кандидатов",
        ));
    }
    if source.candidate_count != candidates.len() {
        return Err(invalid(
            "Значение candidate_count не соответствует числу кандидатов в исходном review.json",
        ));
    }
    Ok(())
}

/// Сортирует множества, сохраняя дубликаты для последующего отказа проверки.
pub fn canonicalize(triage: &mut SemanticTriage) {
    triage
        .individual_decisions
        .sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
    for decision in &mut triage.individual_decisions {
        decision.finding_ids.sort();
    }
    triage.group_decisions.sort_by(|a, b| a.id.cmp(&b.id));
    for group in &mut triage.group_decisions {
        group.candidate_ids.sort();
        group.representative_candidate_ids.sort();
        group.finding_ids.sort();
    }
    triage.findings.sort_by(|a, b| a.id.cmp(&b.id));
    for finding in &mut triage.findings {
        finding.candidate_ids.sort();
    }
    triage.unreviewed_candidate_ids.sort();
}

fn invalid(message: impl Into<String>) -> DomainError {
    DomainError::new(ErrorCode::ReviewArtifactInvalid, message)
}

fn require_nonempty(value: &str, field: &str) -> Result<(), DomainError> {
    if value.trim().is_empty() {
        return Err(invalid(format!(
            "В документе семантического разбора не задано значение: {field}"
        )));
    }
    Ok(())
}

fn unique_strings<'a>(values: &'a [String], field: &str) -> Result<BTreeSet<&'a str>, DomainError> {
    let mut set = BTreeSet::new();
    for value in values {
        require_nonempty(value, field)?;
        if !set.insert(value.as_str()) {
            return Err(invalid(format!("Повтор значения «{value}» в {field}")));
        }
    }
    Ok(set)
}

fn cover<'a>(
    id: &'a str,
    candidates: &BTreeSet<String>,
    coverage: &mut BTreeSet<&'a str>,
) -> Result<(), DomainError> {
    if !candidates.contains(id) {
        return Err(invalid(format!("Неизвестный ID кандидата: {id}")));
    }
    if !coverage.insert(id) {
        return Err(invalid(format!(
            "Кандидат уже охвачен другим решением: {id}"
        )));
    }
    Ok(())
}

fn link_decision<'a>(
    candidate_ids: impl IntoIterator<Item = &'a str>,
    finding_ids: &'a [String],
    disposition: Disposition,
    explanation: &str,
    findings: &BTreeMap<&str, &SemanticFinding>,
    links: &mut BTreeSet<(&'a str, &'a str)>,
) -> Result<(), DomainError> {
    require_nonempty(explanation, "explanation")?;
    let unique_findings = unique_strings(finding_ids, "decision.finding_ids")?;
    if disposition == Disposition::Confirmed && unique_findings.is_empty() {
        return Err(invalid(
            "Для решения confirmed нужно указать хотя бы одно связанное замечание",
        ));
    }
    for finding_id in &unique_findings {
        if !findings.contains_key(finding_id) {
            return Err(invalid(format!("Неизвестный ID замечания: {finding_id}")));
        }
    }
    for candidate_id in candidate_ids {
        for finding_id in &unique_findings {
            links.insert((candidate_id, *finding_id));
        }
    }
    Ok(())
}

/// Проверяет идентичность, точное покрытие и взаимность всех связей.
///
/// Дайджест относится к точным байтам review.json; его вычисляет владелец чтения.
/// Функция принимает входные данные в любом порядке и не нормализует противоречия.
pub fn validate(
    triage: &SemanticTriage,
    pack: &ReviewPack,
    source_pack_sha256: &str,
) -> Result<TriageSummary, DomainError> {
    if triage.schema_version != TRIAGE_SCHEMA_VERSION {
        return Err(invalid(
            "Неподдерживаемое значение schema_version документа семантического разбора",
        ));
    }
    validate_source_for(
        &triage.source,
        pack,
        source_pack_sha256,
        "документа семантического разбора",
    )?;
    let all_candidates = pack.all_candidates();
    let candidates: BTreeSet<_> = all_candidates.iter().map(|c| c.id.clone()).collect();
    let mut findings = BTreeMap::new();
    for finding in &triage.findings {
        require_nonempty(&finding.id, "finding.id")?;
        require_nonempty(&finding.title, "finding.title")?;
        require_nonempty(&finding.description, "finding.description")?;
        if findings.insert(finding.id.as_str(), finding).is_some() {
            return Err(invalid(format!("Повтор ID замечания: {}", finding.id)));
        }
    }
    let mut coverage = BTreeSet::new();
    let mut decision_links = BTreeSet::new();
    let mut reviewed_dispositions = BTreeMap::new();
    for decision in &triage.individual_decisions {
        cover(&decision.candidate_id, &candidates, &mut coverage)?;
        reviewed_dispositions.insert(decision.candidate_id.as_str(), decision.disposition);
        link_decision(
            [decision.candidate_id.as_str()],
            &decision.finding_ids,
            decision.disposition,
            &decision.explanation,
            &findings,
            &mut decision_links,
        )?;
    }
    let mut group_ids = BTreeSet::new();
    for group in &triage.group_decisions {
        require_nonempty(&group.id, "group.id")?;
        if !group_ids.insert(group.id.as_str()) {
            return Err(invalid(format!("Повтор ID группы: {}", group.id)));
        }
        let ids = unique_strings(&group.candidate_ids, "group.candidate_ids")?;
        if ids.len() < 2 {
            return Err(invalid(
                "Группа должна охватывать хотя бы два разных ID кандидатов",
            ));
        }
        let representatives = unique_strings(
            &group.representative_candidate_ids,
            "group.representative_candidate_ids",
        )?;
        if representatives.is_empty() || !representatives.is_subset(&ids) {
            return Err(invalid(
                "ID представителей группы должны быть непустым подмножеством ID кандидатов в ней",
            ));
        }
        for id in &ids {
            cover(id, &candidates, &mut coverage)?;
            reviewed_dispositions.insert(*id, group.disposition);
        }
        link_decision(
            ids,
            &group.finding_ids,
            group.disposition,
            &group.explanation,
            &findings,
            &mut decision_links,
        )?;
    }
    for id in &triage.unreviewed_candidate_ids {
        cover(id, &candidates, &mut coverage)?;
    }
    if coverage.len() != candidates.len() {
        return Err(invalid(
            "Для каждого кандидата исходного пакета нужно указать решение или внести его ID в unreviewed_candidate_ids",
        ));
    }
    let mut finding_links = BTreeSet::new();
    for finding in &triage.findings {
        let ids = unique_strings(&finding.candidate_ids, "finding.candidate_ids")?;
        match finding.provenance {
            FindingProvenance::Independent if !ids.is_empty() => {
                return Err(invalid(
                    "Для замечания со значением independent в provenance список candidate_ids должен быть пуст",
                ));
            }
            FindingProvenance::DirectCandidate | FindingProvenance::CandidateAssisted
                if ids.is_empty() =>
            {
                return Err(invalid(
                    "Для замечания, связанного с кандидатом, нужно указать хотя бы один ID в candidate_ids",
                ));
            }
            _ => {}
        }
        for id in ids {
            if !candidates.contains(id) || !reviewed_dispositions.contains_key(id) {
                return Err(invalid(format!(
                    "Замечание связано с отсутствующим или нерассмотренным кандидатом: {id}"
                )));
            }
            if finding.provenance == FindingProvenance::DirectCandidate
                && reviewed_dispositions.get(id) != Some(&Disposition::Confirmed)
            {
                return Err(invalid(format!(
                    "Для замечания со значением direct_candidate кандидат должен иметь решение confirmed: {id}"
                )));
            }
            finding_links.insert((id, finding.id.as_str()));
        }
    }
    if decision_links != finding_links {
        return Err(invalid(
            "Связи decision.finding_ids и finding.candidate_ids должны в точности совпадать",
        ));
    }
    Ok(summarize(triage, pack))
}

fn count_decision(counts: &mut DecisionCounts, disposition: Disposition, reason: ReasonCode) {
    counts.decision_count += 1;
    *counts.by_disposition.entry(disposition).or_default() += 1;
    *counts.by_reason_code.entry(reason).or_default() += 1;
}

/// Статистика проверенного документа; не создаёт решения для пропущенных ID.
#[must_use]
pub fn summarize(triage: &SemanticTriage, pack: &ReviewPack) -> TriageSummary {
    let mut summary = TriageSummary {
        schema_version: TRIAGE_SCHEMA_VERSION,
        ..TriageSummary::default()
    };
    let mut outcomes = BTreeMap::new();
    for decision in &triage.individual_decisions {
        count_decision(
            &mut summary.individual_review,
            decision.disposition,
            decision.reason_code,
        );
        outcomes.insert(
            decision.candidate_id.as_str(),
            (
                decision.disposition,
                decision.reason_code,
                false,
                !decision.finding_ids.is_empty(),
            ),
        );
    }
    for group in &triage.group_decisions {
        count_decision(
            &mut summary.group_review.decisions,
            group.disposition,
            group.reason_code,
        );
        summary.group_review.covered_candidate_ids += group.candidate_ids.len();
        summary.group_review.representative_candidate_ids +=
            group.representative_candidate_ids.len();
        for id in &group.candidate_ids {
            outcomes.insert(
                id.as_str(),
                (
                    group.disposition,
                    group.reason_code,
                    true,
                    !group.finding_ids.is_empty(),
                ),
            );
        }
    }
    for candidate in pack.all_candidates() {
        summary.total_candidates += 1;
        let detector = summary.by_detector.entry(candidate.detector).or_default();
        detector.total_candidates += 1;
        if let Some(&(disposition, reason, grouped, linked)) = outcomes.get(candidate.id.as_str()) {
            summary.reviewed_candidates += 1;
            detector.reviewed_candidates += 1;
            if grouped {
                detector.group_reviewed_candidates += 1;
            } else {
                detector.individually_reviewed_candidates += 1;
            }
            if linked {
                detector.finding_linked_candidates += 1;
            }
            *detector
                .covered_ids_by_disposition
                .entry(disposition)
                .or_default() += 1;
            *detector
                .covered_ids_by_reason_code
                .entry(reason)
                .or_default() += 1;
        } else {
            summary.unreviewed_candidates += 1;
            detector.unreviewed_candidates += 1;
        }
    }
    summary.findings.total_findings = triage.findings.len();
    for finding in &triage.findings {
        *summary
            .findings
            .by_severity
            .entry(finding.severity)
            .or_default() += 1;
        *summary
            .findings
            .by_provenance
            .entry(finding.provenance)
            .or_default() += 1;
    }
    summary
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

fn markdown_inline(value: &str) -> String {
    let normalized = value.replace(['\n', '\r'], " ");
    escape_markdown_punctuation(&normalized)
}

fn escape_markdown_punctuation(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn markdown_blockquote(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| format!("> {}", escape_markdown_punctuation(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Компактный детерминированный отчёт: агрегаты вместо списка всех кандидатов.
/// Вызывающая сторона сначала проверяет документ по исходному пакету.
#[must_use]
pub fn render_markdown(triage: &SemanticTriage, pack: &ReviewPack) -> String {
    let summary = summarize(triage, pack);
    let mut output = String::from("# Семантическое ревью\n\n");
    let snapshot = &triage.source.snapshot;
    let _ = writeln!(
        output,
        "Репозиторий: `{}`; base: `{}`; head: `{}`; merge-base: `{}`.\n",
        markdown_cell(&snapshot.repository_id),
        markdown_cell(&snapshot.base_sha),
        markdown_cell(&snapshot.head_sha),
        markdown_cell(&snapshot.merge_base_sha)
    );
    let _ = writeln!(
        output,
        "SHA-256 исходного пакета: `{}`\n",
        triage.source.review_pack_sha256
    );
    let _ = writeln!(
        output,
        "Кандидаты: {}; рассмотрено: {}; не рассмотрено: {}.\n",
        summary.total_candidates, summary.reviewed_candidates, summary.unreviewed_candidates
    );
    let _ = writeln!(
        output,
        "Индивидуальных решений: {}. Групповых решений: {}; покрытых IDs: {}; представителей: {}.\n",
        summary.individual_review.decision_count,
        summary.group_review.decisions.decision_count,
        summary.group_review.covered_candidate_ids,
        summary.group_review.representative_candidate_ids
    );
    output.push_str("## Исходы рассмотрения\n\n| Решение | Индивидуальных решений | Групповых решений | ID кандидатов в группах |\n| --- | ---: | ---: | ---: |\n");
    for disposition in [
        Disposition::Confirmed,
        Disposition::Acceptable,
        Disposition::FalsePositive,
        Disposition::NotApplicable,
        Disposition::Uncertain,
    ] {
        let grouped_ids: usize = triage
            .group_decisions
            .iter()
            .filter(|g| g.disposition == disposition)
            .map(|g| g.candidate_ids.len())
            .sum();
        let _ = writeln!(
            output,
            "| {} | {} | {} | {} |",
            disposition.as_str(),
            summary
                .individual_review
                .by_disposition
                .get(&disposition)
                .copied()
                .unwrap_or_default(),
            summary
                .group_review
                .decisions
                .by_disposition
                .get(&disposition)
                .copied()
                .unwrap_or_default(),
            grouped_ids
        );
    }
    output.push_str("\n## Причины\n\n| Код причины | Индивидуальных решений | Групповых решений | ID кандидатов в группах |\n| --- | ---: | ---: | ---: |\n");
    let reasons: BTreeSet<_> = summary
        .individual_review
        .by_reason_code
        .keys()
        .chain(summary.group_review.decisions.by_reason_code.keys())
        .copied()
        .collect();
    for reason in reasons {
        let grouped_ids: usize = triage
            .group_decisions
            .iter()
            .filter(|g| g.reason_code == reason)
            .map(|g| g.candidate_ids.len())
            .sum();
        let _ = writeln!(
            output,
            "| {} | {} | {} | {} |",
            reason.as_str(),
            summary
                .individual_review
                .by_reason_code
                .get(&reason)
                .copied()
                .unwrap_or_default(),
            summary
                .group_review
                .decisions
                .by_reason_code
                .get(&reason)
                .copied()
                .unwrap_or_default(),
            grouped_ids
        );
    }
    output.push_str("\n## Детекторы\n\nЧисла исходов в этой таблице обозначают охваченные ID кандидатов.\n\n| Детектор | Всего | Не рассмотрено | Рассмотрено отдельно | Рассмотрено в группе | confirmed | acceptable | false_positive | not_applicable | uncertain | Связано с замечаниями |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for (name, counts) in &summary.by_detector {
        let get = |disposition| {
            counts
                .covered_ids_by_disposition
                .get(&disposition)
                .copied()
                .unwrap_or_default()
        };
        let _ = writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            markdown_cell(name),
            counts.total_candidates,
            counts.unreviewed_candidates,
            counts.individually_reviewed_candidates,
            counts.group_reviewed_candidates,
            get(Disposition::Confirmed),
            get(Disposition::Acceptable),
            get(Disposition::FalsePositive),
            get(Disposition::NotApplicable),
            get(Disposition::Uncertain),
            counts.finding_linked_candidates
        );
    }
    output.push_str("\n## Замечания\n\n");
    let _ = writeln!(
        output,
        "Всего замечаний: {}.\n",
        summary.findings.total_findings
    );
    output.push_str("| Серьёзность | Число |\n| --- | ---: |\n");
    for (severity, count) in &summary.findings.by_severity {
        let _ = writeln!(output, "| {} | {} |", severity.as_str(), count);
    }
    output.push_str("\n| Происхождение | Число |\n| --- | ---: |\n");
    for (provenance, count) in &summary.findings.by_provenance {
        let _ = writeln!(output, "| {} | {} |", provenance.as_str(), count);
    }
    let mut findings: Vec<_> = triage.findings.iter().collect();
    findings.sort_by(|a, b| (a.severity, &a.id).cmp(&(b.severity, &b.id)));
    for finding in findings {
        let _ = writeln!(
            output,
            "\n### [{}] {}\n\nID: {}; происхождение: {}; связанных кандидатов: {}.\n\n",
            finding.severity.as_str(),
            markdown_inline(&finding.title),
            markdown_inline(&finding.id),
            finding.provenance.as_str(),
            finding.candidate_ids.len()
        );
        output.push_str(&markdown_blockquote(&finding.description));
        output.push('\n');
    }
    output
        .push_str("\nПолные связи решений, групп, представителей и замечаний сохранены в JSON.\n");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_review::language::{self, SourceFile};
    use crate::code_review::model::{
        CandidateEvidence, CandidateOrigin, REVIEW_SCHEMA_VERSION, ReviewScope,
    };

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn pack() -> ReviewPack {
        ReviewPack {
            schema_version: REVIEW_SCHEMA_VERSION,
            target: GitTarget {
                repository_id: "synthetic-repository".into(),
                base_sha: "synthetic-base".into(),
                head_sha: "synthetic-head".into(),
                merge_base_sha: "synthetic-base".into(),
            },
            scope: ReviewScope {
                merge_base_sha: "synthetic-base".into(),
                text_image_limit_bytes: 1024,
                files: Vec::new(),
            },
            diagnostics: Vec::new(),
            candidates: (0..3)
                .map(|index| CandidateEvidence {
                    id: format!("candidate-{index}"),
                    detector: "error_path".into(),
                    path: "src/lib.rs".into(),
                    line: Some(index + 1),
                    column: None,
                    snippet: None,
                    origin: CandidateOrigin::IntroducedOrChanged,
                    signals: vec!["fixture".into()],
                    source: "synthetic-detector".into(),
                    metadata: BTreeMap::new(),
                })
                .collect(),
            language: language::scan(&[SourceFile {
                path: "src/lib.rs".into(),
                content: "// This fixture explains a failure scenario.\n".into(),
            }]),
            dependencies: Vec::new(),
            tests: Vec::new(),
            suppressions: Vec::new(),
            risk_surfaces: Vec::new(),
            tool_runs: Vec::new(),
        }
    }

    fn decision(id: &str) -> CandidateDecision {
        CandidateDecision {
            candidate_id: id.into(),
            disposition: Disposition::Acceptable,
            reason_code: ReasonCode::ExpectedFailurePath,
            explanation: "Ошибка возвращается вызывающему коду по контракту.".into(),
            finding_ids: Vec::new(),
        }
    }

    fn group() -> GroupDecision {
        GroupDecision {
            id: "failure-paths".into(),
            candidate_ids: vec!["candidate-0".into(), "candidate-1".into()],
            representative_candidate_ids: vec!["candidate-0".into()],
            disposition: Disposition::Acceptable,
            reason_code: ReasonCode::ExpectedFailurePath,
            explanation: "Группа проверена по общему контракту возврата ошибки.".into(),
            finding_ids: Vec::new(),
        }
    }

    fn finding(ids: Vec<String>, provenance: FindingProvenance) -> SemanticFinding {
        SemanticFinding {
            id: "finding-1".into(),
            severity: Severity::Major,
            title: "Нарушен контракт возврата ошибки".into(),
            description:
                "src/lib.rs:1: при отказе записи вызывающий код получает успех и теряет данные."
                    .into(),
            provenance,
            candidate_ids: ids,
        }
    }

    fn resolve(triage: &mut SemanticTriage, decision: CandidateDecision) {
        triage
            .unreviewed_candidate_ids
            .retain(|id| id != &decision.candidate_id);
        triage.individual_decisions.push(decision);
    }

    fn resolve_group(triage: &mut SemanticTriage, group: GroupDecision) {
        triage
            .unreviewed_candidate_ids
            .retain(|id| !group.candidate_ids.contains(id));
        triage.group_decisions.push(group);
    }

    fn assert_invalid(triage: &SemanticTriage, pack: &ReviewPack) {
        assert_eq!(
            validate(triage, pack, DIGEST).unwrap_err().code,
            ErrorCode::ReviewArtifactInvalid
        );
    }

    #[test]
    fn initial_review_is_explicitly_unreviewed_in_unified_candidate_space() {
        let pack = pack();
        assert!(!pack.language.candidates.is_empty());
        let triage = initialize(&pack, DIGEST);
        let summary = validate(&triage, &pack, DIGEST).unwrap();
        assert_eq!(summary.total_candidates, pack.all_candidates().len());
        assert_eq!(summary.unreviewed_candidates, summary.total_candidates);
        assert_eq!(summary.reviewed_candidates, 0);
        assert!(
            triage
                .unreviewed_candidate_ids
                .contains(&pack.language.candidates[0].id)
        );
        assert!(triage.individual_decisions.is_empty());
        assert!(triage.group_decisions.is_empty());
    }

    #[test]
    fn empty_source_pack_supports_independent_semantic_finding() {
        let mut pack = pack();
        pack.candidates.clear();
        pack.language.candidates.clear();
        let mut triage = initialize(&pack, DIGEST);
        triage
            .findings
            .push(finding(Vec::new(), FindingProvenance::Independent));
        let summary = validate(&triage, &pack, DIGEST).unwrap();
        assert_eq!(summary.total_candidates, 0);
        assert_eq!(summary.findings.total_findings, 1);
    }

    #[test]
    fn individual_and_language_decisions_preserve_partial_coverage() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        resolve(&mut triage, decision("candidate-0"));
        resolve(&mut triage, decision(&pack.language.candidates[0].id));
        let summary = validate(&triage, &pack, DIGEST).unwrap();
        assert_eq!(summary.individual_review.decision_count, 2);
        assert_eq!(summary.reviewed_candidates, 2);
        assert_eq!(summary.unreviewed_candidates, summary.total_candidates - 2);
        assert_eq!(
            summary.by_detector["residual_foreign_human_text"].individually_reviewed_candidates,
            1
        );
    }

    #[test]
    fn group_counts_are_units_with_exact_candidate_coverage() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        resolve_group(&mut triage, group());
        let summary = validate(&triage, &pack, DIGEST).unwrap();
        assert_eq!(summary.individual_review.decision_count, 0);
        assert_eq!(
            summary.group_review.decisions.by_disposition[&Disposition::Acceptable],
            1
        );
        assert_eq!(summary.group_review.covered_candidate_ids, 2);
        assert_eq!(summary.group_review.representative_candidate_ids, 1);
        assert_eq!(
            summary.by_detector["error_path"].covered_ids_by_disposition[&Disposition::Acceptable],
            2
        );
        assert_eq!(
            summary.by_detector["error_path"].group_reviewed_candidates,
            2
        );
    }

    #[test]
    fn source_identity_digest_version_and_count_are_strict() {
        let pack = pack();
        let triage = initialize(&pack, DIGEST);
        for field in ["repository", "base", "head", "merge_base"] {
            let mut changed = triage.clone();
            match field {
                "repository" => changed.source.snapshot.repository_id.push_str("-other"),
                "base" => changed.source.snapshot.base_sha.push_str("-other"),
                "head" => changed.source.snapshot.head_sha.push_str("-other"),
                _ => changed.source.snapshot.merge_base_sha.push_str("-other"),
            }
            assert_eq!(
                validate(&changed, &pack, DIGEST).unwrap_err().code,
                ErrorCode::BaselineMismatch
            );
        }
        let mut changed = triage.clone();
        changed.schema_version += 1;
        assert_invalid(&changed, &pack);
        changed = triage.clone();
        changed.source.candidate_count += 1;
        assert_invalid(&changed, &pack);
        changed = triage.clone();
        changed.source.review_pack_sha256.replace_range(0..1, "b");
        assert_invalid(&changed, &pack);
        changed.source.review_pack_sha256 = "malformed".into();
        assert!(validate(&changed, &pack, "malformed").is_err());
    }

    #[test]
    fn duplicate_unknown_and_missing_coverage_are_rejected() {
        let pack = pack();
        let original = initialize(&pack, DIGEST);
        let mut changed = original.clone();
        changed.unreviewed_candidate_ids.push("candidate-0".into());
        assert_invalid(&changed, &pack);
        changed = original.clone();
        changed.unreviewed_candidate_ids.push("missing".into());
        assert_invalid(&changed, &pack);
        changed = original.clone();
        changed.unreviewed_candidate_ids.pop();
        assert_invalid(&changed, &pack);
        changed = original.clone();
        changed.individual_decisions.push(decision("candidate-0"));
        assert_invalid(&changed, &pack);
        changed = original;
        resolve(&mut changed, decision("candidate-0"));
        changed.individual_decisions.push(decision("candidate-0"));
        assert_invalid(&changed, &pack);
    }

    #[test]
    fn overlapping_groups_and_individual_group_overlap_are_rejected() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        resolve_group(&mut triage, group());
        let mut overlapping = group();
        overlapping.id = "other-group".into();
        overlapping.candidate_ids = vec!["candidate-1".into(), "candidate-2".into()];
        overlapping.representative_candidate_ids = vec!["candidate-2".into()];
        resolve_group(&mut triage, overlapping);
        assert_invalid(&triage, &pack);
        triage.group_decisions.pop();
        triage.unreviewed_candidate_ids.push("candidate-2".into());
        triage.individual_decisions.push(decision("candidate-0"));
        assert_invalid(&triage, &pack);
    }

    #[test]
    fn groups_require_ids_representatives_and_explanation() {
        let pack = pack();
        for variant in 0..8 {
            let mut changed = group();
            match variant {
                0 => changed.id = " ".into(),
                1 => changed.candidate_ids.clear(),
                2 => {
                    changed.candidate_ids.pop();
                }
                3 => changed.candidate_ids.push("candidate-0".into()),
                4 => changed.representative_candidate_ids.clear(),
                5 => changed
                    .representative_candidate_ids
                    .push("candidate-2".into()),
                6 => changed
                    .representative_candidate_ids
                    .push("candidate-0".into()),
                _ => changed.explanation = "\n".into(),
            }
            let mut triage = initialize(&pack, DIGEST);
            resolve_group(&mut triage, changed);
            assert_invalid(&triage, &pack);
        }
        let mut triage = initialize(&pack, DIGEST);
        resolve_group(&mut triage, group());
        triage.group_decisions.push(group());
        assert_invalid(&triage, &pack);
    }

    #[test]
    fn direct_and_assisted_findings_have_explicit_distinct_provenance() {
        let pack = pack();
        for provenance in [
            FindingProvenance::DirectCandidate,
            FindingProvenance::CandidateAssisted,
        ] {
            let mut triage = initialize(&pack, DIGEST);
            let mut decision = decision("candidate-0");
            decision.disposition = Disposition::Confirmed;
            decision.finding_ids.push("finding-1".into());
            resolve(&mut triage, decision);
            triage
                .findings
                .push(finding(vec!["candidate-0".into()], provenance));
            let summary = validate(&triage, &pack, DIGEST).unwrap();
            assert_eq!(summary.findings.by_provenance[&provenance], 1);
            assert_eq!(
                summary.by_detector["error_path"].finding_linked_candidates,
                1
            );
            assert_eq!(summary.findings.by_provenance.len(), 1);
        }
    }

    #[test]
    fn confirmed_requires_finding_and_mutual_linkage() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        let mut confirmed = decision("candidate-0");
        confirmed.disposition = Disposition::Confirmed;
        resolve(&mut triage, confirmed);
        assert_invalid(&triage, &pack);
        triage.individual_decisions[0]
            .finding_ids
            .push("finding-1".into());
        assert_invalid(&triage, &pack);
        triage.findings.push(finding(
            vec!["candidate-0".into()],
            FindingProvenance::CandidateAssisted,
        ));
        validate(&triage, &pack, DIGEST).unwrap();
        triage.findings[0].candidate_ids.push("candidate-1".into());
        assert_invalid(&triage, &pack);
        resolve(&mut triage, decision("candidate-1"));
        assert_invalid(&triage, &pack);
        triage.individual_decisions[1]
            .finding_ids
            .push("finding-1".into());
        validate(&triage, &pack, DIGEST).unwrap();
        triage.individual_decisions[0]
            .finding_ids
            .push("finding-1".into());
        assert_invalid(&triage, &pack);
    }

    #[test]
    fn group_finding_must_link_every_covered_candidate() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        let mut group = group();
        group.disposition = Disposition::Confirmed;
        group.finding_ids.push("finding-1".into());
        resolve_group(&mut triage, group);
        triage.findings.push(finding(
            vec!["candidate-0".into()],
            FindingProvenance::CandidateAssisted,
        ));
        assert_invalid(&triage, &pack);
        triage.findings[0].candidate_ids.push("candidate-1".into());
        validate(&triage, &pack, DIGEST).unwrap();
    }

    #[test]
    fn malformed_findings_and_links_are_rejected() {
        let pack = pack();
        for variant in 0..8 {
            let mut triage = initialize(&pack, DIGEST);
            let mut finding = finding(Vec::new(), FindingProvenance::Independent);
            match variant {
                0 => finding.id.clear(),
                1 => finding.title = " ".into(),
                2 => finding.description.clear(),
                3 => finding.candidate_ids.push("candidate-0".into()),
                4 => finding.provenance = FindingProvenance::DirectCandidate,
                5 => finding.provenance = FindingProvenance::CandidateAssisted,
                6 => {
                    finding.provenance = FindingProvenance::CandidateAssisted;
                    finding.candidate_ids.push("missing".into());
                }
                _ => {
                    triage.findings.push(finding.clone());
                }
            }
            triage.findings.push(finding);
            assert_invalid(&triage, &pack);
        }
        let mut triage = initialize(&pack, DIGEST);
        let mut changed = decision("candidate-0");
        changed.explanation = " ".into();
        resolve(&mut triage, changed);
        assert_invalid(&triage, &pack);
    }

    #[test]
    fn candidate_assistance_can_link_nonconfirmed_navigation_signal() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        let mut decision = decision("candidate-0");
        decision.finding_ids.push("finding-1".into());
        resolve(&mut triage, decision);
        triage.findings.push(finding(
            vec!["candidate-0".into()],
            FindingProvenance::CandidateAssisted,
        ));
        validate(&triage, &pack, DIGEST).unwrap();
        triage.findings[0].provenance = FindingProvenance::DirectCandidate;
        assert_invalid(&triage, &pack);
        triage.individual_decisions[0].disposition = Disposition::Confirmed;
        validate(&triage, &pack, DIGEST).unwrap();
    }

    #[test]
    fn canonicalization_is_deterministic_and_preserves_invalid_duplicates() {
        let pack = pack();
        let mut a = initialize(&pack, DIGEST);
        resolve_group(&mut a, group());
        resolve(&mut a, decision("candidate-2"));
        a.findings
            .push(finding(Vec::new(), FindingProvenance::Independent));
        let mut second = finding(Vec::new(), FindingProvenance::Independent);
        second.id = "finding-2".into();
        a.findings.push(second);
        let mut b = a.clone();
        b.group_decisions[0].candidate_ids.reverse();
        b.unreviewed_candidate_ids.reverse();
        b.findings.reverse();
        assert_eq!(render_markdown(&a, &pack), render_markdown(&b, &pack));
        canonicalize(&mut a);
        canonicalize(&mut b);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
        b.unreviewed_candidate_ids
            .push(b.unreviewed_candidate_ids[0].clone());
        canonicalize(&mut b);
        assert_invalid(&b, &pack);
    }

    #[test]
    fn finding_description_cannot_inject_markdown_structure() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        let mut finding = finding(Vec::new(), FindingProvenance::Independent);
        finding.description =
            "# Заголовок\n\n| Поле | Значение |\n| --- | --- |\n- пункт\n<script>alert(1)</script>"
                .into();
        triage.findings.push(finding);

        validate(&triage, &pack, DIGEST).unwrap();
        let report = render_markdown(&triage, &pack);

        assert!(report.contains("> \\# Заголовок\n>"));
        assert!(report.contains("> \\| Поле \\| Значение \\|"));
        assert!(report.contains("> \\- пункт"));
        assert!(report.contains("> \\<script\\>alert\\(1\\)\\<\\/script\\>"));
        assert!(!report.contains("\n# Заголовок"));
        assert!(!report.contains("\n| Поле |"));
        assert!(!report.contains("\n<script>"));
    }

    #[test]
    fn finding_title_and_id_cannot_inject_markdown_structure() {
        let pack = pack();
        let mut triage = initialize(&pack, DIGEST);
        let mut finding = finding(Vec::new(), FindingProvenance::Independent);
        finding.title = "# Заголовок\n<script>alert(1)</script>".into();
        finding.id = "id\n## Injected | [link](https://example.test)".into();
        triage.findings.push(finding);

        validate(&triage, &pack, DIGEST).unwrap();
        let report = render_markdown(&triage, &pack);

        assert!(report.contains("### [major] \\# Заголовок \\<script\\>"));
        assert!(report.contains("ID: id \\#\\# Injected \\| \\[link\\]"));
        assert!(report.contains("\\(https\\:\\/\\/example\\.test\\)"));
        assert!(!report.contains("\n## Injected"));
        assert!(!report.contains("\n<script>"));
    }

    #[test]
    fn report_aggregates_large_groups_without_candidate_dump() {
        let mut pack = pack();
        let template = pack.candidates[0].clone();
        pack.candidates = (0..5000)
            .map(|index| {
                let mut candidate = template.clone();
                candidate.id = format!("long-candidate-{index:05}");
                candidate
            })
            .collect();
        let mut triage = initialize(&pack, DIGEST);
        let mut group = group();
        group.candidate_ids = pack.candidates.iter().map(|c| c.id.clone()).collect();
        group.representative_candidate_ids = vec![group.candidate_ids[0].clone()];
        resolve_group(&mut triage, group);
        validate(&triage, &pack, DIGEST).unwrap();
        let report = render_markdown(&triage, &pack);
        assert!(report.len() < 7000);
        assert!(!report.contains("long-candidate-"));
        assert!(report.contains("error_path"));
        assert!(report.contains("5000"));
    }

    #[test]
    fn serde_rejects_unknown_fields_and_unknown_reason_codes() {
        let pack = pack();
        let triage = initialize(&pack, DIGEST);
        let mut json = serde_json::to_value(&triage).unwrap();
        json["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SemanticTriage>(json).is_err());
        let mut json = serde_json::to_value(decision("candidate-0")).unwrap();
        json["reason_code"] = serde_json::json!("ad_hoc_free_text");
        assert!(serde_json::from_value::<CandidateDecision>(json).is_err());
    }

    #[test]
    fn malformed_source_candidate_ids_are_not_silently_deduplicated() {
        let mut pack = pack();
        pack.candidates.push(pack.candidates[0].clone());
        assert_invalid(&initialize(&pack, DIGEST), &pack);
        pack.candidates.pop();
        pack.candidates[0].id.clear();
        assert_invalid(&initialize(&pack, DIGEST), &pack);
    }
}
