//! Операция `review-check`: проверка предложений агента против текущего экспорта.
//!
//! Вход — документ предложений ([`crate::proposal::ProposalDocument`]), отдельный
//! versioned контракт внешнего агента. Выход — отчёт по каждому предложению и,
//! если отчёт чистый, готовый *исполнимый* запрос для `edit`. Это два разных
//! публичных документа: proposal metadata (`proposal_id`, `reason`) принадлежит
//! входу, а `edit_request` — границей записи. Смешивать их нельзя: расширение
//! metadata не должно менять write contract.
//!
//! Resolution целей (`guid` → модель → `flds[].ord` → `fields`) и классификация
//! правок не дублируются: документ компилируется в
//! [`crate::ops::edit::EditRequest`], и дальше работает общий с `edit` код
//! разрешения и валидации. Поэтому статусы `review-check` и `edit` описывают одно
//! и то же действие одинаково.
//!
//! Команда никогда не пишет и не вызывает LLM: она только сравнивает `expected`
//! с текущим значением поля и классифицирует предложение.
//!
//! Статусы одного предложения:
//!
//! - `valid` — текущее значение совпало с `expected`, замена имеет смысл;
//! - `already_correct` — текущее значение уже равно `replacement` (и `expected`);
//! - `already_applied` — текущее значение уже равно `replacement`;
//! - `conflict` — текущее значение не совпало ни с `expected`, ни с
//!   `replacement`: оптимистичное предусловие нарушено;
//! - `invalid` — предложение не удалось разрешить в этом экспорте.
//!
//! `edit_request` выпускается только при `outcome = "ok"`, только из `valid`
//! предложений и только тогда, когда сам исходник проходит предварительные
//! проверки границы записи (`source_editable`). `expected` в нём равен
//! фактическому текущему значению, поэтому такой запрос гарантированно
//! принимается `edit` при неизменном источнике.
//!
//! Если исходник нельзя править (неканоническая форма, `ERROR` экспорта или
//! неоднозначный порядок полей модели), отчёт по предложениям всё равно
//! печатается: он остаётся полезным агенту, а `source_blockers` называет
//! причины, по которым запрос не выпущен. Проверки берутся у `edit`
//! ([`crate::ops::edit::source_blockers`]), а не повторяются здесь.

use std::path::Path;

use crate::error::DomainError;
use crate::index::ExportIndex;
use crate::ops::edit::{self, EditRequest, EditSpec, EditStatus, ProblemKind};
use crate::proposal::{Proposal, ProposalDocument};
use crate::text::bounded_sample;

/// Предел числа предложений в отчёте.
///
/// Отчёт — представление проверки, а не её вход: предел объявлен здесь, а не
/// заимствован у отчёта `edit`, чтобы два контракта можно было менять по
/// отдельности.
pub const MAX_REPORTED_PROPOSALS: usize = 50;

/// Итог проверки документа предложений.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// Все предложения разрешены и не конфликтуют.
    Ok,
    /// Есть предложения, которые не удалось разрешить: документ неисполним.
    Invalid,
    /// Есть конфликты предусловий: состояние экспорта изменилось.
    Stale,
}

impl ReviewOutcome {
    /// Стабильное machine-readable имя итога.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Invalid => "invalid",
            Self::Stale => "stale",
        }
    }
}

/// Статус одного предложения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Предложение можно исполнить.
    Valid,
    /// Значение уже равно и `expected`, и `replacement`.
    AlreadyCorrect,
    /// Значение уже было заменено ранее.
    AlreadyApplied,
    /// Значение не совпало ни с `expected`, ни с `replacement`.
    Conflict,
    /// Предложение не разрешается в этом экспорте.
    Invalid,
}

impl CheckStatus {
    /// Стабильное machine-readable имя статуса.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::AlreadyCorrect => "already_correct",
            Self::AlreadyApplied => "already_applied",
            Self::Conflict => "conflict",
            Self::Invalid => "invalid",
        }
    }
}

/// Счётчики предложений по статусам.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CheckCounts {
    /// Сколько предложений можно исполнить.
    pub valid: usize,
    /// Сколько предложений уже не требуют изменений.
    pub already_correct: usize,
    /// Сколько предложений уже было применено ранее.
    pub already_applied: usize,
    /// Сколько предложений конфликтует с текущим состоянием.
    pub conflict: usize,
    /// Сколько предложений не разрешается.
    pub invalid: usize,
}

