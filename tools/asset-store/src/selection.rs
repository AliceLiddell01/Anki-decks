//! Детерминированный выбор целей semantic validation.

use crate::model::{AssetRecord, HumanDecision, ValidatorIdentity};

/// Режим выбора ассетов для проверки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Только assets без актуального decision для hash + validator id/version.
    New,
    /// Все активные assets из manifest этого store.
    Full,
}

impl SelectionMode {
    /// Machine-readable имя режима.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Full => "full",
        }
    }
}

/// Возвращает записи в устойчивом порядке namespace/key.
pub fn select_assets<'a>(
    assets: &'a [AssetRecord],
    mode: SelectionMode,
    validator: &ValidatorIdentity,
) -> Vec<&'a AssetRecord> {
    let mut selected: Vec<_> = assets
        .iter()
        .filter(|asset| match mode {
            SelectionMode::Full => true,
            SelectionMode::New => {
                let complete_human_approval = asset.current_human_decision()
                    == Some(HumanDecision::Approve)
                    && asset.has_current_validation()
                    && asset.effective_status() == Some(crate::model::SemanticStatus::Verified);
                let current_automated_decision =
                    asset.validation.as_ref().is_some_and(|decision| {
                        decision.is_valid_for_sha(&asset.sha256) && decision.validator == *validator
                    });
                !complete_human_approval && !current_automated_decision
            }
        })
        .collect();
    selected.sort_by(|left, right| left.identity.cmp(&right.identity));
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssetIdentity, DetectedFormat, LifecycleState, Provenance, SemanticStatus,
        ValidationEvidence, ValidationRecord,
    };

    fn asset(key: &str, hash: &str, validation: Option<ValidationRecord>) -> AssetRecord {
        AssetRecord {
            identity: AssetIdentity::new("generic", key).expect("identity valid"),
            storage_path: format!("pending/{hash}.blob"),
            consumer_filename: format!("{hash}.bin"),
            sha256: hash.to_owned(),
            byte_length: 1,
            format: DetectedFormat::Unknown,
            provenance: Provenance {
                source_kind: "local_import".to_owned(),
                source_name: "fixture.bin".to_owned(),
            },
            lifecycle: LifecycleState::Pending,
            validation,
            human_attestation: None,
            domain_metadata: None,
        }
    }

    fn validation(hash: &str, validator: &ValidatorIdentity) -> ValidationRecord {
        ValidationRecord {
            status: SemanticStatus::Verified,
            validator: validator.clone(),
            content_sha256: hash.to_owned(),
            evidence: vec![ValidationEvidence {
                kind: "test".to_owned(),
                summary: "synthetic result".to_owned(),
                details: None,
            }],
        }
    }

    #[test]
    fn new_requires_current_hash_and_validator_version_while_full_selects_all() {
        let v1 = ValidatorIdentity::new("fixture", "1").expect("validator valid");
        let v2 = ValidatorIdentity::new("fixture", "2").expect("validator valid");
        let current_sha = "a".repeat(64);
        let changed_sha = "b".repeat(64);
        let fresh_sha = "c".repeat(64);
        let old_sha = "d".repeat(64);
        let assets = vec![
            asset("already", &current_sha, Some(validation(&current_sha, &v1))),
            asset("changed", &changed_sha, Some(validation(&old_sha, &v1))),
            asset("fresh", &fresh_sha, None),
        ];

        let selected: Vec<_> = select_assets(&assets, SelectionMode::New, &v1)
            .into_iter()
            .map(|record| record.identity.key.as_str())
            .collect();
        assert_eq!(selected, ["changed", "fresh"]);
        assert_eq!(select_assets(&assets, SelectionMode::New, &v2).len(), 3);
        assert_eq!(select_assets(&assets, SelectionMode::Full, &v1).len(), 3);
    }

    #[test]
    fn new_does_not_repeat_automated_validation_for_exact_human_approval() {
        use crate::model::{HumanAttestation, HumanDecision};

        let current = ValidatorIdentity::new("fixture", "1").expect("validator valid");
        let replacement = ValidatorIdentity::new("fixture", "2").expect("validator valid");
        let approved_sha = "a".repeat(64);
        let rejected_sha = "b".repeat(64);
        let mut approved = asset(
            "approved",
            &approved_sha,
            Some(validation(&approved_sha, &current)),
        );
        approved.human_attestation = Some(HumanAttestation {
            identity: approved.identity.clone(),
            content_sha256: approved.sha256.clone(),
            decision: HumanDecision::Approve,
            reason: "проверено человеком".into(),
        });

        assert!(select_assets(&[approved], SelectionMode::New, &current).is_empty());
        let mut rejected = asset("rejected", &rejected_sha, None);
        rejected.human_attestation = Some(HumanAttestation {
            identity: rejected.identity.clone(),
            content_sha256: rejected.sha256.clone(),
            decision: HumanDecision::Reject,
            reason: "кандидат отклонён".into(),
        });
        assert_eq!(
            select_assets(&[rejected], SelectionMode::New, &replacement).len(),
            1
        );
    }
}
