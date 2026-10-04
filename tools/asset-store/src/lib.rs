//! Общее ядро жизненного цикла ресурсов, принадлежащих программе.
//!
//! Общий слой ничего не знает о CrowdAnki, Yarxi или конкретном типе медиа. В нём
//! хранятся явные идентичности, хеши, сведения о происхождении и семантические
//! решения. Предметный CLI `kanji-assets` использует этот API, не дублируя
//! владение хранилищем.

#[cfg(not(target_os = "linux"))]
compile_error!("asset-store currently supports Linux only");

pub mod batch;
pub mod batch_runtime;
pub mod browser_runtime;
pub mod cli;
pub mod diagnostics;
pub mod domain;
pub mod error;
pub mod hashing;
pub mod jpdb;
pub mod kanji_domain;
pub mod kanji_mask;
pub mod kanji_validator;
pub mod model;
pub mod pitch_accent;
pub mod pitch_batch;
pub mod pitch_cli;
pub mod pitch_review;
pub mod selection;
pub mod store;
pub mod temp_workspace;
pub mod validation;
pub mod yarxi;

#[cfg(test)]
#[path = "batch_runtime_tests.rs"]
mod batch_runtime_tests;

#[cfg(test)]
#[path = "pitch_batch_tests.rs"]
mod pitch_batch_tests;

pub use browser_runtime::{
    BrowserExecutableSelection, BrowserExecutableSource, BrowserRuntimeConfig,
    BrowserRuntimeProvenance, BrowserSession, CdpRuntimeMonitor, DeviceMetrics, NetworkOutcome,
    RuntimeSnapshot, TrackedRequest,
};
pub use domain::{
    AssetDomainPolicy, CanonicalAssetLocation, GenericDomainPolicy, KanjiDomainPolicy,
    TrustSemantics,
};
pub use error::{AssetError, ErrorCode};
pub use model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanAttestation, HumanDecision, LifecycleState,
    Manifest, Provenance, SemanticDecision, SemanticStatus, ValidationEvidence, ValidationRecord,
    ValidatorIdentity,
};
pub use pitch_accent::{
    PitchAccentCaptureRect, PitchAccentCoordinateSpace, PitchAccentDarkThemeProof,
    PitchAccentDomainMetadata, PitchAccentDomainPolicy, PitchAccentEvidence,
    PitchAccentGraphEvidence, PitchAccentImageValidator, PitchAccentProvider,
    PitchAccentRenderEvidence, PitchAccentRenderKind, PitchAccentResolvedForm,
};
pub use selection::{SelectionMode, select_assets};
pub use store::{
    AssetStore, HumanAttestationRequest, IngestOutcome, IngestRequest, StoreOptions,
    VerifiedAssetBytes, VerifiedIngestOutcome, VerifiedIngestRequest,
};
pub use validation::{SemanticValidator, ValidationReport, ValidatorFailure};