/// Проверенное предложение.
#[derive(Debug)]
pub struct CheckedProposal {
    /// Позиция в документе (с нуля).
    pub proposal_index: usize,
    /// Идентификатор предложения из документа.
    pub proposal_id: Option<String>,
    /// `guid` заметки.
    pub guid: String,
    /// Имя поля.
    pub field: String,
    /// Статус.
    pub status: CheckStatus,
    /// Позиция заметки в порядке экспорта.
    pub note_index: Option<usize>,
    /// Путь колоды заметки.
    pub deck_path: Option<String>,
    /// Позиция поля в модели.
    pub field_ord: Option<usize>,
    /// Длина текущего значения в символах.
    pub current_len: Option<usize>,
    /// Длина ожидаемого значения в символах.
    pub expected_len: usize,
    /// Длина нового значения в символах.
    pub replacement_len: usize,
    /// Выборка текущего значения.
    pub current_sample: Option<String>,
    /// Выборка ожидаемого значения.
    pub expected_sample: String,
    /// Выборка нового значения.
    pub replacement_sample: String,
    /// Ограниченная выборка пояснения агента, если оно было в предложении.
    pub reason: Option<String>,
    /// Код проблемы для `invalid`-предложений.
    pub problem: Option<&'static str>,
    /// Пояснение проблемы для `invalid`-предложений.
    pub message: Option<String>,
}

/// Причина, по которой по текущему исходнику нельзя выпустить запрос.
#[derive(Debug)]
pub struct SourceBlocker {
    /// Стабильный код ошибки границы записи (`source_not_canonical` и т.п.).
    pub code: &'static str,
    /// Пояснение из той же ошибки, ограниченное по длине.
    pub message: String,
}

/// Результат `review-check`.
#[derive(Debug)]
pub struct ReviewCheckResult {
    /// Каталог экспорта.
    pub export_dir: String,
    /// Полный путь к `deck.json`.
    pub deck_json: String,
    /// Итог проверки.
    pub outcome: ReviewOutcome,
    /// Сколько предложений было в документе.
    pub proposals_total: usize,
    /// Счётчики по статусам (по всем предложениям, включая не показанные).
    pub counts: CheckCounts,
    /// Сколько предложений реально меняют значение.
    pub effective_proposals: usize,
    /// Отчёт по предложениям (обрезан до [`MAX_REPORTED_PROPOSALS`]).
    pub proposals: Vec<CheckedProposal>,
    /// Был ли отчёт по предложениям обрезан.
    pub proposals_truncated: bool,
    /// Проходит ли исходник предварительные проверки границы записи.
    pub source_editable: bool,
    /// Почему исходник нельзя править (пусто, если можно).
    pub source_blockers: Vec<SourceBlocker>,
    /// Готовый запрос для `edit`: чистый отчёт, эффективные правки и
    /// правимый исходник.
    pub edit_request: Option<EditRequest>,
    /// Process exit code: причина блокировки исходника важнее итога по
    /// предложениям, потому что без правки исходника запрос неисполним.
    pub exit_code: u8,
}

