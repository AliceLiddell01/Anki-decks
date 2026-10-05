//! Единая цель интеграционных тестов для asset-store.
//!
//! Исходные contract-файлы остаются отдельными модулями, но Cargo компилирует
//! и связывает их в один исполняемый файл тестов вместо отдельного файла на модуль.

#[path = "asset_store_contract.rs"]
mod asset_store_contract;
#[path = "browser_acquisition_diagnostics_contract.rs"]
mod browser_acquisition_diagnostics_contract;
#[path = "cli_contract.rs"]
mod cli_contract;
#[path = "domain_semantics_regression.rs"]
mod domain_semantics_regression;
#[path = "pitch_cli_contract.rs"]
mod pitch_cli_contract;
#[path = "pitch_corpus_trust_contract.rs"]
mod pitch_corpus_trust_contract;
#[path = "temp_workspace_contract.rs"]
mod temp_workspace_contract;
