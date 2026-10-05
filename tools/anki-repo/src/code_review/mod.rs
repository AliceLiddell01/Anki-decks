//! Evidence для независимого code review: snapshot scope, diagnostics,
//! candidates, language policy и detector-level delta.
//!
//! Ни один элемент evidence или candidate автоматически не становится finding.

pub mod delta;
pub mod detectors;
pub mod diagnostics;
pub mod language;
pub mod model;
pub mod scope;
pub mod workflow;