/// Проверяет документ предложений против текущего состояния экспорта.
///
/// `source_blockers` — причины, по которым исходник нельзя править; их считает
/// общая с `edit` проверка ([`crate::ops::edit::source_blockers`]). Они не
/// отменяют отчёт: агент видит и статусы предложений, и то, что запрос пока
/// неисполним.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InvalidRequest`] и
/// [`ErrorCode::DuplicateEditTarget`] для неисполнимого документа целиком.
/// Проблемы отдельных предложений ошибкой не являются: они часть отчёта.
pub fn review_check(
    export_dir: &Path,
    index: &ExportIndex<'_>,
    document: &ProposalDocument,
    source_blockers: &[DomainError],
) -> Result<ReviewCheckResult, DomainError> {
    // Документ проверяется ещё раз, а не только при разборе: функция публична, и
    // её контракт не должен зависеть от того, что документ пришёл из
    // [`crate::proposal::parse_proposal_bytes`].
    edit::validate_request(&document.to_edit_request())?;

    let mut counts = CheckCounts::default();
    let mut proposals = Vec::new();
    let mut effective = Vec::new();
    let mut saw_ambiguous = false;
    let mut saw_not_found = false;

    for (proposal_index, proposal) in document.proposals.iter().enumerate() {
        let spec = proposal.to_edit_spec();
        let reason = proposal.bounded_reason();
        let checked = match edit::resolve_edit(index, proposal_index, &spec, false) {
            Ok(resolved) => {
                let status = match resolved.status {
                    EditStatus::DryRun | EditStatus::Applied => CheckStatus::Valid,
                    EditStatus::NoopIdentical => CheckStatus::AlreadyCorrect,
                    EditStatus::AlreadyApplied => CheckStatus::AlreadyApplied,
                    EditStatus::Conflict => CheckStatus::Conflict,
                };
                if status == CheckStatus::Valid {
                    effective.push(spec.clone());
                }
                CheckedProposal::from_resolved(proposal_index, &resolved, status, reason)
            }
            Err(problem) => {
                match problem.kind {
                    ProblemKind::Ambiguous => saw_ambiguous = true,
                    ProblemKind::NoteNotFound => saw_not_found = true,
                    ProblemKind::UnknownField | ProblemKind::FieldNotInModel => {}
                }
                CheckedProposal::from_problem(proposal_index, proposal, &problem, reason)
            }
        };

        match checked.status {
            CheckStatus::Valid => counts.valid += 1,
            CheckStatus::AlreadyCorrect => counts.already_correct += 1,
            CheckStatus::AlreadyApplied => counts.already_applied += 1,
            CheckStatus::Conflict => counts.conflict += 1,
            CheckStatus::Invalid => counts.invalid += 1,
        }

        if proposals.len() < MAX_REPORTED_PROPOSALS {
            proposals.push(checked);
        }
    }

    let outcome = if counts.invalid > 0 {
        ReviewOutcome::Invalid
    } else if counts.conflict > 0 {
        ReviewOutcome::Stale
    } else {
        ReviewOutcome::Ok
    };

    let outcome_exit_code = match outcome {
        ReviewOutcome::Ok => 0,
        ReviewOutcome::Stale => 7,
        ReviewOutcome::Invalid if saw_ambiguous => 5,
        ReviewOutcome::Invalid if saw_not_found => 4,
        ReviewOutcome::Invalid => 3,
    };
    // Неправимый исходник делает неисполнимым любой запрос, поэтому его код
    // важнее итога по предложениям: агент не должен принять «ok» за готовность
    // к записи.
    let exit_code = source_blockers
        .first()
        .map_or(outcome_exit_code, |blocker| blocker.code.exit_code());

    let edit_request = build_edit_request(outcome, &effective, source_blockers)?;
    let blockers: Vec<SourceBlocker> = source_blockers
        .iter()
        .map(|blocker| SourceBlocker {
            code: blocker.code.as_str(),
            message: bounded_sample(&blocker.message),
        })
        .collect();

    Ok(ReviewCheckResult {
        export_dir: export_dir.display().to_string(),
        deck_json: export_dir.join("deck.json").display().to_string(),
        outcome,
        proposals_total: document.proposals.len(),
        counts,
        effective_proposals: effective.len(),
        proposals_truncated: document.proposals.len() > proposals.len(),
        proposals,
        source_editable: blockers.is_empty(),
        source_blockers: blockers,
        edit_request,
        exit_code,
    })
}

/// Собирает запрос для `edit` из исполнимых предложений.
///
/// В `expected` переносится значение из предложения, и это безопасно: статус
/// `valid` получают только те предложения, у которых фактическое текущее значение
/// уже совпало с `expected`. Поэтому запрос принимается существующей границей
/// записи, даже если агент описал ожидание неточно — по факту оно совпало.
fn build_edit_request(
    outcome: ReviewOutcome,
    effective: &[EditSpec],
    source_blockers: &[DomainError],
) -> Result<Option<EditRequest>, DomainError> {
    if outcome != ReviewOutcome::Ok || effective.is_empty() || !source_blockers.is_empty() {
        return Ok(None);
    }

    let request = EditRequest {
        edits: effective
            .iter()
            .map(|spec| EditSpec {
                edit_id: spec.edit_id.clone(),
                guid: spec.guid.clone(),
                field: spec.field.clone(),
                expected: spec.expected.clone(),
                replacement: spec.replacement.clone(),
            })
            .collect(),
    };

    // Тот же инвариант, что у запроса `edit`: выпускаемый запрос обязан быть исполнимым.
    edit::validate_request(&request)?;
    Ok(Some(request))
}

