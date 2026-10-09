//! Свидетельства для независимого ревью кода: охват снимка, диагностики,
//! кандидаты, языковая политика и дельта на уровне детекторов.
//!
//! Ни одно свидетельство или кандидат автоматически не становится замечанием.

pub mod delta;
pub mod detectors;
pub mod diagnostics;
pub mod execution;
pub mod language;
pub mod learning;
pub mod model;
pub mod review_queue;
pub mod rust_context;
pub mod scope;
pub mod semantic_triage;
pub mod workflow;
