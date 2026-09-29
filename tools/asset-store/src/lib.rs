//! Общий lifecycle core для program-owned assets.
//!
//! Core ничего не знает о CrowdAnki, Yarxi или конкретном типе медиа. В нём
//! хранятся явные identities, hashes, provenance и semantic decisions. Предметный
//! CLI `kanji-assets` использует этот API, не дублируя владение store.

pub mod cli;
pub mod error;
pub mod model;
pub mod selection;
pub mod store;
pub mod validation;

pub use error::{AssetError, ErrorCode};
pub use model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, Manifest, Provenance,
    SemanticDecision, SemanticStatus, ValidationEvidence, ValidationRecord, ValidatorIdentity,
};
pub use selection::{SelectionMode, select_assets};
pub use store::{AssetStore, IngestOutcome, IngestRequest, StoreOptions};
pub use validation::{SemanticValidator, ValidationReport, ValidatorFailure};