impl CheckedProposal {
    /// Строит запись отчёта по разрешённому предложению.
    fn from_resolved(
        proposal_index: usize,
        resolved: &edit::ResolvedEdit,
        status: CheckStatus,
        reason: Option<String>,
    ) -> Self {
        Self {
            proposal_index,
            proposal_id: resolved.edit_id.clone(),
            guid: resolved.guid.clone(),
            field: resolved.field.clone(),
            status,
            note_index: Some(resolved.note_position),
            deck_path: Some(resolved.deck_path.clone()),
            field_ord: Some(resolved.field_ord),
            current_len: Some(resolved.current.chars().count()),
            expected_len: resolved.expected.chars().count(),
            replacement_len: resolved.replacement.chars().count(),
            current_sample: Some(bounded_sample(&resolved.current)),
            expected_sample: bounded_sample(&resolved.expected),
            replacement_sample: bounded_sample(&resolved.replacement),
            reason,
            problem: None,
            message: None,
        }
    }

    /// Строит запись отчёта по неразрешённому предложению.
    fn from_problem(
        proposal_index: usize,
        proposal: &Proposal,
        problem: &edit::Problem,
        reason: Option<String>,
    ) -> Self {
        let spec = proposal.to_edit_spec();
        Self {
            proposal_index,
            proposal_id: proposal.proposal_id.clone(),
            guid: problem.guid.clone(),
            field: problem.field.clone(),
            status: CheckStatus::Invalid,
            note_index: None,
            deck_path: None,
            field_ord: None,
            current_len: None,
            expected_len: spec.expected.chars().count(),
            replacement_len: spec.replacement.chars().count(),
            current_sample: None,
            expected_sample: bounded_sample(&spec.expected),
            replacement_sample: bounded_sample(&spec.replacement),
            reason,
            problem: Some(problem_code(problem.kind)),
            message: Some(problem.message.clone()),
        }
    }
}

