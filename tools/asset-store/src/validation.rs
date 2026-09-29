//! Расширяемый semantic validation contract.

use std::io::Read;

use serde::Serialize;

use crate::model::{
    AssetIdentity, AssetRecord, LifecycleState, SemanticDecision, ValidatorIdentity,
};
use crate::selection::SelectionMode;

/// Контракт domain validator'а. Production core не содержит validator по умолчанию.
pub trait SemanticValidator {
    /// Устойчивые id и версия алгоритма.
    fn identity(&self) -> ValidatorIdentity;

    /// Принимает фактические bytes и возвращает decision с evidence либо
    /// техническую ошибку. `VERIFIED` появится только если этот вызов вернёт
    /// положительное решение с непустым evidence.
    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure>;
}

/// Технический отказ validator'а; он не сохраняется как semantic decision.
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

/// Отчёт о попытке semantic validation одного объекта.
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

/// Результат запуска общего validator core.
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
