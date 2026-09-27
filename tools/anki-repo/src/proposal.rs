//! Публичный входной контракт `review-check`: документ предложений агента.
//!
//! Это отдельная граница, а не алиас запроса `edit`. Смысл разделения:
//!
//! - `review-check` принимает **недоверенный** документ внешнего агента. Его
//!   форма принадлежит самой команде и версионируется отдельно, поэтому
//!   proposal-specific поля (`proposal_id`, `reason`) не обязаны быть частью
//!   исполняемого запроса.
//! - `edit_request` в результате `review-check` — это **исполнимый** документ
//!   команды `edit`. Он остаётся её контрактом и принимается ею без переупаковки.
//!
//! Общего «одного формата на всё» здесь быть не должно: расширение proposal
//! metadata не имеет права менять write contract, а ужесточение write contract не
//! имеет права молча ломать разбор предложений.
//!
//! Владелец формы — этот модуль; владелец разрешения целей и классификации
//! правок — [`crate::ops::edit`]. Модуль не повторяет `guid` →
//! `note_model_uuid` → `flds[].ord` → `fields` и не дублирует проверку
//! конфликтов: он только разбирает свой документ и компилирует его в
//! [`EditRequest`], после чего работает общий валидатор [`edit::validate_request`].

use serde::Deserialize;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::ops::edit::{self, EditRequest, EditSpec};
use crate::text::bounded_sample;

/// Поддерживаемая версия схемы документа предложений.
pub const SUPPORTED_PROPOSAL_SCHEMA_VERSION: u32 = 1;
/// Жёсткий максимум числа предложений в одном документе.
pub const MAX_PROPOSALS: usize = 20_000;
/// Жёсткий максимум размера документа предложений в байтах.
pub const MAX_PROPOSAL_BYTES: usize = 8 * 1024 * 1024;
/// Имя канала, читаемого вместо файла предложений.
pub const STDIN_PROPOSALS_SOURCE: &str = "-";

/// Одно предложение в разобранном виде.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Необязательный стабильный идентификатор предложения.
    pub proposal_id: Option<String>,
    /// `guid` заметки — цель предложения.
    pub guid: String,
    /// Имя поля модели заметки.
    pub field: String,
    /// Значение, которое агент ожидает увидеть сейчас.
    pub expected: String,
    /// Предлагаемое значение.
    pub replacement: String,
    /// Необязательное пояснение агента; на запись не влияет.
    pub reason: Option<String>,
}

impl Proposal {
    /// Предложение как правка: `proposal_id` становится `edit_id`.
    #[must_use]
    pub fn to_edit_spec(&self) -> EditSpec {
        EditSpec {
            edit_id: self.proposal_id.clone(),
            guid: self.guid.clone(),
            field: self.field.clone(),
            expected: self.expected.clone(),
            replacement: self.replacement.clone(),
        }
    }

    /// Ограниченная выборка пояснения агента.
    ///
    /// `reason` — недоверенный свободный текст, поэтому в отчёт он попадает
    /// только в bounded-форме, как и значения полей.
    #[must_use]
    pub fn bounded_reason(&self) -> Option<String> {
        self.reason.as_deref().map(bounded_sample)
    }
}

/// Разобранный документ предложений.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalDocument {
    /// Предложения в порядке документа.
    pub proposals: Vec<Proposal>,
}

impl ProposalDocument {
    /// Компилирует документ в исполнимый запрос `edit`.
    ///
    /// `reason` в запрос не переносится: это review metadata, а не часть правки.
    /// `expected` переносится как есть — исполнимость проверяет уже
    /// `review-check`, сравнивая его с фактическим значением поля.
    #[must_use]
    pub fn to_edit_request(&self) -> EditRequest {
        EditRequest {
            edits: self.proposals.iter().map(Proposal::to_edit_spec).collect(),
        }
    }
}