/// Стабильный код проблемы предложения для отчёта `review-check`.
///
/// Коды выводятся из того же [`edit::ProblemKind`], по которому `edit` решает
/// судьбу всей правки, но называются по существу проверки предложений.
/// `edit` для последних двух случаев использует общий код `internal_error` и
/// прекращает работу; `review-check` не повторяет это: неразрешимая цель — это
/// неисполнимое предложение в отчёте, а не внутренняя ошибка инструмента.
const fn problem_code(kind: ProblemKind) -> &'static str {
    match kind {
        ProblemKind::Ambiguous => "ambiguous_guid",
        ProblemKind::NoteNotFound => "note_not_found",
        ProblemKind::UnknownField => "unknown_field",
        ProblemKind::FieldNotInModel => "field_not_resolvable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::loader;
    use crate::test_support::{MINIMAL_EXPORT, TempDir, deck_node, export_with};

    /// Документ предложений из троек «guid, поле, expected, replacement».
    fn request(specs: Vec<(&str, &str, &str, &str)>) -> ProposalDocument {
        ProposalDocument {
            proposals: specs
                .into_iter()
                .map(|(guid, field, expected, replacement)| Proposal {
                    proposal_id: None,
                    guid: guid.to_string(),
                    field: field.to_string(),
                    expected: expected.to_string(),
                    replacement: replacement.to_string(),
                    reason: None,
                })
                .collect(),
        }
    }

    fn check(json: &str, request: &ProposalDocument) -> ReviewCheckResult {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        review_check(Path::new("."), &index, request, &[]).expect("review-check")
    }

    fn check_error(json: &str, request: &ProposalDocument) -> DomainError {
        let node = deck_node(json);
        let index = ExportIndex::build(&node);
        review_check(Path::new("."), &index, request, &[]).expect_err("ожидалась ошибка")
    }

    /// Блокер исходника из той же ошибки, что вернула бы граница записи.
    fn source_blocker(code: ErrorCode) -> DomainError {
        DomainError::new(code, "исходник нельзя править")
    }

    #[test]
    fn uneditable_source_blocks_the_request_but_keeps_the_report() {
        let node = deck_node(MINIMAL_EXPORT);
        let index = ExportIndex::build(&node);
        let document = request(vec![("guid-1", "Толкование", "случайность", "случайно")]);
        let blockers = vec![
            source_blocker(ErrorCode::SourceNotCanonical),
            source_blocker(ErrorCode::ExportInvalid),
        ];

        let result = review_check(Path::new("."), &index, &document, &blockers)
            .expect("отчёт всё равно строится");

        assert_eq!(result.counts.valid, 1, "статус предложения не теряется");
        assert_eq!(result.outcome, ReviewOutcome::Ok);
        assert!(!result.source_editable);
        assert_eq!(
            result
                .source_blockers
                .iter()
                .map(|blocker| blocker.code)
                .collect::<Vec<_>>(),
            vec!["source_not_canonical", "export_invalid"]
        );
        assert!(
            result.edit_request.is_none(),
            "неправимый исходник не даёт запроса"
        );
        assert_eq!(
            result.exit_code,
            ErrorCode::SourceNotCanonical.exit_code(),
            "код блокировки важнее итога по предложениям"
        );
    }

    #[test]
    fn valid_proposal_is_reported_and_compiled_into_a_request() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![("guid-1", "Толкование", "случайность", "случайно")]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Ok);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.counts.valid, 1);
        assert_eq!(result.proposals[0].status, CheckStatus::Valid);
        assert_eq!(result.proposals[0].note_index, Some(0));
        assert_eq!(result.proposals[0].deck_path.as_deref(), Some("Test::Deck"));
        assert_eq!(result.proposals[0].field_ord, Some(1));
        assert_eq!(
            result.proposals[0].current_sample.as_deref(),
            Some("случайность")
        );
        assert_eq!(result.proposals[0].problem, None);
        assert!(result.proposals[0].message.is_none());

        let edit_request = result.edit_request.expect("готовый запрос");
        assert_eq!(edit_request.edits.len(), 1);
        assert_eq!(edit_request.edits[0].expected, "случайность");
        assert_eq!(edit_request.edits[0].replacement, "случайно");
    }

    #[test]
    fn mismatched_current_value_is_a_conflict_without_a_request() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![("guid-1", "Толкование", "устаревшее", "новое")]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Stale);
        assert_eq!(result.exit_code, 7);
        assert_eq!(result.counts.conflict, 1);
        assert_eq!(result.proposals[0].status, CheckStatus::Conflict);
        assert!(result.edit_request.is_none());
    }

    #[test]
    fn already_applied_proposal_is_reported_not_conflicting() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![("guid-1", "Толкование", "старое", "случайность")]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Ok);
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.counts.already_applied, 1);
        assert_eq!(result.proposals[0].status, CheckStatus::AlreadyApplied);
        assert!(result.edit_request.is_none(), "эффективных правок нет");
        assert_eq!(result.effective_proposals, 0);
    }

    #[test]
    fn identical_expected_and_replacement_is_already_correct() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![("guid-1", "Толкование", "случайность", "случайность")]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Ok);
        assert_eq!(result.counts.already_correct, 1);
        assert!(result.edit_request.is_none());
    }

    #[test]
    fn unknown_guid_is_reported_as_invalid_not_as_error() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![("нет-такого", "Толкование", "a", "b")]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Invalid);
        assert_eq!(result.exit_code, 4, "seniority: note_not_found → 4");
        assert_eq!(result.proposals[0].status, CheckStatus::Invalid);
        assert_eq!(result.proposals[0].problem, Some("note_not_found"));
        assert!(result.proposals[0].message.is_some());
        assert!(result.edit_request.is_none());
    }

    #[test]
    fn unknown_field_is_reported_as_invalid() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![("guid-1", "НетТакого", "a", "b")]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Invalid);
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.proposals[0].problem, Some("unknown_field"));
    }

    #[test]
    fn ambiguous_guid_outranks_other_problems_in_exit_code() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][1]["guid"] = serde_json::json!("guid-1");
        });
        let result = check(
            &json,
            &request(vec![
                ("нет-такого", "Толкование", "a", "b"),
                ("guid-1", "Толкование", "a", "b"),
            ]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Invalid);
        assert_eq!(result.exit_code, 5);
        assert_eq!(result.proposals[1].problem, Some("ambiguous_guid"));
    }

    #[test]
    fn mixed_document_keeps_valid_edits_out_of_the_request() {
        let result = check(
            MINIMAL_EXPORT,
            &request(vec![
                ("guid-1", "Толкование", "случайность", "случайно"),
                ("guid-2", "Толкование", "устаревшее", "новое"),
            ]),
        );
        assert_eq!(result.outcome, ReviewOutcome::Stale);
        assert_eq!(result.counts.valid, 1);
        assert_eq!(result.counts.conflict, 1);
        assert!(result.edit_request.is_none());
        assert_eq!(result.proposals[0].status, CheckStatus::Valid);
        assert_eq!(result.proposals[1].status, CheckStatus::Conflict);
    }

    #[test]
    fn non_string_field_value_is_invalid() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            value["notes"][0]["fields"][1] = serde_json::json!(42);
        });
        let result = check(&json, &request(vec![("guid-1", "Толкование", "42", "43")]));
        assert_eq!(result.outcome, ReviewOutcome::Invalid);
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.proposals[0].problem, Some("field_not_resolvable"));
    }

    #[test]
    fn report_is_bounded_but_counts_are_complete() {
        let json = export_with(MINIMAL_EXPORT, |value| {
            let template = value["notes"][0].clone();
            for index in 0..(MAX_REPORTED_PROPOSALS + 3) {
                let mut copy = template.clone();
                copy["guid"] = serde_json::json!(format!("bulk-{index}"));
                value["notes"].as_array_mut().expect("notes").push(copy);
            }
        });
        let specs = (0..(MAX_REPORTED_PROPOSALS + 3))
            .map(|index| {
                (
                    Box::leak(format!("bulk-{index}").into_boxed_str()) as &str,
                    "Толкование",
                    "случайность",
                    "новое",
                )
            })
            .collect();
        let result = check(&json, &request(specs));

        assert_eq!(result.proposals_total, MAX_REPORTED_PROPOSALS + 3);
        assert_eq!(result.proposals.len(), MAX_REPORTED_PROPOSALS);
        assert!(result.proposals_truncated);
        assert_eq!(result.counts.valid, MAX_REPORTED_PROPOSALS + 3);
        assert!(
            result.edit_request.is_some(),
            "запрос строится по всем предложениям, а не по отчёту"
        );
        assert_eq!(
            result.edit_request.expect("запрос").edits.len(),
            MAX_REPORTED_PROPOSALS + 3
        );
    }

    #[test]
    fn empty_document_is_rejected_before_any_report() {
        let error = check_error(
            MINIMAL_EXPORT,
            &ProposalDocument {
                proposals: Vec::new(),
            },
        );
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn duplicate_targets_are_rejected_by_the_shared_request_validator() {
        let error = check_error(
            MINIMAL_EXPORT,
            &request(vec![
                ("guid-1", "Толкование", "случайность", "a"),
                ("guid-1", "Толкование", "случайность", "b"),
            ]),
        );
        assert_eq!(error.code, ErrorCode::DuplicateEditTarget);
    }

    #[test]
    fn emitted_request_is_accepted_by_the_edit_boundary_and_writes_nothing() {
        let temp = TempDir::new("review-check-boundary");
        let export = temp.path().join("export");
        std::fs::create_dir_all(&export).expect("каталог экспорта");
        let value: serde_json::Value = serde_json::from_str(MINIMAL_EXPORT).expect("fixture");
        std::fs::write(
            export.join("deck.json"),
            loader::render_canonical_bytes(&value).expect("канонические байты"),
        )
        .expect("deck.json");
        let before = std::fs::read(export.join("deck.json")).expect("байты до");

        let raw = r#"{
            "schema_version": 1,
            "proposals": [
                {"proposal_id": "e1", "guid": "guid-1", "field": "Толкование",
                 "expected": "случайность", "replacement": "случайно"}
            ]
        }"#
        .as_bytes();
        let parsed = crate::proposal::parse_proposal_bytes(raw, "тест").expect("разбор документа");

        let source = edit::read_source(&export).expect("загрузка экспорта");
        let blockers = edit::source_blockers(&source);
        assert!(blockers.is_empty(), "фикстура должна быть правимой");
        let index = ExportIndex::build(&source.root);
        let result = review_check(&export, &index, &parsed, &blockers).expect("проверка");
        let emitted = result.edit_request.expect("запрос");

        let dry_run = edit::edit(&export, &emitted, false).expect("dry-run запроса edit");
        assert!(!dry_run.applied);
        assert_eq!(dry_run.summaries.dry_run, 1);
        assert_eq!(dry_run.effective_edits, 1);
        assert_eq!(
            std::fs::read(export.join("deck.json")).expect("байты после"),
            before,
            "review-check и dry-run не меняют файл"
        );
    }
}
