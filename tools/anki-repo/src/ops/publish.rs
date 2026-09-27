//! Общая публикация кандидата структурной операции.
//!
//! `create` и `retire` меняют структуру экспорта, но заканчиваться обязаны тем
//! же, чем `edit`: канонические байты кандидата, повторный разбор в то же
//! значение, типизированный корень, отсутствие новых `ERROR` и единственная
//! точка записи — [`crate::write::replace_atomically`]. Второй реализации
//! атомарной записи в toolkit'е нет и не должно появиться: она владеет
//! блокировкой `flock(2)`, проверкой «источник не изменился» и публикацией
//! через временный файл.
//!
//! Здесь же объясняется, почему отдельная проверка «diff байтов равен
//! ожидаемому» структурным операциям не нужна. Исходник обязан быть в
//! канонической форме (это уже проверено при загрузке), а
//! [`loader::render_canonical_bytes`] — чистая функция значения. Поэтому
//! `candidate_bytes == render_canonical_bytes(candidate_value)`, и если значение
//! кандидата отличается от значения исходника только в проверенных местах
//! (это доказывает [`crate::ops::structural`]), то байты кандидата отличаются
//! от байтов исходника ровно там же. Никакого «заодно переформатировали
//! остальной файл» в такой схеме быть не может.

use std::path::Path;

use serde_json::Value;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::loader;
use crate::model::DeckNode;
use crate::ops::source::{EditableSource, ValidationDelta, internal, validation_delta};
use crate::ops::validate;
use crate::write;

/// Проверенный кандидат структурной операции.
#[derive(Debug)]
pub struct Candidate {
    /// Канонические байты, которые будут записаны при `--apply`.
    pub bytes: Vec<u8>,
    /// Значение кандидата: то, что рендерилось в [`Candidate::bytes`].
    pub value: Value,
    /// Типизированный корень кандидата.
    pub root: DeckNode,
    /// Сравнение валидации исходника и кандидата.
    pub validation: ValidationDelta,
}

/// Результат публикации кандидата.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publication {
    /// Кандидат записан в `deck.json`.
    pub applied: bool,
    /// Команда работала как dry-run.
    pub dry_run: bool,
}

/// Проверяет значение кандидата и собирает его байты.
///
/// Порядок проверок: канонический рендер → повторный разбор в то же значение →
/// типизированный корень → `validate` кандидата → отсутствие новых `ERROR`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::Internal`] при расхождении повторного разбора,
/// ошибки разбора типизированного корня и [`ErrorCode::ExportInvalid`], если
/// кандидат получает `ERROR`, которых не было в исходном экспорте.
pub fn prepare(
    source: &EditableSource,
    export_dir: &Path,
    value: Value,
) -> Result<Candidate, DomainError> {
    let bytes = loader::render_canonical_bytes(&value)?;
    let reparsed = loader::parse_deck_json_bytes(&bytes, &source.deck_json)?;
    if reparsed != value {
        return Err(internal(format!(
            "кандидат {} не повторяет задуманное значение после сериализации",
            source.deck_json.display()
        )));
    }

    let root = loader::typed_root(reparsed, &source.deck_json)?;
    let after = validate::validate_document(&root, export_dir);
    let validation = validation_delta(&source.before, &after);

    if validation.has_new_errors() {
        return Err(DomainError::with_details(
            ErrorCode::ExportInvalid,
            format!(
                "кандидат получает ERROR, которых не было в исходном экспорте: {}",
                validation.new_error_codes.join(", ")
            ),
            details! {
                "phase" => "candidate",
                "path" => source.deck_json.display().to_string(),
                "errors_before" => validation.before.errors,
                "errors_after" => validation.after.errors,
                "new_error_codes" => validation.new_error_codes.clone(),
            },
        ));
    }

    Ok(Candidate {
        bytes,
        value,
        root,
        validation,
    })
}

/// Публикует кандидата: пишет файл только при `apply`.
///
/// # Errors
///
/// Возвращает [`ErrorCode::WriteFailed`]/[`ErrorCode::SourceChanged`] из
/// [`write::replace_atomically`].
pub fn publish(
    source: &EditableSource,
    candidate: &Candidate,
    apply: bool,
) -> Result<Publication, DomainError> {
    if apply {
        write::replace_atomically(&source.deck_json, &source.source, &candidate.bytes)?;
    }

    Ok(Publication {
        applied: apply,
        dry_run: !apply,
    })
}

/// Разница размеров в байтах между кандидатом и исходником.
#[must_use]
pub fn byte_delta(source: &EditableSource, candidate: &Candidate) -> i64 {
    candidate.bytes.len() as i64 - source.source.len() as i64
}
