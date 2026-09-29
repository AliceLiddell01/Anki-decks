//! Детерминированный выбор целей semantic validation.

use crate::model::{AssetRecord, ValidatorIdentity};

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
            SelectionMode::New => !asset.validation.as_ref().is_some_and(|decision| {
                decision.content_sha256 == asset.sha256 && decision.validator == *validator
            }),
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
            sha256: hash.to_owned(),
            byte_length: 1,
            format: DetectedFormat::Unknown,
            provenance: Provenance {
                source_kind: "local_import".to_owned(),
                source_name: "fixture.bin".to_owned(),
            },
            lifecycle: LifecycleState::Pending,
            validation,
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
        let assets = vec![
            asset("already", "a", Some(validation("a", &v1))),
            asset("changed", "b", Some(validation("old-hash", &v1))),
            asset("fresh", "c", None),
        ];

        let selected: Vec<_> = select_assets(&assets, SelectionMode::New, &v1)
            .into_iter()
            .map(|record| record.identity.key.as_str())
            .collect();
        assert_eq!(selected, ["changed", "fresh"]);
        assert_eq!(select_assets(&assets, SelectionMode::New, &v2).len(), 3);
        assert_eq!(select_assets(&assets, SelectionMode::Full, &v1).len(), 3);
    }
}