/// Разбирает JSON-документ предложений.
///
/// # Errors
///
/// Возвращает [`ErrorCode::InvalidRequest`] для слишком большого, структурно
/// некорректного, пустого документа, неподдерживаемой версии схемы, повторного
/// `proposal_id`, пустого `guid`/`field` и [`ErrorCode::DuplicateEditTarget`],
/// если одна пара «`guid`, поле» предложена дважды.
pub fn parse_proposal_bytes(raw: &[u8], label: &str) -> Result<ProposalDocument, DomainError> {
    if raw.len() > MAX_PROPOSAL_BYTES {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "документ предложений {label} слишком большой: {} байт, максимум {MAX_PROPOSAL_BYTES}",
                raw.len()
            ),
            details! {
                "reason" => "proposals_too_large",
                "source" => label,
                "bytes" => raw.len(),
                "max_bytes" => MAX_PROPOSAL_BYTES,
            },
        ));
    }

    let file: ProposalFile = serde_json::from_slice(raw).map_err(|error| {
        DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!("документ предложений {label} не соответствует схеме: {error}"),
            details! {
                "reason" => "malformed_proposals",
                "source" => label,
                "message" => error.to_string(),
                "line" => error.line(),
                "column" => error.column(),
            },
        )
    })?;

    if file.schema_version != SUPPORTED_PROPOSAL_SCHEMA_VERSION {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "документ предложений {label} имеет schema_version = {}, поддерживается {SUPPORTED_PROPOSAL_SCHEMA_VERSION}",
                file.schema_version
            ),
            details! {
                "reason" => "unsupported_schema_version",
                "source" => label,
                "observed" => file.schema_version,
                "expected" => SUPPORTED_PROPOSAL_SCHEMA_VERSION,
            },
        ));
    }

    if file.proposals.is_empty() {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            "документ предложений не содержит ни одного предложения",
            details! {
                "reason" => "empty_proposals",
                "source" => label,
            },
        ));
    }

    if file.proposals.len() > MAX_PROPOSALS {
        return Err(DomainError::with_details(
            ErrorCode::InvalidRequest,
            format!(
                "в документе предложений {} предложений, максимум {MAX_PROPOSALS}",
                file.proposals.len()
            ),
            details! {
                "reason" => "too_many_proposals",
                "source" => label,
                "proposals" => file.proposals.len(),
                "max_proposals" => MAX_PROPOSALS,
            },
        ));
    }

    let document = ProposalDocument {
        proposals: file
            .proposals
            .into_iter()
            .map(|entry| Proposal {
                proposal_id: entry.proposal_id,
                guid: entry.guid,
                field: entry.field,
                expected: entry.expected,
                replacement: entry.replacement,
                reason: entry.reason,
            })
            .collect(),
    };

    ensure_unique_proposal_ids(&document, label)?;

    // Целевые проверки (пустой `guid`/`field`, повтор пары «`guid`, поле») —
    // общие с `edit`: это одна и та же операция над одним и тем же экспортом.
    edit::validate_request(&document.to_edit_request())?;

    Ok(document)
}

/// Проверяет уникальность `proposal_id` внутри документа.
fn ensure_unique_proposal_ids(document: &ProposalDocument, label: &str) -> Result<(), DomainError> {
    let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (position, proposal) in document.proposals.iter().enumerate() {
        let Some(proposal_id) = proposal.proposal_id.as_deref() else {
            continue;
        };
        if let Some(previous) = seen.insert(proposal_id, position) {
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                format!(
                    "proposal_id {proposal_id:?} повторяется: предложения #{previous} и #{position}"
                ),
                details! {
                    "reason" => "duplicate_proposal_id",
                    "source" => label,
                    "proposal_id" => proposal_id,
                    "first_proposal_index" => previous,
                    "duplicate_proposal_index" => position,
                },
            ));
        }
    }
    Ok(())
}

/// Форма документа предложений на проводе.
///
/// `deny_unknown_fields` здесь принципиален: опечатка в имени поля должна быть
/// отказом, а не молча проигнорированным предложением.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalFile {
    schema_version: u32,
    proposals: Vec<ProposalEntry>,
}

