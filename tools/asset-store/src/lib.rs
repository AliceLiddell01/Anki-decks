//! Общий lifecycle core для program-owned assets.
//!
//! Core ничего не знает о CrowdAnki, Yarxi или конкретном типе медиа. В нём
//! хранятся явные identities, hashes, provenance и semantic decisions. Предметный
//! CLI `kanji-assets` использует этот API, не дублируя владение store.

#[cfg(not(target_os = "linux"))]
compile_error!("asset-store currently supports Linux only");

pub mod batch;
pub mod batch_runtime;
pub mod browser_runtime;
pub mod cli;
pub mod domain;
pub mod error;
pub mod hashing;
pub mod jpdb;
pub mod kanji_domain;
pub mod kanji_mask;
pub mod kanji_validator;
pub mod model;
pub mod pitch_accent;
pub mod selection;
pub mod store;
pub mod validation;
pub mod yarxi;

#[cfg(test)]
#[path = "batch_runtime_tests.rs"]
mod batch_runtime_tests;

pub use browser_runtime::{
    BrowserExecutableSelection, BrowserExecutableSource, BrowserRuntimeConfig,
    BrowserRuntimeProvenance, BrowserSession, CdpRuntimeMonitor, DeviceMetrics, NetworkOutcome,
    RuntimeSnapshot, TrackedRequest,
};
pub use domain::{
    AssetDomainPolicy, CanonicalAssetLocation, GenericDomainPolicy, KanjiDomainPolicy,
};
pub use error::{AssetError, ErrorCode};
pub use model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanAttestation, HumanDecision, LifecycleState,
    Manifest, Provenance, SemanticDecision, SemanticStatus, ValidationEvidence, ValidationRecord,
    ValidatorIdentity,
};
pub use pitch_accent::{
    PitchAccentCapturePadding, PitchAccentCaptureRect, PitchAccentCoordinateSpace,
    PitchAccentDarkThemeProof, PitchAccentDomainMetadata, PitchAccentDomainPolicy,
    PitchAccentEvidence, PitchAccentGraphEvidence, PitchAccentImageValidator, PitchAccentProvider,
    PitchAccentRenderEvidence, PitchAccentRenderKind, PitchAccentResolvedForm,
};
pub use selection::{SelectionMode, select_assets};
pub use store::{
    AssetStore, HumanAttestationRequest, IngestOutcome, IngestRequest, StoreOptions,
    VerifiedAssetBytes, VerifiedIngestOutcome, VerifiedIngestRequest,
};
pub use validation::{SemanticValidator, ValidationReport, ValidatorFailure};
