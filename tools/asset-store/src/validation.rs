//! Расширяемый контракт семантической проверки.

use std::io::Read;

use serde::Serialize;

use crate::model::{
    AssetIdentity, AssetRecord, LifecycleState, SemanticDecision, ValidatorIdentity,
};
use crate::selection::SelectionMode;

/// Контракт валидатора домена. Общий слой по умолчанию не содержит валидатора.
pub trait SemanticValidator {
    /// Устойчивые идентификатор `id` и версия `version` алгоритма.
    fn identity(&self) -> ValidatorIdentity;

    /// Принимает фактические байты и возвращает решение со свидетельствами либо
    /// техническую ошибку. Для автоматического присвоения `VERIFIED` решение
    /// должно быть положительным и содержать непустые свидетельства. Отдельное
    /// одобрение через `AssetStore::attest(Approve)` возможно при актуальной
    /// автоматической записи для текущего SHA-256 и не отменяет `CORRUPT`.
    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure>;
}

/// Технический отказ валидатора; он не сохраняется как семантическое решение.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorFailure {
    pub code: String,
    pub message: String,
}

impl ValidatorFailure {
    /// Создаёт техническую ошибку с устойчивым кодом.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Отчёт о попытке семантической проверки одного объекта.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationAttempt {
    pub identity: AssetIdentity,
    pub from_state: LifecycleState,
    pub to_state: LifecycleState,
    pub content_sha256: String,
    pub status: Option<crate::model::SemanticStatus>,
    pub evidence: Vec<crate::model::ValidationEvidence>,
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker_message: Option<String>,
}

/// Результат работы общего механизма проверки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationReport {
    pub mode: String,
    pub validator: ValidatorIdentity,
    pub considered: usize,
    pub changed: usize,
    pub attempts: Vec<ValidationAttempt>,
    pub blockers: Vec<String>,
}

impl ValidationReport {
    pub(crate) fn new(mode: SelectionMode, validator: ValidatorIdentity) -> Self {
        Self {
            mode: mode.as_str().to_owned(),
            validator,
            considered: 0,
            changed: 0,
            attempts: Vec::new(),
            blockers: Vec::new(),
        }
    }
}