/// Одно предложение на проводе.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalEntry {
    #[serde(default)]
    proposal_id: Option<String>,
    guid: String,
    field: String,
    expected: String,
    replacement: String,
    #[serde(default)]
    reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<ProposalDocument, DomainError> {
        parse_proposal_bytes(json.as_bytes(), "тест")
    }

    fn document(proposals: Vec<Proposal>) -> ProposalDocument {
        ProposalDocument { proposals }
    }

    #[test]
    fn minimal_document_is_accepted() {
        let document = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"guid": "guid-1", "field": "Значение", "expected": "", "replacement": "x"}
               ]}"#,
        )
        .expect("валидный документ");

        assert_eq!(document.proposals.len(), 1);
        assert_eq!(document.proposals[0].proposal_id, None);
        assert_eq!(document.proposals[0].reason, None);
        assert_eq!(document.proposals[0].to_edit_spec().edit_id, None);
    }

    #[test]
    fn proposal_metadata_is_kept_and_compiled_into_the_request() {
        let document = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"proposal_id": "p1", "guid": "guid-1", "field": "Значение",
                  "expected": "было", "replacement": "стало", "reason": "опечатка"}
               ]}"#,
        )
        .expect("валидный документ");

        assert_eq!(
            document.proposals[0].bounded_reason().as_deref(),
            Some("опечатка")
        );

        let request = document.to_edit_request();
        assert_eq!(request.edits.len(), 1);
        assert_eq!(request.edits[0].edit_id.as_deref(), Some("p1"));
        assert_eq!(request.edits[0].expected, "было");
        assert_eq!(request.edits[0].replacement, "стало");
    }

    #[test]
    fn reason_is_bounded_in_the_report() {
        let long = "я".repeat(crate::text::VALUE_SAMPLE_CHARS + 10);
        let document = document(vec![Proposal {
            proposal_id: None,
            guid: "guid-1".to_string(),
            field: "Значение".to_string(),
            expected: String::new(),
            replacement: "x".to_string(),
            reason: Some(long),
        }]);

        let bounded = document.proposals[0].bounded_reason().expect("reason");
        assert_eq!(
            bounded.chars().count(),
            crate::text::VALUE_SAMPLE_CHARS + 1,
            "выборка обрезана и помечена многоточием"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"guid": "guid-1", "field": "Значение", "expected": "", "replacement": "x",
                  "confidence": 0.9}
               ]}"#,
        )
        .expect_err("неизвестное поле");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "malformed_proposals");

        let error = parse(r#"{"schema_version": 1, "edits": [], "proposals": []}"#)
            .expect_err("чужой документ");
        assert_eq!(error.details["reason"], "malformed_proposals");
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let error = parse(
            r#"{"schema_version": 2, "proposals": [
                 {"guid": "guid-1", "field": "Значение", "expected": "", "replacement": "x"}
               ]}"#,
        )
        .expect_err("версия схемы");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "unsupported_schema_version");
        assert_eq!(error.details["observed"], 2);
        assert_eq!(error.details["expected"], SUPPORTED_PROPOSAL_SCHEMA_VERSION);
    }

    #[test]
    fn empty_document_is_rejected() {
        let error = parse(r#"{"schema_version": 1, "proposals": []}"#).expect_err("пустой");
        assert_eq!(error.details["reason"], "empty_proposals");
    }

    #[test]
    fn malformed_document_is_rejected() {
        for raw in ["{", r#"{"schema_version": 1}"#, r#"{"proposals": []}"#] {
            let error = parse(raw).expect_err("битый документ");
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{raw}");
            assert_eq!(error.details["reason"], "malformed_proposals", "{raw}");
        }
    }

    #[test]
    fn oversized_document_is_rejected() {
        let raw = vec![b' '; MAX_PROPOSAL_BYTES + 1];
        let error = parse_proposal_bytes(&raw, "тест").expect_err("слишком большой");
        assert_eq!(error.details["reason"], "proposals_too_large");
    }

    #[test]
    fn duplicate_proposal_id_is_rejected() {
        let error = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"proposal_id": "p", "guid": "guid-1", "field": "a", "expected": "", "replacement": "x"},
                 {"proposal_id": "p", "guid": "guid-2", "field": "a", "expected": "", "replacement": "y"}
               ]}"#,
        )
        .expect_err("повтор proposal_id");

        assert_eq!(error.details["reason"], "duplicate_proposal_id");
        assert_eq!(error.details["duplicate_proposal_index"], 1);
    }

    #[test]
    fn duplicate_target_is_rejected_by_the_shared_validator() {
        let error = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"guid": "guid-1", "field": "a", "expected": "", "replacement": "x"},
                 {"guid": "guid-1", "field": "a", "expected": "", "replacement": "y"}
               ]}"#,
        )
        .expect_err("повтор цели");

        assert_eq!(error.code, ErrorCode::DuplicateEditTarget);
    }

    #[test]
    fn empty_guid_and_field_are_rejected_by_the_shared_validator() {
        let empty_guid = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"guid": "", "field": "a", "expected": "", "replacement": "x"}
               ]}"#,
        )
        .expect_err("пустой guid");
        assert_eq!(empty_guid.details["reason"], "empty_guid");

        let empty_field = parse(
            r#"{"schema_version": 1, "proposals": [
                 {"guid": "guid-1", "field": "", "expected": "", "replacement": "x"}
               ]}"#,
        )
        .expect_err("пустое поле");
        assert_eq!(empty_field.details["reason"], "empty_field");
    }
}
